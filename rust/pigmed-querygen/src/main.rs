//! pigmed-querygen: config-driven synthetic-query generator (docs/QUERYGEN_SPEC.md).
//!
//!   personas    --config C --out personas.jsonl              persona library (+ rendered card)
//!   render      --config C --sample S --articles A [--figs F] --n N --out DIR
//!                                                            prompts + schemas of N articles (stage B from a mock stage A)
//!   mix         --config C [--sample S --articles A --figs F] [--gen DIR...]
//!                                                            library / planned / realised mixes
//!   fig-inputs  --config C --articles A --items-root R --xml-cache X [--manifest M] [--exclude E] [--vectors V] --out DIR
//!   generate    --config C --sample S --articles A [--figs F] --endpoint URL --model M --arm ARM ... --out DIR
//!                                                            writes DIR/<arm>-c3pf and DIR/<arm>-c3fig
//!   mock-serve  --port P [--record FILE]                     schema-faithful fake server (tests, dry runs)
//!   peek        --gen DIR --n N                              random kept queries from a running or finished arm

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use querygen::client::{agent, delta, par, scrape, summary, GenArgs, Generation};
use querygen::figure::{self, FigInput};
use querygen::rules::Article;
use querygen::text::{self, SampleChunk};
use querygen::util::{read_jsonl, uh, write_json, write_jsonl};
use querygen::{mix, mock, Qg};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Parser)]
#[command(about = "PigMed synthetic-query generator (config-driven)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Personas {
        #[arg(long)] config: PathBuf,
        #[arg(long)] n: Option<usize>,
        #[arg(long)] out: PathBuf,
    },
    Render {
        #[arg(long)] config: PathBuf,
        #[arg(long)] sample: PathBuf,
        #[arg(long)] articles: PathBuf,
        #[arg(long)] figs: Option<PathBuf>,
        #[arg(long, default_value_t = 3)] n: usize,
        #[arg(long)] out: PathBuf,
    },
    Mix {
        #[arg(long)] config: PathBuf,
        #[arg(long)] sample: Option<PathBuf>,
        #[arg(long)] articles: Option<PathBuf>,
        #[arg(long)] figs: Option<PathBuf>,
        #[arg(long)] limit: Option<usize>,
        #[arg(long, num_args = 1..)] gen: Vec<PathBuf>,
    },
    FigInputs {
        #[arg(long)] config: PathBuf,
        #[arg(long)] articles: PathBuf,
        #[arg(long)] items_root: PathBuf,
        #[arg(long)] xml_cache: PathBuf,
        #[arg(long)] manifest: Option<PathBuf>,
        /// held-out article ids (one per line, or JSONL with article_id)
        #[arg(long)] exclude: Option<PathBuf>,
        /// EG2 vectors root: keep only figures with an image vector (round-trip corpus)
        #[arg(long)] vectors: Option<PathBuf>,
        /// read images from this mirror (<s3 key>) instead of the bucket (tests)
        #[arg(long)] local_images: Option<PathBuf>,
        #[arg(long)] out: PathBuf,
    },
    Generate {
        #[arg(long)] config: PathBuf,
        #[arg(long)] sample: PathBuf,
        #[arg(long)] articles: PathBuf,
        #[arg(long)] figs: Option<PathBuf>,
        #[arg(long)] endpoint: String,
        #[arg(long)] model: String,
        #[arg(long)] arm: String,
        #[arg(long, default_value_t = 64)] concurrency: usize,
        /// per passage, as pigmed-synth (stage calls get x cli_max_tokens_multiplier, then the per-card budget)
        #[arg(long, default_value_t = 400)] max_tokens: u32,
        #[arg(long, default_value_t = 1.0)] temperature: f64,
        #[arg(long, default_value_t = 0.95)] top_p: f64,
        #[arg(long, default_value = "{}")] extra: String,
        #[arg(long)] limit: Option<usize>,
        #[arg(long, default_value_t = 1)] gpus: usize,
        /// stop starting new articles after this unix time (graceful end inside a job's time limit)
        #[arg(long)] deadline_unix: Option<u64>,
        /// v3.1e per-article metadata (MeSH tree numbers, journal): domain rule, lay pass, field -> role map
        #[arg(long)] article_meta: Option<PathBuf>,
        #[arg(long)] text_only: bool,
        #[arg(long)] fig_only: bool,
        #[arg(long)] out: PathBuf,
    },
    MockServe {
        #[arg(long, default_value_t = 18080)] port: u16,
        #[arg(long)] record: Option<PathBuf>,
        #[arg(long, default_value = "mock")] model: String,
    },
    /// Cross-model VLM judge of figure queries (image + caption + citing text); never judge an arm with its own model
    JudgeFig {
        #[arg(long, num_args = 1..)] gen: Vec<PathBuf>,
        #[arg(long)] figs: PathBuf,
        #[arg(long)] endpoint: String,
        #[arg(long)] model: String,
        #[arg(long, default_value_t = 64)] concurrency: usize,
        #[arg(long, default_value_t = 4000)] max_tokens: u32,
        #[arg(long, default_value = "{}")] extra: String,
        #[arg(long)] out: PathBuf,
    },
    /// Blinded hand-read sample over arms (equal per arm, fig_share figure rows) + key
    Blind {
        #[arg(long, num_args = 1..)] gen: Vec<PathBuf>,
        #[arg(long)] sample: Option<PathBuf>,
        #[arg(long)] figs: Option<PathBuf>,
        #[arg(long, default_value_t = 60)] n: usize,
        #[arg(long, default_value_t = 0.35)] fig_share: f64,
        #[arg(long, default_value = "trial")] seed: String,
        #[arg(long)] out_md: PathBuf,
        #[arg(long)] out_key: PathBuf,
    },
    /// Kept queries of one modality family as pigmed-synth filter input (queries.jsonl + flashembed.jsonl)
    Queries {
        #[arg(long, num_args = 1..)] gen: Vec<PathBuf>,
        /// text | figure
        #[arg(long)] modality: String,
        #[arg(long)] config: PathBuf,
        #[arg(long)] out: PathBuf,
    },
    /// Results table over a trial directory (quality vs throughput, GPU-hours for the target kept count)
    Report {
        #[arg(long)] trial: PathBuf,
        #[arg(long, default_value_t = 2_000_000.0)] target: f64,
        #[arg(long)] out: PathBuf,
    },
    /// Parity: two mock-server request logs and two generation files must agree (every field the reference writes)
    Compare {
        #[arg(long)] req_a: PathBuf,
        #[arg(long)] req_b: PathBuf,
        #[arg(long)] gen_a: PathBuf,
        #[arg(long)] gen_b: PathBuf,
    },
    Peek {
        #[arg(long)] gen: PathBuf,
        #[arg(long, default_value_t = 12)] n: usize,
        #[arg(long, default_value = "")] seed: String,
    },
}

fn group_chunks(chunks: &[SampleChunk]) -> Vec<Vec<&SampleChunk>> {
    let mut groups: Vec<Vec<&SampleChunk>> = vec![];
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for c in chunks {
        let i = *idx.entry(c.article_id.as_str()).or_insert_with(|| { groups.push(vec![]); groups.len() - 1 });
        groups[i].push(c);
    }
    groups
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Personas { config, n, out } => {
            let mut c = querygen::config::Config::load(&config)?;
            if let Some(n) = n { c.personas.size = n; }
            let qg = Qg::new(c)?;
            let rows: Vec<Value> = qg.lib.iter().zip(&qg.cards).map(|(p, c)| { let mut v = serde_json::to_value(p).unwrap(); v["card"] = json!(c); v }).collect();
            write_jsonl(&out, &rows)?;
            println!("{}", json!({"personas": rows.len(), "recipe": qg.cfg.recipe}));
        }
        Cmd::Render { config, sample, articles, figs, n, out } => {
            let qg = Qg::load(&config)?;
            let chunks: Vec<SampleChunk> = read_jsonl(&sample)?;
            let arts: Vec<Article> = read_jsonl(&articles)?;
            let figs: Vec<FigInput> = match figs { Some(p) => read_jsonl(&p)?, None => vec![] };
            let by: HashMap<&str, &Article> = arts.iter().map(|a| (a.article_id.as_str(), a)).collect();
            std::fs::create_dir_all(&out)?;
            let t = &qg.cfg.text.templates;
            let mut done = 0;
            for g in group_chunks(&chunks) {
                let Some(art) = by.get(g[0].article_id.as_str()) else { continue };
                if done >= n { break; }
                done += 1;
                let plan = text::plan_cards(&qg, art, &g);
                let sa = text::schema_a(&qg, &plan);
                let fake = mock::fake(&sa, "", None, &art.article_id);
                let mut cards = text::fill_cards(&qg, &plan, &fake);
                if fake["biomedical"] == json!(false) { cards.iter_mut().for_each(|c| c.off_domain = true); }
                let slots = text::plan_slots(&qg, &cards, art);
                let mut md = format!("# {} ({})\n\n## system A\n\n{}\n\n## stage A user\n\n{}\n\n## stage A schema\n\n```json\n{}\n```\n\n",
                    art.title, art.article_id, qg.cfg.file(&t.system_a), text::prompt_a(&qg, art, &plan), serde_json::to_string_pretty(&sa)?);
                md += &format!("## system B\n\n{}\n\n## stage B user (from a MOCK stage-A reply)\n\n{}\n\n", qg.cfg.file(&t.system_b), text::prompt_b(&qg, &cards, &slots));
                let mine: Vec<&FigInput> = figs.iter().filter(|f| f.article_id == art.article_id).collect();
                if qg.cfg.figure_on() && !mine.is_empty() {
                    let mut fcs = figure::plan_fig_cards(&qg, &art.article_id, plan.len(), &mine, 1.0);
                    let used: Vec<usize> = { let mut u: Vec<usize> = fcs.iter().map(|c| c.fig).collect(); u.dedup(); u };
                    let mut kinds_by: HashMap<usize, String> = HashMap::new();
                    for &fi in &used {
                        let these: Vec<&figure::FigCard> = fcs.iter().filter(|c| c.fig == fi).collect();
                        let fsa = figure::schema_fig_a(&qg, &these);
                        md += &format!("## figure stage A user (image {} attached)\n\n{}\n\n", mine[fi].image, figure::prompt_fig_a(&qg, mine[fi], &these));
                        let fv = mock::fake(&fsa, "", None, &mine[fi].chunk_id);
                        let mut mm: Vec<&mut figure::FigCard> = fcs.iter_mut().filter(|c| c.fig == fi).collect();
                        kinds_by.insert(fi, figure::fill_fig_cards(&qg, &mut mm, &fv));
                    }
                    let kinds: Vec<String> = fcs.iter().map(|c| kinds_by.get(&c.fig).cloned().unwrap_or_default()).collect();
                    let fslots = figure::plan_fig_slots(&qg, &fcs, &kinds, &mine, art);
                    md += &format!("## figure stage B user (from MOCK figure cards)\n\n{}\n\n", figure::prompt_fig_b(&qg, &fcs, &kinds, &fslots));
                }
                std::fs::write(out.join(format!("{done:02}-{}.md", art.article_id)), md)?;
            }
            let cards: Vec<Value> = (0..qg.lib.len()).step_by((qg.lib.len() / 40).max(1)).map(|i| json!({"id": qg.lib[i].id, "card": qg.cards[i]})).collect();
            write_jsonl(&out.join("persona_cards_sample.jsonl"), &cards)?;
            println!("{}", json!({"rendered": done, "out": out}));
        }
        Cmd::Mix { config, sample, articles, figs, limit, gen } => {
            let qg = Qg::load(&config)?;
            let mut v = json!({"library": mix::library_mix(&qg)});
            if let (Some(s), Some(a)) = (sample, articles) {
                let mut chunks: Vec<SampleChunk> = read_jsonl(&s)?;
                if let Some(n) = limit { chunks.truncate(n); }
                let arts: Vec<Article> = read_jsonl(&a)?;
                let figs: Vec<FigInput> = match figs { Some(p) => read_jsonl(&p)?, None => vec![] };
                v["planned"] = mix::plan_mix(&qg, &chunks, &arts, &figs);
            }
            if !gen.is_empty() {
                v["realised"] = mix::gen_mix(&gen.iter().map(|p| p.as_path()).collect::<Vec<_>>())?;
            }
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::FigInputs { config, articles, items_root, xml_cache, manifest, exclude, vectors, local_images, out } => {
            let qg = Qg::load(&config)?;
            let image_ids = match &vectors { Some(v) => Some(image_ids(v)?), None => None };
            let (rows, stats) = figure::build_inputs(&qg, &figure::FigInputArgs { articles: &articles, items_root: &items_root, xml_cache: &xml_cache,
                manifest: manifest.as_deref(), exclude: exclude.as_deref(), image_ids: image_ids.as_ref(), local_images: local_images.as_deref(), out: &out })?;
            write_jsonl(&out.join("figinputs.jsonl"), &rows)?;
            write_json(&out.join("figinputs.stats.json"), &stats)?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        Cmd::Generate { config, sample, articles, figs, endpoint, model, arm, concurrency, max_tokens, temperature, top_p, extra, limit, gpus, deadline_unix, article_meta, text_only, fig_only, out } => {
            let qg = Qg::load(&config)?;
            let mut chunks: Vec<SampleChunk> = read_jsonl(&sample)?;
            if let Some(n) = limit { chunks.truncate(n); }
            let arts: Vec<Article> = read_jsonl(&articles)?;
            let figs: Vec<FigInput> = match figs { Some(p) => read_jsonl(&p)?, None => vec![] };
            let extra: Value = serde_json::from_str(&extra).context("--extra")?;
            let a = GenArgs { endpoint, model, arm: arm.clone(), concurrency, max_tokens: max_tokens * qg.cfg.text.budget.cli_max_tokens_multiplier,
                temperature, top_p, extra, gpus };
            let metas: HashMap<String, querygen::rules::ArticleMeta> = match article_meta {
                Some(p) => read_jsonl::<querygen::rules::ArticleMeta>(&p)?.into_iter().map(|m| (m.article_id.clone(), m)).collect(), None => HashMap::new() };
            generate(&qg, &a, &chunks, &arts, &figs, &metas, deadline_unix, !fig_only, !text_only && qg.cfg.figure_on(), &out)?;
        }
        Cmd::JudgeFig { gen, figs, endpoint, model, concurrency, max_tokens, extra, out } => {
            let figs: HashMap<String, FigInput> = read_jsonl::<FigInput>(&figs)?.into_iter().map(|f| (f.chunk_id.clone(), f)).collect();
            let a = GenArgs { endpoint, model, arm: "judge".into(), concurrency, max_tokens, temperature: 0.0, top_p: 1.0,
                extra: serde_json::from_str(&extra).context("--extra")?, gpus: 1 };
            let mut gens = vec![];
            for g in &gen { gens.extend(read_jsonl::<Generation>(&g.join("generations.jsonl"))?); }
            let (rows, s) = querygen::judge::judge_figs(&a, &gens, &figs);
            write_jsonl(&out.join("judge_fig.jsonl"), &rows)?;
            write_json(&out.join("judge_fig.json"), &s)?;
            println!("{}", serde_json::to_string_pretty(&s)?);
        }
        Cmd::Blind { gen, sample, figs, n, fig_share, seed, out_md, out_key } => {
            let v = querygen::judge::blind(&gen.iter().map(|p| p.as_path()).collect::<Vec<_>>(), sample.as_deref(), figs.as_deref(), n, fig_share, &seed, &out_md, &out_key)?;
            println!("{v}");
        }
        Cmd::Queries { gen, modality, config, out } => {
            let qg = Qg::load(&config)?;
            let v = querygen::report::queries(&gen.iter().map(|p| p.as_path()).collect::<Vec<_>>(), &modality, &qg.cfg.gates.eg2_query_prefix, &out)?;
            println!("{v}");
        }
        Cmd::Report { trial, target, out } => {
            let (md, v) = querygen::report::report(&trial, target)?;
            std::fs::create_dir_all(&out)?;
            std::fs::write(out.join("results.md"), &md)?;
            write_json(&out.join("results.json"), &v)?;
            println!("{md}");
        }
        Cmd::Compare { req_a, req_b, gen_a, gen_b } => {
            let v = compare(&req_a, &req_b, &gen_a, &gen_b)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            anyhow::ensure!(v["pass"] == json!(true), "parity FAILED");
        }
        Cmd::MockServe { port, record, model } => mock::serve(port, record, &model)?,
        Cmd::Peek { gen, n, seed } => {
            let p = if gen.join("generations.jsonl").exists() { gen.join("generations.jsonl") } else { gen.join("generations.partial.jsonl") };
            let gens: Vec<Generation> = read_jsonl(&p)?;
            let mut rows: Vec<(u64, Value)> = vec![];
            for g in &gens {
                for (k, m) in g.meta["slots"].as_object().into_iter().flatten() {
                    if m["rejected"] == json!(false) {
                        rows.push((uh(&[&seed, &g.chunk_id, k]), json!({"query": m["query"], "style": m["style"], "modality": m["modality"],
                            "persona": m["persona_role"], "english": m["english"], "kind": m["figure_kind"], "need": m["need"]})));
                    }
                }
            }
            rows.sort_by_key(|r| r.0);
            for (_, r) in rows.into_iter().take(n) {
                println!("{r}");
            }
        }
    }
    Ok(())
}

fn image_ids(vectors: &Path) -> Result<std::collections::HashSet<String>> {
    let mut s = std::collections::HashSet::new();
    for e in std::fs::read_dir(vectors.join("gemma"))? {
        let d = e?.path();
        if !d.join("meta.json").exists() { continue; }
        let ids: Value = serde_json::from_str(&std::fs::read_to_string(d.join("ids.json"))?)?;
        for (u, m) in ids["unit_id"].as_array().into_iter().flatten().zip(ids["modality"].as_array().into_iter().flatten()) {
            if m == "image" { if let Some(u) = u.as_str() { s.insert(u.to_string()); } }
        }
    }
    Ok(s)
}

/// Text and figure arms over the same articles; one work item = one article. Append-only partial files let a
/// monitor sample queries while the run is going.
#[allow(clippy::too_many_arguments)]
fn generate(qg: &Qg, a: &GenArgs, chunks: &[SampleChunk], arts: &[Article], figs: &[FigInput],
    metas: &HashMap<String, querygen::rules::ArticleMeta>, deadline: Option<u64>,
    do_text: bool, do_fig: bool, out: &Path) -> Result<()> {
    let by: HashMap<&str, &Article> = arts.iter().map(|x| (x.article_id.as_str(), x)).collect();
    let mut groups = group_chunks(chunks);
    groups.retain(|g| by.contains_key(g[0].article_id.as_str()));
    let mut fig_by: HashMap<&str, Vec<&FigInput>> = HashMap::new();
    for f in figs {
        fig_by.entry(f.article_id.as_str()).or_default().push(f);
    }
    let cal: Vec<(String, usize, Vec<&FigInput>)> = groups.iter().map(|g| {
        let art = by[g[0].article_id.as_str()];
        (art.article_id.clone(), text::plan_cards_meta(qg, art, g, metas.get(&art.article_id)).len(), fig_by.get(art.article_id.as_str()).cloned().unwrap_or_default())
    }).collect();
    let (boost, planned_share) = if do_fig { figure::calibrate_boost(qg, &cal) } else { (1.0, 0.0) };
    eprintln!("figure boost {boost} (planned figure share {planned_share:.3})");
    let (td, fd) = (out.join(format!("{}-c3pf", a.arm)), out.join(format!("{}-c3fig", a.arm)));
    let ta = GenArgs { arm: format!("{}-c3pf", a.arm), ..a.clone() };
    let fa = GenArgs { arm: format!("{}-c3fig", a.arm), ..a.clone() };
    // only the arms that run get a directory (a --fig-only run leaves <arm>-c3pf free for an imported text arm)
    let open = |d: &Path, on: bool| -> Result<Option<std::fs::File>> {
        if !on { return Ok(None); }
        std::fs::create_dir_all(d)?;
        Ok(Some(std::fs::OpenOptions::new().create(true).append(true).open(d.join("generations.partial.jsonl"))?))
    };
    let tpart = Mutex::new(open(&td, do_text)?);
    let fpart = Mutex::new(open(&fd, do_fig)?);
    std::fs::create_dir_all(out)?;
    let prog = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(out.join(format!("{}.progress.jsonl", a.arm)))?);
    let ag = agent(a.concurrency);
    let before = scrape(&ag, &a.endpoint);
    let t0 = Instant::now();
    let skipped = std::sync::atomic::AtomicUsize::new(0);
    let res = par(groups.len(), a.concurrency, |i| {
        let g = &groups[i];
        let art = by[g[0].article_id.as_str()];
        if deadline.is_some_and(|d| now() >= d) {
            skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return (vec![], vec![], 0usize, 0usize, 0.0f64);
        }
        let ts = Instant::now();
        let meta = metas.get(&art.article_id);
        let n_text = text::plan_cards_meta(qg, art, g, meta).len();
        let (tg, tcalls) = if do_text { let (g, _, u) = text::text_article_meta(qg, &ag, &ta, art, g, meta); (g, u.calls) } else { (vec![], 0) };
        let (fg, fcalls) = match (do_fig, fig_by.get(art.article_id.as_str())) {
            (true, Some(fs)) => { let (g, u) = figure::fig_article(qg, &ag, &fa, art, n_text, fs, boost, meta); (g, u.calls) }
            _ => (vec![], 0),
        };
        (tg, fg, tcalls, fcalls, ts.elapsed().as_secs_f64())
    }, |r| {
        let line = |g: &Generation| serde_json::to_string(g).unwrap();
        if let Ok(mut g) = tpart.lock() { if let Some(f) = g.as_mut() { for x in &r.0 { let _ = writeln!(f, "{}", line(x)); } let _ = f.flush(); } }
        if let Ok(mut g) = fpart.lock() { if let Some(f) = g.as_mut() { for x in &r.1 { let _ = writeln!(f, "{}", line(x)); } let _ = f.flush(); } }
        let kept = |gs: &[Generation]| gs.iter().map(|g| g.queries.len()).sum::<usize>();
        let slots = |gs: &[Generation]| gs.iter().map(|g| g.meta["slots"].as_object().map(|m| m.len()).unwrap_or(0)).sum::<usize>();
        if let (Ok(mut f), Some(g)) = (prog.lock(), r.0.first().or(r.1.first())) {
            let _ = writeln!(f, "{}", json!({"t": now(), "article": g.article_id, "s": r.4, "text_slots": slots(&r.0), "text_kept": kept(&r.0),
                "fig_slots": slots(&r.1), "fig_kept": kept(&r.1), "errors": r.0.iter().chain(&r.1).filter(|g| g.error.is_some()).count()}));
            let _ = f.flush();
        }
    });
    let wall = t0.elapsed().as_secs_f64();
    let server = delta(&before, &scrape(&ag, &a.endpoint));
    let (mut tg, mut fg, mut tc, mut fc) = (vec![], vec![], 0, 0);
    for r in res {
        tg.extend(r.0);
        fg.extend(r.1);
        tc += r.2;
        fc += r.3;
    }
    tg.sort_by(|x, y| x.chunk_id.cmp(&y.chunk_id));
    fg.sort_by(|x, y| x.chunk_id.cmp(&y.chunk_id));
    let skipped = skipped.into_inner();
    // both arms share the wall clock and the GPUs: per-arm GPU-hour figures use the joint wall time
    for (d, gens, calls, args, layout) in [(&td, &tg, tc, &ta, "c3pf"), (&fd, &fg, fc, &fa, "c3fig")] {
        if gens.is_empty() {
            continue;
        }
        let mut s = summary(args, layout, gens, calls, wall, server.clone());
        s["recipe"] = json!(qg.cfg.recipe);
        s["figure_boost"] = json!(boost);
        s["planned_figure_share"] = json!(planned_share);
        s["articles_skipped_deadline"] = json!(skipped);
        s["wall_shared_with"] = json!(if layout == "c3pf" { &fa.arm } else { &ta.arm });
        let mut drops: std::collections::BTreeMap<String, usize> = Default::default();
        for g in gens.iter() {
            for c in g.meta["cards"].as_array().into_iter().flatten() {
                *drops.entry(c["drop"].as_str().unwrap_or("").to_string()).or_default() += 1;
            }
        }
        s["card_outcomes"] = json!(drops);
        write_jsonl(&d.join("generations.jsonl"), gens.iter())?;
        write_json(&d.join("summary.json"), &s)?;
        println!("{}", json!({"arm": args.arm, "rows": gens.len(), "queries": s["queries"], "slots": s["slots"], "wall_s": wall,
            "queries_per_gpu_hour": s["queries_per_gpu_hour"], "skipped": skipped}));
    }
    // joint summary: kept per GPU-hour over both arms and the realised figure share
    let kept = |gs: &[Generation]| gs.iter().map(|g| g.queries.len()).sum::<usize>() as f64;
    let (kt, kf) = (kept(&tg), kept(&fg));
    write_json(&out.join(format!("{}.joint.json", a.arm)), &json!({"arm": a.arm, "recipe": qg.cfg.recipe, "wall_s": wall, "gpus": a.gpus,
        "kept_text": kt, "kept_figure": kf, "kept_per_gpu_hour": (kt + kf) / wall.max(1e-9) * 3600.0 / a.gpus.max(1) as f64,
        "figure_share_kept": kf / (kt + kf).max(1.0), "figure_boost": boost, "planned_figure_share": planned_share, "calls": tc + fc, "articles": groups.len(), "articles_skipped_deadline": skipped}))?;
    Ok(())
}

/// Requests compared as sorted canonical JSON; generations by chunk id, every field the reference (a) writes.
fn compare(req_a: &Path, req_b: &Path, gen_a: &Path, gen_b: &Path) -> Result<Value> {
    let canon = |p: &Path| -> Result<Vec<String>> {
        let mut v: Vec<String> = read_jsonl::<Value>(p)?.iter().map(|x| serde_json::to_string(&sorted(x)).unwrap()).collect();
        v.sort();
        Ok(v)
    };
    let (ra, rb) = (canon(req_a)?, canon(req_b)?);
    let first_req_diff = ra.iter().zip(&rb).position(|(x, y)| x != y);
    let ga: Vec<Generation> = read_jsonl(gen_a)?;
    let gb: HashMap<String, Generation> = read_jsonl::<Generation>(gen_b)?.into_iter().map(|g| (g.chunk_id.clone(), g)).collect();
    let mut bad: std::collections::BTreeMap<String, usize> = Default::default();
    let mut rejects: std::collections::BTreeMap<String, usize> = Default::default();
    let mut drops: std::collections::BTreeMap<String, usize> = Default::default();
    let mut slots = 0;
    for g in &ga {
        let Some(h) = gb.get(&g.chunk_id) else { *bad.entry("row_missing".into()).or_default() += 1; continue };
        if g.ok != h.ok || g.error != h.error || g.queries != h.queries || g.prompt_tokens != h.prompt_tokens || g.completion_tokens != h.completion_tokens {
            *bad.entry("row".into()).or_default() += 1;
        }
        for (k, m) in g.meta["slots"].as_object().into_iter().flatten() {
            slots += 1;
            *rejects.entry(m["reject"].as_str().unwrap_or("").to_string()).or_default() += 1;
            for (f, v) in m.as_object().into_iter().flatten() {
                if h.meta["slots"][k][f] != *v { *bad.entry(format!("slot.{f}")).or_default() += 1; }
            }
        }
        for (i, c) in g.meta["cards"].as_array().into_iter().flatten().enumerate() {
            *drops.entry(c["drop"].as_str().unwrap_or("").to_string()).or_default() += 1;
            for (f, v) in c.as_object().into_iter().flatten() {
                if h.meta["cards"][i][f] != *v { *bad.entry(format!("card.{f}")).or_default() += 1; }
            }
        }
    }
    let pass = ra == rb && bad.is_empty() && ga.len() == gb.len();
    Ok(json!({"pass": pass, "requests": [ra.len(), rb.len()], "requests_identical": ra == rb, "first_request_diff": first_req_diff,
        "rows": [ga.len(), gb.len()], "slots_compared": slots, "mismatches": bad, "reject_reasons_exercised": rejects, "card_drops_exercised": drops}))
}

fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(keys.into_iter().map(|k| (k.clone(), sorted(&m[k]))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        x => x.clone(),
    }
}
