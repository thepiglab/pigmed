//! Mixes: the persona library against its targets, the planned (pre-generation) mix of an input sample, and the
//! realised mix of generated arms (persona, style, register, modality, rejects, figure kinds).

use crate::client::Generation;
use crate::figure::{plan_fig_cards, FigInput};
use crate::rules::Article;
use crate::text::{allowed_styles, plan_cards, SampleChunk};
use crate::util::read_jsonl;
use crate::Qg;
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

fn shares(counts: &BTreeMap<String, usize>) -> Value {
    let n: usize = counts.values().sum::<usize>().max(1);
    let mut v: Vec<(&String, &usize)> = counts.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    Value::Object(v.into_iter().map(|(k, c)| (k.clone(), json!((*c as f64 / n as f64 * 1000.0).round() / 1000.0))).collect())
}

fn count<'a>(it: impl Iterator<Item = String>) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for x in it {
        *m.entry(x).or_default() += 1;
    }
    m
}

/// Library mix vs targets, with pass/fail checks.
pub fn library_mix(qg: &Qg) -> Value {
    let l = &qg.lib;
    let n = l.len() as f64;
    let group = count(l.iter().map(|p| p.group.clone()));
    let tot: u32 = qg.cfg.personas.groups.iter().map(|g| g.1).sum();
    let mut checks = vec![];
    for (g, w) in &qg.cfg.personas.groups {
        let want = *w as f64 / tot as f64;
        let got = *group.get(g).unwrap_or(&0) as f64 / n;
        checks.push(json!({"check": format!("group {g}"), "target": want, "got": got, "ok": (got - want).abs() < 0.02}));
    }
    let l2 = l.iter().filter(|p| p.english != "native").count() as f64 / n;
    checks.push(json!({"check": "L2 English share >= 0.15", "got": l2, "ok": l2 >= 0.15}));
    let low = l.iter().filter(|p| p.literacy == "low").count() as f64 / n;
    checks.push(json!({"check": "low health literacy >= 0.05 of library (lay only)", "got": low, "ok": low >= 0.05}));
    let roles_used = count(l.iter().map(|p| p.role.clone()));
    let roles_cfg = qg.cfg.personas.roles.len();
    checks.push(json!({"check": "every configured role drawn", "got": roles_used.len(), "target": roles_cfg, "ok": roles_used.len() == roles_cfg}));
    let bad: Vec<String> = l.iter().filter(|p| {
        let r = qg.cfg.personas.roles.iter().find(|r| r.0 == p.role).unwrap();
        p.expertise < r.2 || p.expertise > r.3 || (p.english == "native") != p.first_language.is_empty() || p.age.is_empty() || p.goal.is_empty() || p.context.is_empty()
    }).take(5).map(|p| p.id.clone()).collect();
    checks.push(json!({"check": "compatibility rules (expertise range, L1 iff L2, all attributes set)", "violations": bad, "ok": bad.is_empty()}));
    let distinct_cards = qg.cards.iter().collect::<std::collections::HashSet<_>>().len();
    json!({"recipe": qg.cfg.recipe, "seed": qg.cfg.personas.seed, "size": l.len(), "roles_configured": roles_cfg, "distinct_cards": distinct_cards,
        "group": shares(&group), "english": shares(&count(l.iter().map(|p| p.english.clone()))),
        "first_language": shares(&count(l.iter().filter(|p| !p.first_language.is_empty()).map(|p| p.first_language.clone()))),
        "literacy": shares(&count(l.iter().map(|p| p.literacy.clone()))), "expertise": shares(&count(l.iter().map(|p| p.expertise.to_string()))),
        "age": shares(&count(l.iter().map(|p| p.age.clone()))), "gender": shares(&count(l.iter().map(|p| p.gender.clone()))),
        "goal": shares(&count(l.iter().map(|p| p.goal.clone()))), "context": shares(&count(l.iter().map(|p| p.context.clone()))),
        "typos": shares(&count(l.iter().map(|p| p.typos.to_string()))), "lowercase": l.iter().filter(|p| p.lowercase).count() as f64 / n,
        "top_roles": shares(&roles_used.iter().filter(|(_, c)| **c as f64 / n > 0.012).map(|(k, v)| (k.clone(), *v)).collect()),
        "checks": checks})
}

/// Planned mix of an input sample before any model call: cards and slots per modality, candidate personas, and the
/// style mix the schedule allows for those candidates (uniform over candidates; stage A's reader choice moves it).
pub fn plan_mix(qg: &Qg, chunks: &[SampleChunk], arts: &[Article], figs: &[FigInput]) -> Value {
    let by: HashMap<&str, &Article> = arts.iter().map(|a| (a.article_id.as_str(), a)).collect();
    let mut groups: Vec<(String, Vec<&SampleChunk>)> = vec![];
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for c in chunks {
        let i = *idx.entry(c.article_id.as_str()).or_insert_with(|| { groups.push((c.article_id.clone(), vec![])); groups.len() - 1 });
        groups[i].1.push(c);
    }
    let mut fig_by: HashMap<&str, Vec<&FigInput>> = HashMap::new();
    for f in figs {
        fig_by.entry(f.article_id.as_str()).or_default().push(f);
    }
    let (mut text_cards, mut fig_cards, mut text_slots, mut fig_slots) = (0usize, 0usize, 0f64, 0f64);
    let mut ctypes: BTreeMap<String, usize> = BTreeMap::new();
    let mut cand_groups: BTreeMap<String, usize> = BTreeMap::new();
    let mut style_w: BTreeMap<String, f64> = BTreeMap::new();
    let mut fig_style_w: BTreeMap<String, f64> = BTreeMap::new();
    let mut arts_with_figs = 0;
    let f = qg.cfg.figure.as_ref();
    let cal: Vec<(String, usize, Vec<&FigInput>)> = groups.iter().filter_map(|(aid, cs)| {
        let art = by.get(aid.as_str())?;
        Some((aid.clone(), plan_cards(qg, art, cs).len(), fig_by.get(aid.as_str()).cloned().unwrap_or_default()))
    }).collect();
    let (boost, _) = if qg.cfg.figure_on() { crate::figure::calibrate_boost(qg, &cal) } else { (1.0, 0.0) };
    for (aid, cs) in &groups {
        let Some(art) = by.get(aid.as_str()) else { continue };
        let cards = plan_cards(qg, art, cs);
        text_cards += cards.len();
        text_slots += (cards.len() * qg.cfg.text.styles_per_pair) as f64;
        for c in &cards {
            for &p in &c.cands {
                *cand_groups.entry(qg.lib[p].group.clone()).or_default() += 1;
                let al = allowed_styles(&qg.cfg.styles, Some(&qg.lib[p]), false, true);
                let tot: u32 = al.iter().map(|x| x.1).sum();
                for (s, w) in al {
                    *style_w.entry(s).or_default() += w as f64 / tot.max(1) as f64 / c.cands.len() as f64;
                }
            }
        }
        if let (Some(fc), true) = (f, qg.cfg.figure_on()) {
            let mine = fig_by.get(aid.as_str()).cloned().unwrap_or_default();
            if !mine.is_empty() {
                arts_with_figs += 1;
            }
            for c in plan_fig_cards(qg, aid, cards.len(), &mine, boost) {
                fig_cards += 1;
                *ctypes.entry(c.ctype.clone()).or_default() += 1;
                let ct = crate::figure::card_type(fc, &c.ctype);
                fig_slots += if ct.styles.is_empty() { fc.styles_per_pair as f64 } else { ct.styles.len() as f64 };
                for &p in &c.card.cands {
                    let al = if ct.styles.is_empty() { allowed_styles(&fc.styles, Some(&qg.lib[p]), false, false) } else { ct.styles.iter().map(|s| (s.clone(), 1)).collect() };
                    let tot: u32 = al.iter().map(|x| x.1).sum();
                    for (s, w) in al {
                        *fig_style_w.entry(s).or_default() += w as f64 / tot.max(1) as f64 / c.card.cands.len() as f64;
                    }
                }
            }
        }
    }
    let norm = |m: &BTreeMap<String, f64>| {
        let t: f64 = m.values().sum::<f64>().max(1e-9);
        Value::Object(m.iter().map(|(k, v)| (k.clone(), json!((v / t * 1000.0).round() / 1000.0))).collect::<Map<String, Value>>())
    };
    let all = (text_slots + fig_slots).max(1.0);
    let target = qg.cfg.modality.share("figure") + qg.cfg.modality.share("figure_composed");
    json!({"articles": groups.len(), "articles_with_figures": arts_with_figs, "figures_in_input": figs.len(),
        "text_cards": text_cards, "figure_cards": fig_cards, "figure_card_types": ctypes,
        "planned_slots": {"text": text_slots, "figure": fig_slots}, "planned_figure_share": fig_slots / all, "target_figure_share": target, "figure_boost": boost,
        "candidate_groups": shares(&cand_groups.into_iter().collect()), "text_style_mix_allowed": norm(&style_w),
        "figure_style_mix_allowed": norm(&fig_style_w)})
}

/// Realised mix of generated arms (dirs with generations.jsonl).
pub fn gen_mix(dirs: &[&Path]) -> Result<Value> {
    let mut out = Map::new();
    for d in dirs {
        let gens: Vec<Generation> = read_jsonl(&d.join("generations.jsonl"))?;
        let mut rows: Vec<&Value> = vec![];
        for g in &gens {
            rows.extend(g.meta["slots"].as_object().into_iter().flatten().map(|(_, v)| v));
        }
        let kept: Vec<&&Value> = rows.iter().filter(|r| r["rejected"] == json!(false)).collect();
        let f = |rs: &[&&Value], k: &str| shares(&count(rs.iter().map(|r| r[k].as_str().map(str::to_string).unwrap_or_else(|| r[k].to_string()))));
        let all_refs: Vec<&&Value> = rows.iter().collect();
        let mut cards = 0usize;
        let mut drops: BTreeMap<String, usize> = BTreeMap::new();
        let mut health = (0usize, 0usize);
        let mut lay = (0usize, 0usize);
        for g in &gens {
            for c in g.meta["cards"].as_array().into_iter().flatten() {
                cards += 1;
                *drops.entry(c["drop"].as_str().unwrap_or("").to_string()).or_default() += 1;
                if c["lay"] == json!(true) {
                    lay.0 += 1;
                    lay.1 += (c["drop"] == json!("")) as usize;
                }
                if c["health"] == json!(true) {
                    health.0 += 1;
                    health.1 += (c["drop"] == json!("")) as usize;
                }
            }
        }
        let name = d.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
        out.insert(name, json!({"rows": gens.len(), "slots": rows.len(), "kept": kept.len(),
            "keep_rate": kept.len() as f64 / rows.len().max(1) as f64,
            "reject": shares(&count(rows.iter().filter(|r| r["rejected"] == json!(true)).map(|r| r["reject"].as_str().unwrap_or("").to_string()))),
            "reject_counts": count(rows.iter().filter(|r| r["rejected"] == json!(true)).map(|r| r["reject"].as_str().unwrap_or("").to_string())),
            "cards": cards, "card_outcomes": drops, "health_cards": {"planned": health.0, "kept": health.1}, "lay_cards": {"planned": lay.0, "kept": lay.1},
            "kept_lay_card_share": kept.iter().filter(|r| r["lay_card"] == json!(true)).count() as f64 / kept.len().max(1) as f64,
            "kept_lay_register_share": kept.iter().filter(|r| r["register"] == json!("lay")).count() as f64 / kept.len().max(1) as f64,
            "kept_modality": f(&kept, "modality"), "kept_style": f(&kept, "style"), "kept_register": f(&kept, "register"),
            "kept_persona_group": f(&kept, "persona_group"), "kept_english": f(&kept, "english"), "kept_fallback": f(&kept, "persona_fallback"),
            "kept_figure_kind": f(&kept.iter().copied().filter(|r| r["figure_kind"].is_string()).collect::<Vec<_>>(), "figure_kind"),
            "kept_top_roles": shares(&count(kept.iter().filter_map(|r| r["persona_role"].as_str().map(str::to_string))).into_iter().filter(|(_, c)| *c >= 3).collect()),
            "generated_style": f(&all_refs, "style")}));
    }
    Ok(Value::Object(out))
}
