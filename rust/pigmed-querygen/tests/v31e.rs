//! v3.1e: MeSH / journal domain rule, lay pass, field -> role fallback without a generic expert; and the config chain
//! v3.1d <- fig <- v3.1e <- querygen (v3.2).

use querygen::rules::{biomedical, field_role, patient_relevant, Article, ArticleMeta};
use querygen::text::{card_drop, choose_reader, plan_cards_meta, Card, SampleChunk};
use querygen::Qg;
use serde_json::json;
use std::path::PathBuf;
use std::sync::OnceLock;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../querygen")
}

fn q(name: &str) -> Qg {
    Qg::load(&root().join(name)).unwrap()
}

fn e() -> &'static Qg {
    static Q: OnceLock<Qg> = OnceLock::new();
    Q.get_or_init(|| q("v3.1e.toml"))
}

fn meta(trees: &[&str], journal: &str, pts: &[&str]) -> ArticleMeta {
    ArticleMeta { article_id: "A".into(), mesh_trees: trees.iter().map(|s| s.to_string()).collect(), journal: journal.into(),
        pub_types: pts.iter().map(|s| s.to_string()).collect(), ..Default::default() }
}

#[test]
fn config_chain() {
    let (d, f, v, g) = (q("v3.1d.toml"), q("fig.toml"), e(), q("querygen.toml"));
    assert_eq!((d.cfg.recipe.as_str(), f.cfg.recipe.as_str(), v.cfg.recipe.as_str(), g.cfg.recipe.as_str()), ("v3.1d", "v3.1d-fig", "v3.1e", "v3.2"));
    assert!(!d.cfg.figure_on() && f.cfg.figure_on() && v.cfg.figure_on() && g.cfg.figure_on());
    // the grid recipe keeps v3.1d text (library, styles, generic fallback) but figure cards drop the generic expert
    assert_eq!(d.lib, f.lib);
    assert_eq!((d.cfg.fallback_mode(), f.cfg.fallback_mode(), f.cfg.figure_fallback_mode()), ("generic", "generic", "role_or_map"));
    assert_eq!((v.cfg.fallback_mode(), v.cfg.figure_fallback_mode()), ("role_or_map", "role_or_map"));
    assert!(v.cfg.domain.is_some() && v.cfg.lay.is_some() && f.cfg.domain.is_none());
    assert_eq!(g.cfg.lay.as_ref().unwrap().target_share, 0.12);
    assert!(g.cfg.personas.roles.len() > v.cfg.personas.roles.len());
}

#[test]
fn domain_rule() {
    let c = &e().cfg;
    assert!(biomedical(c, Some(&meta(&["C04.557.470"], "", &[])), false), "MeSH overrides a wrong LLM flag");
    assert!(!biomedical(c, Some(&meta(&["J01.897.280", "H01.181"], "", &[])), true), "chemistry/materials MeSH: not biomedical");
    assert!(!biomedical(c, Some(&meta(&["E05.196"], "", &[])), true), "E alone is not enough");
    assert!(biomedical(c, Some(&meta(&["E05.196", "J01.897"], "", &[])), false), "E with another tree");
    assert!(biomedical(c, Some(&meta(&["G05.360"], "", &[])), false) && !biomedical(c, Some(&meta(&["G01.374"], "", &[])), true));
    assert!(biomedical(c, None, true) && !biomedical(c, None, false), "no MeSH: LLM flag (journal list empty here)");
    assert!(biomedical(&q("v3.1d.toml").cfg, None, true), "no [domain]: the LLM flag");
}

#[test]
fn lay_pass_and_field_roles() {
    let qg = e();
    let c = &qg.cfg;
    let art = Article { article_id: "PMC9.1".into(), title: "t".into(), pub_types: vec!["Randomized Controlled Trial".into()], ..Default::default() };
    assert!(patient_relevant(c, Some(&meta(&["C04.557"], "", &[])), &art));
    assert!(!patient_relevant(c, Some(&meta(&["C04.557"], "", &["Journal Article"])), &art), "meta pub types win");
    assert!(patient_relevant(c, Some(&meta(&["N02.421"], "", &["Journal Article"])), &art));
    assert!(!patient_relevant(c, None, &art), "no MeSH: no lay pass");
    let chunks: Vec<SampleChunk> = (1..=6).map(|k| SampleChunk { chunk_id: format!("u{k}"), article_id: "PMC9.1".into(), marker: format!("P{k}"), ..Default::default() }).collect();
    let refs: Vec<&SampleChunk> = chunks.iter().collect();
    let cards = plan_cards_meta(qg, &art, &refs, Some(&meta(&["C04.557"], "", &[])));
    let lay: Vec<&Card> = cards.iter().filter(|c| c.lay).collect();
    assert_eq!(lay.len(), 5, "cards_per_article = 5 lay cards, on distinct passages");
    assert_eq!(lay.iter().map(|c| &c.marker).collect::<std::collections::HashSet<_>>().len(), 5);
    assert!(lay.iter().all(|c| c.cands.iter().all(|&p| qg.lib[p].group == "lay") && c.cands.len() == 3));
    assert_eq!(plan_cards_meta(qg, &art, &refs[..2], Some(&meta(&["C04.557"], "", &[]))).iter().filter(|c| c.lay).count(), 2, "at most one per passage");
    assert_eq!(plan_cards_meta(qg, &art, &refs, None).iter().filter(|c| c.lay).count(), 0);
    // field -> role: journal first, then the longest MeSH prefix
    assert_eq!(field_role(c, Some(&meta(&["C04.557"], "Journal of Veterinary Internal Medicine", &[]))).as_deref(), Some("veterinarian"));
    assert_eq!(field_role(c, Some(&meta(&["B01.650.940", "B01.050"], "", &[]))).as_deref(), Some("plant scientist"));
    assert_eq!(field_role(c, Some(&meta(&["J01.897"], "Materials Today", &[]))), None);
}

#[test]
fn no_generic_expert_fallback() {
    let qg = e();
    let mut c = Card { id: "P1.c1".into(), cands: vec![1, 2, 3], ..Default::default() };
    // vague stage-A role, no field map -> dropped as no_reader
    choose_reader(qg, &mut c, &json!({"reader": "none", "expert_role": "Researcher"}), "role_or_map", None);
    assert!(c.no_reader && c.persona.is_none());
    assert_eq!(card_drop(qg, &Card { need: "x y z w v".into(), ..c.clone() }), "no_reader");
    // vague role but MeSH says oncology -> oncologist
    let mut c2 = Card { id: "P1.c2".into(), cands: vec![1, 2, 3], ..Default::default() };
    choose_reader(qg, &mut c2, &json!({"reader": "none", "expert_role": "scientist"}), "role_or_map", Some(&meta(&["C04.557"], "", &[])));
    assert!(c2.fallback && c2.expert_role == "oncologist" && c2.persona.is_some());
    let who = querygen::text::who(qg, true, &c2.expert_role, c2.persona.unwrap(), false);
    assert!(who.starts_with("oncologist,") && !who.contains("own field"));
    // concrete stage-A role is kept
    let mut c3 = Card { id: "P1.c3".into(), cands: vec![1, 2, 3], ..Default::default() };
    choose_reader(qg, &mut c3, &json!({"reader": "none", "expert_role": "agronomist"}), "role_or_map", None);
    assert_eq!(c3.expert_role, "agronomist");
    // lay card with no fitting reader -> skip
    let mut c4 = Card { id: "P1.l".into(), cands: vec![1, 2, 3], lay: true, ..Default::default() };
    choose_reader(qg, &mut c4, &json!({"reader": "none", "expert_role": ""}), "role_or_map", None);
    assert!(c4.skip);
}

#[test]
fn article_meta_rows_from_article_mesh() {
    // the pigmed-ctx `article-mesh` format: pmid present, nlm_id null for now, journal / pub_types possibly null
    let row = r#"{"article_id":"PMC1.1","pmid":"123","mesh_trees":["C04.557"],"journal":"Lancet Oncology","nlm_id":null,"pub_types":["Randomized Controlled Trial"],"retired_trees":2,"future_field":1}"#;
    let m: ArticleMeta = serde_json::from_str(row).unwrap();
    assert_eq!((m.pmid.as_str(), m.nlm_id.as_str(), m.mesh_trees.len(), m.retired_trees), ("123", "", 1, 2));
    let bare: ArticleMeta = serde_json::from_str(r#"{"article_id":"PMC2.1","pmid":null,"mesh_trees":null,"journal":null,"nlm_id":null,"pub_types":null}"#).unwrap();
    assert!(bare.mesh_trees.is_empty() && bare.journal.is_empty());
    let art = Article { article_id: "PMC1.1".into(), title: "t".into(), ..Default::default() };
    assert!(patient_relevant(&e().cfg, Some(&m), &art));
    assert!(biomedical(&e().cfg, Some(&bare), true), "no MeSH: falls back to the LLM flag");
}
