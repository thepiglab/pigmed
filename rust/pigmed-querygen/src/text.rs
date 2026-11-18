//! Text arm, layout c3pf (recipe v3.1d), from the config:
//!   stage A  one call per article: per marked passage `slots_per_passage` cards, each offered one candidate reader
//!            per stratum; the model picks the plausible reader (or none: a field expert) and writes that reader's
//!            standalone need. Plus one HEALTH card per article for a patient / carer / clinician reader.
//!   filter   off-domain, skipped, document-referential and generic cards are dropped.
//!   stage B  one call per article that never sees the passage: need + key terms + persona card -> surface queries in
//!            styles drawn from the persona-constrained schedule, with per-style hints.
//!   stage C  typing noise from the persona's habits; the reject chain; duplicates.

use crate::client::{json_body, post, GenArgs, Generation};
use crate::config::{Clause, Config, Style};
use crate::persona::{holds, Persona};
use crate::rules::{Article, ArticleMeta, QueryCtx};
use crate::util::{fill, uh};
use crate::Qg;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SampleChunk {
    pub chunk_id: String,
    pub article_id: String,
    pub cluster: String,
    pub title: String,
    pub abstract_text: String,
    pub section: String,
    pub text: String,
    pub words: usize,
    #[serde(default)]
    pub marker: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Card {
    pub id: String,
    pub marker: String,
    pub persona: Option<usize>,
    pub cands: Vec<usize>,
    pub fallback: bool,
    pub off_domain: bool,
    pub health: bool,
    /// v3.1e: the lay-only card of a patient-relevant article
    pub lay: bool,
    /// v3.1e: no candidate fits and no concrete role is known (stage A or the field map): card dropped
    pub no_reader: bool,
    pub expert_role: String,
    pub need: String,
    pub narrative: String,
    pub pico: [String; 4],
    pub ctype: String,
    pub answer: String,
    pub specificity: String,
    pub audience: Vec<String>,
    pub min_expertise: u8,
    pub entities: Vec<String>,
    pub skip: bool,
}

/// One surface slot: (card, persona, style).
#[derive(Clone, Debug)]
pub struct Slot {
    pub id: String,  // "P1.c2.q1" (internal; never shown to the model)
    pub bid: String, // "slot-07": the id stage B sees
    pub card: usize,
    pub persona: Option<usize>,
    pub stretch: bool,
    pub style: String,
    pub hint: String,
}

// ------------------------------------------------------------------------------------------------ styles (shared)

fn clause_ok(c: &Clause, p: &Persona, stretch: bool) -> bool {
    c.min_expertise.is_none_or(|m| p.expertise >= m)
        && c.max_expertise.is_none_or(|m| p.expertise <= m)
        && (c.groups.is_empty() || c.groups.iter().any(|g| *g == p.group))
        && !c.english_not.iter().any(|e| *e == p.english)
        && !c.literacy_not.iter().any(|l| *l == p.literacy)
        && c.stretch.is_none_or(|s| s == stretch)
}

/// Styles allowed for a persona, with weights (None = no persona: every style without an English rule).
pub fn allowed_styles(styles: &[Style], p: Option<&Persona>, stretch: bool, instr_ok: bool) -> Vec<(String, u32)> {
    styles.iter().filter(|s| s.weight > 0 && (!s.requires_constraint || instr_ok) && match p {
        None => !s.allowed.iter().any(|c| !c.english_not.is_empty()),
        Some(p) => s.allowed.is_empty() || s.allowed.iter().any(|c| clause_ok(c, p, stretch)),
    }).map(|s| {
        let m = p.and_then(|p| s.weight_mult.iter().find(|w| holds(&w.when, p))).map(|w| w.mult).unwrap_or(1);
        (s.name.clone(), s.weight * m)
    }).collect()
}

pub fn pick_styles(styles: &[Style], n: usize, p: Option<&Persona>, stretch: bool, instr_ok: bool, key: &str) -> Vec<String> {
    let mut opts = allowed_styles(styles, p, stretch, instr_ok);
    let mut out = vec![];
    for k in 0..n {
        if opts.is_empty() {
            break;
        }
        let s = crate::util::pick(uh(&[key, "style", &k.to_string()]), &opts).to_string();
        opts.retain(|x| x.0 != s);
        out.push(s);
    }
    out
}

pub fn style<'a>(styles: &'a [Style], name: &str) -> &'a Style {
    styles.iter().find(|s| s.name == name).expect("style in config")
}

pub fn l2_slips<'a>(c: &'a Config, l1: &str) -> &'a str {
    let h = &c.hints.l2_slips;
    h.iter().find(|g| g.l1.iter().any(|x| x == l1)).or_else(|| h.iter().find(|g| g.l1.is_empty())).map(|g| g.slips.as_str()).unwrap_or("")
}

/// Style-specific instruction for one slot (`h`: the slot's hash).
pub fn hint(qg: &Qg, st: &Style, persona: Option<usize>, cons: &[String], kind: &str, h: usize) -> String {
    let c = &qg.cfg;
    match st.hints.as_str() {
        "constraint" => fill(&c.hints.constraint, &[("constraint", &cons[h % cons.len()])]),
        "hedges" => c.hints.hedges[h % c.hints.hedges.len()].clone(),
        "l2" => {
            let l1 = persona.map(|i| qg.lib[i].first_language.as_str()).unwrap_or("");
            let speaker = if l1.is_empty() { c.hints.l2_unknown_speaker.as_str() } else { l1 };
            fill(&c.hints.l2, &[("speaker", speaker), ("slips", l2_slips(c, l1))])
        }
        "scenes" => {
            let lay = persona.is_some_and(|i| c.hints.lay_scene_groups.iter().any(|g| *g == qg.lib[i].group));
            let l = if lay { &c.hints.lay_scenes } else { &c.hints.scenes };
            l[h % l.len()].clone()
        }
        "kind_forms" => {
            let f = c.figure.as_ref().unwrap();
            let forms = f.kind_forms.get(kind).or_else(|| f.kind_forms.get("default")).cloned().unwrap_or_default();
            if forms.is_empty() { String::new() } else { format!("phrase it like {}", forms[h % forms.len()]) }
        }
        _ => String::new(),
    }
}

// ------------------------------------------------------------------------------------------------ stage A

pub fn plan_cards(qg: &Qg, art: &Article, chunks: &[&SampleChunk]) -> Vec<Card> {
    plan_cards_meta(qg, art, chunks, None)
}

/// Lay pass (v3.1e): patient-relevant article, sampled at lay.article_rate.
pub fn wants_lay_card(qg: &Qg, art: &Article, meta: Option<&ArticleMeta>) -> bool {
    let Some(l) = &qg.cfg.lay else { return false };
    crate::rules::patient_relevant(&qg.cfg, meta, art) && (uh(&[&art.article_id, "lay-rate"]) % 1_000_000) as f64 / 1e6 < l.article_rate
}

pub fn plan_cards_meta(qg: &Qg, art: &Article, chunks: &[&SampleChunk], meta: Option<&ArticleMeta>) -> Vec<Card> {
    let t = &qg.cfg.text;
    let n = qg.lib.len() as u64;
    let mut out = vec![];
    for c in chunks {
        for s in 1..=t.slots_per_passage {
            let cands = t.candidate_strata.iter().enumerate().map(|(k, groups)| {
                qg.candidate((uh(&[&c.chunk_id, "pf", &s.to_string(), &k.to_string()]) % n) as usize, groups)
            }).collect();
            out.push(Card { id: format!("{}.c{s}", c.marker), marker: c.marker.clone(), cands, ..Default::default() });
        }
    }
    if t.health_card && !chunks.is_empty() {
        let c = chunks[(uh(&[&art.article_id, "health"]) % chunks.len() as u64) as usize];
        let cands = t.candidate_strata[..t.health_strata].iter().enumerate().map(|(k, groups)| {
            qg.candidate((uh(&[&c.chunk_id, "health", &k.to_string()]) % n) as usize, groups)
        }).collect();
        out.push(Card { id: format!("{}.h", c.marker), marker: c.marker.clone(), cands, health: true, ..Default::default() });
    }
    if !chunks.is_empty() && wants_lay_card(qg, art, meta) {
        let l = qg.cfg.lay.as_ref().unwrap();
        let lay = vec!["lay".to_string()];
        // one lay card on each of up to cards_per_article distinct passages, in hash order
        let mut order: Vec<&&SampleChunk> = chunks.iter().collect();
        order.sort_by_key(|c| uh(&[&art.article_id, "lay", &c.chunk_id]));
        for c in order.into_iter().take(l.cards_per_article.max(1)) {
            let mut cands: Vec<usize> = vec![];
            for k in 0..l.candidates {
                let p = qg.candidate((uh(&[&c.chunk_id, "lay", &k.to_string()]) % n) as usize, &lay);
                if !cands.contains(&p) { cands.push(p); }
            }
            out.push(Card { id: format!("{}.l", c.marker), marker: c.marker.clone(), cands, lay: true, ..Default::default() });
        }
    }
    out
}

pub fn readers(qg: &Qg, cands: &[usize]) -> String {
    let t = &qg.cfg.text;
    cands.iter().zip(&t.reader_labels).map(|(&p, r)| fill(&t.templates.reader, &[("label", r), ("card", &qg.cards[p])])).collect::<Vec<_>>().join("; ")
}

pub fn prompt_a(qg: &Qg, art: &Article, cards: &[Card]) -> String {
    let t = &qg.cfg.text.templates;
    let lines: Vec<String> = cards.iter().map(|c| {
        let tpl = if c.lay { &qg.cfg.lay.as_ref().unwrap().line } else if c.health { &t.health_line } else { &t.card_line };
        fill(tpl, &[("id", &c.id), ("marker", &c.marker), ("readers", &readers(qg, &c.cands))])
    }).collect();
    let rr = if cards.iter().any(|c| !c.cands.is_empty()) { format!("{}\n", qg.cfg.file(&t.reader_rules)) } else { String::new() };
    fill(qg.cfg.file(&t.stage_a), &[("article", &art.text), ("card_lines", &lines.join("\n")), ("passage_rule", &t.passage_rule),
        ("reader_rules", &rr), ("need_rules", qg.cfg.file(&t.need_rules)), ("type_defs", qg.cfg.file(&t.type_defs))])
}

pub fn reader_enum(qg: &Qg) -> Vec<String> {
    qg.cfg.text.reader_labels.iter().cloned().chain(["none".to_string()]).collect()
}

pub fn schema_a(qg: &Qg, cards: &[Card]) -> Value {
    let t = &qg.cfg.text;
    let ids: Vec<&str> = cards.iter().map(|c| c.id.as_str()).collect();
    let mut item = json!({"type": "object", "additionalProperties": false,
        "required": ["card", "need", "narrative", "pico", "type", "answer", "specificity", "audience", "min_expertise", "entities", "skip"],
        "properties": {"card": {"enum": ids}, "need": {"type": "string"}, "narrative": {"type": "string"},
            "pico": {"type": "object", "additionalProperties": false, "required": ["population", "intervention", "comparison", "outcome"],
                "properties": {"population": {"type": "string"}, "intervention": {"type": "string"}, "comparison": {"type": "string"}, "outcome": {"type": "string"}}},
            "type": {"enum": t.intent_types}, "answer": {"type": "string"},
            "specificity": {"enum": ["unique", "few", "many"]},
            "audience": {"type": "array", "minItems": 1, "maxItems": 5, "items": {"enum": t.audience}},
            "min_expertise": {"enum": ["0", "1", "2", "3"]},
            "entities": {"type": "array", "minItems": 0, "maxItems": 5, "items": {"type": "string"}},
            "skip": {"type": "boolean"}}});
    if cards.iter().any(|c| !c.cands.is_empty()) {
        item["properties"]["reader"] = json!({"enum": reader_enum(qg)});
        item["required"].as_array_mut().unwrap().insert(1, json!("reader"));
        item["properties"]["expert_role"] = json!({"type": "string"});
        item["required"].as_array_mut().unwrap().insert(2, json!("expert_role"));
    }
    json!({"type": "object", "required": ["biomedical", "cards"], "additionalProperties": false,
        "properties": {"biomedical": {"type": "boolean"}, "cards": {"type": "array", "minItems": cards.len(), "maxItems": cards.len(), "items": item}}})
}

/// Reader choice for a card from stage A's `reader` field: a candidate; for health / lay cards none = skip; else the
/// fallback. "generic" (v3.1d): a field expert with stage A's role or the generic wording. "role_or_map" (v3.1e): stage
/// A's role if concrete, else the journal / MeSH field map, else no reader (card dropped).
pub fn choose_reader(qg: &Qg, c: &mut Card, e: &Value, mode: &str, meta: Option<&ArticleMeta>) {
    if c.cands.is_empty() {
        return;
    }
    let labels = &qg.cfg.text.reader_labels;
    match labels[..c.cands.len().min(labels.len())].iter().position(|r| e["reader"].as_str() == Some(r.as_str())) {
        Some(i) => { c.persona = Some(c.cands[i]); c.fallback = false; }
        None if c.health || c.lay => { c.skip = true; }
        None => {
            let role: String = e["expert_role"].as_str().unwrap_or("").trim().chars().take(qg.cfg.text.expert_role_max_chars).collect();
            if mode == "role_or_map" {
                let vague = qg.cfg.fallback.as_ref().map(|f| f.vague_roles.iter().any(|v| v.eq_ignore_ascii_case(&role))).unwrap_or(false);
                let role = if !role.is_empty() && !vague { Some(role) } else { crate::rules::field_role(&qg.cfg, meta) };
                match role {
                    Some(r) => { c.persona = Some(qg.fallback_persona(c.cands[0])); c.fallback = true; c.expert_role = r; }
                    None => { c.no_reader = true; }
                }
            } else {
                c.persona = Some(qg.fallback_persona(c.cands[0]));
                c.fallback = true;
                c.expert_role = role;
            }
        }
    }
}

/// Fill planned cards from the stage-A reply; unknown or repeated ids are ignored, missing cards stay skipped.
pub fn fill_cards(qg: &Qg, plan: &[Card], v: &Value) -> Vec<Card> {
    fill_cards_meta(qg, plan, v, None)
}

pub fn fill_cards_meta(qg: &Qg, plan: &[Card], v: &Value, meta: Option<&ArticleMeta>) -> Vec<Card> {
    let mut by: HashMap<String, Card> = plan.iter().map(|c| (c.id.clone(), Card { skip: true, ..c.clone() })).collect();
    let mut seen = HashSet::new();
    for e in v["cards"].as_array().into_iter().flatten() {
        let Some(id) = e["card"].as_str() else { continue };
        if !seen.insert(id.to_string()) {
            continue;
        }
        let Some(c) = by.get_mut(id) else { continue };
        let s = |k: &str| e[k].as_str().unwrap_or("").trim().to_string();
        c.need = s("need");
        c.narrative = s("narrative");
        let ps = |k: &str| e["pico"][k].as_str().unwrap_or("").trim().to_string();
        c.pico = [ps("population"), ps("intervention"), ps("comparison"), ps("outcome")];
        c.ctype = s("type");
        c.answer = s("answer");
        c.specificity = s("specificity");
        c.audience = e["audience"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
        c.min_expertise = s("min_expertise").parse().unwrap_or(0);
        c.entities = e["entities"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(|x| x.trim().to_string())).filter(|x| !x.is_empty()).collect();
        c.skip = e["skip"].as_bool().unwrap_or(false) || c.need.split_whitespace().count() < qg.cfg.text.min_need_words;
        choose_reader(qg, c, e, qg.cfg.fallback_mode(), meta);
    }
    plan.iter().map(|c| by.remove(&c.id).unwrap()).collect()
}

/// Why a card is not used ("" = kept), checks in config order.
pub fn card_drop(qg: &Qg, c: &Card) -> &'static str {
    let f = &qg.cfg.filters;
    for name in &f.card_drop {
        let hit = match name.as_str() {
            "off_domain" => c.off_domain,
            "skip" => c.skip,
            "no_reader" => c.no_reader,
            "doc_ref" => qg.rules.doc_ref(&qg.cfg, &c.need),
            "generic" => f.generic_specificity.iter().any(|s| *s == c.specificity) || qg.rules.research_agenda(&c.need),
            _ => false,
        };
        if hit {
            return crate::rules::intern(name);
        }
    }
    ""
}

/// Persona text shown to stage B.
pub fn who(qg: &Qg, fallback: bool, expert_role: &str, p: usize, stretch: bool) -> String {
    let t = &qg.cfg.text;
    let pc = &qg.cards[p];
    if fallback {
        let role = expert_role.trim();
        let generic = role.is_empty() || t.fallback_generic_roles.iter().any(|g| g.eq_ignore_ascii_case(role));
        let role = if generic { t.fallback_role.as_str() } else { role };
        format!("{role}{}", &pc[pc.find(',').unwrap_or(pc.len())..])
    } else if stretch {
        format!("{pc}{}", t.stretch_suffix)
    } else {
        pc.clone()
    }
}

// ------------------------------------------------------------------------------------------------ stage B

pub fn plan_slots(qg: &Qg, cards: &[Card], art: &Article) -> Vec<Slot> {
    let cons = qg.rules.constraints(&qg.cfg, art);
    let mut out = vec![];
    for (ci, c) in cards.iter().enumerate() {
        if !card_drop(qg, c).is_empty() {
            continue;
        }
        let key = format!("{}|{}", art.article_id, c.id);
        let persona = c.persona;
        let p = persona.map(|i| &qg.lib[i]);
        for (k, st) in pick_styles(&qg.cfg.styles, qg.cfg.text.styles_per_pair, p, false, !cons.is_empty(), &key).into_iter().enumerate() {
            let h = uh(&[&key, "hint", &k.to_string()]) as usize;
            let hint = hint(qg, style(&qg.cfg.styles, &st), persona, &cons, "", h);
            out.push(Slot { id: format!("{}.q{}", c.id, k + 1), bid: format!("slot-{:02}", out.len() + 1), card: ci, persona, stretch: false, style: st, hint });
        }
    }
    out
}

pub fn slot_line(qg: &Qg, styles: &[Style], s: &Slot) -> String {
    let t = &qg.cfg.text.templates;
    let h = if s.hint.is_empty() { String::new() } else { fill(&t.hint, &[("hint", &s.hint)]) };
    fill(&t.slot_line, &[("bid", &s.bid), ("style", &s.style), ("desc", &style(styles, &s.style).desc), ("hint", &h)])
}

pub fn prompt_b(qg: &Qg, cards: &[Card], slots: &[Slot]) -> String {
    let t = &qg.cfg.text.templates;
    let mut by_card: BTreeMap<usize, Vec<&Slot>> = BTreeMap::new();
    for s in slots {
        by_card.entry(s.card).or_default().push(s);
    }
    let blocks: Vec<String> = by_card.into_iter().map(|(ci, ss)| {
        let c = &cards[ci];
        let person = match ss[0].persona { Some(p) => who(qg, c.fallback, &c.expert_role, p, ss[0].stretch), None => t.no_persona.clone() };
        let lines: Vec<String> = ss.iter().map(|s| slot_line(qg, &qg.cfg.styles, s)).collect();
        fill(qg.cfg.file(&t.stage_b_block), &[("person", &person), ("need", &c.need), ("entities", &c.entities.join("; ")), ("slot_lines", &lines.join("\n"))])
    }).collect();
    fill(qg.cfg.file(&t.stage_b), &[("blocks", &blocks.join("\n\n"))])
}

pub fn schema_b(slots: &[Slot]) -> Value {
    let ids: Vec<&str> = slots.iter().map(|s| s.bid.as_str()).collect();
    json!({"type": "object", "required": ["slots"], "additionalProperties": false, "properties": {"slots": {
        "type": "array", "minItems": slots.len(), "maxItems": slots.len(), "items": {"type": "object", "additionalProperties": false,
        "required": ["slot", "query", "skip"], "properties": {"slot": {"enum": ids}, "query": {"type": "string"}, "skip": {"type": "boolean"}}}}}})
}

/// Output budget for a stage call: at least `base`, else per-card tokens plus framing, capped.
pub fn stage_budget(qg: &Qg, base: u32, cards: usize) -> u32 {
    let b = &qg.cfg.text.budget;
    base.max(b.per_card * cards as u32 + b.overhead).min(b.cap)
}

/// (clean, noisy, reject) per internal slot id from a stage-B reply.
pub fn accept_b(qg: &Qg, vb: &Value, slots: &[Slot], need: &dyn Fn(&Slot) -> String, caption: &dyn Fn(&Slot) -> Option<String>,
    styles: &[Style], chain: &[String], key: &str) -> HashMap<String, (String, String, &'static str)> {
    let mut accepted = HashMap::new();
    let mut seen = HashSet::new();
    for e in vb["slots"].as_array().into_iter().flatten() {
        let Some(bid) = e["slot"].as_str() else { continue };
        let Some(s) = slots.iter().find(|s| s.bid == bid) else { continue };
        if accepted.contains_key(&s.id) {
            continue;
        }
        let clean = e["query"].as_str().unwrap_or("").trim().to_string();
        let cap = caption(s);
        let x = QueryCtx { need: &need(s), style: style(styles, &s.style), caption: cap.as_deref() };
        let mut why = qg.rules.reject(&qg.cfg, chain, &clean, &x);
        if why.is_empty() && !seen.insert(clean.to_lowercase()) {
            why = "duplicate";
        }
        if why.is_empty() && e["skip"].as_bool().unwrap_or(false) {
            why = "skip";
        }
        let noisy = match s.persona { Some(p) => crate::rules::noise(&qg.cfg.noise, &clean, &qg.lib[p], &format!("{key}|{}", s.id)), None => clean.clone() };
        accepted.insert(s.id.clone(), (clean, noisy, why));
    }
    accepted
}

pub fn generator_label(a: &GenArgs) -> String {
    std::env::var("PIGMED_GENERATOR").unwrap_or_else(|_| a.model.clone())
}

#[derive(Default)]
pub struct Usage {
    pub pt: u64,
    pub ct: u64,
    pub rt: u64,
    pub rc: u64,
    pub lat: f64,
    pub calls: usize,
}

impl Usage {
    pub fn add(&mut self, r: &crate::client::Reply, lat: f64) {
        self.pt += r.prompt_tokens;
        self.ct += r.completion_tokens;
        self.rt += r.reasoning_tokens;
        self.rc += r.reasoning_chars;
        self.lat += lat;
    }
}

/// One article: stage A, stage B, rows per chunk. Returns the generations and the article's cards.
pub fn text_article(qg: &Qg, ag: &ureq::Agent, a: &GenArgs, art: &Article, chunks: &[&SampleChunk]) -> (Vec<Generation>, Vec<Card>, Usage) {
    text_article_meta(qg, ag, a, art, chunks, None)
}

pub fn text_article_meta(qg: &Qg, ag: &ureq::Agent, a: &GenArgs, art: &Article, chunks: &[&SampleChunk], meta: Option<&ArticleMeta>) -> (Vec<Generation>, Vec<Card>, Usage) {
    let mut out: Vec<Generation> = chunks.iter().map(|c| Generation { chunk_id: c.chunk_id.clone(), article_id: c.article_id.clone(),
        arm: a.arm.clone(), ..Default::default() }).collect();
    let mut u = Usage::default();
    let k = chunks.len().max(1) as u64;
    let plan = plan_cards_meta(qg, art, chunks, meta);
    let a = &GenArgs { max_tokens: stage_budget(qg, a.max_tokens, plan.len()), ..a.clone() };
    let t = &qg.cfg.text.templates;
    let (ra, la) = post(ag, a, qg.cfg.file(&t.system_a), json!(prompt_a(qg, art, &plan)), schema_a(qg, &plan));
    u.calls += 1;
    let ra = match ra { Ok(r) => r, Err(e) => { out.iter_mut().for_each(|g| g.error = Some(format!("stage A: {e}"))); return (out, plan, u); } };
    u.add(&ra, la);
    let Some(va) = json_body(&ra.content) else {
        let msg = format!("stage A unparsed: {} chars, {} completion tokens, max {}: {}", ra.content.len(), ra.completion_tokens, a.max_tokens, ra.content.chars().take(200).collect::<String>());
        out.iter_mut().for_each(|g| g.error = Some(msg.clone()));
        return (out, plan, u);
    };
    let mut cards = fill_cards_meta(qg, &plan, &va, meta);
    if !crate::rules::biomedical(&qg.cfg, meta, va["biomedical"] != json!(false)) {
        cards.iter_mut().for_each(|c| c.off_domain = true);
    }
    let slots = plan_slots(qg, &cards, art);
    let mut accepted: HashMap<String, (String, String, &'static str)> = HashMap::new();
    if !slots.is_empty() {
        let (rb, lb) = post(ag, a, qg.cfg.file(&t.system_b), json!(prompt_b(qg, &cards, &slots)), schema_b(&slots));
        u.calls += 1;
        match rb.ok().and_then(|r| { u.add(&r, lb); json_body(&r.content) }) {
            Some(vb) => {
                accepted = accept_b(qg, &vb, &slots, &|s: &Slot| cards[s.card].need.clone(), &|_| None, &qg.cfg.styles,
                    &qg.cfg.filters.query_reject, &art.article_id);
            }
            None => out.iter_mut().for_each(|g| g.error = Some("stage B failed".into())),
        }
    }
    let card_rows: Vec<Value> = cards.iter().map(|c| card_row(qg, c)).collect();
    let generator = generator_label(a);
    for (g, ch) in out.iter_mut().zip(chunks) {
        g.prompt_tokens = u.pt / k;
        g.completion_tokens = u.ct / k;
        g.reasoning_tokens = u.rt / k;
        g.reasoning_chars = u.rc / k;
        g.latency_s = u.lat;
        let pre = format!("{}.", ch.marker);
        let mut q = Map::new();
        let mut m = Map::new();
        for s in slots.iter().filter(|s| s.id.starts_with(&pre)) {
            let c = &cards[s.card];
            let key = s.id[pre.len()..].to_string();
            let (clean, noisy, why) = accepted.get(&s.id).cloned().unwrap_or((String::new(), String::new(), "missing"));
            let rejected = !why.is_empty();
            let p = s.persona.map(|i| &qg.lib[i]);
            let st = style(&qg.cfg.styles, &s.style);
            m.insert(key.clone(), json!({"task": "T3", "recipe": qg.cfg.recipe, "generator": generator, "intent": c.ctype, "style": s.style, "hint": s.hint,
                "need": c.need, "narrative": c.narrative, "pico": c.pico, "answer": c.answer,
                "specificity": c.specificity, "persona_id": p.map(|p| p.id.clone()), "persona_group": p.map(|p| p.group.clone()),
                "persona": s.persona.map(|i| who(qg, c.fallback, &c.expert_role, i, s.stretch)), "persona_fallback": c.fallback, "english": p.map(|p| p.english.clone()), "stretch": s.stretch,
                "license": art.license_code, "commercial": art.commercial,
                "clean": clean, "query": noisy, "banned": why == "banned", "rejected": rejected, "reject": why,
                "modality": "text_passage", "query_modality": "text", "target_id": ch.chunk_id, "register": st.register,
                "persona_role": p.map(|p| p.role.clone()), "expertise": p.map(|p| p.expertise), "literacy": p.map(|p| p.literacy.clone()),
                "health_card": c.health, "lay_card": c.lay}));
            if !rejected {
                q.insert(key, json!(noisy));
            }
        }
        let mine: Vec<&Value> = card_rows.iter().filter(|r| r["card"].as_str().is_some_and(|x| x.starts_with(&pre))).collect();
        g.ok = !q.is_empty();
        if !g.ok && g.error.is_none() {
            g.error = Some("no accepted slots".into());
        }
        g.queries = q;
        g.meta = json!({"slots": m, "cards": mine});
    }
    (out, cards, u)
}

pub fn card_row(qg: &Qg, c: &Card) -> Value {
    json!({"card": c.id, "need": c.need, "narrative": c.narrative, "pico": c.pico, "type": c.ctype, "want_type": "",
        "answer": c.answer, "specificity": c.specificity, "audience": c.audience, "min_expertise": c.min_expertise,
        "entities": c.entities, "persona": c.persona.map(|p| qg.lib[p].id.clone()), "persona_fallback": c.fallback, "persona_matched": false,
        "candidates": c.cands.iter().map(|&p| qg.lib[p].id.clone()).collect::<Vec<_>>(), "drop": card_drop(qg, c),
        "expert_role": c.expert_role, "health": c.health, "lay": c.lay, "no_reader": c.no_reader})
}
