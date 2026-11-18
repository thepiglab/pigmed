//! Per published chunk shard (`chunks/<name>.parquet` + `media/<name>.parquet`): for every kept article (train split,
//! commercial licence, not retracted, not held out) fetch its JATS once and write (formats match the
//! trainer in train/):
//!   articles.jsonl.gz      S2: {article_id, group, title, abstract, chunks:[{id, heading, text}]} body chunks in order
//!   ctx_queries.jsonl.gz   S2 ctx_queries (kind=caption): caption -> chunk(s) holding the paragraph citing the figure
//!   figures.jsonl.gz       S3: one row per figure {id, article_id, group, caption, mentions, licence, s3_key, md5, ...}
//!   cite_ctx.jsonl.gz      citation contexts with the cited reference ids (resolved globally by `cite-resolve`)
//!   windows.jsonl.gz       optional (--windows): 8k-token windows of whole chunks, title + abstract prefix
//!   stats.json

use crate::units::{read_chunks, read_media, ArticleUnits, Media};
use crate::xmlctx::{self, Ctx};
use anyhow::{anyhow, bail, Result};
use flate2::{write::GzEncoder, Compression};
use rayon::prelude::*;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

pub const S3_BASE: &str = "https://pmc-oa-opendata.s3.amazonaws.com/";

/// Kept article (group). From pigmed-synth `pairs-keep` (keep.tsv: id, group, journal, abs_ok).
pub struct Keep {
    pub group: String,
}

pub fn read_keep(p: &Path, exclude: &HashSet<String>) -> Result<HashMap<String, Keep>> {
    let mut m = HashMap::new();
    for line in crate::io::reader(p)?.lines() {
        let line = line?;
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() >= 2 && !exclude.contains(f[0]) && !exclude.contains(f[0].split('.').next().unwrap_or("")) {
            m.insert(f[0].to_string(), Keep { group: f[1].to_string() });
        }
    }
    Ok(m)
}

// ---------------------------------------------------------------- tokens

/// Token counter: exact EG2 (HF tokenizer.json) or an estimate from words.
pub enum Counter {
    #[cfg(feature = "tokens")]
    Hf(tokenizers::Tokenizer),
    Words(f64),
}

impl Counter {
    pub fn load(path: Option<&Path>, per_word: f64) -> Result<Counter> {
        match path {
            #[cfg(feature = "tokens")]
            Some(p) => Ok(Counter::Hf(tokenizers::Tokenizer::from_file(p).map_err(|e| anyhow!("tokenizer {}: {e}", p.display()))?)),
            #[cfg(not(feature = "tokens"))]
            Some(_) => bail!("built without the tokens feature"),
            None => Ok(Counter::Words(per_word)),
        }
    }
    pub fn name(&self) -> String {
        match self {
            #[cfg(feature = "tokens")]
            Counter::Hf(_) => "eg2".into(),
            Counter::Words(f) => format!("words*{f}"),
        }
    }
    pub fn count(&self, s: &str) -> usize {
        match self {
            #[cfg(feature = "tokens")]
            Counter::Hf(t) => t.encode(s, false).map(|e| e.len()).unwrap_or_else(|_| (s.split_whitespace().count() as f64 * 1.5).ceil() as usize),
            Counter::Words(f) => (s.split_whitespace().count() as f64 * f).ceil() as usize,
        }
    }
}

// ---------------------------------------------------------------- S2 windows

#[derive(Clone, Copy, Debug)]
pub struct WinCfg {
    /// token budget of a window's text (prompt headroom left to the trainer)
    pub max_tokens: usize,
    /// token cap of the title + abstract prefix on windows after the first
    pub prefix_tokens: usize,
    /// tokens shared by consecutive windows (whole chunks)
    pub overlap_tokens: usize,
}

impl Default for WinCfg {
    fn default() -> Self {
        WinCfg { max_tokens: 8064, prefix_tokens: 768, overlap_tokens: 2048 }
    }
}

const SEP: &str = "\n\n";

fn cut_words(s: &str, n: usize) -> String {
    s.split_whitespace().take(n).collect::<Vec<_>>().join(" ")
}

/// Windows of whole chunks. Window 0 = title, then abstract and body chunks in order; windows >= 1 = title +
/// abstract prefix (not chunks), then body chunks. Consecutive windows share >= `overlap_tokens` of whole chunks
/// when the chunks allow; every chunk appears in at least one window.
pub fn windows(a: &ArticleUnits, group: &str, c: &Counter, cfg: WinCfg) -> Vec<Value> {
    let toks: Vec<usize> = a.units.iter().map(|u| c.count(&u.text)).collect();
    let sep = c.count("x\n\ny").saturating_sub(c.count("x y")).max(0) + 1;
    let title = a.title.trim().to_string();
    let title_t = if title.is_empty() { 0 } else { c.count(&title) + sep };
    let abs: Vec<usize> = (0..a.units.len()).filter(|&i| a.units[i].is_abstract).collect();
    let body: Vec<usize> = (0..a.units.len()).filter(|&i| !a.units[i].is_abstract).collect();
    // prefix for windows >= 1: title + abstract units up to prefix_tokens (the last one cut by words)
    let mut prefix = title.clone();
    let mut pt = title_t;
    for &i in &abs {
        let room = cfg.prefix_tokens.saturating_sub(pt);
        if room < 16 {
            break;
        }
        let t = if toks[i] + sep <= room { a.units[i].text.clone() } else {
            let words = a.units[i].text.split_whitespace().count();
            cut_words(&a.units[i].text, (words * room / (toks[i] + sep).max(1)).max(1))
        };
        pt += c.count(&t) + sep;
        prefix = if prefix.is_empty() { t } else { format!("{prefix}{SEP}{t}") };
    }
    let mut out = vec![];
    let make = |w: usize, head: &str, idx: &[usize]| -> Value {
        let mut text = head.to_string();
        let mut chunks = vec![];
        for &i in idx {
            if !text.is_empty() {
                text.push_str(SEP);
            }
            let start = text.chars().count();
            text.push_str(&a.units[i].text);
            chunks.push(json!({"unit_id": a.units[i].unit_id, "start": start, "end": text.chars().count(),
                "section": a.units[i].section, "is_abstract": a.units[i].is_abstract, "tokens": toks[i]}));
        }
        json!({"window_id": format!("{}#w{w}", a.article_id), "article_id": a.article_id,
            "pmcid": a.article_id.split('.').next().unwrap_or(""), "doi": a.doi, "license_code": a.license_code, "group": group,
            "w": w, "prefix": if w == 0 { String::new() } else { head.to_string() }, "text": text, "chunks": chunks,
            "tokens": c.count(&text), "tokenizer": c.name()})
    };
    // window 0: title + abstract + body until the budget
    let mut cur: Vec<usize> = abs.clone();
    let mut used = title_t + abs.iter().map(|&i| toks[i] + sep).sum::<usize>();
    let mut k = 0;
    while k < body.len() && used + toks[body[k]] + sep <= cfg.max_tokens {
        used += toks[body[k]] + sep;
        cur.push(body[k]);
        k += 1;
    }
    if k == 0 && !body.is_empty() && cur.is_empty() {
        cur.push(body[0]);
        k = 1;
    }
    out.push(make(0, &title, &cur));
    let mut w = 1;
    while k < body.len() {
        // overlap: step back over whole body chunks of the previous window up to overlap_tokens
        let mut start = k;
        let mut ov = 0;
        while start > 0 && cur.contains(&body[start - 1]) && ov + toks[body[start - 1]] + sep <= cfg.overlap_tokens {
            start -= 1;
            ov += toks[body[start]] + sep;
        }
        let mut idx = vec![];
        let mut used = pt;
        let mut j = start;
        while j < body.len() && (idx.is_empty() || used + toks[body[j]] + sep <= cfg.max_tokens) {
            used += toks[body[j]] + sep;
            idx.push(body[j]);
            j += 1;
        }
        if j <= k {
            // no progress possible with this overlap: take at least one new chunk
            idx = vec![body[k]];
            j = k + 1;
        }
        out.push(make(w, &prefix, &idx));
        cur = idx;
        k = j;
        w += 1;
    }
    let n = out.len();
    for v in out.iter_mut() {
        v["n_windows"] = json!(n);
    }
    out
}

// ---------------------------------------------------------------- S2 articles

/// S2 article row; `ids_only` leaves chunk text out (the trainer joins it from the published chunk parquet).
pub fn article_row(a: &ArticleUnits, group: &str, ids_only: bool) -> Value {
    let abs: Vec<&str> = a.units.iter().filter(|u| u.is_abstract).map(|u| u.text.as_str()).collect();
    let chunks: Vec<Value> = a.units.iter().filter(|u| !u.is_abstract).map(|u| {
        // heading = exactly the v2 embed heading (section[0]); the full path is a separate, unrendered field
        let mut c = json!({"id": u.unit_id, "heading": u.heading, "section": u.section});
        if !ids_only {
            c["text"] = json!(u.text);
        }
        c
    }).collect();
    json!({"article_id": a.article_id, "group": group, "title": a.title, "abstract": abs.join("\n"), "chunks": chunks,
        "pmcid": a.article_id.split('.').next().unwrap_or(""), "doi": a.doi, "license_code": a.license_code})
}

// ---------------------------------------------------------------- shared text helpers

fn norm_words(s: &str) -> Vec<String> {
    s.split(|ch: char| !ch.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

fn shingles(w: &[String], n: usize) -> HashSet<String> {
    if w.len() < n {
        return if w.is_empty() { HashSet::new() } else { [w.join(" ")].into() };
    }
    w.windows(n).map(|x| x.join(" ")).collect()
}

/// Body chunks that hold a paragraph: share >= half of its 6-word shingles (or the best chunk when it shares >= 30%).
pub fn chunks_holding(a: &ArticleUnits, para: &str) -> Vec<String> {
    let ps = shingles(&norm_words(para), 6);
    if ps.is_empty() {
        return vec![];
    }
    let scored: Vec<(f64, &str)> = a.units.iter().filter(|u| !u.is_abstract).map(|u| {
        let us = shingles(&norm_words(&u.text), 6);
        (ps.intersection(&us).count() as f64 / ps.len() as f64, u.unit_id.as_str())
    }).collect();
    let hit: Vec<String> = scored.iter().filter(|(f, _)| *f >= 0.5).map(|(_, u)| u.to_string()).collect();
    if !hit.is_empty() {
        return hit;
    }
    scored.iter().filter(|(f, _)| *f >= 0.3).max_by(|x, y| x.0.total_cmp(&y.0)).map(|(_, u)| vec![u.to_string()]).unwrap_or_default()
}

pub fn rights_phrase(s: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\b(reproduced|reprinted|adapted|modified|redrawn|taken) (with permission )?(from|by)\b|with (kind )?permission|©|\(c\) ?\d{4}|copyright|courtesy of|all rights reserved|licensed under|creative commons|cc[- ]by").unwrap())
        .is_match(s)
}

/// Caption as a query: label ("Figure 2.", "Fig. 2:") removed from the start.
pub fn caption_query(caption: &str, label: &str) -> String {
    static L: OnceLock<Regex> = OnceLock::new();
    let re = L.get_or_init(|| Regex::new(r"^(?i)\s*(supplementary |extended data )?(fig(ure)?s?\.?|plate)\s*[a-z]?\d+[a-z]?\s*[.:|\-\u{2013}]?\s*").unwrap());
    let mut c = caption.trim().to_string();
    if !label.is_empty() && c.starts_with(label) {
        c = c[label.len()..].trim_start_matches(['.', ':', ' ', '|', '-']).to_string();
    }
    re.replace(&c, "").trim().to_string()
}

fn wc(s: &str) -> usize {
    s.split_whitespace().count()
}

fn qid(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    hex::encode(h.finalize())[..20].to_string()
}

// ---------------------------------------------------------------- S2 caption positives

pub fn caption_positives(ctx: &Ctx, a: &ArticleUnits, group: &str) -> Vec<Value> {
    let mut out = vec![];
    for f in &ctx.figures {
        let q = caption_query(&f.caption, &f.label);
        if f.rights_notice || rights_phrase(&f.caption) || !(5..=120).contains(&wc(&q)) {
            continue;
        }
        let mut pos: Vec<String> = vec![];
        for p in ctx.paragraphs.iter().filter(|p| p.figs.contains(&f.figure_id)) {
            for u in chunks_holding(a, &p.text) {
                if !pos.contains(&u) {
                    pos.push(u);
                }
            }
        }
        if pos.is_empty() {
            continue;
        }
        pos.truncate(3);
        out.push(json!({"id": qid(&["caption", &a.article_id, &f.figure_id]), "text": q, "article_id": a.article_id, "pos": pos,
            "kind": "caption", "group": group, "figure_id": f.figure_id}));
    }
    out
}

// ---------------------------------------------------------------- S3 figure pairs

const IMAGE_EXT: [&str; 7] = ["jpg", "jpeg", "png", "gif", "tif", "tiff", "webp"];

/// One row per usable figure (S3 `figures.jsonl`): caption and up to 3 in-text mention sentences. `image` is filled
/// by the fetcher (path relative to the images root).
pub fn figure_rows(ctx: &Ctx, a: &ArticleUnits, media: &[&Media], group: &str) -> Vec<Value> {
    let by_fig: HashMap<&str, &xmlctx::Fig> = ctx.figures.iter().map(|f| (f.figure_id.as_str(), f)).collect();
    let ok: Vec<&&Media> = media.iter().filter(|m| {
        let ext = m.s3_key.rsplit('.').next().unwrap_or("").to_lowercase();
        let f = by_fig.get(m.figure_id.as_str());
        m.commercial && IMAGE_EXT.contains(&ext.as_str()) && !m.s3_key.is_empty()
            && f.map_or(true, |f| !f.rights_notice && !rights_phrase(&f.caption)) && !rights_phrase(&m.caption)
    }).collect();
    let all: Vec<&str> = ok.iter().map(|m| m.unit_id.as_str()).collect();
    let mut out = vec![];
    for m in &ok {
        let f = by_fig.get(m.figure_id.as_str());
        let caption = f.map(|f| f.caption.as_str()).filter(|c| !c.is_empty()).unwrap_or(m.caption.as_str());
        let q = caption_query(caption, f.map(|f| f.label.as_str()).unwrap_or(""));
        let mut mentions: Vec<String> = vec![];
        for s in ctx.sentences.iter().filter(|s| s.figs.iter().any(|x| *x == m.figure_id)) {
            if (6..=80).contains(&wc(&s.plain)) && !mentions.contains(&s.plain) && mentions.len() < 3 {
                mentions.push(s.plain.clone());
            }
        }
        if wc(&q) < 3 && mentions.is_empty() {
            continue;
        }
        out.push(json!({"id": m.unit_id, "article_id": a.article_id, "group": group, "image": Value::Null,
            "caption": if (3..=200).contains(&wc(&q)) { json!(q) } else { Value::Null }, "mentions": mentions,
            "licence": if m.license_code.is_empty() { &a.license_code } else { &m.license_code },
            "figure_id": m.figure_id, "s3_key": m.s3_key, "md5": m.md5, "bytes": m.bytes,
            "pmcid": a.article_id.split('.').next().unwrap_or(""), "doi": a.doi,
            "same_article_figures": all.iter().copied().filter(|u| *u != m.unit_id).collect::<Vec<_>>()}));
    }
    out
}

// ---------------------------------------------------------------- citation contexts

pub fn cite_contexts(ctx: &Ctx, group: &str) -> Vec<Value> {
    let mut out = vec![];
    for s in &ctx.sentences {
        let refs: Vec<&String> = s.bibr.iter().filter(|r| ctx.refs.get(*r).is_some_and(|x| !x.is_empty())).collect();
        if refs.is_empty() || s.bibr.len() > 3 || !(8..=80).contains(&wc(&s.plain)) {
            continue;
        }
        for r in refs {
            let x = &ctx.refs[r];
            out.push(json!({"qid": qid(&["citation", &ctx.article_id, &s.plain, r]), "query": s.plain, "citing_article_id": ctx.article_id,
                "citing_group": group, "section": s.section, "rid": r, "pmid": x.pmid, "pmcid": x.pmcid, "doi": x.doi}));
        }
    }
    out
}

// ---------------------------------------------------------------- XML source

pub struct XmlSource {
    pub dir: Option<PathBuf>,
    pub net: bool,
    agent: ureq::Agent,
}

impl XmlSource {
    pub fn new(dir: Option<PathBuf>, net: bool) -> Self {
        XmlSource { dir, net, agent: ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).build() }
    }
    pub fn get(&self, aid: &str) -> Result<String> {
        if let Some(d) = &self.dir {
            for name in [format!("{aid}.xml"), format!("{aid}.xml.gz")] {
                let p = d.join(&name);
                if p.exists() {
                    let mut s = String::new();
                    crate::io::reader(&p)?.read_to_string(&mut s)?;
                    return Ok(s);
                }
            }
        }
        if !self.net {
            bail!("no XML for {aid}");
        }
        let url = format!("{S3_BASE}{aid}/{aid}.xml");
        let mut last = String::new();
        for attempt in 0..5u32 {
            match self.agent.get(&url).call() {
                Ok(r) => {
                    let mut s = String::new();
                    r.into_reader().read_to_string(&mut s)?;
                    return Ok(s);
                }
                Err(ureq::Error::Status(c @ (403 | 404), _)) => bail!("NotFound {c} {url}"),
                Err(e) => last = e.to_string(),
            }
            std::thread::sleep(Duration::from_secs(2u64.pow(attempt).min(20)));
        }
        Err(anyhow!("failed {url}: {last}"))
    }
}

// ---------------------------------------------------------------- shard driver

pub struct ShardArgs {
    pub chunks: PathBuf,
    pub media: Option<PathBuf>,
    pub keep: PathBuf,
    pub exclude: Vec<PathBuf>,
    pub xml: XmlSource,
    pub tokenizer: Option<PathBuf>,
    pub per_word: f64,
    pub win: WinCfg,
    pub threads: usize,
    pub limit: Option<usize>,
    /// also write 8k windows (the S1 trainer windows articles itself by default)
    pub windows: bool,
    /// articles.jsonl without chunk text
    pub ids_only: bool,
    /// held-out shard (S3 / caption test set): no figures and no caption ctx_queries from it
    pub no_figures: bool,
    pub out: PathBuf,
}

fn gz(path: &Path) -> Result<GzEncoder<BufWriter<std::fs::File>>> {
    Ok(GzEncoder::new(BufWriter::new(std::fs::File::create(path)?), Compression::new(6)))
}

pub fn read_ids(paths: &[PathBuf]) -> Result<HashSet<String>> {
    let mut s = HashSet::new();
    for p in paths {
        for line in crate::io::reader(p)?.lines() {
            let line = line?;
            let id = line.split(['\t', ',']).next().unwrap_or("").trim().to_string();
            if !id.is_empty() {
                s.insert(id);
            }
        }
    }
    Ok(s)
}

pub fn run(a: &ShardArgs) -> Result<Value> {
    let exclude = read_ids(&a.exclude)?;
    let keep = read_keep(&a.keep, &exclude)?;
    let is_kept = |x: &str| keep.contains_key(x);
    let mut arts = read_chunks(&a.chunks, &is_kept)?;
    // commercial only; articles without body text chunks (abstract-only items) are no use for S2 or figures
    arts.retain(|x| x.commercial && x.units.iter().any(|u| !u.is_abstract));
    if let Some(n) = a.limit {
        arts.truncate(n);
    }
    let media = match &a.media {
        Some(m) if m.exists() => read_media(m, &is_kept)?,
        _ => vec![],
    };
    let mut by_art: HashMap<&str, Vec<&Media>> = HashMap::new();
    for m in &media {
        by_art.entry(m.article_id.as_str()).or_default().push(m);
    }
    let counter = Counter::load(a.tokenizer.as_deref(), a.per_word)?;
    let tmp = a.out.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let pool = rayon::ThreadPoolBuilder::new().num_threads(a.threads).build()?;
    type Out = (Value, Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>, Option<String>);
    let results: Vec<Out> = pool.install(|| arts.par_iter().map(|art| {
        let k = &keep[&art.article_id];
        let w = if a.windows { windows(art, &k.group, &counter, a.win) } else { vec![] };
        match a.xml.get(&art.article_id).and_then(|x| xmlctx::parse(&x, &art.article_id)) {
            Ok(ctx) => {
                let m = by_art.get(art.article_id.as_str()).cloned().unwrap_or_default();
                let (cq, fr) = if a.no_figures { (vec![], vec![]) } else { (caption_positives(&ctx, art, &k.group), figure_rows(&ctx, art, &m, &k.group)) };
                (article_row(art, &k.group, a.ids_only), cq, fr, cite_contexts(&ctx, &k.group), w, None)
            }
            Err(e) => (article_row(art, &k.group, a.ids_only), vec![], vec![], vec![], w, Some(format!("{}\t{}", art.article_id, e.to_string().replace(['\t', '\n'], " ")))),
        }
    }).collect());
    let mut names = vec!["articles", "ctx_queries", "figures", "cite_ctx"];
    if a.windows {
        names.push("windows");
    }
    let mut ws: Vec<_> = names.iter().map(|n| gz(&tmp.join(format!("{n}.jsonl.gz")))).collect::<Result<_>>()?;
    let mut counts = [0usize; 5];
    let n_text = if a.ids_only { crate::units::write_chunk_text(&tmp.join("chunk_text.parquet"), &arts)? } else { 0 };
    let mut errw = BufWriter::new(std::fs::File::create(tmp.join("errors.tsv"))?);
    let (mut errors, mut chunks, mut figs_caption, mut figs_mention, mut mentions) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut win_tokens: BTreeMap<String, usize> = BTreeMap::new();
    for (ar, cq, fr, cc, w, err) in &results {
        chunks += ar["chunks"].as_array().map_or(0, |c| c.len());
        for v in fr {
            figs_caption += (!v["caption"].is_null()) as usize;
            let n = v["mentions"].as_array().map_or(0, |m| m.len());
            figs_mention += (n > 0) as usize;
            mentions += n;
        }
        for v in w {
            let t = v["tokens"].as_u64().unwrap_or(0) as usize;
            *win_tokens.entry(format!("{}k", (t / 1024).min(9))).or_default() += 1;
        }
        let one = [ar.clone()];
        let groups: [&[Value]; 5] = [&one, cq, fr, cc, w];
        for (k, rows) in groups.into_iter().enumerate().take(names.len()) {
            for v in rows {
                serde_json::to_writer(&mut ws[k], v)?;
                ws[k].write_all(b"\n")?;
                counts[k] += 1;
            }
        }
        if let Some(e) = err {
            writeln!(errw, "{e}")?;
            errors += 1;
        }
    }
    for w in ws {
        w.finish()?.flush()?;
    }
    errw.flush()?;
    let stats = json!({"chunks_file": a.chunks.display().to_string().rsplit('/').next(), "articles": counts[0], "xml_errors": errors,
        "body_chunks": chunks, "ctx_queries": counts[1], "figures": counts[2], "figures_with_caption": figs_caption,
        "figures_with_mentions": figs_mention, "mentions": mentions, "figures_in_media": media.len(), "cite_ctx": counts[3],
        "windows": counts[4], "window_tokens": win_tokens, "tokenizer": counter.name(), "ids_only": a.ids_only, "chunk_text_rows": n_text, "no_figures": a.no_figures});
    std::fs::write(tmp.join("stats.json"), serde_json::to_string_pretty(&stats)?)?;
    let _ = std::fs::remove_dir_all(&a.out);
    std::fs::rename(&tmp, &a.out)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::Unit;

    fn art(units: &[(&str, &str, bool)]) -> ArticleUnits {
        ArticleUnits { article_id: "PMC100.1".into(), title: "Colchicine after lacunar stroke".into(), doi: "10.1/x".into(),
            license_code: "CC BY".into(), commercial: true,
            units: units.iter().map(|(id, t, abs)| Unit { unit_id: id.to_string(), text: t.to_string(),
                heading: Some(if *abs { "Abstract".into() } else { "Results".into() }),
                section: if *abs { "Abstract".into() } else { "Results".into() }, is_abstract: *abs }).collect() }
    }

    fn words(n: usize, tag: &str) -> String {
        (0..n).map(|i| format!("{tag}{i}")).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn short_article_is_one_window_with_exact_spans() {
        let a = art(&[("a1", "We tested colchicine.", true), ("b1", "Body one.", false), ("b2", "Body two.", false)]);
        let w = windows(&a, "g1", &Counter::Words(1.0), WinCfg::default());
        assert_eq!(w.len(), 1);
        let t = w[0]["text"].as_str().unwrap();
        assert!(t.starts_with("Colchicine after lacunar stroke\n\nWe tested colchicine."));
        for c in w[0]["chunks"].as_array().unwrap() {
            let (s, e) = (c["start"].as_u64().unwrap() as usize, c["end"].as_u64().unwrap() as usize);
            let sub: String = t.chars().skip(s).take(e - s).collect();
            let want = a.units.iter().find(|u| u.unit_id == c["unit_id"].as_str().unwrap()).unwrap().text.clone();
            assert_eq!(sub, want);
        }
        assert_eq!((w[0]["prefix"].as_str().unwrap(), w[0]["n_windows"].as_u64().unwrap()), ("", 1));
    }

    #[test]
    fn long_article_windows_overlap_cover_and_fit() {
        let mut u: Vec<(String, String, bool)> = vec![("a1".into(), words(150, "abs"), true)];
        for i in 0..40 {
            u.push((format!("b{i}"), words(400, &format!("w{i}x")), false));
        }
        let refs: Vec<(&str, &str, bool)> = u.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), *c)).collect();
        let a = art(&refs);
        let cfg = WinCfg { max_tokens: 3000, prefix_tokens: 200, overlap_tokens: 800 };
        let w = windows(&a, "g1", &Counter::Words(1.0), cfg);
        assert!(w.len() >= 6, "{}", w.len());
        let mut seen = HashSet::new();
        for (i, x) in w.iter().enumerate() {
            assert!(x["tokens"].as_u64().unwrap() as usize <= cfg.max_tokens + 10, "window {i} {}", x["tokens"]);
            let ids: Vec<&str> = x["chunks"].as_array().unwrap().iter().map(|c| c["unit_id"].as_str().unwrap()).collect();
            seen.extend(ids.iter().map(|s| s.to_string()));
            if i > 0 {
                assert!(x["prefix"].as_str().unwrap().starts_with("Colchicine after lacunar stroke\n\nabs0"));
                assert!(!ids.contains(&"a1"));
                let prev: Vec<&str> = w[i - 1]["chunks"].as_array().unwrap().iter().map(|c| c["unit_id"].as_str().unwrap()).collect();
                assert!(ids.iter().any(|x| prev.contains(x)), "window {i} shares a chunk with {}", i - 1);
            }
        }
        assert_eq!(seen.len(), 41, "every chunk in some window");
    }

    #[test]
    fn caption_query_and_rights() {
        assert_eq!(caption_query("Figure 2. Infarct volume by group.", "Figure 2"), "Infarct volume by group.");
        assert_eq!(caption_query("Fig. 3: Lesion maps.", ""), "Lesion maps.");
        assert!(rights_phrase("Lesion map reproduced from a prior atlas."));
        assert!(rights_phrase("Adapted with permission from Smith (2019)."));
        assert!(!rights_phrase("Mean infarct volume by treatment group."));
    }

    #[test]
    fn fixture_article_end_to_end() {
        let ctx = xmlctx::parse(xmlctx::FIXTURE, "PMC100.1").unwrap();
        let a = art(&[("a1", "We tested colchicine.", true),
            ("c1", "Colchicine lowers recurrent stroke risk after lacunar infarction. Earlier trials were small.", false),
            ("c2", "Imaging. Infarct volume fell by 30% with colchicine (Figure 1A). Edema was unchanged 4.", false)]);
        let pc = caption_positives(&ctx, &a, "g1");
        assert_eq!(pc.len(), 1, "f2 has a rights notice");
        assert_eq!(pc[0]["text"], "Infarct volume by group. Mean ± SD.");
        assert_eq!((pc[0]["pos"].clone(), pc[0]["kind"].clone()), (json!(["c2"]), json!("caption")));
        let ar = article_row(&a, "g1", false);
        assert!(article_row(&a, "g1", true)["chunks"][0].get("text").is_none());
        assert_eq!((ar["abstract"].as_str().unwrap(), ar["chunks"].as_array().unwrap().len()), ("We tested colchicine.", 2));
        assert_eq!((ar["chunks"][0]["id"].as_str().unwrap(), ar["chunks"][0]["heading"].as_str().unwrap()), ("c1", "Results"));
        let m1 = Media { unit_id: "m1".into(), article_id: "PMC100.1".into(), figure_id: "f1".into(), s3_key: "PMC100.1/pone.0001.g001.jpg".into(),
            md5: "x".into(), commercial: true, ..Default::default() };
        let m2 = Media { unit_id: "m2".into(), figure_id: "f2".into(), s3_key: "PMC100.1/pone.0001.g002.jpg".into(), commercial: true, ..m1.clone() };
        let fr = figure_rows(&ctx, &a, &[&m1, &m2], "g1");
        assert_eq!(fr.len(), 1, "f2 dropped (rights notice)");
        assert_eq!((fr[0]["id"].as_str().unwrap(), fr[0]["caption"].as_str().unwrap()), ("m1", "Infarct volume by group. Mean ± SD."));
        assert_eq!(fr[0]["mentions"], json!(["Infarct volume fell by 30% with colchicine."]));
        assert_eq!(fr[0]["same_article_figures"], json!([]));
        let cc = cite_contexts(&ctx, "g1");
        let rids: Vec<&str> = cc.iter().map(|v| v["rid"].as_str().unwrap()).collect();
        // r4 has no ids; the 6-word "Earlier trials..." sentence is too short (< 8 words)
        assert_eq!(rids, vec!["r1", "r2", "r3"]);
        assert_eq!(cc[0]["query"], "Colchicine lowers recurrent stroke risk after lacunar infarction.");
    }
}
