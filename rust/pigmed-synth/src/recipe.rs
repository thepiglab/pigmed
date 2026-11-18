//! Query recipe v2: every query slot gets a deterministic spec (intent, format,
//! length, difficulty, persona) drawn from target shares; text is one call per article with personas brainstormed
//! first; figures get a *blind* call (image + title + abstract, caption withheld) and a *grounded* call (image +
//! caption + citing text, plus one composed image+question query). A cross-model `judge` scores any arm's queries,
//! and `metrics` measures templating, caption copying and lexical overlap.

use crate::figure::FigureSample;
use crate::generate::{image_data_url, post, scrape, GenArgs, Generation, Layout, Reply};
use crate::sample::{hash, Article, SampleChunk};
use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const SYSTEM2: &str = "You write realistic search queries for a biomedical literature search engine that indexes \
article passages and figures. Write what real people type: researchers, clinicians, students, patients. Do not write \
exam questions. Follow each slot's spec exactly. A query must make sense to someone who has never seen the article: \
name the specific drug, gene, organism, population, assay or condition instead of saying \"this study\", \"the \
authors\", \"the passage\", \"the figure above\" or \"Figure 2\". Never copy four or more consecutive words from the \
material you are given. Vary how queries begin; do not start most of them with \"What is\" or \"What are\". If a slot \
cannot be written honestly from the material, set \"skip\": true and leave \"query\" empty. Output JSON only.";

const T_INTENTS: [(&str, u32); 10] = [("fact_lookup", 15), ("result_number", 15), ("mechanism_why", 10),
    ("method_protocol", 10), ("comparison", 8), ("clinical_decision", 8), ("contextual", 14), ("lay_question", 8),
    ("claim", 7), ("open_ended", 5)];
const FORMATS: [(&str, u32); 5] = [("question", 50), ("keyword", 15), ("statement", 10), ("instruction", 15), ("scenario", 10)];
const LENGTHS: [(&str, u32); 4] = [("short", 35), ("medium", 40), ("long", 15), ("keyword", 10)];
const DIFFS: [(&str, u32); 3] = [("direct", 30), ("paraphrase", 45), ("inferential", 25)];
const PERSONAS: [(&str, u32); 5] = [("1", 30), ("2", 25), ("3", 20), ("4", 15), ("5", 10)];
const F1_INTENTS: [(&str, u32); 3] = [("visual_find", 50), ("appearance_detail", 35), ("keyword_find", 15)];
const F2_INTENTS: [(&str, u32); 4] = [("finding_question", 45), ("clinician_use", 25), ("contextual", 20), ("comparison_in_figure", 10)];
const T2_INTENTS: [(&str, u32); 3] = [("comparison", 60), ("mechanism_why", 20), ("open_ended", 20)];
const FIG_ROLES: [(&str, u32); 3] = [("biomedical researcher", 45), ("clinician", 35), ("student", 20)];
const FIG_ROLES_CLINICAL: [(&str, u32); 4] = [("biomedical researcher", 35), ("clinician", 40), ("student", 15), ("patient", 10)];
pub const TEXT_SLOTS: usize = 4;

/// Style examples rotated into each call (3 per call), marked as style only.
pub const EXEMPLARS: [&str; 40] = [
    "does metformin lower lactate in septic shock patients with kidney injury",
    "tirzepatide weight loss at 72 weeks vs semaglutide",
    "why do BRCA1-deficient cells depend on PARP for replication fork stability",
    "how was minimal residual disease measured in pediatric B-ALL trials",
    "my mother has macular degeneration, can an eye injection stop her vision getting worse",
    "evidence that gut microbiome diversity predicts response to anti-PD-1 therapy in melanoma",
    "IL-6 receptor blockade ventilator-free days COVID-19",
    "find studies comparing robotic and laparoscopic distal pancreatectomy blood loss",
    "a 70-year-old on warfarin needs a dental extraction; is tranexamic acid mouthwash enough to stop bleeding",
    "Plasmodium falciparum kelch13 mutations artemisinin delayed clearance Africa",
    "is vitamin D supplementation linked to fewer fractures in older women",
    "which CRISPR delivery method gave the highest editing efficiency in primary T cells",
    "mouse model sepsis cecal ligation puncture survival curve neutrophil depletion",
    "how much does sodium intake reduction lower systolic pressure in hypertensive adults",
    "zebrafish fin regeneration blastema Wnt signaling",
    "can children with peanut allergy safely tolerate baked egg",
    "dose-dependent hepatotoxicity of acetaminophen in chronic alcohol users",
    "mechanism by which SGLT2 inhibitors protect the failing heart",
    "find a figure showing kaplan-meier curves of overall survival by PD-L1 expression in lung cancer",
    "confocal images of tau tangles spreading between hippocampal neurons",
    "chest CT ground-glass opacities progression in early viral pneumonia",
    "western blot showing phosphorylated ERK decreasing after MEK inhibitor treatment",
    "what explains the plateau in the dose-response curve shown here",
    "ROC curve AUC of troponin assay for myocardial infarction at presentation",
    "H&E section showing granulomas with central necrosis in lung tissue",
    "schematic of the mTORC1 nutrient sensing pathway with Rag GTPases",
    "volcano plot of differentially expressed genes after hypoxia in endothelial cells",
    "MRI of the knee with bone marrow edema after anterior cruciate ligament tear",
    "forest plot of statin trials and stroke risk reduction",
    "bacterial growth curves under increasing ciprofloxacin concentrations",
    "does exercise training improve fatigue in people undergoing chemotherapy for breast cancer",
    "antibody titers after a third mRNA vaccine dose in kidney transplant recipients",
    "risk factors for surgical site infection after cesarean delivery in low-income countries",
    "how researchers quantified mitochondrial membrane potential with TMRM",
    "loss of smell duration after COVID-19 in adolescents",
    "microglial activation markers in postmortem ALS spinal cord",
    "cost-effectiveness of HPV self-sampling for cervical screening",
    "which antibiotic regimen had the lowest relapse rate for Clostridioides difficile",
    "heat map of cytokine levels across disease severity groups",
    "electron micrographs of autophagosomes accumulating in muscle fibers",
];

fn draw<'a>(key: &[&str], items: &[(&'a str, u32)]) -> &'a str {
    let h = hash(key);
    let x = u64::from_str_radix(&h[..12], 16).unwrap_or(0);
    let tot: u32 = items.iter().map(|(_, w)| *w).sum();
    let mut r = (x % tot as u64) as u32;
    for (v, w) in items {
        if r < *w {
            return v;
        }
        r -= w;
    }
    items[items.len() - 1].0
}

#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct Spec {
    pub slot: String,
    pub task: String,
    pub intent: String,
    pub format: String,
    pub length: String,
    pub difficulty: String,
    pub persona: String,
}

impl Spec {
    fn line(&self) -> String {
        let len = match self.length.as_str() {
            "short" => "short (5-9 words)", "medium" => "medium (10-18 words)", "long" => "long (19-40 words)",
            "keyword" => "2-6 words", x => x,
        };
        let mut s = format!("intent={}; format={}; length={}", self.intent, self.format, len);
        if !self.difficulty.is_empty() {
            s += &format!("; difficulty={}", self.difficulty);
        }
        if !self.persona.is_empty() {
            s += &format!("; persona={}", self.persona);
        }
        s
    }
}

fn shape(mut s: Spec) -> Spec {
    if s.format == "keyword" || s.length == "keyword" || s.intent == "keyword_find" {
        s.format = "keyword".into();
        s.length = "keyword".into();
    } else if s.format == "scenario" {
        s.length = "long".into();
    }
    if s.intent == "lay_question" {
        s.persona = "a patient or caregiver (plain words)".into();
        s.format = "question".into();
        if s.length == "keyword" {
            s.length = "medium".into();
        }
    }
    s
}

/// Spec for text slot `slot` of passage `unit`; intents are distinct within a passage, `contextual` only when the
/// passage does not already name the article's subject.
pub fn text_spec(unit: &str, slot: &str, contextual_ok: bool, taken: &[String]) -> Spec {
    let mut intent = "";
    for k in 0..16 {
        let ks = k.to_string();
        let c = draw(&[unit, slot, "intent", &ks], &T_INTENTS);
        if (c != "contextual" || contextual_ok) && !taken.iter().any(|t| t == c) {
            intent = c;
            break;
        }
    }
    if intent.is_empty() {
        intent = T_INTENTS.iter().map(|x| x.0).find(|c| (*c != "contextual" || contextual_ok) && !taken.iter().any(|t| t == c)).unwrap_or("fact_lookup");
    }
    shape(Spec {
        slot: slot.into(), task: "T1".into(), intent: intent.into(),
        format: draw(&[unit, slot, "format"], &FORMATS).into(), length: draw(&[unit, slot, "length"], &LENGTHS).into(),
        difficulty: draw(&[unit, slot, "difficulty"], &DIFFS).into(), persona: draw(&[unit, slot, "persona"], &PERSONAS).into(),
    })
}

fn fig_spec(f: &FigureSample, slot: &str, task: &str, intents: &[(&str, u32)], taken: &[String]) -> Spec {
    let unit = f.chunk_id.as_str();
    let mut intent = draw(&[unit, slot, "intent"], intents);
    for k in 0..16 {
        if !taken.iter().any(|t| t == intent) {
            break;
        }
        intent = draw(&[unit, slot, "intent", &k.to_string()], intents);
    }
    if taken.iter().any(|t| t == intent) {
        intent = intents.iter().map(|x| x.0).find(|c| !taken.iter().any(|t| t == c)).unwrap_or(intent);
    }
    let roles: &[(&str, u32)] = if f.section == "radiology_clinical" { &FIG_ROLES_CLINICAL } else { &FIG_ROLES };
    shape(Spec {
        slot: slot.into(), task: task.into(), intent: intent.into(),
        format: if intent == "visual_find" { draw(&[unit, slot, "format"], &[("instruction", 60), ("question", 25), ("statement", 15)]) }
                else { draw(&[unit, slot, "format"], &[("question", 70), ("keyword", 10), ("instruction", 20)]) }.into(),
        length: draw(&[unit, slot, "length"], &[("short", 40), ("medium", 45), ("long", 15)]).into(),
        difficulty: if task == "F2" { draw(&[unit, slot, "difficulty"], &[("direct", 25), ("paraphrase", 50), ("inferential", 25)]).into() } else { String::new() },
        persona: draw(&[unit, slot, "persona"], roles).into(),
    })
}

fn content_words(s: &str) -> Vec<String> {
    const STOP: [&str; 24] = ["about", "after", "among", "analysis", "based", "between", "during", "effect", "effects",
        "from", "into", "patients", "study", "their", "these", "through", "under", "using", "with", "within", "without",
        "review", "case", "report"];
    s.split(|c: char| !c.is_alphanumeric() && c != '-').map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 5 && !STOP.contains(&w.as_str())).collect()
}

/// A passage that names none of the title's content words can take a `contextual` slot.
pub fn contextual_ok(title: &str, passage: &str) -> bool {
    let p: HashSet<String> = content_words(passage).into_iter().collect();
    let t = content_words(title);
    !t.is_empty() && !t.iter().any(|w| p.contains(w))
}

fn exemplars(key: &str) -> String {
    let mut idx: Vec<usize> = vec![];
    for k in 0..12 {
        let i = (u64::from_str_radix(&hash(&[key, "ex", &k.to_string()])[..8], 16).unwrap_or(0) % EXEMPLARS.len() as u64) as usize;
        if !idx.contains(&i) {
            idx.push(i);
        }
        if idx.len() == 3 {
            break;
        }
    }
    idx.iter().map(|i| format!("- {}", EXEMPLARS[*i])).collect::<Vec<_>>().join("\n")
}

const INTENT_DEFS: &str = "INTENTS: fact_lookup = a specific fact; result_number = a measured outcome or value; \
mechanism_why = why or how something happens; method_protocol = how something was done or measured; comparison = \
between groups, treatments or conditions; clinical_decision = what to do for a patient; lay_question = plain words, no \
jargon; claim = a statement someone wants verified (may be true per the passage); contextual = names the article's \
main subject (from the title or the rest of the article) that the passage itself never names; open_ended = broad but \
still answered here.\nFORMATS: question; keyword (2-6 search terms, no stop words); statement (a claim); instruction \
(\"find studies that ...\", \"evidence on ...\"); scenario (1-2 sentences describing a situation, ending in the need).\n\
DIFFICULTY: direct = may use the passage's key terms; paraphrase = at most 2 content words shared with the passage; \
inferential = needs article context the passage lacks, or one step of reasoning.";

/// Text plan for one article: per-passage slot specs (+ an optional two-passage slot).
pub struct TextPlan {
    pub specs: Vec<(String, Spec)>, // (marker, spec); marker "P2+P4" for the T2 slot
    pub t2: Option<(String, String)>,
}

pub fn text_plan(art: &Article, chunks: &[&SampleChunk]) -> TextPlan {
    let mut specs = vec![];
    for c in chunks {
        let ok = contextual_ok(&art.title, &c.text);
        let mut taken: Vec<String> = vec![];
        for s in 1..=TEXT_SLOTS {
            let sp = text_spec(&c.chunk_id, &format!("s{s}"), ok, &taken);
            taken.push(sp.intent.clone());
            specs.push((c.marker.clone(), sp));
        }
    }
    let mut t2 = None;
    if chunks.len() >= 2 && u64::from_str_radix(&hash(&[&art.article_id, "t2"])[..8], 16).unwrap_or(1) % 10 == 0 {
        let mut ms: Vec<&SampleChunk> = chunks.to_vec();
        ms.sort_by_key(|c| hash(&[&art.article_id, "t2pick", &c.chunk_id]));
        let (a, b) = (ms[0].marker.clone(), ms[1].marker.clone());
        let mk = format!("{a}+{b}");
        let sp = shape(Spec { slot: "m1".into(), task: "T2".into(), intent: draw(&[&art.article_id, "t2i"], &T2_INTENTS).into(),
            format: "question".into(), length: "medium".into(), difficulty: "inferential".into(),
            persona: draw(&[&art.article_id, "t2p"], &PERSONAS).into() });
        specs.push((mk, sp));
        t2 = Some((a, b));
    }
    TextPlan { specs, t2 }
}

fn slot_id(marker: &str, sp: &Spec) -> String {
    format!("{marker}.{}", sp.slot)
}

pub fn text_prompt(art: &Article, plan: &TextPlan) -> String {
    let lines: Vec<String> = plan.specs.iter().map(|(m, sp)| {
        let extra = if sp.task == "T2" { "; must need BOTH passages" } else { "" };
        format!("[[{m}]] {}: {}{extra}", slot_id(m, sp), sp.line())
    }).collect();
    format!("ARTICLE. Passages that need queries are marked [[P1]] ... [[/P1]] and so on.\n<<<\n{}\n>>>\n\n\
STEP 1. In \"personas\", list 5 kinds of people who would plausibly search for something this article answers \
(role + what they want), most likely first.\n\n\
STEP 2. For each slot below, write one query that the marked passage answers. The spec gives intent, format, \
length, difficulty and persona (1-5 from your list).\n{}\n\n{INTENT_DEFS}\n\
Answer each query in at most 15 words in \"answer\".\n\n\
Style examples (other topics; show register only, do not reuse their topics or wording):\n{}",
        art.text, lines.join("\n"), exemplars(&art.article_id))
}

fn slots_schema(ids: &[String], item: Value, personas: bool) -> Value {
    let mut item = item;
    item["properties"]["slot"] = json!({"enum": ids});
    let slots = json!({"type": "array", "minItems": ids.len(), "maxItems": ids.len(), "items": item});
    if personas {
        json!({"type": "object", "required": ["personas", "slots"], "additionalProperties": false, "properties": {
            "personas": {"type": "array", "minItems": 5, "maxItems": 5, "items": {"type": "string"}}, "slots": slots}})
    } else {
        json!({"type": "object", "required": ["slots"], "additionalProperties": false, "properties": {"slots": slots}})
    }
}

pub fn text_schema(plan: &TextPlan) -> Value {
    let ids: Vec<String> = plan.specs.iter().map(|(m, sp)| slot_id(m, sp)).collect();
    slots_schema(&ids, json!({"type": "object", "additionalProperties": false, "required": ["slot", "query", "answer", "skip"],
        "properties": {"slot": {}, "query": {"type": "string"}, "answer": {"type": "string"}, "skip": {"type": "boolean"}}}), true)
}

pub(crate) fn json_body(content: &str) -> Option<Value> {
    let c = content.rsplit("</think>").next().unwrap_or(content);
    let (s, e) = (c.find('{')?, c.rfind('}')?);
    serde_json::from_str(&c[s..=e]).ok()
}

fn banned(q: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)according to|in this (study|article|paper)|the authors|\bfig(ure|\.)?\s*\d|\bpanel [a-h]\b|the passage|the figure above").unwrap())
        .is_match(q)
}

/// Accepted slot entries by slot id. A query is rejected when skipped, empty, equal to its intent name, or a
/// duplicate of another slot's query.
fn accept(v: &Value, ids: &[String], specs: &HashMap<String, Spec>) -> (BTreeMap<String, Value>, BTreeMap<String, Value>) {
    let mut ok = BTreeMap::new();
    let mut meta = BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    for e in v["slots"].as_array().into_iter().flatten() {
        let Some(id) = e["slot"].as_str() else { continue };
        if !ids.iter().any(|x| x == id) || meta.contains_key(id) {
            continue;
        }
        let q = e["query"].as_str().unwrap_or("").trim().to_string();
        let sp = specs.get(id).cloned().unwrap_or_default();
        let mut m = serde_json::to_value(&sp).unwrap();
        for (k, x) in e.as_object().into_iter().flatten() {
            if k != "slot" && k != "query" {
                m[k] = x.clone();
            }
        }
        let skip = e["skip"].as_bool().unwrap_or(false);
        let low = e["confidence"].as_str() == Some("low");
        let ql = q.to_lowercase();
        let degenerate = ql.is_empty() || ql == sp.intent.replace('_', " ") || ql == sp.intent || !seen.insert(ql.clone());
        m["banned"] = json!(banned(&q));
        m["low_confidence"] = json!(low);
        m["rejected"] = json!(skip || degenerate || low);
        m["query"] = json!(q);
        if !(skip || degenerate || low) {
            ok.insert(id.to_string(), json!(q));
        }
        meta.insert(id.to_string(), m);
    }
    (ok, meta)
}

fn usage(g: &mut Generation, r: &Reply, k: u64) {
    g.prompt_tokens = r.prompt_tokens / k;
    g.completion_tokens = r.completion_tokens / k;
    g.reasoning_tokens = r.reasoning_tokens / k;
    g.reasoning_chars = r.reasoning_chars / k;
}

fn text_article(agent: &ureq::Agent, a: &GenArgs, art: &Article, chunks: &[&SampleChunk]) -> Vec<Generation> {
    let plan = text_plan(art, chunks);
    let ids: Vec<String> = plan.specs.iter().map(|(m, sp)| slot_id(m, sp)).collect();
    let specs: HashMap<String, Spec> = plan.specs.iter().map(|(m, sp)| (slot_id(m, sp), sp.clone())).collect();
    let (res, lat) = post(agent, a, SYSTEM2, json!(text_prompt(art, &plan)), text_schema(&plan));
    let k = chunks.len().max(1) as u64;
    let mut out: Vec<Generation> = chunks.iter().map(|c| Generation { chunk_id: c.chunk_id.clone(), article_id: c.article_id.clone(),
        arm: a.arm.clone(), latency_s: lat, ..Default::default() }).collect();
    match res {
        Ok(r) => {
            let v = json_body(&r.content);
            let (ok, meta) = v.as_ref().map(|v| accept(v, &ids, &specs)).unwrap_or_default();
            let personas = v.as_ref().map(|v| v["personas"].clone()).unwrap_or(Value::Null);
            for (g, c) in out.iter_mut().zip(chunks) {
                usage(g, &r, k);
                let pre = format!("{}.", c.marker);
                let mut q = Map::new();
                let mut m = Map::new();
                for (id, x) in &meta {
                    let mine = id.starts_with(&pre) || plan.t2.as_ref().is_some_and(|(p, _)| *p == c.marker && id.starts_with(&format!("{p}+")));
                    if !mine {
                        continue;
                    }
                    let key = id.rsplit('.').next().unwrap_or(id).to_string();
                    let mut x = x.clone();
                    if let Some((p1, p2)) = &plan.t2 {
                        if id.starts_with(&format!("{p1}+")) {
                            let other = chunks.iter().find(|y| y.marker == *p2).map(|y| y.chunk_id.clone()).unwrap_or_default();
                            x["positives"] = json!([c.chunk_id, other]);
                        }
                    }
                    if let Some(s) = ok.get(id) {
                        q.insert(key.clone(), s.clone());
                    }
                    m.insert(key, x);
                }
                g.ok = !q.is_empty();
                if !g.ok {
                    g.error = Some(format!("no accepted slots: {}", r.content.chars().take(200).collect::<String>()));
                }
                g.queries = q;
                g.meta = json!({"slots": m, "personas": personas});
            }
        }
        Err(e) => out.iter_mut().for_each(|g| g.error = Some(e.clone())),
    }
    out
}

// ---------- figures ----------

pub fn blind_specs(f: &FigureSample) -> Vec<Spec> {
    let s1 = fig_spec(f, "f1", "F1", &F1_INTENTS, &[]);
    let s2 = fig_spec(f, "f2", "F1", &F1_INTENTS, &[s1.intent.clone()]);
    vec![s1, s2]
}

pub fn grounded_specs(f: &FigureSample) -> Vec<Spec> {
    let g1 = fig_spec(f, "g1", "F2", &F2_INTENTS, &[]);
    let g2 = fig_spec(f, "g2", "F2", &F2_INTENTS, &[g1.intent.clone()]);
    let c1 = Spec { slot: "c1".into(), task: "I1".into(), intent: "composed".into(), format: "question".into(),
        length: "medium".into(), difficulty: String::new(), persona: draw(&[&f.chunk_id, "c1", "persona"], &FIG_ROLES).into() };
    vec![g1, g2, c1]
}

pub fn blind_prompt(f: &FigureSample, specs: &[Spec]) -> String {
    let lines: Vec<String> = specs.iter().map(|s| format!("{}: {}", s.slot, s.line())).collect();
    format!("Article title: {}\nAbstract (for context only): {}\n\n\
The image is one figure from this article. You are NOT given its caption on purpose: write queries from what is \
visible, using the title and abstract only to name things correctly (organ, assay, organism, condition). Describe only \
what you can see. If you cannot tell what a structure or curve is, describe its appearance (colour, shape, layout, \
axis labels you can read) instead of guessing a name. Do not mention panel letters.\n\nSlots (persona = who is searching):\n{}\n\n\
visual_find = a request to find a figure that shows something specific, e.g. a kind of plot of a named outcome, an \
imaging finding, a stained tissue feature, a pathway diagram of named components.\n\
appearance_detail = a query that only someone looking at the image could write: a visible trend, crossover, location, \
morphology, value read from an axis, or the number or arrangement of groups.\n\
keyword_find = 2-6 search terms naming what the figure shows.\n\
For each query also give \"visible_basis\": the concrete visual detail it relies on (at most 15 words), and \
\"confidence\" (high, medium or low) that the query is correct for this image.\n\n\
Style examples (other topics; register only):\n{}",
        f.title, f.abstract_text, lines.join("\n"), exemplars(&f.chunk_id))
}

pub fn grounded_prompt(f: &FigureSample, specs: &[Spec]) -> String {
    let lines: Vec<String> = specs.iter().map(|s| format!("{}: {}", s.slot, s.line())).collect();
    let citing: Vec<String> = f.citing.iter().take(2).enumerate().map(|(i, t)| format!("[{}] {}", i + 1, t)).collect();
    format!("Article title: {}\nAbstract (context only): {}\nFigure caption: {}\nText that cites the figure:\n{}\n\n\
Write queries for which THIS figure (image and caption together) is the best answer in a library of millions of \
figures. Use the caption and citing text to understand the figure, but do not restate the caption: ask for the result, \
not the description.\n\nSlots (persona = who is searching):\n{}\n\n\
finding_question = asks for the result the figure shows (direction, size, difference, time course, localisation).\n\
clinician_use = how a clinician or researcher would use what the figure shows.\n\
contextual = names the article's main subject (drug, disease, population) even if the caption does not.\n\
comparison_in_figure = asks which group, condition or time point differs.\n\
composed (c1) = a question to send TOGETHER WITH THIS IMAGE to the search engine. Point at the image (\"this curve\", \
\"the lesion shown\", \"these stained cells\") instead of describing it, and ask something that the citing text answers \
(an explanation, implication or interpretation). The question text alone, without the image, must NOT be enough to find \
the answer. Give \"target\": which citing paragraph (1 or 2) answers it; 0 for other slots.\n\
For every query give \"answer\" (at most 15 words) and \"evidence\": image, caption, citing_text or several.\n\n\
Style examples (other topics; register only):\n{}",
        f.title, f.abstract_text, f.text, if citing.is_empty() { "(none)".to_string() } else { citing.join("\n") },
        lines.join("\n"), exemplars(&f.chunk_id))
}

fn fig_call(agent: &ureq::Agent, a: &GenArgs, f: &FigureSample, blind: bool) -> Generation {
    let mut g = Generation { chunk_id: f.chunk_id.clone(), article_id: f.article_id.clone(), arm: a.arm.clone(), ..Default::default() };
    let url = match image_data_url(&f.image) { Ok(u) => u, Err(e) => { g.error = Some(format!("image: {e}")); return g; } };
    let specs = if blind { blind_specs(f) } else { grounded_specs(f) };
    let ids: Vec<String> = specs.iter().map(|s| s.slot.clone()).collect();
    let by: HashMap<String, Spec> = specs.iter().map(|s| (s.slot.clone(), s.clone())).collect();
    let (text, schema) = if blind {
        (blind_prompt(f, &specs), slots_schema(&ids, json!({"type": "object", "additionalProperties": false,
            "required": ["slot", "query", "visible_basis", "confidence", "skip"], "properties": {"slot": {}, "query": {"type": "string"},
            "visible_basis": {"type": "string"}, "confidence": {"enum": ["high", "medium", "low"]}, "skip": {"type": "boolean"}}}), false))
    } else {
        (grounded_prompt(f, &specs), slots_schema(&ids, json!({"type": "object", "additionalProperties": false,
            "required": ["slot", "query", "answer", "evidence", "target", "skip"], "properties": {"slot": {}, "query": {"type": "string"},
            "answer": {"type": "string"}, "evidence": {"enum": ["image", "caption", "citing_text", "several"]},
            "target": {"type": "integer", "minimum": 0, "maximum": 2}, "skip": {"type": "boolean"}}}), false))
    };
    let user = json!([{"type": "image_url", "image_url": {"url": url}}, {"type": "text", "text": text}]);
    let (res, lat) = post(agent, a, SYSTEM2, user, schema);
    g.latency_s = lat;
    match res {
        Ok(r) => {
            usage(&mut g, &r, 1);
            let (ok, meta) = json_body(&r.content).map(|v| accept(&v, &ids, &by)).unwrap_or_default();
            g.ok = !ok.is_empty();
            if !g.ok {
                g.error = Some(format!("no accepted slots: {}", r.content.chars().take(200).collect::<String>()));
            }
            g.queries = ok.into_iter().collect();
            g.meta = json!({"slots": meta.into_iter().collect::<Map<String, Value>>()});
        }
        Err(e) => g.error = Some(e),
    }
    g
}

// ---------- runners ----------

/// Run `f` over `n` work items with `conc` threads; results in completion order.
pub fn par<R: Send>(n: usize, conc: usize, f: impl Fn(usize) -> Vec<R> + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<R>> = Mutex::new(vec![]);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..conc.max(1) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let r = f(i);
                let mut o = out.lock().unwrap();
                o.extend(r);
                if (i + 1) % 100 == 0 {
                    eprintln!("{}/{n} {:.0}s", i + 1, t0.elapsed().as_secs_f64());
                }
            });
        }
    });
    out.into_inner().unwrap()
}

pub(crate) fn agent(conc: usize) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(3600)).max_idle_connections(conc * 2).build()
}

pub(crate) fn summary(a: &GenArgs, gens: &[Generation], calls: usize, wall: f64, server: Map<String, Value>) -> Value {
    let ok = gens.iter().filter(|g| g.ok).count();
    let nq: usize = gens.iter().map(|g| g.queries.len()).sum();
    let slots: usize = gens.iter().map(|g| g.meta["slots"].as_object().map(|m| m.len()).unwrap_or(0)).sum();
    let banned = gens.iter().flat_map(|g| g.meta["slots"].as_object().into_iter().flatten()).filter(|(_, m)| m["banned"] == json!(true)).count();
    let (pt, ct): (u64, u64) = gens.iter().fold((0, 0), |(p, c), g| (p + g.prompt_tokens, c + g.completion_tokens));
    let n = gens.len().max(1) as f64;
    let qph = nq as f64 / wall * 3600.0;
    json!({
        "arm": a.arm, "model": a.model, "layout": format!("{:?}", a.layout), "gpus": a.gpus, "chunks": gens.len(), "calls": calls,
        "ok_chunks": ok, "json_valid_rate": ok as f64 / n, "slots": slots, "queries": nq,
        "accepted_rate": nq as f64 / slots.max(1) as f64, "banned_rate": banned as f64 / slots.max(1) as f64,
        "wall_s": wall, "concurrency": a.concurrency, "prompt_tokens": pt, "completion_tokens": ct,
        "input_tokens_per_s": pt as f64 / wall, "output_tokens_per_s": ct as f64 / wall,
        "queries_per_hour": qph, "queries_per_gpu_hour": qph / a.gpus.max(1) as f64,
        "prompt_tokens_per_chunk": pt as f64 / n, "completion_tokens_per_chunk": ct as f64 / n,
        "reasoning_tokens_per_chunk": gens.iter().map(|g| g.reasoning_tokens).sum::<u64>() as f64 / n,
        "reasoning_chars_per_chunk": gens.iter().map(|g| g.reasoning_chars).sum::<u64>() as f64 / n,
        "server_metrics_delta": server,
    })
}

pub(crate) fn delta(before: &HashMap<String, f64>, after: &HashMap<String, f64>) -> Map<String, Value> {
    after.iter().map(|(k, v)| (k.clone(), json!(v - before.get(k).copied().unwrap_or(0.0)))).collect()
}

pub fn run_text(a: &GenArgs, chunks: &[SampleChunk], articles: &[Article]) -> Result<(Vec<Generation>, Value)> {
    let ag = agent(a.concurrency);
    let by: HashMap<&str, &Article> = articles.iter().map(|x| (x.article_id.as_str(), x)).collect();
    let mut groups: Vec<Vec<&SampleChunk>> = vec![];
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for c in chunks {
        let i = *idx.entry(c.article_id.as_str()).or_insert_with(|| { groups.push(vec![]); groups.len() - 1 });
        groups[i].push(c);
    }
    groups.retain(|g| by.contains_key(g[0].article_id.as_str()));
    let before = scrape(&ag, &a.endpoint);
    let t0 = Instant::now();
    let gens = par(groups.len(), a.concurrency, |i| text_article(&ag, a, by[groups[i][0].article_id.as_str()], &groups[i]));
    let wall = t0.elapsed().as_secs_f64();
    let s = summary(a, &gens, groups.len(), wall, delta(&before, &scrape(&ag, &a.endpoint)));
    Ok((gens, s))
}

pub fn run_figs(a: &GenArgs, figs: &[FigureSample]) -> Result<(Vec<Generation>, Value)> {
    let ag = agent(a.concurrency);
    let blind = a.layout == Layout::FigBlind;
    let before = scrape(&ag, &a.endpoint);
    let t0 = Instant::now();
    let gens = par(figs.len(), a.concurrency, |i| vec![fig_call(&ag, a, &figs[i], blind)]);
    let wall = t0.elapsed().as_secs_f64();
    let s = summary(a, &gens, figs.len(), wall, delta(&before, &scrape(&ag, &a.endpoint)));
    Ok((gens, s))
}

// ---------- judge ----------

pub const JUDGE_SYSTEM: &str = "You are a careful biomedical search-quality judge. Judge each query against the given \
material only. Output JSON only.";

const JUDGE_RULES: &str = "For each query, judge whether the given figure or passage is a correct, specific answer.\n\
relevance: 3 = fully answers; 2 = mostly; 1 = related but does not answer; 0 = wrong.\n\
supported: are all factual claims in the query consistent with the material? (true/false)\n\
image_needed: for figures, could the query be answered from the caption alone without seeing the image? \
(caption_enough / image_needed); for passages use not_applicable.\n\
specific: would this query match only a handful of passages or figures in a library of millions? (true/false)\n\
natural: would a real person type this? (1 = no, 2 = maybe, 3 = yes)\n\
Queries marked [sent with the image] are sent together with the figure image: judge them against the citing text.";

#[derive(Serialize, Clone, Debug, Default)]
pub struct Judgment {
    pub judge: String,
    pub arm: String,
    pub chunk_id: String,
    pub key: String,
    pub task: String,
    pub intent: String,
    pub relevance: i64,
    pub supported: bool,
    pub image_needed: String,
    pub specific: bool,
    pub natural: i64,
}

fn qinfo(g: &Generation, key: &str) -> (String, String) {
    let m = &g.meta["slots"][key];
    (m["task"].as_str().unwrap_or("").to_string(), m["intent"].as_str().unwrap_or(key).to_string())
}

fn judge_one(ag: &ureq::Agent, a: &GenArgs, judge: &str, g: &Generation, chunk: Option<&SampleChunk>, fig: Option<&FigureSample>) -> Vec<Judgment> {
    let keys: Vec<String> = g.queries.keys().take(8).cloned().collect();
    if keys.is_empty() {
        return vec![];
    }
    let qs: Vec<String> = keys.iter().map(|k| {
        let (task, _) = qinfo(g, k);
        let tag = if task == "I1" { " [sent with the image]" } else { "" };
        format!("{k}: {}{tag}", g.queries[k].as_str().unwrap_or(""))
    }).collect();
    let (user, figure) = match (fig, chunk) {
        (Some(f), _) => {
            let Ok(url) = image_data_url(&f.image) else { return vec![] };
            let citing: Vec<String> = f.citing.iter().take(2).enumerate().map(|(i, t)| format!("[{}] {}", i + 1, t)).collect();
            (json!([{"type": "image_url", "image_url": {"url": url}}, {"type": "text", "text": format!(
                "FIGURE (image above)\nArticle title: {}\nCaption: {}\nCiting text:\n{}\n\nQUERIES\n{}\n\n{JUDGE_RULES}",
                f.title, f.text, citing.join("\n"), qs.join("\n"))}]), true)
        }
        (None, Some(c)) => (json!(format!("PASSAGE\nArticle title: {}\nSection: {}\n<<<\n{}\n>>>\n\nQUERIES\n{}\n\n{JUDGE_RULES}",
            c.title, c.section, c.text, qs.join("\n"))), false),
        _ => return vec![],
    };
    let item = json!({"type": "object", "additionalProperties": false,
        "required": ["id", "relevance", "supported", "image_needed", "specific", "natural"],
        "properties": {"id": {"enum": keys}, "relevance": {"type": "integer", "minimum": 0, "maximum": 3},
            "supported": {"type": "boolean"}, "image_needed": {"enum": ["caption_enough", "image_needed", "not_applicable"]},
            "specific": {"type": "boolean"}, "natural": {"type": "integer", "minimum": 1, "maximum": 3}}});
    let schema = json!({"type": "object", "required": ["judgments"], "additionalProperties": false,
        "properties": {"judgments": {"type": "array", "minItems": keys.len(), "maxItems": keys.len(), "items": item}}});
    let (res, _) = post(ag, a, JUDGE_SYSTEM, user, schema);
    let Ok(r) = res else { return vec![] };
    let Some(v) = json_body(&r.content) else { return vec![] };
    let mut seen = HashSet::new();
    v["judgments"].as_array().into_iter().flatten().filter_map(|j| {
        let key = j["id"].as_str()?.to_string();
        if !keys.contains(&key) || !seen.insert(key.clone()) {
            return None;
        }
        let (task, intent) = qinfo(g, &key);
        Some(Judgment { judge: judge.into(), arm: g.arm.clone(), chunk_id: g.chunk_id.clone(), key, task, intent,
            relevance: j["relevance"].as_i64().unwrap_or(-1), supported: j["supported"].as_bool().unwrap_or(false),
            image_needed: if figure { j["image_needed"].as_str().unwrap_or("").into() } else { "not_applicable".into() },
            specific: j["specific"].as_bool().unwrap_or(false), natural: j["natural"].as_i64().unwrap_or(0) })
    }).collect()
}

pub fn judge(a: &GenArgs, judge_name: &str, gens: &[Generation], chunks: &HashMap<String, SampleChunk>,
             figs: &HashMap<String, FigureSample>) -> (Vec<Judgment>, Value) {
    let ag = agent(a.concurrency);
    let t0 = Instant::now();
    let rows = par(gens.len(), a.concurrency, |i| {
        let g = &gens[i];
        judge_one(&ag, a, judge_name, g, chunks.get(&g.chunk_id), figs.get(&g.chunk_id))
    });
    let mut by: BTreeMap<String, Vec<&Judgment>> = BTreeMap::new();
    for j in &rows {
        by.entry(j.task.clone()).or_default().push(j);
        by.entry("all".into()).or_default().push(j);
    }
    let asked: usize = gens.iter().map(|g| g.queries.len().min(8)).sum();
    let stats: Map<String, Value> = by.iter().map(|(t, js)| {
        let n = js.len().max(1) as f64;
        let fig: Vec<&&Judgment> = js.iter().filter(|j| j.image_needed != "not_applicable").collect();
        (t.clone(), json!({"n": js.len(),
            "relevance_mean": js.iter().map(|j| j.relevance as f64).sum::<f64>() / n,
            "relevance_ge2": js.iter().filter(|j| j.relevance >= 2).count() as f64 / n,
            "supported": js.iter().filter(|j| j.supported).count() as f64 / n,
            "specific": js.iter().filter(|j| j.specific).count() as f64 / n,
            "natural_mean": js.iter().map(|j| j.natural as f64).sum::<f64>() / n,
            "image_needed": if fig.is_empty() { Value::Null } else { json!(fig.iter().filter(|j| j.image_needed == "image_needed").count() as f64 / fig.len() as f64) },
            "pass": js.iter().filter(|j| j.relevance >= 2 && j.supported).count() as f64 / n}))
    }).collect();
    let s = json!({"judge": judge_name, "arm": gens.first().map(|g| g.arm.clone()), "asked": asked, "judged": rows.len(),
        "wall_s": t0.elapsed().as_secs_f64(), "by_task": stats});
    (rows, s)
}

// ---------- intrinsic metrics ----------

pub(crate) fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

/// Longest run of consecutive tokens shared by `q` and `src`.
pub fn longest_common_span(q: &[String], src: &[String]) -> usize {
    let mut best = 0;
    let mut prev = vec![0usize; src.len() + 1];
    for x in q {
        let mut cur = vec![0usize; src.len() + 1];
        for (j, y) in src.iter().enumerate() {
            if x == y {
                cur[j + 1] = prev[j] + 1;
                best = best.max(cur[j + 1]);
            }
        }
        prev = cur;
    }
    best
}

/// ROUGE-L recall of the query against `src` (LCS / query length).
pub fn rouge_l(q: &[String], src: &[String]) -> f64 {
    if q.is_empty() {
        return 0.0;
    }
    let mut prev = vec![0usize; src.len() + 1];
    for x in q {
        let mut cur = vec![0usize; src.len() + 1];
        for (j, y) in src.iter().enumerate() {
            cur[j + 1] = if x == y { prev[j] + 1 } else { cur[j].max(prev[j + 1]) };
        }
        prev = cur;
    }
    prev[src.len()] as f64 / q.len() as f64
}

fn distinct_n(qs: &[Vec<String>], n: usize) -> f64 {
    let (mut all, mut set) = (0usize, HashSet::new());
    for q in qs {
        for w in q.windows(n) {
            all += 1;
            set.insert(w.join(" "));
        }
    }
    if all == 0 { 0.0 } else { set.len() as f64 / all as f64 }
}

/// Per arm x task: diversity (distinct-2/3, top first-3-word opener share), copying (caption span, ROUGE-L for
/// figures; content-word overlap with the passage for text), banned-phrase and rejection rates, mean length.
pub fn metrics(gens: &[Generation], chunks: &HashMap<String, SampleChunk>, figs: &HashMap<String, FigureSample>) -> Value {
    let mut groups: BTreeMap<(String, String), Vec<(Vec<String>, Option<&str>)>> = BTreeMap::new();
    let mut slots: BTreeMap<(String, String), (usize, usize, usize)> = BTreeMap::new(); // (slots, rejected, banned)
    for g in gens {
        let src: Option<&str> = figs.get(&g.chunk_id).map(|f| f.text.as_str()).or(chunks.get(&g.chunk_id).map(|c| c.text.as_str()));
        if let Some(m) = g.meta["slots"].as_object() {
            for (_, x) in m {
                let e = slots.entry((g.arm.clone(), x["task"].as_str().unwrap_or("?").to_string())).or_default();
                e.0 += 1;
                e.1 += (x["rejected"] == json!(true)) as usize;
                e.2 += (x["banned"] == json!(true)) as usize;
            }
        }
        for (k, q) in &g.queries {
            let task = g.meta["slots"][k]["task"].as_str().unwrap_or(k).to_string();
            groups.entry((g.arm.clone(), task)).or_default().push((words(q.as_str().unwrap_or("")), src));
        }
    }
    let mut out = Map::new();
    for ((arm, task), qs) in &groups {
        let mut openers: HashMap<String, usize> = HashMap::new();
        for (w, _) in qs {
            *openers.entry(w.iter().take(3).cloned().collect::<Vec<_>>().join(" ")).or_default() += 1;
        }
        let top = openers.iter().max_by_key(|x| *x.1).map(|(k, v)| (k.clone(), *v)).unwrap_or_default();
        let n = qs.len().max(1) as f64;
        let ws: Vec<Vec<String>> = qs.iter().map(|x| x.0.clone()).collect();
        let mut spans: Vec<usize> = qs.iter().map(|(w, s)| s.map(|s| longest_common_span(w, &words(s))).unwrap_or(0)).collect();
        spans.sort();
        let rl = qs.iter().map(|(w, s)| s.map(|s| rouge_l(w, &words(s))).unwrap_or(0.0)).sum::<f64>() / n;
        let (ns, rej, ban) = slots.get(&(arm.clone(), task.clone())).copied().unwrap_or_default();
        out.entry(arm.clone()).or_insert_with(|| json!({}))[task.as_str()] = json!({
            "queries": qs.len(), "slots": ns, "rejected_rate": rej as f64 / ns.max(1) as f64, "banned_rate": ban as f64 / ns.max(1) as f64,
            "mean_words": ws.iter().map(|w| w.len()).sum::<usize>() as f64 / n,
            "distinct2": distinct_n(&ws, 2), "distinct3": distinct_n(&ws, 3),
            "top_opener": top.0, "top_opener_share": top.1 as f64 / n,
            "source_span_median": spans.get(spans.len() / 2).copied().unwrap_or(0), "source_span_ge4": spans.iter().filter(|s| **s >= 4).count() as f64 / n,
            "source_rouge_l": rl,
        });
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fig() -> FigureSample {
        FigureSample { chunk_id: "u1".into(), article_id: "PMC1".into(), title: "Tirzepatide in obesity".into(),
            text: "Kaplan-Meier curves of survival".into(), section: "chart".into(), citing: vec!["As shown in Fig. 2".into()],
            ..Default::default() }
    }

    #[test]
    fn specs_are_deterministic_distinct_and_shaped() {
        let a = text_spec("c1", "s1", true, &[]);
        assert_eq!(a, text_spec("c1", "s1", true, &[]));
        let mut taken = vec![];
        for s in 1..=4 {
            let sp = text_spec("chunkX", &format!("s{s}"), false, &taken);
            assert_ne!(sp.intent, "contextual");
            assert!(!taken.contains(&sp.intent));
            if sp.format == "keyword" { assert_eq!(sp.length, "keyword"); }
            if sp.format == "scenario" { assert_eq!(sp.length, "long"); }
            taken.push(sp.intent);
        }
        let b = blind_specs(&fig());
        assert_ne!(b[0].intent, b[1].intent);
        let g = grounded_specs(&fig());
        assert_eq!(g[2].task, "I1");
        // shares roughly follow the targets over many units
        let n = 4000;
        let ctx = (0..n).filter(|i| text_spec(&format!("u{i}"), "s1", true, &[]).intent == "contextual").count() as f64 / n as f64;
        assert!((ctx - 0.14).abs() < 0.03, "{ctx}");
    }

    #[test]
    fn contextual_check_and_accept() {
        assert!(contextual_ok("Tirzepatide in obesity", "the drug reduced body weight by 20%"));
        assert!(!contextual_ok("Tirzepatide in obesity", "tirzepatide reduced body weight"));
        let specs: HashMap<String, Spec> = [("P1.s1".to_string(), Spec { intent: "claim".into(), task: "T1".into(), ..Default::default() }),
            ("P1.s2".to_string(), Spec { intent: "fact_lookup".into(), task: "T1".into(), ..Default::default() })].into();
        let ids: Vec<String> = specs.keys().cloned().collect();
        let v = json!({"slots": [{"slot": "P1.s1", "query": "claim", "answer": "", "skip": false},
                                 {"slot": "P1.s2", "query": "In this study, what dose of tirzepatide?", "answer": "15 mg", "skip": false}]});
        let (ok, meta) = accept(&v, &ids, &specs);
        assert!(!ok.contains_key("P1.s1"), "degenerate query must be rejected");
        assert!(ok.contains_key("P1.s2"));
        assert_eq!(meta["P1.s2"]["banned"], json!(true));
        assert_eq!(meta["P1.s2"]["answer"], json!("15 mg"));
    }

    #[test]
    fn copy_metrics() {
        let w = |s: &str| words(s);
        assert_eq!(longest_common_span(&w("kaplan meier curves of overall survival"), &w("Kaplan-Meier curves of survival")), 4);
        assert!((rouge_l(&w("a b c d"), &w("a x b c")) - 0.75).abs() < 1e-9);
        assert!(distinct_n(&[w("a b c"), w("a b d")], 2) > 0.7);
    }

    #[test]
    fn text_plan_and_schema() {
        let art = Article { article_id: "PMC9".into(), title: "T".into(), text: "[[P1]] x [[/P1]] [[P2]] y [[/P2]]".into(), words: 4,
            passages: vec!["P1".into(), "P2".into()], ..Default::default() };
        let c = |id: &str, m: &str| SampleChunk { chunk_id: id.into(), article_id: "PMC9".into(), marker: m.into(), text: "x".into(), ..Default::default() };
        let (c1, c2) = (c("a", "P1"), c("b", "P2"));
        let plan = text_plan(&art, &[&c1, &c2]);
        assert!(plan.specs.len() >= 8);
        let s = text_schema(&plan);
        assert_eq!(s["properties"]["slots"]["minItems"], json!(plan.specs.len()));
        assert!(text_prompt(&art, &plan).contains("P1.s1"));
    }
}
