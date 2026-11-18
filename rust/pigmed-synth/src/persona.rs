//! Persona library for query recipe v3 (docs/RETRIEVER_QUERY_RECIPE_V3.md §3): a deterministic attribute sampler
//! with compatibility rules, rendered to a short card. No LLM and no external data: persona `i` of seed `s` is always
//! the same, so every query records which persona produced it.

use crate::sample::hash;
use serde::{Deserialize, Serialize};

/// (role, group, min expertise, max expertise, weight within group)
const ROLES: &[(&str, &str, u8, u8, u32)] = &[
    ("general practitioner", "clinician", 2, 2, 10), ("emergency physician", "clinician", 2, 3, 6),
    ("hospitalist", "clinician", 2, 2, 5), ("surgeon", "clinician", 2, 3, 5), ("oncologist", "clinician", 2, 3, 5),
    ("psychiatrist", "clinician", 2, 3, 4), ("paediatrician", "clinician", 2, 3, 5), ("cardiologist", "clinician", 2, 3, 4),
    ("nurse", "clinician", 2, 2, 12), ("nurse practitioner", "clinician", 2, 2, 5), ("pharmacist", "clinician", 2, 3, 7),
    ("dentist", "clinician", 2, 2, 3), ("physiotherapist", "clinician", 2, 2, 3), ("midwife", "clinician", 2, 2, 3),
    ("paramedic", "clinician", 2, 2, 3), ("dietitian", "clinician", 2, 2, 3), ("infectious disease physician", "clinician", 2, 3, 3),
    ("wet-lab biologist", "researcher", 2, 3, 12), ("bioinformatician", "researcher", 2, 3, 7), ("epidemiologist", "researcher", 2, 3, 6),
    ("clinical trialist", "researcher", 2, 3, 4), ("PhD student", "researcher", 1, 2, 12), ("postdoc", "researcher", 2, 3, 9),
    ("principal investigator", "researcher", 3, 3, 6), ("biostatistician", "researcher", 2, 2, 3), ("neuroscientist", "researcher", 2, 3, 4),
    ("immunologist", "researcher", 2, 3, 4), ("microbiologist", "researcher", 2, 3, 4), ("pharmacologist", "researcher", 2, 3, 4),
    ("medical student", "student", 1, 2, 12), ("nursing student", "student", 1, 1, 7), ("pharmacy student", "student", 1, 1, 4),
    ("undergraduate biology student", "student", 1, 1, 9), ("high-school student", "student", 0, 1, 4),
    ("public-health master's student", "student", 1, 1, 3),
    ("patient with a new diagnosis", "lay", 0, 1, 10), ("patient with a long-term condition", "lay", 0, 1, 10),
    ("worried healthy adult", "lay", 0, 0, 5), ("parent of a sick child", "lay", 0, 1, 8), ("adult child caring for an elderly parent", "lay", 0, 1, 6),
    ("partner of a patient", "lay", 0, 1, 4), ("pregnant person", "lay", 0, 1, 3), ("athlete", "lay", 0, 1, 2),
    ("medical librarian", "other", 1, 2, 4), ("systematic reviewer", "other", 2, 2, 3), ("health journalist", "other", 1, 1, 4),
    ("health policy analyst", "other", 1, 2, 3), ("pharma medical-affairs scientist", "other", 2, 3, 3), ("regulatory writer", "other", 2, 2, 2),
    ("health-app developer", "other", 0, 1, 2), ("veterinarian", "other", 2, 2, 2), ("science teacher", "other", 1, 1, 2),
    ("patent attorney", "other", 1, 1, 1),
];

/// Target share of personas by group (§3: clinicians 30, researchers 30, students 12, lay 20, other 8).
pub const GROUPS: [(&str, u32); 5] = [("clinician", 30), ("researcher", 30), ("student", 12), ("lay", 20), ("other", 8)];
const AGES: [(&str, u32); 5] = [("16-19", 4), ("20-29", 26), ("30-44", 32), ("45-64", 28), ("65+", 10)];
const GENDERS: [(&str, u32); 3] = [("woman", 49), ("man", 48), ("non-binary person", 3)];
const L1: [(&str, u32); 14] = [("Spanish", 14), ("Portuguese", 7), ("Hindi", 8), ("Arabic", 8), ("French", 6), ("German", 5),
    ("Vietnamese", 4), ("Tagalog", 4), ("Korean", 4), ("Swahili", 3), ("Bengali", 5), ("Russian", 5), ("Turkish", 4), ("Japanese", 4)];
const GOALS: [(&str, u32); 6] = [("decide what to do for a patient or for themself", 12), ("understand how something works", 22),
    ("check whether a claim is true", 16), ("find a method or protocol", 14), ("gather evidence for a review, paper or grant", 14),
    ("find a number to cite", 12)];
const CONTEXTS: [(&str, u32); 4] = [("phone, thumb-typing", 30), ("phone, voice search", 8), ("desktop search box", 42), ("literature database", 20)];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Persona {
    pub id: String,
    pub role: String,
    pub group: String,
    pub age: String,
    pub gender: String,
    /// "native" | "fluent L2" | "limited L2"
    pub english: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub first_language: String,
    /// "low" | "medium" | "high"
    pub literacy: String,
    /// 0 none, 1 some, 2 professional, 3 specialist
    pub expertise: u8,
    pub goal: String,
    pub context: String,
    pub terse: bool,
    pub lowercase: bool,
    pub no_punct: bool,
    /// 0 none, 1 low, 2 high
    pub typos: u8,
    pub abbrev: bool,
}

fn u(seed: &str, i: usize, attr: &str) -> u64 {
    u64::from_str_radix(&hash(&[seed, &i.to_string(), attr])[..12], 16).unwrap_or(0)
}

pub fn pick<'a>(x: u64, items: &[(&'a str, u32)]) -> &'a str {
    let tot: u32 = items.iter().map(|(_, w)| *w).sum();
    let mut r = (x % tot.max(1) as u64) as u32;
    for (v, w) in items {
        if r < *w {
            return v;
        }
        r -= w;
    }
    items[items.len() - 1].0
}

fn chance(x: u64, p: f64) -> bool {
    (x % 1_000_000) as f64 / 1e6 < p
}

/// Persona `i` of library `seed`.
pub fn persona(seed: &str, i: usize) -> Persona {
    let group = pick(u(seed, i, "group"), &GROUPS);
    let roles: Vec<(&str, u32)> = ROLES.iter().filter(|r| r.1 == group).map(|r| (r.0, r.4)).collect();
    let role = pick(u(seed, i, "role"), &roles);
    let &(_, _, lo, hi, _) = ROLES.iter().find(|r| r.0 == role).unwrap();
    let expertise = lo + (u(seed, i, "exp") % (hi - lo + 1) as u64) as u8;
    let age = match role {
        "high-school student" => "16-19",
        "undergraduate biology student" | "medical student" | "nursing student" | "pharmacy student" | "PhD student" =>
            pick(u(seed, i, "age"), &[("20-29", 85), ("30-44", 15)]),
        r if group == "lay" && r.contains("elderly parent") => pick(u(seed, i, "age"), &[("30-44", 30), ("45-64", 70)]),
        "pregnant person" => pick(u(seed, i, "age"), &[("20-29", 45), ("30-44", 55)]),
        _ if group == "lay" => pick(u(seed, i, "age"), &AGES),
        _ => pick(u(seed, i, "age"), &[("20-29", 12), ("30-44", 45), ("45-64", 35), ("65+", 8)]),
    };
    let gender = if role == "pregnant person" { "woman" } else { pick(u(seed, i, "gender"), &GENDERS) };
    let english = if group == "lay" {
        pick(u(seed, i, "en"), &[("native", 62), ("fluent L2", 20), ("limited L2", 18)])
    } else {
        pick(u(seed, i, "en"), &[("native", 62), ("fluent L2", 31), ("limited L2", 7)])
    };
    let first_language = if english == "native" { String::new() } else { pick(u(seed, i, "l1"), &L1).to_string() };
    let literacy = match (group, expertise) {
        ("lay", _) => pick(u(seed, i, "lit"), &[("low", 35), ("medium", 45), ("high", 20)]),
        ("student", 0 | 1) => pick(u(seed, i, "lit"), &[("medium", 60), ("high", 40)]),
        (_, 0 | 1) => pick(u(seed, i, "lit"), &[("medium", 50), ("high", 50)]),
        _ => "high",
    };
    let goal = match group {
        "lay" => pick(u(seed, i, "goal"), &[("decide what to do for a patient or for themself", 45), ("understand how something works", 35),
            ("check whether a claim is true", 20)]),
        "clinician" => pick(u(seed, i, "goal"), &[("decide what to do for a patient", 40), ("understand how something works", 15),
            ("check whether a claim is true", 20), ("find a method or protocol", 10), ("find a number to cite", 15)]),
        "researcher" => pick(u(seed, i, "goal"), &[("understand how something works", 25), ("check whether a claim is true", 10),
            ("find a method or protocol", 25), ("gather evidence for a review, paper or grant", 25), ("find a number to cite", 15)]),
        _ => pick(u(seed, i, "goal"), &GOALS),
    };
    let context = match group {
        "lay" => pick(u(seed, i, "ctx"), &[("phone, thumb-typing", 55), ("phone, voice search", 15), ("desktop search box", 30)]),
        "researcher" => pick(u(seed, i, "ctx"), &[("desktop search box", 45), ("literature database", 45), ("phone, thumb-typing", 10)]),
        _ => pick(u(seed, i, "ctx"), &CONTEXTS),
    };
    let phone = context.starts_with("phone, thumb");
    let terse = chance(u(seed, i, "terse"), if expertise >= 2 { 0.55 } else { 0.3 });
    let lowercase = chance(u(seed, i, "lower"), if phone { 0.6 } else { 0.3 });
    let no_punct = chance(u(seed, i, "punct"), if phone { 0.55 } else { 0.3 });
    let typo_p = match (phone, group == "lay" || english == "limited L2") { (true, true) => 0.75, (true, false) => 0.45, (false, true) => 0.4, _ => 0.18 };
    let typos = if chance(u(seed, i, "typo"), typo_p) { 1 + chance(u(seed, i, "typo2"), 0.3) as u8 } else { 0 };
    let abbrev = expertise >= 2 && chance(u(seed, i, "abbr"), 0.5);
    Persona { id: format!("{seed}-{i:05}"), role: role.into(), group: group.into(), age: age.into(), gender: gender.into(),
        english: english.into(), first_language, literacy: literacy.into(), expertise, goal: goal.into(), context: context.into(),
        terse, lowercase, no_punct, typos, abbrev }
}

pub fn library(seed: &str, n: usize) -> Vec<Persona> {
    (0..n).map(|i| persona(seed, i)).collect()
}

impl Persona {
    /// Short card shown to the surface writer. Typing habits (case, punctuation, typos) are applied afterwards in
    /// code (noise.rs), so the card only describes voice.
    pub fn card(&self) -> String {
        let exp = ["no background", "some background", "professional knowledge", "specialist knowledge"][self.expertise.min(3) as usize];
        let en = match self.english.as_str() {
            "native" => "native English speaker".to_string(),
            e => format!("{} English, first language {}", if e == "fluent L2" { "fluent" } else { "limited" }, self.first_language),
        };
        let style = match (self.terse, self.abbrev) {
            (true, true) => "terse, uses abbreviations",
            (true, false) => "terse",
            (false, true) => "full sentences, uses abbreviations",
            (false, false) => "writes in full sentences",
        };
        format!("{}, {}, age {}; {} in medicine/biology; {}; health literacy {}; wants to {}; searching on {}; {}",
            self.role, self.gender, self.age, exp, en, self.literacy, self.goal, self.context, style)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_mix_follows_targets() {
        assert_eq!(persona("t", 7), persona("t", 7));
        let lib = library("lib", 6000);
        let share = |g: &str| lib.iter().filter(|p| p.group == g).count() as f64 / lib.len() as f64;
        assert!((share("clinician") - 0.30).abs() < 0.03);
        assert!((share("lay") - 0.20).abs() < 0.03);
        let l2 = lib.iter().filter(|p| p.english != "native").count() as f64 / lib.len() as f64;
        assert!(l2 >= 0.15, "{l2}");
        for p in &lib {
            if p.role == "high-school student" { assert!(p.age == "16-19" && p.expertise <= 1); }
            if p.group == "clinician" { assert!(p.expertise >= 2 && p.literacy == "high"); }
            if p.english == "native" { assert!(p.first_language.is_empty()); } else { assert!(!p.first_language.is_empty()); }
        }
        assert!(lib[0].card().contains(&lib[0].role));
    }
}
