//! querygen.toml: the single source of truth for the generator (personas, styles, intents, modality targets,
//! templates, filters, gates). `extends` merges a parent file: tables key by key, any other value (arrays included)
//! replaced; `personas.extra_roles` is appended to the parent's roles. Templates are files next to the config.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub type Weighted = Vec<(String, u32)>;

/// AND of the given fields; `any` = OR of nested predicates. Empty = always true.
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct When {
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub role_contains: String,
    #[serde(default)]
    pub expertise: Vec<u8>,
    #[serde(default)]
    pub english: Vec<String>,
    #[serde(default)]
    pub contexts: Vec<String>,
    #[serde(default)]
    pub literacy: Vec<String>,
    #[serde(default)]
    pub any: Vec<When>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum Table {
    /// name of a list in [personas] (e.g. "l1")
    Named(String),
    Inline(Weighted),
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct AttrRule {
    #[serde(default)]
    pub when: When,
    pub fixed: Option<String>,
    pub table: Option<Table>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct HabitRule {
    #[serde(default)]
    pub when: When,
    pub p: f64,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Attrs {
    pub age: Vec<AttrRule>,
    pub gender: Vec<AttrRule>,
    pub english: Vec<AttrRule>,
    pub first_language: Vec<AttrRule>,
    pub literacy: Vec<AttrRule>,
    pub goal: Vec<AttrRule>,
    pub context: Vec<AttrRule>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Habits {
    pub terse: Vec<HabitRule>,
    pub lowercase: Vec<HabitRule>,
    pub no_punct: Vec<HabitRule>,
    pub typos: Vec<HabitRule>,
    pub abbrev: Vec<HabitRule>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct HabitsExtra {
    pub typo_second: f64,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CardText {
    pub template: String,
    pub expertise: Vec<String>,
    pub english_native: String,
    pub english_l2: String,
    pub l2_level: BTreeMap<String, String>,
    pub style: BTreeMap<String, String>,
}

/// (role, group, min expertise, max expertise, weight within group)
pub type Role = (String, String, u8, u8, u32);

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Personas {
    pub seed: String,
    pub size: usize,
    pub groups: Weighted,
    pub roles: Vec<Role>,
    #[serde(default)]
    pub extra_roles: Vec<Role>,
    pub l1: Weighted,
    pub attrs: Attrs,
    pub habits: Habits,
    pub habits_extra: HabitsExtra,
    pub card: CardText,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub cli_max_tokens_multiplier: u32,
    pub per_card: u32,
    pub overhead: u32,
    pub cap: u32,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct TextTemplates {
    pub system_a: String,
    pub system_b: String,
    pub stage_a: String,
    pub stage_b: String,
    pub stage_b_block: String,
    pub need_rules: String,
    pub type_defs: String,
    pub reader_rules: String,
    pub passage_rule: String,
    pub card_line: String,
    pub health_line: String,
    pub reader: String,
    pub slot_line: String,
    pub hint: String,
    pub no_persona: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Text {
    pub slots_per_passage: usize,
    pub candidate_strata: Vec<Vec<String>>,
    pub reader_labels: Vec<String>,
    pub health_card: bool,
    pub health_strata: usize,
    pub styles_per_pair: usize,
    pub min_need_words: usize,
    pub expert_role_max_chars: usize,
    pub fallback_role: String,
    pub fallback_generic_roles: Vec<String>,
    pub stretch_suffix: String,
    pub intent_types: Vec<String>,
    pub audience: Vec<String>,
    pub budget: Budget,
    pub templates: TextTemplates,
}

/// OR of clauses; a clause is an AND of the given fields.
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Clause {
    pub min_expertise: Option<u8>,
    pub max_expertise: Option<u8>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub english_not: Vec<String>,
    #[serde(default)]
    pub literacy_not: Vec<String>,
    pub stretch: Option<bool>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct WeightMult {
    pub when: When,
    pub mult: u32,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Style {
    pub name: String,
    pub weight: u32,
    pub register: String,
    pub desc: String,
    #[serde(default)]
    pub allowed: Vec<Clause>,
    #[serde(default)]
    pub weight_mult: Vec<WeightMult>,
    pub max_words: Option<usize>,
    #[serde(default)]
    pub no_question_mark: bool,
    #[serde(default)]
    pub requires_constraint: bool,
    /// hedges | scenes | l2 | constraint | kind_forms
    #[serde(default)]
    pub hints: String,
    /// reject checks this style is exempt from (e.g. composed queries point at the image: "unresolved")
    #[serde(default)]
    pub skip_checks: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct L2Slips {
    pub l1: Vec<String>,
    pub slips: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Hints {
    pub constraint: String,
    pub l2: String,
    pub l2_unknown_speaker: String,
    pub hedges: Vec<String>,
    pub scenes: Vec<String>,
    pub lay_scenes: Vec<String>,
    pub lay_scene_groups: Vec<String>,
    pub l2_slips: Vec<L2Slips>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct MetaRule {
    #[serde(default)]
    pub exclusive: String,
    #[serde(default)]
    pub pub_types_any: Vec<String>,
    #[serde(default)]
    pub article_types_any: Vec<String>,
    #[serde(default)]
    pub article_type_is: String,
    #[serde(default)]
    pub pub_types_none: Vec<String>,
    pub add: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct YearRule {
    pub min: i32,
    pub max: i32,
    pub forms: Vec<(String, i32)>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct TitleRule {
    pub name: String,
    pub regex: String,
    pub add: String,
    #[serde(default)]
    pub also_if: Vec<String>,
    #[serde(default)]
    pub unless: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Constraints {
    pub meta: Vec<MetaRule>,
    pub year: YearRule,
    pub title: Vec<TitleRule>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct FilterRegex {
    pub leak: String,
    pub doc_ref: String,
    pub doc_ref_group: String,
    pub exam: String,
    pub tex: String,
    pub research_agenda: String,
    pub local_label_part: String,
    pub local_label_code: String,
    pub unresolved_they: String,
    pub unresolved_this: String,
    pub unresolved_the: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct FilterWords {
    pub doc_ref_group_ok: Vec<String>,
    pub unresolved_bare_heads: Vec<String>,
    pub unresolved_generic_mods: Vec<String>,
    pub unresolved_back_mods: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct FigureFilters {
    pub query_reject: Vec<String>,
    pub doc_ref: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    pub card_drop: Vec<String>,
    pub generic_specificity: Vec<String>,
    pub query_reject: Vec<String>,
    pub min_words: usize,
    pub copy_jaccard: f64,
    pub spill_chars: Vec<String>,
    pub regex: FilterRegex,
    pub words: FilterWords,
    pub figure: Option<FigureFilters>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Noise {
    pub keyboard_rows: Vec<String>,
    pub misspellings: Vec<(String, String)>,
    pub punct_removed: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub name: String,
    pub share: f64,
    pub query: String,
    pub status: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Modality {
    #[serde(default = "one")]
    pub kept_ratio: f64,
    pub targets: Vec<Target>,
}

fn one() -> f64 {
    1.0
}

impl Modality {
    pub fn share(&self, name: &str) -> f64 {
        self.targets.iter().find(|t| t.name == name).map(|t| t.share).unwrap_or(0.0)
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct GateRule {
    pub modality: String,
    pub styles: Vec<String>,
    pub keep: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Gates {
    pub eg2_query_prefix: String,
    pub rules: Vec<GateRule>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CaptionType {
    pub name: String,
    pub regex: Option<String>,
    pub min_panels: Option<usize>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CardType {
    pub name: String,
    pub weight: u32,
    pub target: String,
    #[serde(default = "text_q")]
    pub query_modality: String,
    pub desc: String,
    /// caption types this card type is planned for (empty = all)
    #[serde(default)]
    pub sections: Vec<String>,
    #[serde(default)]
    pub requires_citing_chunk: bool,
    /// fixed style list (one slot per style); empty = drawn from figure.styles
    #[serde(default)]
    pub styles: Vec<String>,
}

fn one_usize() -> usize {
    1
}

fn text_q() -> String {
    "text".into()
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct FigTemplates {
    pub system_a: String,
    pub stage_a: String,
    pub need_rules: String,
    pub card_defs: String,
    pub stage_b: String,
    pub stage_b_block: String,
    pub card_line: String,
    pub none_text: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Figure {
    /// reader fallback for figure cards ("" = the text mode)
    #[serde(default)]
    pub fallback_mode: String,
    pub max_figures_per_article: usize,
    pub max_cards_per_figure: usize,
    pub styles_per_pair: usize,
    pub image_max_side: u32,
    pub citing_paragraphs: usize,
    pub citing_max_words: usize,
    pub abstract_max_words: usize,
    pub caption_max_words: usize,
    pub min_caption_words: usize,
    pub caption_copy_jaccard: f64,
    pub caption_types: Vec<CaptionType>,
    pub panel_regex: String,
    pub rights_regex: String,
    pub kinds: Vec<(String, String)>,
    pub card_types: Vec<CardType>,
    pub templates: FigTemplates,
    pub styles: Vec<Style>,
    pub kind_forms: BTreeMap<String, Vec<String>>,
}

/// v3.1e: MeSH / journal domain rule (the LLM flag only without MeSH).
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Domain {
    pub trees_any: Vec<String>,
    pub trees_with_other: Vec<String>,
    pub journal_list: String,
    pub no_mesh: String,
}

/// v3.1e: lay-only card on patient-relevant articles.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Lay {
    pub enabled: bool,
    pub patient_trees: Vec<String>,
    pub clinical_pub_types: Vec<String>,
    pub trees_alone: Vec<String>,
    pub target_share: f64,
    pub target_min: f64,
    pub target_max: f64,
    pub article_rate: f64,
    /// lay cards per patient-relevant article, one per distinct passage (hash order), at most the article's passages
    #[serde(default = "one_usize")]
    pub cards_per_article: usize,
    pub candidates: usize,
    pub line: String,
}

/// Reader fallback when stage A picks no candidate: "generic" (v3.1d) | "role_or_map" (v3.1e).
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Fallback {
    pub mode: String,
    #[serde(default)]
    pub vague_roles: Vec<String>,
    #[serde(default)]
    pub mesh_roles: Vec<(String, String)>,
    #[serde(default)]
    pub journal_roles: Vec<(String, String)>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub extends: Option<String>,
    pub recipe: String,
    pub layouts: Vec<String>,
    pub personas: Personas,
    pub text: Text,
    pub styles: Vec<Style>,
    pub hints: Hints,
    pub constraints: Constraints,
    pub filters: Filters,
    pub noise: Noise,
    pub modality: Modality,
    pub gates: Gates,
    pub figure: Option<Figure>,
    pub domain: Option<Domain>,
    pub lay: Option<Lay>,
    pub fallback: Option<Fallback>,
    /// biomedical journal list (lowercase titles / NLM ids), from domain.journal_list
    #[serde(skip)]
    pub journals: std::collections::HashSet<String>,
    /// directory of the config file (templates resolve against it)
    #[serde(skip)]
    pub dir: PathBuf,
    /// loaded template files by name
    #[serde(skip)]
    pub files: BTreeMap<String, String>,
}

fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(bv) if bv.is_table() && v.is_table() => merge(bv, v),
                    _ => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

fn load_value(path: &Path, depth: usize) -> Result<toml::Value> {
    if depth > 4 {
        bail!("extends chain too deep at {}", path.display());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut v: toml::Value = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let Some(parent) = v.get("extends").and_then(|x| x.as_str()).map(str::to_string) else { return Ok(v) };
    let mut base = load_value(&path.parent().unwrap_or(Path::new(".")).join(&parent), depth + 1)?;
    // extra_roles append to the merged roles rather than replacing them
    let extra = v.get_mut("personas").and_then(|p| p.as_table_mut()).and_then(|p| p.remove("extra_roles"));
    v.as_table_mut().unwrap().remove("extends");
    merge(&mut base, v);
    if let Some(toml::Value::Array(extra)) = extra {
        if let Some(toml::Value::Array(roles)) = base.get_mut("personas").and_then(|p| p.get_mut("roles")) {
            roles.extend(extra);
        }
    }
    Ok(base)
}

/// Template text: the file as written, minus one trailing newline.
fn read_template(dir: &Path, name: &str) -> Result<String> {
    let t = std::fs::read_to_string(dir.join("templates").join(name)).with_context(|| format!("template {name}"))?;
    Ok(t.strip_suffix('\n').unwrap_or(&t).to_string())
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let v = load_value(path, 0)?;
        let mut c: Config = v.try_into().with_context(|| format!("config {}", path.display()))?;
        c.dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let t = &c.text.templates;
        let mut names = vec![t.system_a.clone(), t.system_b.clone(), t.stage_a.clone(), t.stage_b.clone(), t.stage_b_block.clone(),
            t.need_rules.clone(), t.type_defs.clone(), t.reader_rules.clone()];
        if let Some(f) = &c.figure {
            let ft = &f.templates;
            names.extend([ft.system_a.clone(), ft.stage_a.clone(), ft.need_rules.clone(), ft.card_defs.clone(), ft.stage_b.clone(),
                ft.stage_b_block.clone()]);
        }
        for n in names {
            let body = read_template(&c.dir, &n)?;
            c.files.insert(n, body);
        }
        if let Some(d) = &c.domain {
            if let Ok(t) = std::fs::read_to_string(c.dir.join(&d.journal_list)) {
                c.journals = t.lines().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
            }
        }
        c.validate()?;
        Ok(c)
    }

    pub fn file(&self, name: &str) -> &str {
        self.files.get(name).map(String::as_str).unwrap_or("")
    }

    pub fn fallback_mode(&self) -> &str {
        self.fallback.as_ref().map(|f| f.mode.as_str()).unwrap_or("generic")
    }

    pub fn figure_fallback_mode(&self) -> &str {
        match self.figure.as_ref().map(|f| f.fallback_mode.as_str()) { Some(m) if !m.is_empty() => m, _ => self.fallback_mode() }
    }

    pub fn figure_on(&self) -> bool {
        self.figure.is_some() && self.layouts.iter().any(|l| l == "c3fig")
    }

    fn validate(&self) -> Result<()> {
        let p = &self.personas;
        for (g, _) in &p.groups {
            if !p.roles.iter().any(|r| &r.1 == g) {
                bail!("group {g} has no roles");
            }
        }
        for r in &p.roles {
            if r.2 > r.3 || r.3 > 3 {
                bail!("role {}: bad expertise range", r.0);
            }
            if !p.groups.iter().any(|g| g.0 == r.1) {
                bail!("role {}: unknown group {}", r.0, r.1);
            }
        }
        let mut seen = std::collections::HashSet::new();
        for r in &p.roles {
            if !seen.insert(&r.0) {
                bail!("duplicate role {}", r.0);
            }
        }
        if self.text.reader_labels.len() != self.text.candidate_strata.len() {
            bail!("reader_labels and candidate_strata differ in length");
        }
        if let Some(l) = &self.lay {
            if l.candidates == 0 || l.candidates > self.text.reader_labels.len() {
                bail!("lay.candidates must be 1..={}", self.text.reader_labels.len());
            }
        }
        let tot: f64 = self.modality.targets.iter().map(|t| t.share).sum();
        if (tot - 1.0).abs() > 1e-6 {
            bail!("modality shares sum to {tot}, not 1");
        }
        if let Some(f) = &self.figure {
            for ct in &f.card_types {
                for s in &ct.styles {
                    if !f.styles.iter().any(|x| &x.name == s) {
                        bail!("card type {}: unknown style {s}", ct.name);
                    }
                }
            }
        }
        Ok(())
    }
}
