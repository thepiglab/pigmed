//! Figure arm, layout c3fig (recipe v3.2).
//!
//!   inputs   `fig-inputs`: per article up to N figures (train split, commercial licence, no rights phrase or JATS
//!            permissions in the figure, held-out articles excluded), image fetched from the PMC Open Data bucket
//!            and resized (longest side <= image_max_side), citing paragraphs from the JATS, citing chunk.
//!   plan     figure cards per article sized so figure queries are the configured share of all queries; card types
//!            (find_figure, finding, appearance, composed) drawn per figure without replacement.
//!   stage A  one call per figure WITH the image + caption + citing text: figure kind, legibility and one standalone
//!            figure need per card, written for the reader stage A picks from the candidates (persona-first).
//!   stage B  one call per article, no image and no caption: persona surface queries in figure styles ("find a
//!            figure showing", visual keywords, lay "what does .. look like", half-remembered, non-native) and,
//!            for composed cards, the question typed while showing the image.
//!   stage C  noise; reject chain incl. caption_copy.

use crate::client::{json_body, post, GenArgs, Generation};
use crate::config::{CardType, Figure};
use crate::rules::{Article, ArticleMeta};
use crate::text::{accept_b, choose_reader, generator_label, hint, pick_styles, readers, reader_enum, slot_line, stage_budget, style, who, Card, Slot, Usage};
use crate::util::{fill, first_words, jaccard, read_jsonl, uh};
use crate::Qg;
use anyhow::{Context, Result};
use base64::Engine;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One figure unit; field-compatible with pigmed-synth's FigureSample (chunk_id = figure unit id, section = caption
/// type, text = caption), plus licence and the article's other figures.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct FigInput {
    pub chunk_id: String,
    pub article_id: String,
    #[serde(default)]
    pub cluster: String,
    pub title: String,
    pub abstract_text: String,
    pub section: String,
    pub text: String,
    pub words: usize,
    #[serde(default)]
    pub marker: String,
    pub figure_id: String,
    pub image: String,
    pub citing: Vec<String>,
    pub citing_chunk: Option<String>,
    #[serde(default)]
    pub licence: String,
}

#[derive(Clone, Debug)]
pub struct FigCard {
    pub fig: usize,
    pub ctype: String,
    pub card: Card,
}

fn fig_cfg(qg: &Qg) -> &Figure {
    qg.cfg.figure.as_ref().expect("[figure] in config")
}

pub fn card_type<'a>(f: &'a Figure, name: &str) -> &'a CardType {
    f.card_types.iter().find(|c| c.name == name).expect("card type")
}

/// Caption-based figure type (first matching caption type; multipanel when >= min_panels distinct panel letters).
pub fn caption_type(f: &Figure, caption: &str) -> Option<String> {
    let panel = Regex::new(&f.panel_regex).ok()?;
    let letters: HashSet<String> = panel.captures_iter(caption).filter_map(|c| c.get(1).or(c.get(2)).or(c.get(3)).map(|m| m.as_str().to_string())).collect();
    for t in &f.caption_types {
        let hit = match (&t.regex, t.min_panels) {
            (_, Some(n)) => letters.len() >= n,
            (Some(r), None) => Regex::new(r).map(|r| r.is_match(caption)).unwrap_or(false),
            _ => false,
        };
        if hit {
            return Some(t.name.clone());
        }
    }
    None
}

// ------------------------------------------------------------------------------------------------ plan

/// Mean query slots per figure card (composed cards have their fixed style list).
fn slots_per_card(qg: &Qg) -> f64 {
    let f = fig_cfg(qg);
    let tot: f64 = f.card_types.iter().map(|c| c.weight as f64).sum::<f64>().max(1.0);
    f.card_types.iter().map(|c| c.weight as f64 * if c.styles.is_empty() { f.styles_per_pair as f64 } else { c.styles.len() as f64 }).sum::<f64>() / tot
}

/// Number of figure cards for an article with `text_cards` planned text cards, so that figure + composed queries
/// make up their target share (before filtering; `kept_ratio` corrects for different keep rates).
pub fn wanted_fig_cards(qg: &Qg, text_cards: usize, boost: f64) -> usize {
    let m = &qg.cfg.modality;
    let s = m.share("figure") + m.share("figure_composed");
    if s <= 0.0 || s >= 1.0 {
        return 0;
    }
    let text_slots = (text_cards * qg.cfg.text.styles_per_pair) as f64;
    let want = text_slots * s / (1.0 - s) * m.kept_ratio * boost;
    (want / slots_per_card(qg)).round().max(1.0) as usize
}

/// Card types allowed for a figure.
fn allowed_types<'a>(f: &'a Figure, fig: &FigInput) -> Vec<(String, u32)> {
    f.card_types.iter().filter(|c| (c.sections.is_empty() || c.sections.iter().any(|s| *s == fig.section))
        && (!c.requires_citing_chunk || fig.citing_chunk.is_some())).map(|c| (c.name.clone(), c.weight)).collect()
}

pub fn plan_fig_cards(qg: &Qg, article_id: &str, text_cards: usize, figs: &[&FigInput], boost: f64) -> Vec<FigCard> {
    let f = fig_cfg(qg);
    let mut order: Vec<usize> = (0..figs.len()).collect();
    order.sort_by_key(|&i| uh(&[&figs[i].chunk_id, "order"]));
    order.truncate(f.max_figures_per_article);
    let want = wanted_fig_cards(qg, text_cards, boost);
    if want == 0 || order.is_empty() {
        return vec![];
    }
    // round-robin over the article's figures up to max_cards_per_figure, each limited by its allowed card types
    let caps: Vec<usize> = order.iter().map(|&i| allowed_types(f, figs[i]).len().min(f.max_cards_per_figure)).collect();
    let mut k = vec![0usize; order.len()];
    let mut left = want;
    while left > 0 && k.iter().zip(&caps).any(|(a, b)| a < b) {
        for j in 0..order.len() {
            if left > 0 && k[j] < caps[j] {
                k[j] += 1;
                left -= 1;
            }
        }
    }
    let n = qg.lib.len() as u64;
    let mut out = vec![];
    for (j, &fi) in order.iter().enumerate() {
        let fig = figs[fi];
        let mut opts = allowed_types(f, fig);
        for c in 0..k[j] {
            let t = crate::util::pick(uh(&[&fig.chunk_id, "ctype", &c.to_string()]), &opts).to_string();
            opts.retain(|x| x.0 != t);
            let id = format!("F{}.c{}", j + 1, c + 1);
            let cands = qg.cfg.text.candidate_strata.iter().enumerate().map(|(s, groups)| {
                qg.candidate((uh(&[&fig.chunk_id, "pf", &c.to_string(), &s.to_string()]) % n) as usize, groups)
            }).collect();
            out.push(FigCard { fig: fi, ctype: t.clone(), card: Card { id, marker: format!("F{}", j + 1), cands, ctype: t, ..Default::default() } });
        }
    }
    let _ = article_id;
    out
}

/// Planned figure slots of one article's figure cards.
pub fn planned_slots(qg: &Qg, cards: &[FigCard]) -> f64 {
    let f = fig_cfg(qg);
    cards.iter().map(|c| { let ct = card_type(f, &c.ctype); if ct.styles.is_empty() { f.styles_per_pair as f64 } else { ct.styles.len() as f64 } }).sum()
}

/// Boost (>= 1) that brings the planned figure share of this input up to the target: articles with few usable
/// figures cannot carry their share, so articles with more figures get more cards (up to the per-figure and
/// per-article caps). Deterministic for a given input; written into the run summary. (text cards, figures) per article.
pub fn calibrate_boost(qg: &Qg, arts: &[(String, usize, Vec<&FigInput>)]) -> (f64, f64) {
    let m = &qg.cfg.modality;
    let target = m.share("figure") + m.share("figure_composed");
    if target <= 0.0 || arts.iter().all(|a| a.2.is_empty()) {
        return (1.0, 0.0);
    }
    let text: f64 = arts.iter().map(|a| (a.1 * qg.cfg.text.styles_per_pair) as f64).sum();
    let share = |b: f64| {
        let fig: f64 = arts.iter().map(|(id, n, figs)| planned_slots(qg, &plan_fig_cards(qg, id, *n, figs, b))).sum();
        fig / (fig + text).max(1.0)
    };
    let mut b = 1.0;
    while b < 4.0 && share(b) < target {
        b += 0.05;
    }
    let b = (b * 100.0).round() / 100.0;
    (b, share(b))
}

// ------------------------------------------------------------------------------------------------ stage A (image)

pub fn prompt_fig_a(qg: &Qg, fig: &FigInput, cards: &[&FigCard]) -> String {
    let f = fig_cfg(qg);
    let t = &f.templates;
    let lines: Vec<String> = cards.iter().map(|c| fill(&t.card_line, &[("id", &c.card.id), ("ctype", &c.ctype), ("readers", &readers(qg, &c.card.cands))])).collect();
    let citing: Vec<String> = fig.citing.iter().take(f.citing_paragraphs).enumerate()
        .map(|(i, p)| format!("[{}] {}", i + 1, first_words(p, f.citing_max_words))).collect();
    let rr = format!("{}\n", qg.cfg.file(&qg.cfg.text.templates.reader_rules));
    let kinds: Vec<&str> = f.kinds.iter().map(|k| k.0.as_str()).collect();
    fill(qg.cfg.file(&t.stage_a), &[("title", &fig.title), ("abstract", &first_words(&fig.abstract_text, f.abstract_max_words)),
        ("caption", &first_words(&fig.text, f.caption_max_words)),
        ("citing", &if citing.is_empty() { t.none_text.clone() } else { citing.join("\n") }), ("card_lines", &lines.join("\n")),
        ("reader_rules", &rr), ("need_rules", qg.cfg.file(&t.need_rules)), ("card_defs", qg.cfg.file(&t.card_defs)), ("kinds", &kinds.join(", "))])
}

pub fn schema_fig_a(qg: &Qg, cards: &[&FigCard]) -> Value {
    let f = fig_cfg(qg);
    let ids: Vec<&str> = cards.iter().map(|c| c.card.id.as_str()).collect();
    let kinds: Vec<&str> = f.kinds.iter().map(|k| k.0.as_str()).collect();
    let item = json!({"type": "object", "additionalProperties": false,
        "required": ["card", "reader", "expert_role", "need", "visible", "answer", "specificity", "audience", "min_expertise", "entities", "skip"],
        "properties": {"card": {"enum": ids}, "reader": {"enum": reader_enum(qg)}, "expert_role": {"type": "string"},
            "need": {"type": "string"}, "visible": {"type": "string"}, "answer": {"type": "string"},
            "specificity": {"enum": ["unique", "few", "many"]},
            "audience": {"type": "array", "minItems": 1, "maxItems": 5, "items": {"enum": qg.cfg.text.audience}},
            "min_expertise": {"enum": ["0", "1", "2", "3"]},
            "entities": {"type": "array", "minItems": 0, "maxItems": 5, "items": {"type": "string"}},
            "skip": {"type": "boolean"}}});
    json!({"type": "object", "required": ["biomedical", "figure_kind", "legible", "cards"], "additionalProperties": false,
        "properties": {"biomedical": {"type": "boolean"}, "figure_kind": {"enum": kinds}, "legible": {"type": "boolean"},
            "cards": {"type": "array", "minItems": cards.len(), "maxItems": cards.len(), "items": item}}})
}

/// Fill one figure's cards from its stage-A reply. Returns the figure kind.
pub fn fill_fig_cards(qg: &Qg, cards: &mut [&mut FigCard], v: &Value) -> String {
    fill_fig_cards_meta(qg, cards, v, None)
}

pub fn fill_fig_cards_meta(qg: &Qg, cards: &mut [&mut FigCard], v: &Value, meta: Option<&ArticleMeta>) -> String {
    let mut seen = HashSet::new();
    for c in cards.iter_mut() {
        c.card.skip = true;
    }
    for e in v["cards"].as_array().into_iter().flatten() {
        let Some(id) = e["card"].as_str() else { continue };
        if !seen.insert(id.to_string()) {
            continue;
        }
        let Some(fc) = cards.iter_mut().find(|c| c.card.id == id) else { continue };
        let c = &mut fc.card;
        let s = |k: &str| e[k].as_str().unwrap_or("").trim().to_string();
        c.need = s("need");
        c.narrative = s("visible");
        c.answer = s("answer");
        c.specificity = s("specificity");
        c.audience = e["audience"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
        c.min_expertise = s("min_expertise").parse().unwrap_or(0);
        c.entities = e["entities"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(|x| x.trim().to_string())).filter(|x| !x.is_empty()).collect();
        c.skip = e["skip"].as_bool().unwrap_or(false) || c.need.split_whitespace().count() < qg.cfg.text.min_need_words;
        choose_reader(qg, c, e, qg.cfg.figure_fallback_mode(), meta);
    }
    let kind = v["figure_kind"].as_str().unwrap_or("other").to_string();
    let off = !crate::rules::biomedical(&qg.cfg, meta, v["biomedical"] != json!(false));
    let illegible = v["legible"] == json!(false);
    for c in cards.iter_mut() {
        c.card.off_domain = off;
        if illegible {
            c.card.skip = true;
        }
    }
    kind
}

pub fn image_data_url(path: &str) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("image {path}"))?;
    let mime = if path.ends_with(".png") { "image/png" } else if path.ends_with(".webp") { "image/webp" } else { "image/jpeg" };
    Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

// ------------------------------------------------------------------------------------------------ stage B

pub fn plan_fig_slots(qg: &Qg, cards: &[FigCard], kinds: &[String], figs: &[&FigInput], art: &Article) -> Vec<Slot> {
    let f = fig_cfg(qg);
    let mut out = vec![];
    for (ci, fc) in cards.iter().enumerate() {
        if !crate::text::card_drop(qg, &fc.card).is_empty() {
            continue;
        }
        let ct = card_type(f, &fc.ctype);
        let key = format!("{}|{}|{}", art.article_id, figs[fc.fig].chunk_id, fc.card.id);
        let p = fc.card.persona.map(|i| &qg.lib[i]);
        let styles = if ct.styles.is_empty() { pick_styles(&f.styles, f.styles_per_pair, p, false, false, &key) } else { ct.styles.clone() };
        for (k, st) in styles.into_iter().enumerate() {
            let h = uh(&[&key, "hint", &k.to_string()]) as usize;
            let hint = hint(qg, style(&f.styles, &st), fc.card.persona, &[], &kinds[ci], h);
            out.push(Slot { id: format!("{}.q{}", fc.card.id, k + 1), bid: format!("slot-{:02}", out.len() + 1), card: ci, persona: fc.card.persona,
                stretch: false, style: st, hint });
        }
    }
    out
}

fn kind_label<'a>(f: &'a Figure, kind: &str) -> &'a str {
    f.kinds.iter().find(|k| k.0 == kind).map(|k| k.1.as_str()).unwrap_or("figure")
}

pub fn prompt_fig_b(qg: &Qg, cards: &[FigCard], kinds: &[String], slots: &[Slot]) -> String {
    let f = fig_cfg(qg);
    let mut by_card: BTreeMap<usize, Vec<&Slot>> = BTreeMap::new();
    for s in slots {
        by_card.entry(s.card).or_default().push(s);
    }
    let blocks: Vec<String> = by_card.into_iter().map(|(ci, ss)| {
        let c = &cards[ci].card;
        let person = match ss[0].persona { Some(p) => who(qg, c.fallback, &c.expert_role, p, false), None => qg.cfg.text.templates.no_persona.clone() };
        let label = kind_label(f, &kinds[ci]);
        let looking = if card_type(f, &cards[ci].ctype).target == "figure" { format!("a figure ({label})") } else {
            format!("an explanation of a figure they are looking at ({label}); the query is sent together with the image") };
        let lines: Vec<String> = ss.iter().map(|s| slot_line(qg, &f.styles, s)).collect();
        fill(qg.cfg.file(&f.templates.stage_b_block), &[("person", &person), ("looking_for", &looking), ("need", &c.need),
            ("entities", &c.entities.join("; ")), ("slot_lines", &lines.join("\n"))])
    }).collect();
    fill(qg.cfg.file(&f.templates.stage_b), &[("blocks", &blocks.join("\n\n"))])
}

/// One article's figures: stage A per figure (image), one stage-B call. Rows per figure.
pub fn fig_article(qg: &Qg, ag: &ureq::Agent, a: &GenArgs, art: &Article, text_cards: usize, figs: &[&FigInput], boost: f64,
    meta: Option<&ArticleMeta>) -> (Vec<Generation>, Usage) {
    let f = fig_cfg(qg);
    let mut u = Usage::default();
    let mut cards = plan_fig_cards(qg, &art.article_id, text_cards, figs, boost);
    let mut out: Vec<Generation> = vec![];
    let mut used: Vec<usize> = cards.iter().map(|c| c.fig).collect();
    used.dedup();
    for &fi in &used {
        out.push(Generation { chunk_id: figs[fi].chunk_id.clone(), article_id: art.article_id.clone(), arm: a.arm.clone(), ..Default::default() });
    }
    if cards.is_empty() {
        return (out, u);
    }
    let mut kinds_by_fig: HashMap<usize, String> = HashMap::new();
    let mut errors: HashMap<usize, String> = HashMap::new();
    for &fi in &used {
        let fig = figs[fi];
        let mine: Vec<&FigCard> = cards.iter().filter(|c| c.fig == fi).collect();
        let url = match image_data_url(&fig.image) { Ok(x) => x, Err(e) => { errors.insert(fi, format!("image: {e}")); continue; } };
        let ga = GenArgs { max_tokens: stage_budget(qg, a.max_tokens, mine.len()), ..a.clone() };
        let user = json!([{"type": "image_url", "image_url": {"url": url}}, {"type": "text", "text": prompt_fig_a(qg, fig, &mine)}]);
        let (r, lat) = post(ag, &ga, qg.cfg.file(&f.templates.system_a), user, schema_fig_a(qg, &mine));
        u.calls += 1;
        match r.ok().and_then(|r| { u.add(&r, lat); json_body(&r.content) }) {
            Some(v) => {
                let mut mine_mut: Vec<&mut FigCard> = cards.iter_mut().filter(|c| c.fig == fi).collect();
                let kind = fill_fig_cards_meta(qg, &mut mine_mut, &v, meta);
                kinds_by_fig.insert(fi, kind);
            }
            None => {
                errors.insert(fi, "figure stage A failed".into());
                cards.iter_mut().filter(|c| c.fig == fi).for_each(|c| c.card.skip = true);
            }
        }
    }
    let kinds: Vec<String> = cards.iter().map(|c| kinds_by_fig.get(&c.fig).cloned().unwrap_or_else(|| "other".into())).collect();
    let slots = plan_fig_slots(qg, &cards, &kinds, figs, art);
    let mut accepted = HashMap::new();
    if !slots.is_empty() {
        let ga = GenArgs { max_tokens: stage_budget(qg, a.max_tokens, slots.len().div_ceil(2)), ..a.clone() };
        let (rb, lb) = post(ag, &ga, qg.cfg.file(&qg.cfg.text.templates.system_b), json!(prompt_fig_b(qg, &cards, &kinds, &slots)), crate::text::schema_b(&slots));
        u.calls += 1;
        match rb.ok().and_then(|r| { u.add(&r, lb); json_body(&r.content) }) {
            Some(vb) => {
                let chain = qg.cfg.filters.figure.as_ref().map(|x| x.query_reject.clone()).unwrap_or_else(|| qg.cfg.filters.query_reject.clone());
                accepted = accept_b(qg, &vb, &slots, &|s: &Slot| cards[s.card].card.need.clone(),
                    &|s: &Slot| Some(figs[cards[s.card].fig].text.clone()), &f.styles, &chain, &art.article_id);
            }
            None => { errors.insert(usize::MAX, "figure stage B failed".into()); }
        }
    }
    let generator = generator_label(a);
    let k = out.len().max(1) as u64;
    for (g, &fi) in out.iter_mut().zip(&used) {
        let fig = figs[fi];
        g.prompt_tokens = u.pt / k;
        g.completion_tokens = u.ct / k;
        g.reasoning_tokens = u.rt / k;
        g.reasoning_chars = u.rc / k;
        g.latency_s = u.lat;
        let mut q = Map::new();
        let mut m = Map::new();
        for s in slots.iter().filter(|s| cards[s.card].fig == fi) {
            let fc = &cards[s.card];
            let c = &fc.card;
            let ct = card_type(f, &fc.ctype);
            let key = s.id.splitn(2, '.').nth(1).unwrap_or(&s.id).to_string();
            let (clean, noisy, why) = accepted.get(&s.id).cloned().unwrap_or((String::new(), String::new(), "missing"));
            let rejected = !why.is_empty();
            let p = s.persona.map(|i| &qg.lib[i]);
            let composed = ct.target != "figure";
            m.insert(key.clone(), json!({"task": if composed { "I1" } else { "F3" }, "recipe": qg.cfg.recipe, "generator": generator,
                "intent": fc.ctype, "style": s.style, "hint": s.hint, "need": c.need, "visible": c.narrative, "answer": c.answer,
                "specificity": c.specificity, "persona_id": p.map(|p| p.id.clone()), "persona_group": p.map(|p| p.group.clone()),
                "persona": s.persona.map(|i| who(qg, c.fallback, &c.expert_role, i, false)), "persona_fallback": c.fallback,
                "english": p.map(|p| p.english.clone()), "persona_role": p.map(|p| p.role.clone()), "expertise": p.map(|p| p.expertise),
                "literacy": p.map(|p| p.literacy.clone()), "stretch": false, "license": fig.licence, "commercial": art.commercial,
                "clean": clean, "query": noisy, "banned": why == "banned", "rejected": rejected, "reject": why,
                "modality": if composed { "figure_composed" } else { "figure" }, "query_modality": ct.query_modality,
                "target_id": if composed { fig.citing_chunk.clone().unwrap_or_default() } else { fig.chunk_id.clone() },
                "figure_id": fig.chunk_id, "figure_kind": kinds[s.card], "caption_type": fig.section, "image": fig.image,
                "caption_overlap": jaccard(&clean, &fig.text),
                "register": style(&f.styles, &s.style).register}));
            if !rejected {
                q.insert(key, json!(noisy));
            }
        }
        let card_rows: Vec<Value> = cards.iter().enumerate().filter(|(_, c)| c.fig == fi).map(|(i, c)| {
            let mut r = crate::text::card_row(qg, &c.card);
            r["figure_kind"] = json!(kinds[i]);
            r["visible"] = json!(c.card.narrative);
            r
        }).collect();
        g.ok = !q.is_empty();
        g.error = errors.get(&fi).cloned().or_else(|| errors.get(&usize::MAX).cloned()).or_else(|| (!g.ok).then(|| "no accepted slots".to_string()));
        g.queries = q;
        g.meta = json!({"slots": m, "cards": card_rows});
    }
    (out, u)
}

// ------------------------------------------------------------------------------------------------ fig-inputs

fn ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Per figure id: (citing paragraphs, rights notice in the <fig>).
pub fn jats_figs(xml: &str, max_par: usize, max_words: usize) -> HashMap<String, (Vec<String>, bool)> {
    let opts = roxmltree::ParsingOptions { allow_dtd: true, nodes_limit: u32::MAX };
    let mut out: HashMap<String, (Vec<String>, bool)> = HashMap::new();
    let Ok(doc) = roxmltree::Document::parse_with_options(xml, opts) else { return out };
    for fig in doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == "fig") {
        let Some(id) = fig.attribute("id") else { continue };
        let rights = fig.descendants().any(|d| d.is_element() && matches!(d.tag_name().name(), "permissions" | "copyright-statement" | "license" | "copyright-holder"));
        out.entry(id.to_string()).or_default().1 = rights;
    }
    for p in doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == "p") {
        if p.ancestors().skip(1).any(|a| a.is_element() && matches!(a.tag_name().name(), "fig" | "caption" | "table-wrap")) {
            continue;
        }
        let rids: Vec<String> = p.descendants().filter(|x| x.is_element() && x.tag_name().name() == "xref" && x.attribute("ref-type") == Some("fig"))
            .flat_map(|x| x.attribute("rid").unwrap_or("").split_whitespace().map(str::to_string).collect::<Vec<_>>()).collect();
        if rids.is_empty() {
            continue;
        }
        let t: String = p.descendants().filter(|d| d.is_text()).filter_map(|d| d.text()).collect();
        let t = first_words(&ws(&t), max_words);
        if t.is_empty() {
            continue;
        }
        for r in rids {
            let e = out.entry(r).or_default();
            if e.0.len() < max_par && !e.0.contains(&t) {
                e.0.push(t.clone());
            }
        }
    }
    out
}

fn find_items(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(root)? {
        let p = e?.path();
        if p.is_dir() {
            find_items(&p, out)?;
        } else if p.file_name().is_some_and(|n| n == "items.jsonl.gz") {
            out.push(p);
        }
    }
    Ok(())
}

/// Fetch (or copy from `local`) and resize to longest side <= max_side, JPEG q90.
pub fn fetch_resize(agent: &ureq::Agent, url: &str, local: Option<&Path>, dest: &Path, max_side: u32) -> Result<()> {
    if dest.exists() {
        return Ok(());
    }
    let key = url.strip_prefix("s3://pmc-oa-opendata/").unwrap_or(url);
    let key = key.split('?').next().unwrap_or(key);
    let body = match local {
        Some(dir) => std::fs::read(dir.join(key)).with_context(|| format!("local {key}"))?,
        None => {
            let mut b = vec![];
            std::io::Read::read_to_end(&mut agent.get(&format!("https://pmc-oa-opendata.s3.amazonaws.com/{key}")).call()?.into_reader(), &mut b)?;
            b
        }
    };
    let mut rd = image::ImageReader::new(std::io::Cursor::new(&body)).with_guessed_format()?;
    let mut lim = image::Limits::default();
    lim.max_alloc = Some(1 << 30);
    rd.limits(lim);
    let img = rd.decode().context("decode")?;
    let img = if img.width().max(img.height()) > max_side { img.resize(max_side, max_side, image::imageops::FilterType::Lanczos3) } else { img };
    let tmp = dest.with_extension("tmp");
    {
        let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, 90).encode_image(&img.to_rgb8())?;
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

pub struct FigInputArgs<'a> {
    pub articles: &'a Path,
    pub items_root: &'a Path,
    pub xml_cache: &'a Path,
    pub manifest: Option<&'a Path>,
    pub exclude: Option<&'a Path>,
    pub image_ids: Option<&'a HashSet<String>>,
    pub local_images: Option<&'a Path>,
    pub out: &'a Path,
}

pub fn build_inputs(qg: &Qg, a: &FigInputArgs) -> Result<(Vec<FigInput>, Value)> {
    let f = fig_cfg(qg);
    let rights = Regex::new(&f.rights_regex)?;
    let arts: Vec<Article> = read_jsonl(a.articles)?;
    let mut exclude: HashSet<String> = HashSet::new();
    if let Some(p) = a.exclude {
        for l in std::fs::read_to_string(p)?.lines() {
            let l = l.trim();
            if l.starts_with('{') {
                if let Some(x) = serde_json::from_str::<Value>(l).ok().and_then(|v| v["article_id"].as_str().map(str::to_string)) { exclude.insert(x); }
            } else if !l.is_empty() {
                exclude.insert(l.to_string());
            }
        }
    }
    let split: Option<HashMap<String, String>> = match a.manifest {
        Some(p) => Some(read_jsonl::<Value>(p)?.into_iter().filter_map(|r| Some((r["article_id"].as_str()?.to_string(), r["split"].as_str()?.to_string()))).collect()),
        None => None,
    };
    let mut why: BTreeMap<String, usize> = BTreeMap::new();
    let mut bump = |k: &str| *why.entry(k.to_string()).or_default() += 1;
    let mut keep: HashMap<String, &Article> = HashMap::new();
    for x in &arts {
        if x.commercial == Some(false) { bump("article_noncommercial"); continue; }
        if exclude.contains(&x.article_id) { bump("article_held_out"); continue; }
        if let Some(s) = &split { if s.get(&x.article_id).map(String::as_str) != Some("train") { bump("article_not_train"); continue; } }
        keep.insert(x.article_id.clone(), x);
    }
    let mut files = vec![];
    find_items(a.items_root, &mut files)?;
    files.sort();
    // unit_id -> (article, caption, url, figure_id); text units per article; abstracts
    let mut figs: Vec<(String, String, String, String, String)> = vec![];
    let mut texts: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut abstracts: HashMap<String, Vec<String>> = HashMap::new();
    for p in &files {
        for r in read_jsonl::<Value>(p)? {
            let Some(aid) = r["article_id"].as_str() else { continue };
            if !keep.contains_key(aid) {
                continue;
            }
            match r["kind"].as_str().unwrap_or("") {
                "figure" => figs.push((r["unit_id"].as_str().unwrap_or("").into(), aid.into(), r["caption"].as_str().unwrap_or("").into(),
                    r["source_url"].as_str().unwrap_or("").into(), r["locator"]["figure_id"].as_str().unwrap_or("").into())),
                "text" => {
                    let sec = r["locator"]["section"].as_array().map(|s| s.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" > ")).unwrap_or_default();
                    let t = r["text"].as_str().unwrap_or("").to_string();
                    if sec.to_lowercase().starts_with("abstract") { abstracts.entry(aid.into()).or_default().push(t); }
                    else { texts.entry(aid.into()).or_default().push((r["unit_id"].as_str().unwrap_or("").into(), t)); }
                }
                _ => {}
            }
        }
    }
    let mut jats: HashMap<String, HashMap<String, (Vec<String>, bool)>> = HashMap::new();
    let mut cands: BTreeMap<String, Vec<FigInput>> = BTreeMap::new();
    for (uid, aid, cap, url, fid) in figs {
        let ext = url.split('?').next().unwrap_or("").rsplit('.').next().unwrap_or("").to_lowercase();
        if !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp" | "gif" | "tif" | "tiff") { bump("fig_not_image"); continue; }
        if cap.split_whitespace().count() < f.min_caption_words { bump("fig_short_caption"); continue; }
        if rights.is_match(&cap) { bump("fig_rights_phrase"); continue; }
        if a.image_ids.is_some_and(|s| !s.contains(&uid)) { bump("fig_no_image_vector"); continue; }
        let j = jats.entry(aid.clone()).or_insert_with(|| {
            std::fs::read(a.xml_cache.join(format!("{aid}.xml.gz"))).ok()
                .and_then(|b| { let mut s = String::new(); std::io::Read::read_to_string(&mut flate2::read::MultiGzDecoder::new(&b[..]), &mut s).ok().map(|_| s) })
                .map(|x| jats_figs(&x, f.citing_paragraphs, f.citing_max_words)).unwrap_or_default()
        });
        let (citing, notice) = j.get(&fid).cloned().unwrap_or_default();
        if notice { bump("fig_jats_permissions"); continue; }
        let Some(ty) = caption_type(f, &cap) else { bump("fig_untyped_caption"); continue };
        let citing_chunk = citing.first().and_then(|c| {
            let probe = first_words(c, 10);
            texts.get(&aid).and_then(|v| v.iter().find(|(_, t)| ws(t).contains(&probe)).map(|(u, _)| u.clone()))
        });
        let art = keep[&aid];
        cands.entry(aid.clone()).or_default().push(FigInput { chunk_id: uid.clone(), article_id: aid.clone(), cluster: String::new(),
            title: art.title.clone(), abstract_text: abstracts.get(&aid).map(|v| v.join(" ")).unwrap_or_default(), section: ty,
            words: cap.split_whitespace().count(), text: cap, marker: String::new(), figure_id: fid, image: url, citing, citing_chunk,
            licence: art.license_code.clone() });
    }
    // per article: prefer figures with citing text, spread over caption types, hash order inside
    let img_dir = a.out.join("figs");
    std::fs::create_dir_all(&img_dir)?;
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(120)).build();
    let mut out = vec![];
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_article: BTreeMap<usize, usize> = BTreeMap::new();
    for (aid, mut v) in cands {
        v.sort_by_key(|x| (x.citing.is_empty(), uh(&[&x.chunk_id, "pick"])));
        let mut picked: Vec<FigInput> = vec![];
        let mut types: HashSet<String> = HashSet::new();
        for pass in 0..2 {
            for x in &v {
                if picked.len() >= f.max_figures_per_article || picked.iter().any(|p| p.chunk_id == x.chunk_id) { continue; }
                if pass == 0 && types.contains(&x.section) { continue; }
                types.insert(x.section.clone());
                picked.push(x.clone());
            }
        }
        let mut n = 0;
        for mut x in picked {
            let dest = img_dir.join(format!("{}.jpg", x.chunk_id));
            match fetch_resize(&agent, &x.image, a.local_images, &dest, f.image_max_side) {
                Ok(()) => {
                    x.image = dest.to_string_lossy().to_string();
                    *by_type.entry(x.section.clone()).or_default() += 1;
                    n += 1;
                    out.push(x);
                }
                Err(e) => { bump("fig_fetch_failed"); eprintln!("{aid} {}: {e}", x.chunk_id); }
            }
        }
        *per_article.entry(n).or_default() += 1;
    }
    let with_figs: HashSet<&str> = out.iter().map(|x| x.article_id.as_str()).collect();
    let stats = json!({"articles_in": arts.len(), "articles_kept": keep.len(), "articles_with_figures": with_figs.len(),
        "figures": out.len(), "by_caption_type": by_type, "figures_per_article": per_article,
        "with_citing_chunk": out.iter().filter(|x| x.citing_chunk.is_some()).count(), "dropped": why,
        "max_side": f.image_max_side});
    Ok((out, stats))
}
