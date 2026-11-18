//! Query recipe v3 (docs/RETRIEVER_QUERY_RECIPE_V3.md): separate *what is asked* from *how it is typed*.
//!
//!   stage A  one call per article: standalone intent cards per marked passage (need, answer, specificity,
//!            audience, entities). Persona-first arm: each card is written for a given persona.
//!   filter   cards that refer to the document, are skipped, or are generic (specificity = many) are dropped
//!            from single-positive output and recorded.
//!   match    intent-first: each kept card gets a persona from the library whose group fits its audience
//!            (10% "stretch": a lay persona with an expert need); no-persona arm: none.
//!   stage B  one call per article that never sees the passage: need + entities + persona card -> 2 surface
//!            forms per (card, persona), styles drawn from a schedule constrained by the persona.
//!   stage C  typing noise from the persona's habits (noise.rs). The noisy form is the query; the clean one is kept.
//!
//! Generations use the same format as recipe v2 (queries + meta.slots), so `queries`, `filter`, `report` and
//! `metrics` run unchanged. `judge3` rates any arm's queries with no source (standalone, specificity, exam
//! framing, naturalness), so v2 and v3 are judged the same way.

use crate::generate::{post, scrape, GenArgs, Generation, Layout};
use crate::persona::{library, pick, Persona};
use crate::recipe::{agent, contextual_ok, delta, json_body, par, summary, text_spec, TEXT_SLOTS};
use crate::sample::{hash, Article, SampleChunk};
use anyhow::Result;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Instant;

/// Recipe version written into every row (release provenance).
pub const RECIPE: &str = "v3.1d";
pub const LIBRARY_SEED: &str = "v3";
/// Persona-first: candidate readers offered per card; stage A picks the plausible one or none (v3.1 fix 7).
const CANDS: usize = 3;
pub const LIBRARY_SIZE: usize = 20_000;
const STYLES_PER_PAIR: usize = 2;
const STRETCH: f64 = 0.10;

pub fn lib() -> &'static [Persona] {
    static L: OnceLock<Vec<Persona>> = OnceLock::new();
    L.get_or_init(|| library(LIBRARY_SEED, LIBRARY_SIZE))
}

const TYPES: [&str; 10] = ["fact_lookup", "result_number", "mechanism_why", "method_protocol", "comparison",
    "clinical_decision", "contextual", "lay_question", "claim", "open_ended"];
const AUDIENCE: [&str; 5] = ["clinician", "researcher", "student", "lay", "other"];

pub const SYSTEM_A: &str = "You read biomedical articles and write down the information needs that a passage answers, \
as standalone statements of need. A need must make sense to someone who has never seen the article: name the \
specific drug, gene, organism, population, intervention, assay or condition. Never refer to the article or its \
local labels: no \"this study\", \"the authors\", \"the cohort\", \"the treatment group\", \"group A\", arm or \
group names that only this paper uses, \"Figure 2\" or \"the passage\". Use a public trial or dataset name only if \
searchers would know it. Needs must be answered by the marked passage. Output JSON only.";

pub const SYSTEM_B: &str = "You write what real people type into a biomedical literature search engine. You are \
given a person and an information need; write the search the person would type for that need, in the style \
asked. Sound like that person: their vocabulary, knowledge and English. Do not write exam questions or \
instructions to an assistant (never start with \"You\", \"Imagine\" or \"Suppose\"). Do not refer to any article \
(\"this study\", \"the paper\"). Each style must read differently. Do not add facts beyond the need. Output JSON only.";

const NEED_RULES: &str = "For each card:\n\
- need: one sentence stating what someone wants to know, standalone (see the rules above); 8-30 words.\n\
- type: the requested type when the passage allows it, else the closest type.\n\
- narrative: 1-2 sentences, like a TREC topic narrative: what a relevant document must contain to satisfy the \
need, and what would NOT count (near misses).\n\
- pico: for clinical or intervention questions fill population, intervention, comparison and outcome (short \
phrases); otherwise leave all four empty.\n\
- answer: the passage's answer in at most 15 words.\n\
- specificity: how many published papers would answer this need. unique = essentially this paper; few = a handful; \
many = textbook knowledge or a very common finding.\n\
- audience: which kinds of people would plausibly have this need (clinician, researcher, student, lay, other).\n\
- min_expertise: 0 anyone, 1 some background, 2 professional, 3 specialist in this area.\n\
- entities: 1-5 key named things (drug, gene, disease, organism, measure).\n\
- skip: true only if the passage cannot honestly answer a need of this kind (leave the other fields short).\n\
Prefer needs that a real searcher would have (a result, a comparison, a method detail, a decision) over generic \
definitions.";

const TYPE_DEFS: &str = "TYPES: fact_lookup = a specific fact; result_number = a measured outcome or value; \
mechanism_why = why or how something happens; method_protocol = how something was done or measured; comparison = \
between groups, treatments or conditions; clinical_decision = what to do for a patient; lay_question = a need a \
patient or family member has; claim = a statement someone wants checked; contextual = involves the article's main \
subject that the passage itself does not name; open_ended = broad but still answered here.";

/// (style, weight, description shown to the writer)
/// Weights target the research doc's starting register mix (Rec. 3): professional (expert_terse, natural_question,
/// keyword, part of scenario) ~45, lay (lay_paraphrase, lay personas' questions) ~25, messy (non_native + typing
/// noise) ~15, partial recall (half_remembered) ~7, instruction-bearing ~8. Persona constraints move the realised
/// mix; pilot3 reports it.
const STYLES: [(&str, u32, &str); 8] = [
    ("expert_terse", 14, "a professional's terse search string, with technical terms and maybe abbreviations, no question words"),
    ("natural_question", 24, "a plain question in this person's own words (not the need's wording)"),
    ("keyword", 16, "2-5 search terms this person would know, typed into a search box: no question, no sentence"),
    ("lay_paraphrase", 14, "everyday words instead of jargon (\"water pill\" not \"diuretic\"), as a non-expert would say it"),
    ("half_remembered", 7, "the person only half-remembers it: vague or partly wrong details, still clearly the same need"),
    ("scenario", 8, "one or two sentences about the person's own situation (no names), then what they want to know"),
    ("non_native", 12, "written by this person in English as a second language: visible slips typical of their first language (missing or wrong articles, wrong prepositions or tense, unusual word order), still understandable"),
    ("instruction", 8, "the search plus the given constraint on what to return, as people add to a search box"),
];

fn style_desc(s: &str) -> &'static str {
    STYLES.iter().find(|x| x.0 == s).map(|x| x.2).unwrap_or("")
}

pub fn doc_ref(q: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    static GRP: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)\b(this|these|the present|the current|our|that) (study|studies|trial|article|paper|cohort|analysis|experiment|work|patients|participants|subjects|sample|dataset|report|review|investigation|research)\b|according to|\bthe (authors|researchers|investigators|study|trial|paper|article|passage|experiment|above|figure)\b|\bfig(ure|\.)?\s*\d|\btable\s*\d|\bpanel [a-h]\b|\b(group|arm) [a-d1-9]\b").unwrap());
    if re.is_match(q) {
        return true;
    }
    // "the <x> group|arm|cohort": study-local unless <x> names a kind of population or a chemical group
    const OK: [&str; 22] = ["age", "blood", "risk", "high-risk", "low-risk", "ethnic", "functional", "methyl", "phosphate",
        "hydroxyl", "amino", "carboxyl", "support", "working", "prosthetic", "acetyl", "racial", "income", "socioeconomic",
        "peer", "family", "patient"];
    let g = GRP.get_or_init(|| Regex::new(r"(?i)\bthe ((?:[a-z0-9-]+ ){0,2}[a-z0-9-]+) (group|groups|arm|arms|cohort)\b").unwrap());
    g.captures_iter(q).any(|c| !OK.contains(&c[1].rsplit(' ').next().unwrap_or("").to_lowercase().as_str()))
}

pub fn exam(q: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^\s*(you (need|are|have|want|must|plan|should|were)|imagine|suppose|assume|as an? [a-z ]+, you)\b|\byou are (planning|asked|designing|tasked)\b").unwrap())
        .is_match(q)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Card {
    pub id: String,        // "P1.c2"
    pub marker: String,    // "P1"
    pub want_type: String, // scheduled type (IF/NP) or "" (PF)
    pub persona: Option<usize>, // PF: library index of the reader stage A chose
    /// PF: candidate readers offered to stage A (library indices)
    pub cands: Vec<usize>,
    /// PF: stage A said none of the candidates would read the article; persona is a field-expert fallback
    pub fallback: bool,
    /// PF: stage A said none of the candidates fit; persona matched to the card's audience (v3.1b)
    pub matched: bool,
    /// stage A judged the article not biomedical (v3.1c): no queries from it
    pub off_domain: bool,
    /// v3.1d: the extra per-article card for a patient, carer or clinician reader (skipped for non-clinical papers)
    pub health: bool,
    /// v3.1d: stage A's concrete role for the field expert when no candidate fits ("veterinary surgeon", ...)
    pub expert_role: String,
    pub need: String,
    /// TREC-style narrative: what counts as relevant and what does not
    pub narrative: String,
    /// optional PICO slots (population, intervention, comparison, outcome); empty when not a clinical question
    pub pico: [String; 4],
    pub ctype: String,
    pub answer: String,
    pub specificity: String,
    pub audience: Vec<String>,
    pub min_expertise: u8,
    pub entities: Vec<String>,
    pub skip: bool,
}

fn uh(parts: &[&str]) -> u64 {
    u64::from_str_radix(&hash(parts)[..12], 16).unwrap_or(0)
}

/// Stage A plan: per passage, TEXT_SLOTS cards with a scheduled type (IF/NP, same scheduler as v2) or a persona (PF).
pub fn plan_cards(layout: Layout, art: &Article, chunks: &[&SampleChunk]) -> Vec<Card> {
    let mut out = vec![];
    for c in chunks {
        let ok = contextual_ok(&art.title, &c.text);
        let mut taken: Vec<String> = vec![];
        for s in 1..=TEXT_SLOTS {
            let id = format!("{}.c{s}", c.marker);
            if layout == Layout::TextV3Pf {
                // v3.1c: stratified candidates, so a plausible reader of each kind is always on offer:
                // (a) lay / patient / family, (b) clinician or student, (c) researcher or other professional
                let cands = CAND_GROUPS.iter().enumerate().map(|(k, groups)| {
                    let start = (uh(&[&c.chunk_id, "pf", &s.to_string(), &k.to_string()]) % LIBRARY_SIZE as u64) as usize;
                    (0..LIBRARY_SIZE).map(|j| (start + j) % LIBRARY_SIZE).find(|&i| groups.contains(&lib()[i].group.as_str())).unwrap_or(start)
                }).collect();
                out.push(Card { id, marker: c.marker.clone(), cands, ..Default::default() });
            } else {
                let sp = text_spec(&c.chunk_id, &format!("s{s}"), ok, &taken);
                taken.push(sp.intent.clone());
                out.push(Card { id, marker: c.marker.clone(), want_type: sp.intent, ..Default::default() });
            }
        }
    }
    // v3.1d: one extra card per article, on a hash-chosen passage, for a patient / carer (a) or clinician (b) reader
    if layout == Layout::TextV3Pf && !chunks.is_empty() {
        let c = chunks[(uh(&[&art.article_id, "health"]) % chunks.len() as u64) as usize];
        let cands = CAND_GROUPS[..2].iter().enumerate().map(|(k, groups)| {
            let start = (uh(&[&c.chunk_id, "health", &k.to_string()]) % LIBRARY_SIZE as u64) as usize;
            (0..LIBRARY_SIZE).map(|j| (start + j) % LIBRARY_SIZE).find(|&i| groups.contains(&lib()[i].group.as_str())).unwrap_or(start)
        }).collect();
        out.push(Card { id: format!("{}.h", c.marker), marker: c.marker.clone(), cands, health: true, ..Default::default() });
    }
    out
}

pub fn prompt_a(layout: Layout, art: &Article, cards: &[Card]) -> String {
    let lines: Vec<String> = cards.iter().map(|c| if c.cands.is_empty() {
        format!("{}: passage [[{}]]; requested type: {}", c.id, c.marker, c.want_type)
    } else {
        let who: Vec<String> = c.cands.iter().zip(READERS).map(|(&p, r)| format!("({r}) {}", lib()[p].card())).collect();
        if c.health {
            format!("{}: passage [[{}]]; HEALTH CARD, candidate readers: {}. Only if this is a clinical or health article \
that a patient, carer or clinician would really read: set reader to that person and write the need they would have \
that this passage answers. Otherwise set skip true.", c.id, c.marker, who.join("; "))
        } else {
            format!("{}: passage [[{}]]; candidate readers: {}. Set reader to the one most likely to actually read THIS article \
and have a need this passage answers, or none if none of them plausibly would (then name the expert_role of a real \
professional who would); write that reader's need.", c.id, c.marker, who.join("; "))
        }
    }).collect();
    let rules = if cards.iter().any(|c| !c.cands.is_empty()) { READER_RULES } else { "" };
    let pf = if layout == Layout::TextV3Pf { "Cards for different people on the same passage should differ when their needs differ.\n" } else {
        "Cards on the same passage must state different needs.\n" };
    format!("ARTICLE. Passages are marked [[P1]] ... [[/P1]] and so on.\n<<<\n{}\n>>>\n\nFirst set biomedical: true if the \
article is about human or animal health, medicine, biology, biomedical science or public health; false for physics, \
chemistry, materials, engineering, computing, economics or social topics without a health focus.\n\nWrite one intent card \
per line below.\n{}\n\n{pf}{rules}{NEED_RULES}\n{TYPE_DEFS}", art.text, lines.join("\n"))
}

const READERS: [&str; 4] = ["a", "b", "c", "none"];
/// Candidate strata (v3.1c): one lay reader, one clinician or student, one researcher or other professional.
const CAND_GROUPS: [&[&str]; 3] = [&["lay"], &["clinician", "student"], &["researcher", "other"]];
const READER_RULES: &str = "Choosing the reader: a reader is plausible only if their role AND goal would lead them to \
this article and to this need. A patient or family member asks about their own or a relative's health, treatment, \
risk or everyday life, not lab methods, assay details, taxonomy or statistics. A clinician asks about patients, \
diagnosis, treatment or care. Researchers ask about methods, results and mechanisms in their own kind of field. Other \
professionals (journalist, librarian, policy analyst, veterinarian, ...) ask what their job needs. If no candidate \
fits (for example a plant, veterinary, agricultural, ecological or technical paper that none of them would read), set \
reader to none and set expert_role to the concrete job of someone who would read it (e.g. \"veterinary surgeon\", \
\"agronomist\", \"hospital pharmacist\", \"cardiologist\", \"epidemiologist\", \"bioinformatician\", \"sports \
physiotherapist\"), not a generic \"researcher\". Otherwise leave expert_role empty.\n";

pub fn schema_a(cards: &[Card]) -> Value {
    let ids: Vec<&str> = cards.iter().map(|c| c.id.as_str()).collect();
    let mut item = json!({"type": "object", "additionalProperties": false,
        "required": ["card", "need", "narrative", "pico", "type", "answer", "specificity", "audience", "min_expertise", "entities", "skip"],
        "properties": {"card": {"enum": ids}, "need": {"type": "string"}, "narrative": {"type": "string"},
            "pico": {"type": "object", "additionalProperties": false, "required": ["population", "intervention", "comparison", "outcome"],
                "properties": {"population": {"type": "string"}, "intervention": {"type": "string"}, "comparison": {"type": "string"}, "outcome": {"type": "string"}}}, "type": {"enum": TYPES}, "answer": {"type": "string"},
            "specificity": {"enum": ["unique", "few", "many"]},
            "audience": {"type": "array", "minItems": 1, "maxItems": 5, "items": {"enum": AUDIENCE}},
            "min_expertise": {"enum": ["0", "1", "2", "3"]},
            "entities": {"type": "array", "minItems": 0, "maxItems": 5, "items": {"type": "string"}},
            "skip": {"type": "boolean"}}});
    if cards.iter().any(|c| !c.cands.is_empty()) {
        item["properties"]["reader"] = json!({"enum": READERS[..CANDS].iter().chain(["none"].iter()).collect::<Vec<_>>()});
        item["required"].as_array_mut().unwrap().insert(1, json!("reader"));
        item["properties"]["expert_role"] = json!({"type": "string"});
        item["required"].as_array_mut().unwrap().insert(2, json!("expert_role"));
    }
    json!({"type": "object", "required": ["biomedical", "cards"], "additionalProperties": false,
        "properties": {"biomedical": {"type": "boolean"}, "cards": {"type": "array", "minItems": cards.len(), "maxItems": cards.len(), "items": item}}})
}

/// Fill planned cards from the stage-A reply; unknown or repeated ids are ignored, missing cards stay skipped.
pub fn fill_cards(plan: &[Card], v: &Value) -> Vec<Card> {
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
        c.skip = e["skip"].as_bool().unwrap_or(false) || c.need.split_whitespace().count() < 4;
        if !c.cands.is_empty() {
            match READERS[..c.cands.len()].iter().position(|r| e["reader"].as_str() == Some(*r)) {
                Some(i) => { c.persona = Some(c.cands[i]); c.fallback = false; }
                // health card without a fitting patient / clinician reader: no card
                None if c.health => { c.skip = true; }
                // v3.1d: an expert of the article's own field, with stage A's concrete role and a library persona's
                // demographics and typing habits (any professional group, expertise >= 2)
                None => {
                    c.persona = Some(fallback_persona(c.cands[0]));
                    c.fallback = true;
                    c.expert_role = s("expert_role").chars().take(60).collect();
                }
            }
        }
    }
    plan.iter().map(|c| by.remove(&c.id).unwrap()).collect()
}

/// A field expert used when stage A says none of the candidate readers would read the article: a library researcher
/// with professional expertise, shown to stage B as working in the article's own field (`who`).
pub fn fallback_persona(seed: usize) -> usize {
    let l = lib();
    (0..l.len()).map(|k| (seed + k) % l.len()).find(|&i| l[i].group != "lay" && l[i].expertise >= 2).unwrap_or(seed % l.len())
}

/// Persona text shown to stage B.
pub fn who(c: &Card, p: usize, stretch: bool) -> String {
    let pc = lib()[p].card();
    if c.fallback {
        let role = c.expert_role.trim();
        let role = if role.is_empty() || role.eq_ignore_ascii_case("researcher") { "expert working in this article's own field" } else { role };
        format!("{role}{}", &pc[pc.find(',').unwrap_or(pc.len())..])
    } else if stretch {
        format!("{pc} (has no training in this area: uses everyday words)")
    } else {
        pc
    }
}

/// Research-agenda needs ("where should research on X go", "future directions", "research gaps"): answered by every
/// discussion section, so generic (v3.1d).
pub fn research_agenda(s: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\bfuture (research|directions?|studies|work|perspectives?)\b|\bresearch (gaps?|agenda|priorities)\b|\bwhere (should|does|will) (research|the field)\b|\bnext steps (for|in) (research|the field)\b|\bwhat (further|more) research\b").unwrap()).is_match(s)
}

/// Study-local labels (v3.1d): participant / arm labels ("Rider B", "patient 3", "Group A") and bare short codes
/// ("B510") that only the paper defines. A code stays when the query says what it is ("strain B510", "compound
/// 4b", "Bacillus ... TF28").
pub fn local_label(q: &str) -> bool {
    static PART: OnceLock<Regex> = OnceLock::new();
    static CODE: OnceLock<Regex> = OnceLock::new();
    let part = PART.get_or_init(|| Regex::new(r"\b(?:[Rr]ider|[Pp]articipant|[Ss]ubject|[Pp]atient|[Cc]ase|[Vv]olunteer|[Dd]onor|[Hh]orse|[Dd]og|[Mm]ouse|[Aa]nimal|[Ss]ite|[Ww]ard|[Cc]linic|[Hh]ospital|[Ss]chool|[Vv]illage|[Hh]ousehold|[Ff]arm|[Ii]nterviewee|[Rr]espondent|[Nn]urse|[Ss]tudent|[Tt]eacher|[Cc]ohort|[Aa]rm|[Gg]roup)\s+(?:[A-Z]|\d{1,3}|[A-Z]\d{1,2}|[IVX]{1,4})\b").unwrap());
    let code = CODE.get_or_init(|| Regex::new(r"(?i)(?:\b(strain|isolate|compound|cell line|clone|line|variant|mutant|construct|plasmid|peptide|derivative|analogue|analog|inhibitor|drug|candidate|cultivar|accession|batch|sample|formulation)\s+)?\b([A-Z]{1,2}\d{3,}[A-Za-z]?)\b").unwrap());
    part.is_match(q) || code.captures_iter(q).any(|c| c.get(1).is_none())
}

/// Why a card is not used for single-positive queries ("" = kept).
pub fn card_drop(c: &Card) -> &'static str {
    if c.off_domain { "off_domain" } else if c.skip { "skip" } else if doc_ref(&c.need) { "doc_ref" }
    else if c.specificity == "many" || research_agenda(&c.need) { "generic" } else { "" }
}

/// Intent-first match: a library persona whose group is in the card's audience and whose expertise is at least the
/// card's minimum; with probability STRETCH a lay persona regardless. Returns (index, stretch).
pub fn match_persona(c: &Card, key: &str) -> Option<(usize, bool)> {
    let l = lib();
    let stretch = c.min_expertise >= 1 && !c.audience.iter().any(|a| a == "lay") && (uh(&[key, "stretch"]) % 1000) as f64 / 1000.0 < STRETCH;
    let start = (uh(&[key, "persona"]) % l.len() as u64) as usize;
    for k in 0..l.len() {
        let i = (start + k) % l.len();
        let p = &l[i];
        let fits = if stretch { p.group == "lay" } else {
            (c.audience.is_empty() || c.audience.iter().any(|a| *a == p.group)) && p.expertise >= c.min_expertise };
        if fits {
            return Some((i, stretch));
        }
    }
    None
}

/// Styles allowed for a persona (None = no-persona arm: every style but non_native). `instr_ok`: the article has at
/// least one constraint that is true of it (instruction style needs one).
pub fn allowed_styles(p: Option<&Persona>, stretch: bool, instr_ok: bool) -> Vec<(&'static str, u32)> {
    STYLES.iter().filter(|(s, _, _)| (*s != "instruction" || instr_ok) && match p {
        None => *s != "non_native",
        Some(p) => match *s {
            "expert_terse" => p.expertise >= 2 && !stretch,
            "keyword" => p.expertise >= 1 && !stretch,
            "lay_paraphrase" => p.expertise <= 1 || p.literacy != "high" || stretch,
            "scenario" => matches!(p.group.as_str(), "clinician" | "lay" | "student"),
            "instruction" => p.expertise >= 1,
            "non_native" => p.english != "native",
            _ => true,
        },
    }).map(|(s, w, _)| {
        // v3.1: limited-English personas write non-native twice as often; fluent L2 at the base rate
        let w = if *s == "non_native" && p.is_some_and(|p| p.english == "limited L2") { w * 2 } else { *w };
        (*s, w)
    }).collect()
}

pub fn pick_styles(p: Option<&Persona>, stretch: bool, instr_ok: bool, key: &str) -> Vec<&'static str> {
    let mut opts = allowed_styles(p, stretch, instr_ok);
    let mut out = vec![];
    for k in 0..STYLES_PER_PAIR {
        if opts.is_empty() {
            break;
        }
        let s = pick(uh(&[key, "style", &k.to_string()]), &opts);
        out.push(s);
        opts.retain(|x| x.0 != s);
    }
    out
}

/// Constraints that are true of the article by construction (corpus metadata and title), for instruction-style
/// queries (v3.1 fix 6): the positive must satisfy the filter the query states.
pub fn constraints(art: &Article) -> Vec<String> {
    static KID: OnceLock<Regex> = OnceLock::new();
    static MOUSE: OnceLock<Regex> = OnceLock::new();
    static RAT: OnceLock<Regex> = OnceLock::new();
    static HUMAN: OnceLock<Regex> = OnceLock::new();
    let pt: Vec<String> = art.pub_types.iter().map(|x| x.to_lowercase()).collect();
    let has = |s: &str| pt.iter().any(|x| x.contains(s));
    let at = art.article_type.as_str();
    let mut v: Vec<String> = vec![];
    if has("randomized controlled trial") {
        v.extend(["only RCTs".into(), "randomized trials only".into()]);
    }
    if has("meta-analysis") || has("systematic review") || at == "systematic-review" {
        v.push("systematic reviews or meta-analyses only".into());
    } else if at == "review-article" || has("review") {
        v.push("reviews only".into());
    }
    if at == "case-report" || has("case reports") {
        v.push("case reports".into());
    }
    if at == "research-article" && !has("review") {
        v.extend(["original research, not reviews".into(), "primary studies only".into()]);
    }
    if has("observational study") {
        v.push("observational studies".into());
    }
    if let Some(y) = art.year.filter(|y| (1990..=2100).contains(y)) {
        v.extend([format!("since {y}"), format!("since {}", y - 1), format!("since {}", y - 3), format!("published after {}", y - 2)]);
    }
    let t = art.title.as_str();
    let mouse = MOUSE.get_or_init(|| Regex::new(r"(?i)\b(mice|mouse|murine)\b").unwrap()).is_match(t);
    let rat = RAT.get_or_init(|| Regex::new(r"(?i)\brats?\b").unwrap()).is_match(t);
    let kid = KID.get_or_init(|| Regex::new(r"(?i)\b(child|children|childhood|pa?ediatric|infants?|neonat\w*|adolescen\w*)\b").unwrap()).is_match(t);
    if kid {
        v.push("in children".into());
    }
    if mouse {
        v.push("mouse studies".into());
    }
    if rat {
        v.push("rat studies".into());
    }
    if !mouse && !rat && (kid || HUMAN.get_or_init(|| Regex::new(r"(?i)\b(patients?|clinical|adults?|women|men|humans?)\b").unwrap()).is_match(t)) {
        v.push("human studies".into());
    }
    v
}

/// Typical English-as-a-second-language slips by first language (v3.1b: non_native queries were judged
/// native-sounding 48% of the time; the slot now names concrete slips to make).
pub fn l2_slips(l1: &str) -> &'static str {
    match l1 {
        "Spanish" | "Portuguese" | "French" => "extra or wrong articles (\"the diabetes\"), adjective after the noun, \
false friends (\"actual\" for current, \"realize a study\"), \"make\" for \"do\"",
        "Hindi" | "Bengali" | "Tagalog" => "wrong prepositions (\"discuss about\", \"effect for\"), progressive verbs \
(\"is having\"), dropped or extra articles, \"itself\"/\"only\" for emphasis",
        "Arabic" | "Turkish" | "Swahili" => "dropped \"is/are\", wrong prepositions, missing articles, question without \
inversion (\"why the drug cause...\")",
        "Vietnamese" | "Korean" | "Japanese" => "missing articles and plural -s, no past tense, topic first (\"diabetes, \
which drug better\")",
        "Russian" | "German" => "missing articles, unusual word order (verb late), \"since 3 years\", literal phrasing",
        _ => "missing articles, wrong prepositions, unusual word order",
    }
}

const HEDGES: [&str; 7] = ["begin with \"that\" or \"the one\" (\"that drug that...\")", "include one detail the person is unsure of, \
with a question mark", "put \"something about\" in the middle, not at the start", "\"was it X or Y\"", "no hedge words: just a vague, \
incomplete description", "end with \"can't remember the name\" or similar", "\"I think\" or \"think it's called\" after the key term"];
const SCENES: [&str; 5] = ["begin with \"my\" (my mother, my patient, my lab...)", "begin with the situation (\"after...\", \"since...\", \
\"we have...\")", "begin with the question, then one clause about the situation", "begin with the person's role (\"nurse here,\", \
\"PhD student,\")", "begin with \"our\" (our clinic, our team...)"];
/// v3.1d: scenario openers for patients and family: their own life, never a clinic, team or lab.
const LAY_SCENES: [&str; 4] = ["begin with \"my\" (my mum, my son, my doctor said...)", "begin with their situation (\"since my \
diagnosis...\", \"after the surgery...\")", "begin with the question, then one clause about their situation", "begin with \"I\" \
and their own situation (\"I have...\", \"I'm caring for...\")"];

/// One surface slot: (card, persona, style).
#[derive(Clone, Debug)]
pub struct Slot {
    pub id: String,  // "P1.c2.q1" (internal; never shown to the model)
    pub bid: String, // "slot-07": the id stage B sees (v3.1 fix 5)
    pub card: usize,
    pub persona: Option<usize>,
    pub stretch: bool,
    pub style: &'static str,
    /// style-specific instruction: the true constraint (instruction), an opener form (half_remembered, scenario)
    pub hint: String,
}

pub fn plan_slots(layout: Layout, cards: &[Card], art: &Article) -> Vec<Slot> {
    let cons = constraints(art);
    let mut out = vec![];
    for (ci, c) in cards.iter().enumerate() {
        if !card_drop(c).is_empty() {
            continue;
        }
        let key = format!("{}|{}", art.article_id, c.id);
        let (persona, stretch) = match layout {
            Layout::TextV3Pf => (c.persona, false),
            Layout::TextV3Np => (None, false),
            _ => match match_persona(c, &key) { Some((i, s)) => (Some(i), s), None => (None, false) },
        };
        for (k, style) in pick_styles(persona.map(|i| &lib()[i]), stretch, !cons.is_empty(), &key).into_iter().enumerate() {
            let h = uh(&[&key, "hint", &k.to_string()]) as usize;
            let hint = match style {
                "instruction" => format!("add this constraint (true of what they want; keep its meaning): {}", cons[h % cons.len()]),
                "half_remembered" => HEDGES[h % HEDGES.len()].to_string(),
                "non_native" => {
                    let l1 = persona.map(|i| lib()[i].first_language.as_str()).unwrap_or("");
                    format!("make at least two slips typical of a {} speaker: {}; never perfect English", if l1.is_empty() { "second-language" } else { l1 }, l2_slips(l1))
                }
                "scenario" if persona.is_some_and(|i| lib()[i].group == "lay") => LAY_SCENES[h % LAY_SCENES.len()].to_string(),
                "scenario" => SCENES[h % SCENES.len()].to_string(),
                _ => String::new(),
            };
            out.push(Slot { id: format!("{}.q{}", c.id, k + 1), bid: format!("slot-{:02}", out.len() + 1), card: ci, persona, stretch, style, hint });
        }
    }
    out
}

pub fn prompt_b(cards: &[Card], slots: &[Slot]) -> String {
    let mut by_card: BTreeMap<usize, Vec<&Slot>> = BTreeMap::new();
    for s in slots {
        by_card.entry(s.card).or_default().push(s);
    }
    let mut blocks = vec![];
    for (ci, ss) in by_card {
        let c = &cards[ci];
        let person = match ss[0].persona {
            Some(p) => who(c, p, ss[0].stretch),
            None => "a person searching the biomedical literature".to_string(),
        };
        let lines: Vec<String> = ss.iter().map(|s| {
            let h = if s.hint.is_empty() { String::new() } else { format!(" ({})", s.hint) };
            format!("  {} style={}: {}{h}", s.bid, s.style, style_desc(s.style))
        }).collect();
        blocks.push(format!("PERSON: {person}\nNEED: {}\nKEY TERMS: {}\n{}", c.need, c.entities.join("; "), lines.join("\n")));
    }
    format!("For each slot, write the search query this person would type for this need, in the slot's style. \
Write 3-25 words (keyword style: 2-6 words). The person decides which key terms to use and may misname them as \
their knowledge allows. Do not copy or closely paraphrase the need sentence: use at most three words in a row from \
it. The query text must not contain slot names. Never point at something only the article's reader knows: no \"they\", \"this <thing>\", \"the lab\" or \"the method\" unless the query names it (a study, trial or cohort may be named, e.g. \"the Ningbo air pollution study\"). No LaTeX.\n\n{}", blocks.join("\n\n"))
}

pub fn schema_b(slots: &[Slot]) -> Value {
    let ids: Vec<&str> = slots.iter().map(|s| s.bid.as_str()).collect();
    json!({"type": "object", "required": ["slots"], "additionalProperties": false, "properties": {"slots": {
        "type": "array", "minItems": slots.len(), "maxItems": slots.len(), "items": {"type": "object", "additionalProperties": false,
        "required": ["slot", "query", "skip"], "properties": {"slot": {"enum": ids}, "query": {"type": "string"}, "skip": {"type": "boolean"}}}}}})
}

/// Output budget for a v3 call: at least `base`, else ~320 tokens per card plus framing, capped below the context.
pub fn stage_budget(base: u32, cards: usize) -> u32 {
    base.max(320 * cards as u32 + 256).min(16_000)
}

/// Token-set Jaccard similarity (lowercase alphanumeric tokens).
pub fn jaccard(a: &str, b: &str) -> f64 {
    let t = |s: &str| s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect::<HashSet<_>>();
    let (x, y) = (t(a), t(b));
    let u = x.union(&y).count();
    if u == 0 { 0.0 } else { x.intersection(&y).count() as f64 / u as f64 }
}

/// Why a stage-B query is rejected ("" = kept). v3.1: leaked slot or passage ids (fix 5), JSON spilled into the
/// text, copies of the need (fix 1, Jaccard >= 0.7), and styles the text plainly does not follow (fix 8).
/// LaTeX left in a query ("$\alpha$7Tg", "\textit{...}").
pub fn tex(q: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\$[^$]*\$|\\[a-zA-Z]+\{?|\{\\").unwrap()).is_match(q)
}

/// A reference only the paper's reader can resolve (v3.1c): "did they use ...", "how they did ...", "this fungus",
/// "the lab", "the special cell stain". Named known-item lookups stay ("the ningbo air pollution study", "the polish
/// aml study"): a definite phrase is unresolved only when its modifiers are all generic words.
pub fn unresolved(q: &str) -> bool {
    static THEY: OnceLock<Regex> = OnceLock::new();
    static THIS: OnceLock<Regex> = OnceLock::new();
    static THE: OnceLock<Regex> = OnceLock::new();
    let they = THEY.get_or_init(|| Regex::new(r"(?i)\b(did|do|does|how|why|what|where|when|which|were|have|had) they\b|\bthey (used|did|found|measured|tested|showed|reported|chose|compared|recruited|collected|analy[sz]ed)\b").unwrap());
    let this = THIS.get_or_init(|| Regex::new(r"(?i)\b(this|these|that|those) (fungus|bacteri\w*|virus|drug|compound|protein|gene|molecule|method|model|technique|approach|stain|assay|device|tool|cohort|sample|samples|patients|participants|mice|rats|animals|cells|lab|team|experiment|intervention|program|programme|thing|stuff)\b").unwrap());
    let the = THE.get_or_init(|| Regex::new(r"(?i)\bthe ((?:[a-z-]+ ){0,2})(lab|team|group's|authors|stain|method|model|technique|approach|assay|procedure|protocol|device|tool|compound|drug|intervention|program|programme|sample|samples|dataset|data set)\b").unwrap());
    const GENERIC: [&str; 18] = ["special", "new", "novel", "same", "specific", "different", "other", "proposed", "current", "given",
        "above", "following", "previous", "particular", "their", "this", "that", "used"];
    const BACK: [&str; 10] = ["special", "same", "proposed", "above", "given", "following", "previous", "aforementioned", "described", "used"];
    they.is_match(q) || this.is_match(q) || the.captures_iter(q).any(|c| {
        let mods: Vec<&str> = c[1].split_whitespace().collect();
        let head = c[2].to_lowercase();
        (mods.is_empty() && matches!(head.as_str(), "lab" | "team" | "group's" | "authors" | "stain" | "device" | "tool" | "compound" | "drug" | "intervention" | "sample" | "samples" | "dataset" | "data set"))
            || (!mods.is_empty() && mods.iter().all(|m| GENERIC.contains(&m.to_lowercase().as_str())))
            // words that point back into the paper whatever follows: "the special cell stain", "the same assay"
            || mods.first().is_some_and(|m| BACK.contains(&m.to_lowercase().as_str()))
    })
}

pub fn reject(q: &str, need: &str, style: &str) -> &'static str {
    static LEAK: OnceLock<Regex> = OnceLock::new();
    let words = q.split_whitespace().count();
    if words < 2 {
        "empty"
    } else if LEAK.get_or_init(|| Regex::new(r"(?i)\bp\d+\.c\d+(\.q\d+)?\b|\bslot-\d+\b").unwrap()).is_match(q) {
        "leak"
    } else if q.contains('{') || q.contains('}') || q.contains('\n') || q.contains("\":") {
        "spill"
    } else if doc_ref(q) || exam(q) {
        "banned"
    } else if tex(q) {
        "tex"
    } else if unresolved(q) {
        "unresolved"
    } else if local_label(q) {
        "local_label"
    } else if research_agenda(q) {
        "generic"
    } else if jaccard(q, need) >= 0.7 {
        "copy"
    } else if (style == "keyword" && (words > 7 || q.contains('?'))) || (style == "expert_terse" && words > 15) {
        "style"
    } else {
        ""
    }
}

fn text_article3(ag: &ureq::Agent, a: &GenArgs, art: &Article, chunks: &[&SampleChunk]) -> Vec<Generation> {
    let mut out: Vec<Generation> = chunks.iter().map(|c| Generation { chunk_id: c.chunk_id.clone(), article_id: c.article_id.clone(),
        arm: a.arm.clone(), ..Default::default() }).collect();
    let k = chunks.len().max(1) as u64;
    let plan = plan_cards(a.layout, art, chunks);
    // Stage A writes ~20 cards per article (need, narrative, PICO, answer, entities): budget per card, not per call.
    let a = &GenArgs { max_tokens: stage_budget(a.max_tokens, plan.len()), ..a.clone() };
    let (ra, la) = post(ag, a, SYSTEM_A, json!(prompt_a(a.layout, art, &plan)), schema_a(&plan));
    let ra = match ra { Ok(r) => r, Err(e) => { out.iter_mut().for_each(|g| g.error = Some(format!("stage A: {e}"))); return out; } };
    let Some(va) = json_body(&ra.content) else {
        out.iter_mut().for_each(|g| g.error = Some(format!("stage A unparsed: {}", format!("{} chars, {} completion tokens, max {}: {}", ra.content.len(), ra.completion_tokens, a.max_tokens, ra.content.chars().take(200).collect::<String>()))));
        return out;
    };
    let mut cards = fill_cards(&plan, &va);
    if va["biomedical"] == json!(false) {
        cards.iter_mut().for_each(|c| c.off_domain = true);
    }
    let slots = plan_slots(a.layout, &cards, art);
    let (mut pt, mut ct, mut lat) = (ra.prompt_tokens, ra.completion_tokens, la);
    let mut accepted: HashMap<String, (String, String, &'static str)> = HashMap::new(); // slot -> (clean, noisy, reject reason)
    if !slots.is_empty() {
        let (rb, lb) = post(ag, a, SYSTEM_B, json!(prompt_b(&cards, &slots)), schema_b(&slots));
        lat += lb;
        match rb.ok().and_then(|r| { pt += r.prompt_tokens; ct += r.completion_tokens; json_body(&r.content) }) {
            Some(vb) => {
                let mut seen = HashSet::new();
                for e in vb["slots"].as_array().into_iter().flatten() {
                    let Some(bid) = e["slot"].as_str() else { continue };
                    let Some(s) = slots.iter().find(|s| s.bid == bid) else { continue };
                    let id = s.id.as_str();
                    if accepted.contains_key(id) {
                        continue;
                    }
                    let clean = e["query"].as_str().unwrap_or("").trim().to_string();
                    let mut why = reject(&clean, &cards[s.card].need, s.style);
                    if why.is_empty() && !seen.insert(clean.to_lowercase()) {
                        why = "duplicate";
                    }
                    if why.is_empty() && e["skip"].as_bool().unwrap_or(false) {
                        why = "skip";
                    }
                    let noisy = match s.persona { Some(p) => crate::noise::apply(&clean, &lib()[p], &format!("{}|{id}", art.article_id)), None => clean.clone() };
                    accepted.insert(id.to_string(), (clean, noisy, why));
                }
            }
            None => out.iter_mut().for_each(|g| g.error = Some("stage B failed".into())),
        }
    }
    let card_rows: Vec<Value> = cards.iter().map(|c| json!({"card": c.id, "need": c.need, "narrative": c.narrative, "pico": c.pico, "type": c.ctype, "want_type": c.want_type,
        "answer": c.answer, "specificity": c.specificity, "audience": c.audience, "min_expertise": c.min_expertise,
        "entities": c.entities, "persona": c.persona.map(|p| lib()[p].id.clone()), "persona_fallback": c.fallback, "persona_matched": c.matched,
        "candidates": c.cands.iter().map(|&p| lib()[p].id.clone()).collect::<Vec<_>>(), "drop": card_drop(c)})).collect();
    let generator = std::env::var("PIGMED_GENERATOR").unwrap_or_else(|_| a.model.clone());
    for (g, ch) in out.iter_mut().zip(chunks) {
        g.prompt_tokens = pt / k;
        g.completion_tokens = ct / k;
        g.latency_s = lat;
        let pre = format!("{}.", ch.marker);
        let mut q = Map::new();
        let mut m = Map::new();
        for s in slots.iter().filter(|s| s.id.starts_with(&pre)) {
            let c = &cards[s.card];
            let key = s.id[pre.len()..].to_string();
            let (clean, noisy, why) = accepted.get(&s.id).cloned().unwrap_or((String::new(), String::new(), "missing"));
            let rejected = !why.is_empty();
            let p = s.persona.map(|i| &lib()[i]);
            m.insert(key.clone(), json!({"task": "T3", "recipe": RECIPE, "generator": generator, "intent": c.ctype, "style": s.style, "hint": s.hint,
                "need": c.need, "narrative": c.narrative, "pico": c.pico, "answer": c.answer,
                "specificity": c.specificity, "persona_id": p.map(|p| p.id.clone()), "persona_group": p.map(|p| p.group.clone()),
                "persona": s.persona.map(|i| who(c, i, s.stretch)), "persona_fallback": c.fallback, "english": p.map(|p| p.english.clone()), "stretch": s.stretch,
                "license": art.license_code, "commercial": art.commercial,
                "clean": clean, "query": noisy, "banned": why == "banned", "rejected": rejected, "reject": why}));
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
    out
}

pub fn run_text3(a: &GenArgs, chunks: &[SampleChunk], articles: &[Article]) -> Result<(Vec<Generation>, Value)> {
    let ag = agent(a.concurrency);
    let by: HashMap<&str, &Article> = articles.iter().map(|x| (x.article_id.as_str(), x)).collect();
    let mut groups: Vec<Vec<&SampleChunk>> = vec![];
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for c in chunks {
        let i = *idx.entry(c.article_id.as_str()).or_insert_with(|| { groups.push(vec![]); groups.len() - 1 });
        groups[i].push(c);
    }
    groups.retain(|g| by.contains_key(g[0].article_id.as_str()));
    let _ = lib();
    let before = scrape(&ag, &a.endpoint);
    let t0 = Instant::now();
    let gens = par(groups.len(), a.concurrency, |i| text_article3(&ag, a, by[groups[i][0].article_id.as_str()], &groups[i]));
    let wall = t0.elapsed().as_secs_f64();
    let mut s = summary(a, &gens, groups.len() * 2, wall, delta(&before, &scrape(&ag, &a.endpoint)));
    let mut drops: BTreeMap<String, usize> = BTreeMap::new();
    let mut ncards = 0;
    for g in &gens {
        for c in g.meta["cards"].as_array().into_iter().flatten() {
            ncards += 1;
            *drops.entry(c["drop"].as_str().unwrap_or("").to_string()).or_default() += 1;
        }
    }
    s["cards"] = json!(ncards);
    s["card_outcomes"] = json!(drops);
    s["api"] = crate::generate::api().report();
    Ok((gens, s))
}

// ---------- judge3: source-free query judge, same for every arm ----------

pub const JUDGE3_SYSTEM: &str = "You judge search queries written for a biomedical literature search engine. You see \
only the queries, not any article. Output JSON only.";

const JUDGE3_RULES: &str = "For each query:\n\
- standalone: true if a stranger can understand what is being searched for without seeing any particular article \
(false if it says \"this study\", \"the authors\", or uses labels only one paper would use, like \"the crude ES \
group\" or \"cohort B\").\n\
- specificity: how many published papers would answer it well. unique = one or two; few = a handful; many = \
hundreds (textbook or very common knowledge).\n\
- exam: true if it reads like an exam question or a task instruction (\"You need to...\", \"You are planning...\", \
\"Which of the following\").\n\
- natural: 1 = no real person would type this; 2 = plausible but stilted; 3 = a real person could well type this.\n\
- register: expert, student, lay.\n\
- style_ok: when a query is marked [written as: style], true if it really reads that way (a keyword query is a few \
terms, not a sentence; a non_native query shows second-language slips; a lay_paraphrase avoids jargon; a \
half_remembered query is vague or hedged); true when no style is marked.";

pub fn judge3(a: &GenArgs, gens: &[Generation], per_call: usize) -> (Vec<Value>, Value) {
    let ag = agent(a.concurrency);
    let mut items: Vec<(String, String, String)> = vec![];
    for g in gens {
        for (k, v) in &g.queries {
            if let Some(q) = v.as_str() {
                let style = g.meta["slots"][k]["style"].as_str().filter(|s| *s != "v2").unwrap_or("").to_string();
                items.push((format!("{}-{}-{}", g.arm, k, g.chunk_id), q.to_string(), style));
            }
        }
    }
    let batches: Vec<Vec<(String, String, String)>> = items.chunks(per_call.max(1)).map(|c| c.to_vec()).collect();
    let rows = par(batches.len(), a.concurrency, |i| {
        let b = &batches[i];
        let ids: Vec<String> = (1..=b.len()).map(|k| format!("q{k}")).collect();
        let list: Vec<String> = b.iter().zip(&ids).map(|((_, q, st), id)| if st.is_empty() { format!("{id}: {q}") } else {
            format!("{id} [written as: {st}]: {q}") }).collect();
        let item = json!({"type": "object", "additionalProperties": false, "required": ["id", "standalone", "specificity", "exam", "natural", "register", "style_ok"],
            "properties": {"id": {"enum": ids}, "standalone": {"type": "boolean"}, "specificity": {"enum": ["unique", "few", "many"]},
                "exam": {"type": "boolean"}, "natural": {"enum": ["1", "2", "3"]}, "register": {"enum": ["expert", "student", "lay"]},
                "style_ok": {"type": "boolean"}}});
        let schema = json!({"type": "object", "required": ["ratings"], "additionalProperties": false, "properties": {"ratings": {
            "type": "array", "minItems": b.len(), "maxItems": b.len(), "items": item}}});
        let (r, _) = post(&ag, a, JUDGE3_SYSTEM, json!(format!("{JUDGE3_RULES}\n\nQUERIES:\n{}", list.join("\n"))), schema);
        let v = r.ok().and_then(|r| json_body(&r.content)).unwrap_or(Value::Null);
        let by: HashMap<String, Value> = v["ratings"].as_array().into_iter().flatten()
            .filter_map(|x| Some((x["id"].as_str()?.to_string(), x.clone()))).collect();
        b.iter().zip(&ids).filter_map(|((qid, q, st), id)| {
            let x = by.get(id)?;
            Some(json!({"qid": qid, "query": q, "standalone": x["standalone"], "specificity": x["specificity"], "exam": x["exam"],
                "natural": x["natural"].as_str().and_then(|s| s.parse::<u8>().ok()), "register": x["register"],
                "style": st, "style_ok": if st.is_empty() { Value::Null } else { x["style_ok"].clone() }}))
        }).collect()
    });
    let n = rows.len().max(1) as f64;
    let share = |f: &dyn Fn(&Value) -> bool| rows.iter().filter(|r| f(r)).count() as f64 / n;
    let mut reg: BTreeMap<String, usize> = BTreeMap::new();
    for r in &rows {
        *reg.entry(r["register"].as_str().unwrap_or("?").to_string()).or_default() += 1;
    }
    let style_ok = {
            let styled: Vec<&Value> = rows.iter().filter(|r| !r["style_ok"].is_null()).collect();
            let mut by: BTreeMap<String, (usize, usize)> = BTreeMap::new();
            for r in &styled {
                let e = by.entry(r["style"].as_str().unwrap_or("").to_string()).or_default();
                e.0 += 1;
                e.1 += (r["style_ok"] == json!(true)) as usize;
            }
            json!({"all": styled.iter().filter(|r| r["style_ok"] == json!(true)).count() as f64 / styled.len().max(1) as f64,
                "by_style": by.into_iter().map(|(k, (n, ok))| (k, json!(ok as f64 / n.max(1) as f64))).collect::<Map<String, Value>>()})
    };
    let s = json!({"arm": gens.first().map(|g| g.arm.clone()), "queries": items.len(), "judged": rows.len(),
        "standalone": share(&|r| r["standalone"] == json!(true)), "doc_referential": share(&|r| r["standalone"] == json!(false)),
        "generic_many": share(&|r| r["specificity"] == json!("many")), "exam": share(&|r| r["exam"] == json!(true)),
        "natural_mean": rows.iter().filter_map(|r| r["natural"].as_u64()).sum::<u64>() as f64 / n,
        "register": reg, "style_ok": style_ok});
    (rows, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_ref_and_exam_rules() {
        assert!(doc_ref("How did the mean lesion size in the crude ES group change"));
        assert!(doc_ref("Can I trust the findings of this study on statins"));
        assert!(doc_ref("results in group B after 12 weeks"));
        assert!(!doc_ref("colchicine recurrent stroke risk in the high-risk group"));
        assert!(!doc_ref("does metformin lower lactate in septic shock"));
        assert!(!doc_ref("phosphorylation of the methyl group on histone H3"));
        assert!(exam("You need to choose an assay for ..."));
        assert!(exam("You are planning a trial of ..."));
        assert!(!exam("your doctor said statins, are they safe"));
    }

    fn setup() -> (Article, SampleChunk, SampleChunk) {
        let art = Article { article_id: "PMC1.1".into(), title: "Colchicine after stroke".into(),
            text: "[[P1]] a [[/P1]] [[P2]] b [[/P2]]".into(), words: 4, passages: vec!["P1".into(), "P2".into()], ..Default::default() };
        let c = |id: &str, m: &str| SampleChunk { chunk_id: id.into(), article_id: "PMC1.1".into(), marker: m.into(), text: "x y z".into(), ..Default::default() };
        (art, c("u1", "P1"), c("u2", "P2"))
    }

    #[test]
    fn plan_fill_match_and_slots() {
        let (art, a, b) = setup();
        for lay in [Layout::TextV3If, Layout::TextV3Pf, Layout::TextV3Np] {
            let plan = plan_cards(lay, &art, &[&a, &b]);
            // persona-first adds one health card per article (v3.1d)
            assert_eq!(plan.len(), 2 * TEXT_SLOTS + (lay == Layout::TextV3Pf) as usize);
            assert_eq!(plan, plan_cards(lay, &art, &[&a, &b]));
            assert_eq!(lay == Layout::TextV3Pf, plan[0].cands.len() == CANDS);
            if lay == Layout::TextV3Pf {
                assert!(prompt_a(lay, &art, &plan).contains("candidate readers"));
                assert_eq!(schema_a(&plan)["properties"]["cards"]["items"]["properties"]["reader"]["enum"], json!(["a", "b", "c", "none"]));
            }
            assert_eq!(schema_a(&plan)["properties"]["cards"]["minItems"], json!(plan.len()));
            assert!(prompt_a(lay, &art, &plan).contains("P1.c1"));
            let v = json!({"cards": [
                {"card": "P1.c1", "need": "Whether low-dose colchicine reduces recurrent stroke after lacunar infarct", "type": "result_number", "narrative": "Relevant: trial outcomes. Not: other stroke types.",
                 "pico": {"population": "lacunar stroke", "intervention": "colchicine", "comparison": "placebo", "outcome": "recurrence"},
                 "answer": "no", "specificity": "few", "audience": ["clinician", "lay"], "min_expertise": "0", "entities": ["colchicine"], "skip": false},
                {"card": "P1.c2", "need": "Lesion size in the crude ES group after treatment with drug", "type": "fact_lookup",
                 "answer": "x", "specificity": "unique", "audience": ["researcher"], "min_expertise": "2", "entities": [], "skip": false},
                {"card": "P1.c3", "need": "What is the function of mitochondria in human cells generally", "type": "open_ended",
                 "answer": "x", "specificity": "many", "audience": ["student"], "min_expertise": "0", "entities": [], "skip": false}]});
            let cards = fill_cards(&plan, &v);
            assert_eq!(card_drop(&cards[0]), "");
            assert_eq!(card_drop(&cards[1]), "doc_ref");
            assert_eq!(card_drop(&cards[2]), "generic");
            assert_eq!(card_drop(&cards[3]), "skip");
            if lay == Layout::TextV3Pf {
                // no "reader" in the reply -> an expert of the article's own field (v3.1c)
                assert!(cards[0].fallback && lib()[cards[0].persona.unwrap()].group != "lay");
                assert!(who(&cards[0], cards[0].persona.unwrap(), false).starts_with("expert working in this article's own field,"));
                let named = Card { expert_role: "veterinary surgeon".into(), ..cards[0].clone() };
                assert!(who(&named, named.persona.unwrap(), false).starts_with("veterinary surgeon,"));
                assert!(plan.last().unwrap().health && plan.last().unwrap().cands.len() == 2);
                // stratified candidates: lay, clinician/student, researcher/other
                let g: Vec<&str> = plan[0].cands.iter().map(|&i| lib()[i].group.as_str()).collect();
                assert!(g[0] == "lay" && ["clinician", "student"].contains(&g[1]) && ["researcher", "other"].contains(&g[2]), "{g:?}");
                assert!(prompt_a(lay, &art, &plan).contains("a reader is plausible only if"));
            }
            let slots = plan_slots(lay, &cards, &art);
            assert_eq!(slots.len(), STYLES_PER_PAIR);
            assert!(slots.iter().all(|s| s.card == 0 && (s.persona.is_none() == (lay == Layout::TextV3Np))));
            assert_ne!(slots[0].style, slots[1].style);
            let pb = prompt_b(&cards, &slots);
            assert!(pb.contains("slot-01") && !pb.contains("P1.c1") && !pb.contains("[[P1]]") && !pb.contains("x y z"));
            assert!(!slots.iter().any(|s| s.style == "instruction"), "no metadata -> no instruction slots");
        }
    }

    #[test]
    fn v31_rejects_and_constraints() {
        let need = "How does the selection of competition events by handlers bias agility performance data?";
        assert_eq!(reject("How does the selection of competition events by handlers potentially bias agility performance data?", need, "natural_question"), "copy");
        assert_eq!(reject("do handlers pick easy agility trials and skew the results", need, "natural_question"), "");
        assert_eq!(reject("agnw swcnt pet uv resin transfer protocol p2.c2.q2", "x", "keyword"), "leak");
        assert_eq!(reject("SWCNT AgNW peak current 0.8 V P4.c2.q2", "x", "keyword"), "leak");
        assert_eq!(reject("what is it slot-03 about", "x", "natural_question"), "leak");
        assert_eq!(reject("hematology end-of-life France\n\n{\"P1.c2.q2\": \"x\"}", "x", "natural_question"), "leak");
        assert_eq!(reject("palliative care MAID support\n\n{\"a\": 1}", "x", "natural_question"), "spill");
        assert_eq!(reject("How is the purification of rAXE performed and how was the molecular mass evaluated?", "x", "keyword"), "style");
        assert_eq!(reject("coenzyme Q10 statin myopathy", "x", "keyword"), "");
        // v3.1c: TeX and references only the paper's reader can resolve
        assert_eq!(reject("Sca-1 $\\alpha$7Tg eccentric exercise inflammation", "x", "keyword"), "tex");
        for q in ["Did they use sliding windows instead of annotated regions for noncoding associations",
                  "how they did the transcriptomic analysis of Fusarium graminearum",
                  "The lab is looking at how this fungus reacts to the bacteria",
                  "how long to wait with the special cell stain to see different cell types"] {
            assert_eq!(reject(q, "x", "natural_question"), "unresolved", "{q}");
        }
        // v3.1d: study-local labels and research-agenda queries
        for q in ["Rider B kinematic patterns for different obstacle heights", "What are the environmental consequences of using B510?",
                  "Patient 3 recovery time after hip surgery"] {
            assert_eq!(reject(q, "x", "natural_question"), "local_label", "{q}");
        }
        for q in ["Bacillus amyloliquefaciens strain TF28 secondary metabolites", "BRCA1 TP53 HER2 CD8 IL6 breast cancer",
                  "efficacy of compound 4b against MRSA", "H1N1 vaccine coverage 2019"] {
            assert_ne!(reject(q, "x", "keyword"), "local_label", "{q}");
        }
        assert_eq!(reject("Where should research on MSCC go from here?", "x", "natural_question"), "generic");
        assert!(research_agenda("What are the future research directions for MSCC?"));
        for q in ["did the ningbo air pollution study account for personal risk factors",
                  "whhat were the complete remission rates across the different age cohorts in the polish aml study",
                  "how long can they wait before surgery for appendicitis in kids"] {
            assert_ne!(reject(q, "x", "natural_question"), "unresolved", "{q}");
        }
        let art = Article { title: "Single-stage repair of hypospadias in children: a randomized trial".into(), year: Some(2019),
            article_type: "research-article".into(), pub_types: vec!["Randomized Controlled Trial".into(), "Journal Article".into()], ..Default::default() };
        let c = constraints(&art);
        for want in ["only RCTs", "original research, not reviews", "since 2019", "since 2016", "in children", "human studies"] {
            assert!(c.iter().any(|x| x == want), "{want} in {c:?}");
        }
        assert!(!c.iter().any(|x| x.contains("review") && !x.contains("not reviews")) && !c.iter().any(|x| x == "since 2020"));
        let mice = Article { title: "Gut microbiota in obese mice".into(), article_type: "case-report".into(), ..Default::default() };
        assert_eq!(constraints(&mice), vec!["case reports".to_string(), "mouse studies".to_string()]);
        assert!(constraints(&Article::default()).is_empty());
    }

    #[test]
    fn persona_match_respects_audience() {
        let c = Card { audience: vec!["researcher".into()], min_expertise: 2, ..Default::default() };
        let mut stretched = 0;
        for k in 0..400 {
            let (i, st) = match_persona(&c, &format!("k{k}")).unwrap();
            let p = &lib()[i];
            if st { stretched += 1; assert_eq!(p.group, "lay"); } else { assert!(p.group == "researcher" && p.expertise >= 2); }
        }
        assert!((20..=70).contains(&stretched), "{stretched}");
        let lay = &lib().iter().find(|p| p.group == "lay" && p.english == "limited L2").unwrap();
        let st = allowed_styles(Some(lay), false, true);
        assert!(st.iter().any(|s| s.0 == "non_native") && !st.iter().any(|s| s.0 == "expert_terse"));
        assert!(!allowed_styles(None, false, true).iter().any(|s| s.0 == "non_native"));
        // v3.1: no keyword style for people without background; no instruction without a true constraint
        let novice = lib().iter().find(|p| p.expertise == 0).unwrap();
        assert!(!allowed_styles(Some(novice), false, true).iter().any(|s| s.0 == "keyword"));
        assert!(!allowed_styles(None, false, false).iter().any(|s| s.0 == "instruction"));
    }
}
