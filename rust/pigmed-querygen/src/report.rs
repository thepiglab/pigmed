//! Trial read-out: query files for the EG2 round trip (pigmed-synth `filter` format) and the results table
//! (quality vs throughput, GPU-hours for 2M kept) over a trial directory:
//!   T/gen/<arm>/{arm.json, <arm>.joint.json, <arm>-c3pf/, <arm>-c3fig/}
//!   T/filter/{text,fig}/{queries.jsonl, ranks.jsonl}        (pigmed-synth filter output)
//!   T/judge-<judge>/text/<arm>-c3pf.summary.json            (pigmed-synth judge3, source-free)
//!   T/judge-<judge>/fig/judge_fig.json                      (judge-fig: by_arm)

use crate::client::Generation;
use crate::util::{read_jsonl, write_jsonl};
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// QueryMeta rows (pigmed-synth filter input) + flashembed input for kept queries of one modality family:
/// "text" = text_passage targets, "figure" = figure targets (composed image+question queries are judged, not
/// round-tripped: the text-only EG2 query path cannot embed the image half).
pub fn queries(gen_dirs: &[&Path], modality: &str, prefix: &str, out: &Path) -> Result<Value> {
    let mut metas = vec![];
    for d in gen_dirs {
        for g in read_jsonl::<Generation>(&d.join("generations.jsonl"))?.into_iter().filter(|g| g.ok) {
            for (k, v) in &g.queries {
                let m = &g.meta["slots"][k];
                if m["modality"].as_str() != Some(if modality == "figure" { "figure" } else { "text_passage" }) {
                    continue;
                }
                let Some(q) = v.as_str() else { continue };
                metas.push(json!({"qid": format!("{}-{k}-{}", g.arm, g.chunk_id), "arm": g.arm, "qtype": m["style"], "chunk_id": g.chunk_id,
                    "article_id": g.article_id, "text": q.trim(), "task": m["task"]}));
            }
        }
    }
    let fe: Vec<Value> = metas.iter().map(|m| json!({"id": m["qid"], "text": format!("{prefix}{}", m["text"].as_str().unwrap_or(""))})).collect();
    write_jsonl(&out.join("queries.jsonl"), &metas)?;
    write_jsonl(&out.join("flashembed.jsonl"), &fe)?;
    Ok(json!({"queries": metas.len(), "modality": modality}))
}

fn load(p: &Path) -> Value {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

fn pct(x: f64) -> String {
    if x.is_nan() { "–".into() } else { format!("{:.0}%", x * 100.0) }
}

fn num(x: f64) -> String {
    if x.is_nan() { "–".into() } else if x >= 100.0 { format!("{:.0}", x) } else { format!("{:.2}", x) }
}

/// EG2 keep per query id: text styles with an expert gate need the source chunk in top-3, lay styles the source
/// article in top-10; figures the source figure in top-100 of the image corpus.
fn eg2_keep(trial: &Path, styles: &HashMap<String, String>) -> HashMap<String, bool> {
    const LAY: [&str; 3] = ["lay_paraphrase", "half_remembered", "non_native"];
    let mut out = HashMap::new();
    for (sub, fig) in [("text", false), ("fig", true)] {
        let Ok(rows) = read_jsonl::<Value>(&trial.join("filter").join(sub).join("ranks.jsonl")) else { continue };
        for r in rows {
            let qid = r["qid"].as_str().unwrap_or("").to_string();
            let (c, a) = (r["chunk_rank"].as_u64().unwrap_or(0), r["article_rank"].as_u64().unwrap_or(0));
            let style = styles.get(&qid).map(String::as_str).unwrap_or("");
            let keep = if fig { (1..=100).contains(&c) } else if LAY.contains(&style) { (1..=10).contains(&a) } else { (1..=3).contains(&c) };
            out.insert(qid, keep);
        }
    }
    out
}

pub fn report(trial: &Path, target_kept: f64) -> Result<(String, Value)> {
    let mut arms: Vec<String> = std::fs::read_dir(trial.join("gen"))?.filter_map(|e| e.ok()).filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string()).collect();
    arms.sort();
    // style per qid, for the gates
    let mut styles: HashMap<String, String> = HashMap::new();
    let mut gens_by_arm: BTreeMap<String, Vec<Generation>> = BTreeMap::new();
    for a in &arms {
        for lay in ["c3pf", "c3fig"] {
            let p = trial.join("gen").join(a).join(format!("{a}-{lay}")).join("generations.jsonl");
            let Ok(gs) = read_jsonl::<Generation>(&p) else { continue };
            for g in &gs {
                for (k, m) in g.meta["slots"].as_object().into_iter().flatten() {
                    styles.insert(format!("{}-{k}-{}", g.arm, g.chunk_id), m["style"].as_str().unwrap_or("").to_string());
                }
            }
            gens_by_arm.entry(a.clone()).or_default().extend(gs);
        }
    }
    let keep = eg2_keep(trial, &styles);
    // judges: every judge-* dir; an arm's score is the mean over judges that are not its own family
    let judges: Vec<std::path::PathBuf> = std::fs::read_dir(trial)?.filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("judge-"))).collect();
    let mut rows = vec![];
    let mut md = String::from("| arm | model | prec. | level | GPUs | articles | wall h | gen/GPU-h | kept/GPU-h | fig share | EG2 keep text | EG2 keep fig | final/GPU-h | GPU-h for 2M | standalone | generic | natural | style ok | fig rel=2 | needs img | caption para | lay share | L2 share | reasoning tok/call |\n|");
    md += &"---|".repeat(24);
    md += "\n";
    for a in &arms {
        let d = trial.join("gen").join(a);
        let meta = load(&d.join("arm.json"));
        let joint = load(&d.join(format!("{a}.joint.json")));
        let st = load(&d.join(format!("{a}-c3pf/summary.json")));
        let sf = load(&d.join(format!("{a}-c3fig/summary.json")));
        let gpus = f(&meta["gpus"]).max(f(&joint["gpus"])).max(1.0);
        // text imported from an earlier run of the same setting (v3.1d trial): its wall clock and calls add to this arm's
        let imported = meta["text"].as_str().is_some_and(|t| t.starts_with("import:"));
        let wall_h = (f(&joint["wall_s"]).max(0.0) + if imported { f(&st["wall_s"]).max(0.0) } else { 0.0 }) / 3600.0;
        let gen_slots = f(&st["slots"]).max(0.0) + f(&sf["slots"]).max(0.0);
        let gs = gens_by_arm.get(a).map(Vec::as_slice).unwrap_or(&[]);
        let mut kq: Vec<(&str, &Value, String)> = vec![];
        for g in gs {
            for (k, m) in g.meta["slots"].as_object().into_iter().flatten() {
                if m["rejected"] == json!(false) {
                    kq.push((m["modality"].as_str().unwrap_or(""), m, format!("{}-{k}-{}", g.arm, g.chunk_id)));
                }
            }
        }
        let rate = |modality: &str| {
            let ks: Vec<bool> = kq.iter().filter(|x| x.0 == modality).filter_map(|x| keep.get(&x.2).copied()).collect();
            if ks.is_empty() { f64::NAN } else { ks.iter().filter(|b| **b).count() as f64 / ks.len() as f64 }
        };
        let (kt, kf) = (rate("text_passage"), rate("figure"));
        // final kept: EG2-gated text + figure, composed counted at the figure rate until the judge gate is applied
        let n = |m: &str| kq.iter().filter(|x| x.0 == m).count() as f64;
        let final_kept = n("text_passage") * if kt.is_nan() { 1.0 } else { kt } + (n("figure") + n("figure_composed")) * if kf.is_nan() { 1.0 } else { kf };
        let final_ph = final_kept / (wall_h * gpus).max(1e-9);
        let share = |key: &str, val: &str| kq.iter().filter(|x| x.1[key].as_str() == Some(val)).count() as f64 / kq.len().max(1) as f64;
        let l2 = kq.iter().filter(|x| x.1["english"].as_str().is_some_and(|e| e != "native")).count() as f64 / kq.len().max(1) as f64;
        let calls = (f(&joint["calls"]).max(0.0) + if imported { f(&st["calls"]).max(0.0) } else { 0.0 }).max(1.0);
        let text_arm = gs.iter().map(|g| g.arm.clone()).find(|x| !x.ends_with("-c3fig")).unwrap_or(format!("{a}-c3pf"));
        let kept_n = kq.len() as f64;
        let kept_fig = kq.iter().filter(|x| x.0.starts_with("figure")).count() as f64;
        let reasoning = gs.iter().map(|g| g.reasoning_tokens as f64).sum::<f64>() / calls;
        // judges
        let family = meta["family"].as_str().unwrap_or("").to_string();
        let (mut j_st, mut j_gen, mut j_nat, mut j_sty, mut j_rel, mut j_img, mut j_cap) = (vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
        for j in &judges {
            let jm = load(&j.join("judge.json"));
            if jm["family"].as_str() == Some(family.as_str()) {
                continue;
            }
            let t = load(&j.join("text").join(format!("{text_arm}.summary.json")));
            if !t.is_null() {
                j_st.push(f(&t["standalone"]));
                j_gen.push(f(&t["generic_many"]));
                j_nat.push(f(&t["natural_mean"]));
                j_sty.push(f(&t["style_ok"]["all"]));
            }
            let fj = &load(&j.join("fig").join("judge_fig.json"))["by_arm"][format!("{a}-c3fig")];
            if !fj.is_null() {
                j_rel.push(f(&fj["relevant2"]));
                j_img.push(f(&fj["needs_image"]));
                j_cap.push(f(&fj["caption_paraphrase"]));
            }
        }
        let mean = |v: &[f64]| if v.is_empty() { f64::NAN } else { v.iter().sum::<f64>() / v.len() as f64 };
        let row = json!({"arm": a, "model": meta["model"], "precision": meta["precision"], "level": meta["level"], "gpus": gpus,
            "articles": joint["articles"], "skipped": joint["articles_skipped_deadline"], "wall_h": wall_h,
            "text": meta["text"], "generated_per_gpu_h": gen_slots / (wall_h * gpus).max(1e-9), "kept_per_gpu_h": kept_n / (wall_h * gpus).max(1e-9),
            "figure_share_kept": kept_fig / kept_n.max(1.0), "eg2_keep_text": kt, "eg2_keep_fig": kf, "final_per_gpu_h": final_ph,
            "gpu_h_for_target": target_kept / final_ph.max(1e-9), "standalone": mean(&j_st), "generic": mean(&j_gen), "natural": mean(&j_nat),
            "style_ok": mean(&j_sty), "fig_relevant2": mean(&j_rel), "needs_image": mean(&j_img), "caption_paraphrase": mean(&j_cap),
            "lay_share": share("register", "lay"), "l2_share": l2, "reasoning_tokens_per_call": reasoning,
            "reject_counts": crate::mix::gen_mix(&[&d.join(format!("{a}-c3pf")), &d.join(format!("{a}-c3fig"))]).ok()});
        md += &format!("| {a} | {} | {} | {} | {} | {}/{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            meta["model"].as_str().unwrap_or(""), meta["precision"].as_str().unwrap_or(""), meta["level"].as_str().unwrap_or(""), gpus,
            f(&joint["articles"]) - f(&joint["articles_skipped_deadline"]), joint["articles"], num(wall_h), num(row["generated_per_gpu_h"].as_f64().unwrap_or(f64::NAN)),
            num(kept_n / (wall_h * gpus).max(1e-9)), pct(kept_fig / kept_n.max(1.0)), pct(kt), pct(kf), num(final_ph), num(target_kept / final_ph.max(1e-9)),
            pct(mean(&j_st)), pct(mean(&j_gen)), num(mean(&j_nat)), pct(mean(&j_sty)), pct(mean(&j_rel)), pct(mean(&j_img)), pct(mean(&j_cap)),
            pct(share("register", "lay")), pct(l2), num(reasoning));
        rows.push(row);
    }
    let mut v = Map::new();
    v.insert("target_kept".into(), json!(target_kept));
    v.insert("arms".into(), json!(rows));
    Ok((md, Value::Object(v)))
}
