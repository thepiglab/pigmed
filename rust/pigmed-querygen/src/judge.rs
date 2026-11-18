//! Trial read-outs: a cross-model VLM judge for figure queries (sees the image, caption and citing text; never the
//! generator's own model), and a blinded, stratified hand-read sample with a separate key.

use crate::client::{json_body, par, post, GenArgs, Generation};
use crate::figure::{image_data_url, FigInput};
use crate::text::SampleChunk;
use crate::util::{first_words, read_jsonl, uh};
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const FIG_JUDGE_SYSTEM: &str = "You judge search queries written to find figures in a biomedical literature search \
engine. You see one figure (image), its caption and the text that cites it, and some queries. Output JSON only.";

const FIG_JUDGE_RULES: &str = "For each query:\n\
- relevant: 2 if this figure clearly shows what the query asks for, 1 if partly, 0 if not. For a query marked \
[with image] the person sends the question together with this image: 2 if the citing text clearly answers it, 1 if \
partly, 0 if not.\n\
- needs_image: true if the image itself (not only the caption) is needed to meet the query.\n\
- caption_paraphrase: true if the query mostly restates the caption.\n\
- standalone: true if a stranger understands what is searched for without seeing this article (a [with image] query \
may point at the image).\n\
- natural: 1 = no real person would type this; 2 = plausible but stilted; 3 = a real person could well type this.\n\
- register: expert, student, lay.";

/// Judge every kept query of the figure arms with a VLM that is not the generator.
pub fn judge_figs(a: &GenArgs, gens: &[Generation], figs: &HashMap<String, FigInput>) -> (Vec<Value>, Value) {
    let ag = crate::client::agent(a.concurrency);
    let units: Vec<&Generation> = gens.iter().filter(|g| g.ok && figs.contains_key(&g.chunk_id)).collect();
    let rows = par(units.len(), a.concurrency, |i| {
        let g = units[i];
        let f = &figs[&g.chunk_id];
        let items: Vec<(String, String, bool)> = g.queries.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string(),
            g.meta["slots"][k]["query_modality"] == json!("image+text")))).collect();
        if items.is_empty() {
            return vec![];
        }
        let Ok(url) = image_data_url(&f.image) else { return vec![] };
        let ids: Vec<String> = (1..=items.len()).map(|k| format!("q{k}")).collect();
        let list: Vec<String> = items.iter().zip(&ids).map(|((_, q, img), id)| format!("{id}{}: {q}", if *img { " [with image]" } else { "" })).collect();
        let citing: Vec<String> = f.citing.iter().take(2).map(|c| first_words(c, 200)).collect();
        let text = format!("Figure caption: {}\nText that cites the figure:\n{}\n\n{FIG_JUDGE_RULES}\n\nQUERIES:\n{}",
            first_words(&f.text, 300), if citing.is_empty() { "(none)".into() } else { citing.join("\n") }, list.join("\n"));
        let item = json!({"type": "object", "additionalProperties": false, "required": ["id", "relevant", "needs_image", "caption_paraphrase", "standalone", "natural", "register"],
            "properties": {"id": {"enum": ids}, "relevant": {"enum": ["0", "1", "2"]}, "needs_image": {"type": "boolean"}, "caption_paraphrase": {"type": "boolean"},
                "standalone": {"type": "boolean"}, "natural": {"enum": ["1", "2", "3"]}, "register": {"enum": ["expert", "student", "lay"]}}});
        let schema = json!({"type": "object", "required": ["ratings"], "additionalProperties": false, "properties": {"ratings": {
            "type": "array", "minItems": items.len(), "maxItems": items.len(), "items": item}}});
        let user = json!([{"type": "image_url", "image_url": {"url": url}}, {"type": "text", "text": text}]);
        let (r, _) = post(&ag, a, FIG_JUDGE_SYSTEM, user, schema);
        let v = r.ok().and_then(|r| json_body(&r.content)).unwrap_or(Value::Null);
        let by: HashMap<String, Value> = v["ratings"].as_array().into_iter().flatten().filter_map(|x| Some((x["id"].as_str()?.to_string(), x.clone()))).collect();
        items.iter().zip(&ids).filter_map(|((k, q, img), id)| {
            let x = by.get(id)?;
            let m = &g.meta["slots"][k];
            Some(json!({"qid": format!("{}-{k}-{}", g.arm, g.chunk_id), "arm": g.arm, "query": q, "with_image": img, "style": m["style"],
                "intent": m["intent"], "figure_kind": m["figure_kind"],
                "relevant": x["relevant"].as_str().and_then(|s| s.parse::<u8>().ok()), "needs_image": x["needs_image"],
                "caption_paraphrase": x["caption_paraphrase"], "standalone": x["standalone"],
                "natural": x["natural"].as_str().and_then(|s| s.parse::<u8>().ok()), "register": x["register"]}))
        }).collect::<Vec<_>>()
    }, |_| {}).into_iter().flatten().collect::<Vec<_>>();
    let n = rows.len().max(1) as f64;
    let share = |f: &dyn Fn(&Value) -> bool| rows.iter().filter(|r| f(r)).count() as f64 / n;
    let mut by_arm: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for r in &rows {
        by_arm.entry(r["arm"].as_str().unwrap_or("").to_string()).or_default().push(r);
    }
    let arm_stats: Map<String, Value> = by_arm.iter().map(|(k, rs)| {
        let m = rs.len().max(1) as f64;
        (k.clone(), json!({"judged": rs.len(), "relevant2": rs.iter().filter(|r| r["relevant"] == json!(2)).count() as f64 / m,
            "relevant_mean": rs.iter().filter_map(|r| r["relevant"].as_u64()).sum::<u64>() as f64 / m,
            "needs_image": rs.iter().filter(|r| r["needs_image"] == json!(true)).count() as f64 / m,
            "caption_paraphrase": rs.iter().filter(|r| r["caption_paraphrase"] == json!(true)).count() as f64 / m,
            "standalone": rs.iter().filter(|r| r["standalone"] == json!(true)).count() as f64 / m,
            "natural_mean": rs.iter().filter_map(|r| r["natural"].as_u64()).sum::<u64>() as f64 / m}))
    }).collect();
    let s = json!({"judge": a.model, "judged": rows.len(), "relevant2": share(&|r| r["relevant"] == json!(2)),
        "needs_image": share(&|r| r["needs_image"] == json!(true)), "caption_paraphrase": share(&|r| r["caption_paraphrase"] == json!(true)),
        "by_arm": arm_stats});
    (rows, s)
}

/// Blinded hand-read sample: `n` kept queries, equal per arm, with `fig_share` of them figure-targeted when the arm
/// has figure queries. Writes a markdown table (no arm names) and a key.
pub fn blind(gen_dirs: &[&Path], sample: Option<&Path>, figs: Option<&Path>, n: usize, fig_share: f64, seed: &str, out_md: &Path, out_key: &Path) -> Result<Value> {
    let chunks: HashMap<String, SampleChunk> = match sample { Some(p) => read_jsonl::<SampleChunk>(p)?.into_iter().map(|c| (c.chunk_id.clone(), c)).collect(), None => HashMap::new() };
    let fig_by: HashMap<String, FigInput> = match figs { Some(p) => read_jsonl::<FigInput>(p)?.into_iter().map(|f| (f.chunk_id.clone(), f)).collect(), None => HashMap::new() };
    // arm (model setting, without the -c3pf / -c3fig suffix) -> (text items, figure items)
    let mut arms: BTreeMap<String, (Vec<Value>, Vec<Value>)> = BTreeMap::new();
    for d in gen_dirs {
        let gens: Vec<Generation> = read_jsonl(&d.join("generations.jsonl"))?;
        // the grid arm is the parent directory when the layout is <arm>/<arm>-c3pf|c3fig (imported text runs keep
        // their own arm names inside), else the arm name without its layout suffix
        let parent = d.canonicalize().ok().and_then(|_| d.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()));
        let own = d.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let grid_arm = parent.filter(|p| own.starts_with(p.as_str()));
        for g in &gens {
            let arm = grid_arm.clone().unwrap_or_else(|| g.arm.trim_end_matches("-c3pf").trim_end_matches("-c3fig").to_string());
            for (k, v) in &g.queries {
                let m = &g.meta["slots"][k];
                let fig = m["modality"].as_str().is_some_and(|x| x.starts_with("figure"));
                let item = json!({"arm": arm, "chunk_id": g.chunk_id, "slot": k, "query": v, "style": m["style"], "persona": m["persona"],
                    "modality": m["modality"], "need": m["need"], "figure_kind": m["figure_kind"], "target_id": m["target_id"],
                    "h": uh(&[seed, &g.arm, &g.chunk_id, k])});
                let e = arms.entry(arm.clone()).or_default();
                if fig { e.1.push(item) } else { e.0.push(item) }
            }
        }
    }
    let na = arms.len().max(1);
    let per_arm = (n / na).max(1);
    let mut picked: Vec<Value> = vec![];
    for (ai, (_, (mut t, mut f))) in arms.into_iter().enumerate() {
        // spread the remainder: the first n % arms arms get one more row
        let per_arm = per_arm + usize::from(ai < n % na && n >= na);
        t.sort_by_key(|x| x["h"].as_u64());
        f.sort_by_key(|x| x["h"].as_u64());
        let nf = if f.is_empty() { 0 } else { ((per_arm as f64) * fig_share).round() as usize }.min(f.len());
        let nt = (per_arm - nf).min(t.len());
        let nf = (per_arm - nt).min(f.len()); // top up from figures when text is short (and vice versa above)
        picked.extend(f.into_iter().take(nf));
        picked.extend(t.into_iter().take(nt));
    }
    picked.sort_by_key(|x| uh(&[seed, "order", x["chunk_id"].as_str().unwrap_or(""), x["slot"].as_str().unwrap_or(""), x["arm"].as_str().unwrap_or("")]));
    let cell = |s: &str| s.replace('|', "/").replace('\n', " ");
    let mut md = String::from("# Blinded hand-read (generator trial)\n\nScore each row: standalone 0/1 · specific 0/1 · natural 1-3 · plausible for persona 1-3 · answered by the target 0/1 · for figure rows: needs the image 0/1.\nFigure rows name the image file (longest side 1280 px) and show the caption start; text rows show the passage start.\n\n| # | query | style | persona | target | stand. | spec. | nat. | pers. | ans. | img |\n|---|---|---|---|---|---|---|---|---|---|---|\n");
    let mut key = Map::new();
    for (i, x) in picked.iter().enumerate() {
        let target = if x["modality"].as_str().is_some_and(|m| m.starts_with("figure")) {
            let f = fig_by.get(x["chunk_id"].as_str().unwrap_or(""));
            let img = f.map(|f| Path::new(&f.image).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).unwrap_or_default();
            format!("{} [{}] {img}: {}", if x["modality"] == json!("figure_composed") { "IMAGE+question → citing text" } else { "FIGURE" },
                x["figure_kind"].as_str().unwrap_or(""), f.map(|f| first_words(&f.text, 25)).unwrap_or_default())
        } else {
            format!("PASSAGE: {}", chunks.get(x["chunk_id"].as_str().unwrap_or("")).map(|c| first_words(&c.text, 40)).unwrap_or_default())
        };
        md += &format!("| {} | {} | {} | {} | {} | | | | | | |\n", i + 1, cell(x["query"].as_str().unwrap_or("")), x["style"].as_str().unwrap_or(""),
            cell(&first_words(x["persona"].as_str().unwrap_or(""), 14)), cell(&target));
        key.insert((i + 1).to_string(), json!({"arm": x["arm"], "chunk_id": x["chunk_id"], "slot": x["slot"], "need": x["need"], "target_id": x["target_id"]}));
    }
    std::fs::write(out_md, md)?;
    std::fs::write(out_key, serde_json::to_string_pretty(&key)?)?;
    let figs_n = picked.iter().filter(|x| x["modality"].as_str().is_some_and(|m| m.starts_with("figure"))).count();
    Ok(json!({"rows": picked.len(), "figure_rows": figs_n, "per_arm": per_arm}))
}
