//! Config loading, rules and the figure arm on small fixtures. Parity with pigmed-synth v3.1d is checked end to end by
//! querygen/parity.sh (it needs the pigmed-synth binary and a sample).

use querygen::figure::{self, FigInput};
use querygen::rules::{Article, QueryCtx};
use querygen::text::style;
use querygen::Qg;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../querygen")
}

fn v31d() -> &'static Qg {
    static Q: OnceLock<Qg> = OnceLock::new();
    Q.get_or_init(|| Qg::load(&root().join("v3.1d.toml")).unwrap())
}

fn v32() -> &'static Qg {
    static Q: OnceLock<Qg> = OnceLock::new();
    Q.get_or_init(|| Qg::load(&root().join("querygen.toml")).unwrap())
}

#[test]
fn configs_load_and_extend() {
    let (a, b) = (v31d(), v32());
    assert_eq!(a.cfg.recipe, "v3.1d");
    assert_eq!(b.cfg.recipe, "v3.2");
    assert!(b.cfg.personas.roles.len() > a.cfg.personas.roles.len() + 50, "extra roles appended");
    assert_eq!(&b.cfg.personas.roles[..a.cfg.personas.roles.len()], &a.cfg.personas.roles[..], "parent roles kept in order");
    assert_eq!(a.lib.len(), 20_000);
    assert!(b.cfg.figure_on() && !a.cfg.figure_on());
    // inherited untouched: styles, filters, noise
    assert_eq!(a.cfg.styles.len(), b.cfg.styles.len());
    assert_eq!(a.cfg.filters.regex.doc_ref, b.cfg.filters.regex.doc_ref);
}

#[test]
fn persona_rules_hold_in_v32() {
    let q = v32();
    for p in &q.lib {
        let r = q.cfg.personas.roles.iter().find(|r| r.0 == p.role).unwrap();
        assert!(p.expertise >= r.2 && p.expertise <= r.3, "{p:?}");
        assert_eq!(p.english == "native", p.first_language.is_empty());
        if p.role == "teenager looking up symptoms" || p.role == "high-school student" { assert_eq!(p.age, "16-19"); }
        if p.role == "retired nurse" { assert_eq!(p.age, "65+"); }
        if p.role == "pregnant person" { assert_eq!(p.gender, "woman"); }
        if p.group == "lay" { assert!(p.goal != "find a number to cite"); }
    }
    assert_eq!(querygen::persona::persona(&q.cfg.personas, 7), q.lib[7]);
}

#[test]
fn reject_chain_text_and_figure() {
    let q = v32();
    let c = &q.cfg;
    let chain = &c.filters.query_reject;
    let fchain = &c.filters.figure.as_ref().unwrap().query_reject;
    let fs = &c.figure.as_ref().unwrap().styles;
    let kw = QueryCtx { need: "Whether colchicine prevents recurrent lacunar stroke", style: style(&c.styles, "keyword"), caption: None };
    let nq = QueryCtx { need: "Whether colchicine prevents recurrent lacunar stroke", style: style(&c.styles, "natural_question"), caption: None };
    assert_eq!(q.rules.reject(c, chain, "colchicine lacunar stroke", &kw), "");
    assert_eq!(q.rules.reject(c, chain, "does colchicine help after a lacunar stroke?", &kw), "style");
    assert_eq!(q.rules.reject(c, chain, "what did this study find on colchicine", &nq), "banned");
    assert_eq!(q.rules.reject(c, chain, "find the figure showing colchicine effects", &nq), "banned");
    assert_eq!(q.rules.reject(c, chain, "Whether colchicine prevents recurrent lacunar stroke", &nq), "copy");
    // figure arm: "find the figure showing" is a real query, figure numbers are not
    let cap = "Kaplan-Meier curves of overall survival in patients with melanoma treated with nivolumab plus ipilimumab or nivolumab alone";
    let ff = QueryCtx { need: "survival of melanoma patients on combination immunotherapy", style: style(fs, "find_figure"), caption: Some(cap) };
    assert_eq!(q.rules.reject(c, fchain, "find the figure showing survival curves for nivolumab plus ipilimumab in melanoma", &ff), "");
    assert_eq!(q.rules.reject(c, fchain, "figure 2 survival nivolumab melanoma", &ff), "banned");
    assert_eq!(q.rules.reject(c, fchain, "kaplan meier curves overall survival patients melanoma treated nivolumab plus ipilimumab or nivolumab alone", &ff), "caption_copy");
    // composed queries point at the image: exempt from `unresolved`, still banned for doc references
    let co = QueryCtx { need: "why the curves separate after a year", style: style(fs, "composed"), caption: Some(cap) };
    assert_eq!(q.rules.reject(c, fchain, "why do these curves only separate after a year", &co), "");
    assert_eq!(q.rules.reject(c, fchain, "what does the arrow point to in these stained cells", &co), "");
    assert_eq!(q.rules.reject(c, fchain, "why did they stop the drug in this cohort", &co), "banned");
    assert_eq!(q.rules.reject(c, fchain, "what did the authors conclude from this curve", &co), "banned");
    let nq_fig = QueryCtx { need: "x", style: style(fs, "natural_question"), caption: Some(cap) };
    assert_eq!(q.rules.reject(c, fchain, "how did they measure survival", &nq_fig), "unresolved");
}

#[test]
fn constraints_are_true_of_the_article() {
    let q = v31d();
    let art = Article { article_id: "a".into(), title: "Colchicine in children with pericarditis: a randomized trial".into(), year: Some(2021),
        article_type: "research-article".into(), pub_types: vec!["Randomized Controlled Trial".into()], ..Default::default() };
    let c = q.rules.constraints(&q.cfg, &art);
    assert_eq!(c, vec!["only RCTs", "randomized trials only", "original research, not reviews", "primary studies only", "since 2021", "since 2020",
        "since 2018", "published after 2019", "in children", "human studies"]);
    let rev = Article { title: "Statins in mice: a systematic review".into(), article_type: "review-article".into(),
        pub_types: vec!["Systematic Review".into(), "Review".into()], ..Default::default() };
    assert_eq!(q.rules.constraints(&q.cfg, &rev), vec!["systematic reviews or meta-analyses only", "mouse studies"]);
}

fn fig(id: &str, section: &str, citing: bool) -> FigInput {
    FigInput { chunk_id: id.into(), article_id: "A1".into(), title: "t".into(), section: section.into(), text: "caption words".into(),
        figure_id: "F1".into(), image: "x.jpg".into(), citing: vec!["p".into()], citing_chunk: citing.then(|| "u1".to_string()), ..Default::default() }
}

#[test]
fn figure_plan_respects_types_and_share() {
    let q = v32();
    let f = q.cfg.figure.as_ref().unwrap();
    let figs = [fig("f1", "chart", false), fig("f2", "microscopy", true), fig("f3", "diagram", true)];
    let refs: Vec<&FigInput> = figs.iter().collect();
    let cards = figure::plan_fig_cards(q, "A1", 21, &refs, 1.0);
    assert!(!cards.is_empty());
    for c in &cards {
        let fi = &figs[c.fig];
        if c.ctype == "composed" { assert!(fi.citing_chunk.is_some()); }
        if c.ctype == "appearance" { assert!(["radiology_clinical", "microscopy", "multipanel"].contains(&fi.section.as_str())); }
        assert_eq!(c.card.cands.len(), 3);
    }
    let per_fig = |i: usize| cards.iter().filter(|c| c.fig == i).count();
    assert!(per_fig(0) <= 2 && per_fig(1) <= f.max_cards_per_figure);
    // deterministic
    let again = figure::plan_fig_cards(q, "A1", 21, &refs, 1.0);
    assert_eq!(cards.iter().map(|c| (&c.ctype, c.fig)).collect::<Vec<_>>(), again.iter().map(|c| (&c.ctype, c.fig)).collect::<Vec<_>>());
    // 21 text cards x 2 slots at a 30% target -> ~18 figure slots -> ~10 cards
    assert_eq!(figure::wanted_fig_cards(q, 21, 1.0), 10);
    assert_eq!(figure::wanted_fig_cards(v31d(), 21, 1.0), 0);
}

#[test]
fn figure_schema_and_fill() {
    let q = v32();
    let figs = [fig("f2", "microscopy", true)];
    let refs: Vec<&FigInput> = figs.iter().collect();
    let mut cards = figure::plan_fig_cards(q, "A1", 21, &refs, 1.0);
    let sch = figure::schema_fig_a(q, &cards.iter().collect::<Vec<_>>());
    assert_eq!(sch["properties"]["cards"]["minItems"], json!(cards.len()));
    let reply = json!({"biomedical": true, "figure_kind": "histology_stain", "legible": true, "cards": cards.iter().map(|c| json!({
        "card": c.card.id, "reader": "none", "expert_role": "dermatopathologist", "need": "What fungal hyphae look like on PAS staining of a skin biopsy",
        "visible": "magenta branching hyphae in the dermis", "answer": "septate branching hyphae", "specificity": "few", "audience": ["clinician"],
        "min_expertise": "2", "entities": ["PAS stain", "hyphae"], "skip": false})).collect::<Vec<Value>>()});
    let mut m: Vec<&mut figure::FigCard> = cards.iter_mut().collect();
    assert_eq!(figure::fill_fig_cards(q, &mut m, &reply), "histology_stain");
    for c in &cards {
        assert!(c.card.fallback && c.card.persona.is_some() && !c.card.skip && c.card.expert_role == "dermatopathologist");
    }
    let prompt = figure::prompt_fig_a(q, &figs[0], &cards.iter().collect::<Vec<_>>());
    assert!(prompt.contains("FIGURE KINDS: kaplan_meier_curve") && prompt.contains("card type") && !prompt.contains("{{"));
}

#[test]
fn jats_citing_paragraphs_and_permissions() {
    let xml = r#"<article><body><p>Lesions grew (<xref ref-type="fig" rid="F1 F2">Figs 1-2</xref>).</p>
<p>Unrelated text.</p><p>See <xref ref-type="fig" rid="F2">Fig 2</xref> for staining.</p>
<fig id="F1"><caption><p>Lesion growth (<xref ref-type="fig" rid="F1">self</xref>).</p></caption></fig>
<fig id="F2"><caption><p>Staining.</p></caption><permissions><copyright-statement>(c) 2019 X</copyright-statement></permissions></fig></body></article>"#;
    let m = figure::jats_figs(xml, 2, 200);
    assert_eq!(m["F1"].0, vec!["Lesions grew (Figs 1-2)."]);
    assert!(!m["F1"].1);
    assert_eq!(m["F2"].0.len(), 2);
    assert!(m["F2"].1, "F2 carries a permissions block");
}

#[test]
fn fig_inputs_end_to_end_on_fixtures() {
    let q = v32();
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    // article list (one commercial, one non-commercial, one held out)
    let arts = [json!({"article_id": "PMC1.1", "title": "Melanoma survival", "text": "", "words": 0, "passages": [], "commercial": true, "license_code": "CC BY"}),
        json!({"article_id": "PMC2.1", "title": "x", "text": "", "words": 0, "passages": [], "commercial": false}),
        json!({"article_id": "PMC3.1", "title": "y", "text": "", "words": 0, "passages": [], "commercial": true})];
    std::fs::write(p.join("arts.jsonl"), arts.iter().map(|a| a.to_string() + "\n").collect::<String>()).unwrap();
    std::fs::write(p.join("exclude.txt"), "PMC3.1\n").unwrap();
    // items: two figures (one with a rights phrase), a text unit holding the citing paragraph, an abstract
    let items = p.join("items/run1");
    std::fs::create_dir_all(&items).unwrap();
    let rows = [json!({"article_id": "PMC1.1", "kind": "figure", "unit_id": "u-f1", "caption": "Kaplan-Meier curves of overall survival in melanoma by treatment arm over five years",
            "source_url": "s3://pmc-oa-opendata/PMC1.1/f1.png", "locator": {"figure_id": "F1"}}),
        json!({"article_id": "PMC1.1", "kind": "figure", "unit_id": "u-f2", "caption": "Survival plot reproduced with permission from Smith et al. 2010 cohort study",
            "source_url": "s3://pmc-oa-opendata/PMC1.1/f2.png", "locator": {"figure_id": "F2"}}),
        json!({"article_id": "PMC1.1", "kind": "text", "unit_id": "u-t1", "text": "Overall survival was longer with the combination (Figure 1) than with monotherapy.", "locator": {"section": ["Results"]}}),
        json!({"article_id": "PMC1.1", "kind": "text", "unit_id": "u-ab", "text": "Abstract text.", "locator": {"section": ["Abstract"]}})];
    let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(items.join("items.jsonl.gz")).unwrap(), flate2::Compression::fast());
    for r in rows { writeln!(gz, "{r}").unwrap(); }
    gz.finish().unwrap();
    let xml = r#"<article><body><p>Overall survival was longer with the combination (<xref ref-type="fig" rid="F1">Figure 1</xref>) than with monotherapy.</p></body></article>"#;
    std::fs::create_dir_all(p.join("xml")).unwrap();
    let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(p.join("xml/PMC1.1.xml.gz")).unwrap(), flate2::Compression::fast());
    gz.write_all(xml.as_bytes()).unwrap();
    gz.finish().unwrap();
    // local image mirror: a 3000x1500 PNG, must come out <= image_max_side
    std::fs::create_dir_all(p.join("mirror/PMC1.1")).unwrap();
    image::RgbImage::from_pixel(3000, 1500, image::Rgb([200, 30, 30])).save(p.join("mirror/PMC1.1/f1.png")).unwrap();
    let out = p.join("out");
    let (rows, stats) = figure::build_inputs(q, &figure::FigInputArgs { articles: &p.join("arts.jsonl"), items_root: &p.join("items"),
        xml_cache: &p.join("xml"), manifest: None, exclude: Some(&p.join("exclude.txt")), image_ids: None, local_images: Some(&p.join("mirror")), out: &out }).unwrap();
    assert_eq!(rows.len(), 1, "{stats}");
    let r = &rows[0];
    assert_eq!((r.chunk_id.as_str(), r.section.as_str(), r.citing_chunk.as_deref()), ("u-f1", "chart", Some("u-t1")));
    assert_eq!(r.abstract_text, "Abstract text.");
    let img = image::open(&r.image).unwrap();
    assert_eq!((img.width(), img.height()), (1280, 640));
    assert_eq!(stats["dropped"]["fig_rights_phrase"], json!(1));
    assert_eq!(stats["dropped"]["article_noncommercial"], json!(1));
    assert_eq!(stats["dropped"]["article_held_out"], json!(1));
}
