//! Persona library from the config: a deterministic attribute sampler with compatibility rules (first matching rule
//! per attribute), rendered to a short card. Persona `i` of seed `s` is always the same; with querygen/v3.1d.toml the
//! library is byte-identical to pigmed-synth's v3 library (tests/parity.rs).

use crate::config::{AttrRule, HabitRule, Personas, Table, When};
use crate::util::{chance, fill, hash, pick};
use serde::{Deserialize, Serialize};

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

/// Does `w` hold for the (partly drawn) persona `p`?
pub fn holds(w: &When, p: &Persona) -> bool {
    (w.roles.is_empty() || w.roles.iter().any(|r| *r == p.role))
        && (w.groups.is_empty() || w.groups.iter().any(|g| *g == p.group))
        && (w.role_contains.is_empty() || p.role.contains(&w.role_contains))
        && (w.expertise.is_empty() || w.expertise.contains(&p.expertise))
        && (w.english.is_empty() || w.english.iter().any(|e| *e == p.english))
        && (w.contexts.is_empty() || w.contexts.iter().any(|c| *c == p.context))
        && (w.literacy.is_empty() || w.literacy.iter().any(|c| *c == p.literacy))
        && (w.any.is_empty() || w.any.iter().any(|x| holds(x, p)))
}

fn attr(cfg: &Personas, rules: &[AttrRule], p: &Persona, x: u64) -> String {
    for r in rules {
        if !holds(&r.when, p) {
            continue;
        }
        if let Some(f) = &r.fixed {
            return f.clone();
        }
        return match &r.table {
            Some(Table::Inline(t)) => pick(x, t).to_string(),
            Some(Table::Named(n)) if n == "l1" => pick(x, &cfg.l1).to_string(),
            _ => String::new(),
        };
    }
    String::new()
}

fn habit(rules: &[HabitRule], p: &Persona) -> f64 {
    rules.iter().find(|r| holds(&r.when, p)).map(|r| r.p).unwrap_or(0.0)
}

/// Persona `i` of the library.
pub fn persona(cfg: &Personas, i: usize) -> Persona {
    let seed = cfg.seed.as_str();
    let group = pick(u(seed, i, "group"), &cfg.groups).to_string();
    let roles: Vec<(String, u32)> = cfg.roles.iter().filter(|r| r.1 == group).map(|r| (r.0.clone(), r.4)).collect();
    let role = pick(u(seed, i, "role"), &roles).to_string();
    let r = cfg.roles.iter().find(|r| r.0 == role).unwrap();
    let expertise = r.2 + (u(seed, i, "exp") % (r.3 - r.2 + 1) as u64) as u8;
    let mut p = Persona { id: format!("{seed}-{i:05}"), role, group, expertise, ..Default::default() };
    let a = &cfg.attrs;
    p.age = attr(cfg, &a.age, &p, u(seed, i, "age"));
    p.gender = attr(cfg, &a.gender, &p, u(seed, i, "gender"));
    p.english = attr(cfg, &a.english, &p, u(seed, i, "en"));
    p.first_language = attr(cfg, &a.first_language, &p, u(seed, i, "l1"));
    p.literacy = attr(cfg, &a.literacy, &p, u(seed, i, "lit"));
    p.goal = attr(cfg, &a.goal, &p, u(seed, i, "goal"));
    p.context = attr(cfg, &a.context, &p, u(seed, i, "ctx"));
    let h = &cfg.habits;
    p.terse = chance(u(seed, i, "terse"), habit(&h.terse, &p));
    p.lowercase = chance(u(seed, i, "lower"), habit(&h.lowercase, &p));
    p.no_punct = chance(u(seed, i, "punct"), habit(&h.no_punct, &p));
    p.typos = if chance(u(seed, i, "typo"), habit(&h.typos, &p)) { 1 + chance(u(seed, i, "typo2"), cfg.habits_extra.typo_second) as u8 } else { 0 };
    // abbrev rules with p = 0 never fire, as in v3.1d (`expertise >= 2 && chance(.., 0.5)`)
    p.abbrev = chance(u(seed, i, "abbr"), habit(&h.abbrev, &p));
    p
}

pub fn library(cfg: &Personas) -> Vec<Persona> {
    (0..cfg.size).map(|i| persona(cfg, i)).collect()
}

/// Short card shown to the writers. Typing habits are applied afterwards in code (noise.rs), so the card only
/// describes voice.
pub fn card(cfg: &Personas, p: &Persona) -> String {
    let c = &cfg.card;
    let exp = &c.expertise[(p.expertise as usize).min(c.expertise.len() - 1)];
    let en = if p.english == "native" {
        c.english_native.clone()
    } else {
        let level = c.l2_level.get(&p.english).map(String::as_str).unwrap_or("limited");
        fill(&c.english_l2, &[("level", level), ("l1", &p.first_language)])
    };
    let key = match (p.terse, p.abbrev) { (true, true) => "terse_abbrev", (true, false) => "terse", (false, true) => "abbrev", (false, false) => "plain" };
    let style = c.style.get(key).map(String::as_str).unwrap_or("");
    fill(&c.template, &[("role", &p.role), ("gender", &p.gender), ("age", &p.age), ("expertise", exp), ("english", &en),
        ("literacy", &p.literacy), ("goal", &p.goal), ("context", &p.context), ("style", style)])
}
