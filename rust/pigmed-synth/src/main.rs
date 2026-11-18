//! pigmed-synth: the synthetic-query generator A/B (docs/RETRIEVER_DATA_PLAN.md, "Synthetic queries").
//!
//!   pigmed-synth sample   --manifest splits/articles.jsonl.gz --records records.jsonl.gz --atlas assignments.json
//!                         --items-root <bundle items root> --out sample.jsonl [--articles 1000 --chunks 5]
//!   pigmed-synth generate --sample sample.jsonl --endpoint http://127.0.0.1:8000/v1 --model M --arm NAME --out DIR
//!   pigmed-synth queries  --gen DIR... --out DIR        (flashembed query input + query metadata)
//!   pigmed-synth filter   --queries DIR --qvec DIR --vectors VECTORS --out DIR
//!   pigmed-synth report   --sample sample.jsonl --queries DIR --filter DIR --gen DIR... --out DIR

mod export;
mod figure;
mod filter;
mod generate;
mod io;
mod recipe;
mod report;
mod sample;
mod samplehf;
mod pairs;
mod persona;
mod noise;
mod recipe3;
mod pilot3;
mod pls;
mod claude;

use anyhow::{Context, Result};
use std::io::BufRead;
use clap::{Parser, Subcommand};
use filter::{QueryMeta, Ranks};
use generate::Generation;
use io::{read_jsonl, write_json, write_jsonl};
use sample::SampleChunk;
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Topic-balanced chunk sample (train split, commercial licence)
    Sample {
        #[arg(long)] manifest: PathBuf,
        #[arg(long)] records: PathBuf,
        #[arg(long)] atlas: PathBuf,
        #[arg(long)] items_root: PathBuf,
        #[arg(long, default_value_t = 1000)] articles: usize,
        #[arg(long, default_value_t = 5)] chunks: usize,
        #[arg(long, default_value_t = 1500)] min_words: usize,
        /// non-target article text kept for layouts B/C (words; targets always kept)
        #[arg(long, default_value_t = 5000)] max_article_words: usize,
        #[arg(long, default_value = "pigmed-synth-v1")] seed: String,
        #[arg(long)] out: PathBuf,
    },
    /// Mass generation: article ids eligible for generation (train, commercial, abstract, body, article type)
    Eligible {
        #[arg(long)] manifest: PathBuf,
        #[arg(long)] records: PathBuf,
        #[arg(long, default_value_t = 1500)] min_words: usize,
        #[arg(long)] out: PathBuf,
    },
    /// Mass generation inputs from one published v2 chunk shard (parquet): chunks.jsonl + articles.jsonl in --out
    SampleHf {
        #[arg(long)] eligible: PathBuf,
        #[arg(long, num_args = 1..)] chunks: Vec<PathBuf>,
        /// share of eligible articles drawn (stable hash; nested across rates)
        #[arg(long, default_value_t = 1.0)] rate: f64,
        #[arg(long, default_value_t = 5)] chunks_per_article: usize,
        #[arg(long, default_value_t = 5000)] max_article_words: usize,
        #[arg(long, default_value = "pigmed-mass-v1")] seed: String,
        #[arg(long)] out: PathBuf,
    },
    /// S1 pairs: train && commercial && !retracted articles -> keep.tsv (abstract->body off for --exclude-records)
    PairsKeep {
        #[arg(long)] manifest: PathBuf,
        #[arg(long)] exclude_records: Option<PathBuf>,
        #[arg(long)] out: PathBuf,
    },
    /// S1 pairs from one v2 chunk shard: title.jsonl, abssent.jsonl, articles.tsv in --out
    PairsShard {
        #[arg(long)] keep: PathBuf,
        #[arg(long, num_args = 1..)] chunks: Vec<PathBuf>,
        #[arg(long, default_value = "pigmed-pairs-v1")] seed: String,
        #[arg(long)] out: PathBuf,
    },
    /// S1 pairs: sample (one per group, journal cap, <= cap per type) + candidate corpus
    PairsMerge {
        #[arg(long)] shards: PathBuf,
        #[arg(long, default_value_t = 2_000_000)] cap: usize,
        #[arg(long, default_value_t = 300_000)] corpus: usize,
        #[arg(long, default_value = "pigmed-pairs-v1")] seed: String,
        #[arg(long)] out: PathBuf,
    },
    /// Full-split pairs merge, streaming: one article per group, all its pairs, pmcid/doi/license_code per pair
    PairsMergeStream {
        #[arg(long)] shards: PathBuf,
        /// full-corpus records.jsonl.gz (doi, license_code)
        #[arg(long)] records: PathBuf,
        #[arg(long, default_value = "pigmed-pairs-v1")] seed: String,
        #[arg(long)] out: PathBuf,
    },
    /// Generate queries for every sampled chunk through an OpenAI-compatible server
    Generate {
        #[arg(long)] sample: PathBuf,
        #[arg(long)] endpoint: String,
        #[arg(long)] model: String,
        #[arg(long)] arm: String,
        #[arg(long, default_value_t = 256)] concurrency: usize,
        #[arg(long, default_value_t = 400)] max_tokens: u32,
        #[arg(long, default_value_t = 0.7)] temperature: f64,
        #[arg(long, default_value_t = 0.95)] top_p: f64,
        /// extra request fields as JSON, e.g. '{"chat_template_kwargs":{"enable_thinking":false}}'
        #[arg(long, default_value = "{}")] extra: String,
        #[arg(long)] limit: Option<usize>,
        /// a = per chunk with abstract; b = per chunk with the article as a shared cached prefix; c = per article
        #[arg(long, default_value = "a")] layout: generate::Layout,
        /// articles.jsonl from `sample` (layouts b, c)
        #[arg(long)] articles: Option<PathBuf>,
        /// GPUs serving the model (for queries per GPU-hour)
        #[arg(long, default_value_t = 1)] gpus: usize,
        #[arg(long)] out: PathBuf,
    },
    /// No-LLM reference arm: queries copied from the chunk (keyword = frequent terms, questions = its first sentence,
    /// contextual = the title). Upper bound on lexical round-trip, and a dry run of the pipeline.
    Baseline {
        #[arg(long)] sample: PathBuf,
        #[arg(long, default_value = "copy")] arm: String,
        #[arg(long)] out: PathBuf,
    },
    /// Figure track sample (typed figures, images resized, citing paragraphs) + figure->citing-chunk pairs
    FigSample {
        #[arg(long)] manifest: PathBuf,
        #[arg(long)] records: PathBuf,
        #[arg(long)] atlas: PathBuf,
        #[arg(long)] items_root: PathBuf,
        #[arg(long)] xml_cache: PathBuf,
        #[arg(long)] vectors: PathBuf,
        #[arg(long, default_value_t = 200)] per_type: usize,
        #[arg(long, default_value_t = 1280)] max_side: u32,
        #[arg(long, default_value = "pigmed-fig-v1")] seed: String,
        #[arg(long)] out: PathBuf,
    },
    /// EG2 rank of each figure's citing chunk, using the figure's own image vector as the query
    Fig2text {
        #[arg(long)] pairs: PathBuf,
        #[arg(long)] vectors: PathBuf,
        #[arg(long, default_value = "gemma")] model: String,
        #[arg(long)] out: PathBuf,
    },
    /// Query metadata + flashembed input (EG2 query prompt)
    Queries {
        #[arg(long, num_args = 1..)] gen: Vec<PathBuf>,
        #[arg(long)] out: PathBuf,
    },
    /// EG2 round-trip ranks over the whole corpus, plus near-duplicate queries
    Filter {
        #[arg(long)] queries: PathBuf,
        #[arg(long)] qvec: PathBuf,
        #[arg(long)] vectors: PathBuf,
        #[arg(long, default_value = "gemma")] model: String,
        /// corpus modality the queries are ranked against: text (chunks) or image (figures)
        #[arg(long, default_value = "text")] modality: String,
        #[arg(long, default_value_t = 100)] cap: usize,
        #[arg(long)] out: PathBuf,
    },
    /// Recipe v2 judge: one call per unit scores its queries (relevance, supported, image_needed, specific, natural)
    Judge {
        #[arg(long, num_args = 1..)] gen: Vec<PathBuf>,
        #[arg(long)] sample: PathBuf,
        #[arg(long)] figs: PathBuf,
        #[arg(long)] endpoint: String,
        #[arg(long)] model: String,
        /// judge label written into every row (e.g. judge-inkling-high)
        #[arg(long)] judge: String,
        #[arg(long, default_value_t = 128)] concurrency: usize,
        #[arg(long, default_value_t = 8192)] max_tokens: u32,
        #[arg(long, default_value_t = 0.3)] temperature: f64,
        #[arg(long, default_value_t = 1.0)] top_p: f64,
        #[arg(long, default_value = "{}")] extra: String,
        #[arg(long)] out: PathBuf,
    },
    /// Recipe v2 intrinsic metrics: diversity, opener share, caption/passage copying, rejection and banned rates
    Metrics {
        #[arg(long, num_args = 1..)] gen: Vec<PathBuf>,
        #[arg(long)] sample: PathBuf,
        #[arg(long)] figs: PathBuf,
        #[arg(long)] out: PathBuf,
    },
    /// Parquet export (one file per arm) for the Hugging Face dataset: query + spec, target, generator provenance,
    /// EG2 round-trip ranks, licence, split
    Export {
        /// filter dirs holding queries.jsonl + ranks.jsonl
        #[arg(long, num_args = 1..)] filters: Vec<PathBuf>,
        /// dirs whose sub-dirs are arms with generations.jsonl (recipe v2 slot specs)
        #[arg(long, num_args = 0..)] gen_roots: Vec<PathBuf>,
        #[arg(long)] generators: PathBuf,
        /// split manifest (articles.jsonl.gz) and records (records.jsonl.gz, licence)
        #[arg(long)] manifest: PathBuf,
        #[arg(long)] records: PathBuf,
        /// figure samples (any number), to label figure targets
        #[arg(long, num_args = 0..)] figs: Vec<PathBuf>,
        #[arg(long)] out: PathBuf,
    },
    /// Add corpus metadata (year, article type, PubMed publication types, licence) to a sample's articles.jsonl,
    /// streaming the corpus records once (recipe v3.1 instruction constraints and release provenance).
    AnnotateArticles {
        #[arg(long)] articles: PathBuf,
        /// full-corpus records.jsonl.gz (splits build)
        #[arg(long)] records: PathBuf,
        #[arg(long)] out: PathBuf,
    },
    /// Spend and budget of a hosted API key (LiteLLM-style /key/info), key read from a file; prints no key material
    ApiSpend {
        /// API root of a LiteLLM-style hosted endpoint
        #[arg(long)] root: String,
        #[arg(long)] key_file: PathBuf,
    },
    /// Recipe v3 persona library as JSONL (deterministic; docs/RETRIEVER_QUERY_RECIPE_V3.md §3)
    Personas {
        #[arg(long, default_value_t = recipe3::LIBRARY_SIZE)] n: usize,
        #[arg(long)] out: PathBuf,
    },
    /// Source-free query judge for any arm (standalone, specificity, exam framing, naturalness, register)
    Judge3 {
        #[arg(long, num_args = 1..)] gen: Vec<PathBuf>,
        #[arg(long)] endpoint: String,
        #[arg(long)] model: String,
        #[arg(long, default_value_t = 128)] concurrency: usize,
        #[arg(long, default_value_t = 4096)] max_tokens: u32,
        #[arg(long, default_value_t = 0.3)] temperature: f64,
        #[arg(long, default_value_t = 1.0)] top_p: f64,
        #[arg(long, default_value_t = 20)] per_call: usize,
        #[arg(long, default_value = "{}")] extra: String,
        #[arg(long)] out: PathBuf,
    },
    /// v2-vs-v3 pilot analysis: keep rates (both gates), diversity, register mix, judge3 rates, realism vs real
    /// query sets, 40-query sample and blinded hand-read table
    Pilot3 {
        #[arg(long)] queries: PathBuf,
        #[arg(long)] filter: PathBuf,
        #[arg(long)] qvec: PathBuf,
        #[arg(long, num_args = 1..)] gen: Vec<PathBuf>,
        #[arg(long)] judge: Option<PathBuf>,
        /// real query sets, name=flashembed_out_dir (eval only)
        #[arg(long, num_args = 0..)] refs: Vec<String>,
        #[arg(long)] out: PathBuf,
    },
    /// Lay eval set from authors' plain-language summaries of held-out articles in one chunk shard
    PlsShard {
        #[arg(long)] chunks: PathBuf,
        #[arg(long)] manifest: PathBuf,
        #[arg(long, default_value = "pigmed-pls-v1")] seed: String,
        #[arg(long)] out: PathBuf,
    },
    /// Evaluation-only Claude Sonnet 5.5 judge (Batches API) on a sample of kept queries per arm; hard $ cap via ledger
    ClaudeJudge {
        #[arg(long)] queries: PathBuf,
        #[arg(long)] filter: PathBuf,
        #[arg(long, num_args = 1..)] gen: Vec<PathBuf>,
        #[arg(long)] sample: PathBuf,
        #[arg(long, default_value_t = 400)] per_arm: usize,
        #[arg(long)] key_file: PathBuf,
        #[arg(long)] ledger: PathBuf,
        #[arg(long)] cap_usd: f64,
        #[arg(long, default_value = "v3pilot")] label: String,
        /// write the requests and the estimate, submit nothing
        #[arg(long)] dry_run: bool,
        #[arg(long, default_value_t = 60)] poll_s: u64,
        #[arg(long, default_value_t = 20000)] max_wait_s: u64,
        #[arg(long)] out: PathBuf,
    },
    /// Keep-rates, quality proxies, review sample
    Report {
        #[arg(long)] sample: PathBuf,
        #[arg(long)] queries: PathBuf,
        #[arg(long)] filter: PathBuf,
        #[arg(long, num_args = 1..)] gen: Vec<PathBuf>,
        #[arg(long, default_value_t = 30)] review: usize,
        #[arg(long)] out: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Sample { manifest, records, atlas, items_root, articles, chunks, min_words, max_article_words, seed, out } => {
            let (s, arts_full) = sample::run(&sample::SampleArgs { manifest: &manifest, records: &records, atlas: &atlas,
                items_root: &items_root, articles, chunks_per_article: chunks, min_words, max_article_words, seed: &seed })?;
            write_jsonl(&out.with_file_name("articles.jsonl"), &arts_full)?;
            let arts: std::collections::HashSet<&str> = s.iter().map(|c| c.article_id.as_str()).collect();
            let clusters: std::collections::HashSet<&str> = s.iter().map(|c| c.cluster.as_str()).collect();
            write_jsonl(&out, &s)?;
            println!("{}", json!({"chunks": s.len(), "articles": arts.len(), "clusters": clusters.len(),
                "mean_words": s.iter().map(|c| c.words).sum::<usize>() as f64 / s.len().max(1) as f64}));
        }
        Cmd::PairsKeep { manifest, exclude_records, out } => {
            let rows = pairs::keep_list(&manifest, exclude_records.as_deref())?;
            let body: String = rows.iter().map(|(a, g, j, ok)| format!("{a}\t{g}\t{j}\t{}\n", *ok as u8)).collect();
            std::fs::write(&out, body)?;
            println!("{}", json!({"keep": rows.len(), "abs_ok": rows.iter().filter(|r| r.3).count()}));
        }
        Cmd::PairsShard { keep, chunks, seed, out } => {
            let k = pairs::read_keep(&keep)?;
            let (mut t, mut a, mut rows) = (vec![], vec![], vec![]);
            for f in &chunks {
                let (t1, a1, r1) = pairs::shard(f, &k, &seed)?;
                t.extend(t1); a.extend(a1); rows.extend(r1);
            }
            std::fs::create_dir_all(&out)?;
            write_jsonl(&out.join("title.jsonl"), &t)?;
            write_jsonl(&out.join("abssent.jsonl"), &a)?;
            std::fs::write(out.join("articles.tsv"), rows.join("\n") + if rows.is_empty() { "" } else { "\n" })?;
            println!("{}", json!({"articles": rows.len(), "title": t.len(), "abssent": a.len()}));
        }
        Cmd::PairsMergeStream { shards, records, seed, out } => {
            let rep = pairs::merge_stream(&shards, &records, &seed, &out)?;
            println!("{}", serde_json::to_string_pretty(&rep)?);
        }
        Cmd::PairsMerge { shards, cap, corpus, seed, out } => {
            println!("{}", serde_json::to_string_pretty(&pairs::merge(&shards, cap, corpus, &seed, &out)?)?);
        }
        Cmd::Eligible { manifest, records, min_words, out } => {
            let ids = samplehf::eligible(&manifest, &records, min_words)?;
            std::fs::write(&out, ids.join("\n") + "\n")?;
            println!("{}", json!({"eligible": ids.len()}));
        }
        Cmd::SampleHf { eligible, chunks, rate, chunks_per_article, max_article_words, seed, out } => {
            let el: std::collections::HashSet<String> = std::fs::read_to_string(&eligible)?.lines()
                .filter(|l| !l.is_empty()).map(str::to_string).collect();
            let args = samplehf::HfArgs { eligible: &el, rate, chunks_per_article, max_article_words, seed: &seed };
            let (mut all_c, mut all_a) = (vec![], vec![]);
            for f in &chunks {
                let (c, a) = samplehf::run(f, &args)?;
                all_c.extend(c);
                all_a.extend(a);
            }
            std::fs::create_dir_all(&out)?;
            write_jsonl(&out.join("articles.jsonl"), &all_a)?; // write_jsonl is tmp + rename
            write_jsonl(&out.join("chunks.jsonl"), &all_c)?;
            println!("{}", json!({"articles": all_a.len(), "chunks": all_c.len(), "shards": chunks.len()}));
        }
        Cmd::Generate { sample, endpoint, model, arm, concurrency, max_tokens, temperature, top_p, extra, limit, layout, articles, gpus, out } => {
            let mut chunks: Vec<SampleChunk> = read_jsonl(&sample)?;
            if let Some(n) = limit {
                chunks.truncate(n);
            }
            let arts: Vec<sample::Article> = match articles { Some(p) => read_jsonl(&p)?, None => vec![] };
            anyhow::ensure!(matches!(layout, generate::Layout::Abstract | generate::Layout::Figure) || !arts.is_empty(), "layouts b and c need --articles");
            let extra: Value = serde_json::from_str(&extra).context("--extra")?;
            let max_tokens = if matches!(layout, generate::Layout::PerArticle | generate::Layout::TextV2 | generate::Layout::TextV3If | generate::Layout::TextV3Pf | generate::Layout::TextV3Np) { max_tokens * 6 } else { max_tokens };
            let a = generate::GenArgs { endpoint, model, arm, concurrency, max_tokens, temperature, top_p, extra, layout, gpus };
            let (gens, summary) = if matches!(layout, generate::Layout::FigBlind | generate::Layout::FigGrounded) {
                let mut figs: Vec<figure::FigureSample> = read_jsonl(&sample)?;
                if let Some(n) = limit { figs.truncate(n); }
                recipe::run_figs(&a, &figs)?
            } else if layout == generate::Layout::TextV2 {
                recipe::run_text(&a, &chunks, &arts)?
            } else if matches!(layout, generate::Layout::TextV3If | generate::Layout::TextV3Pf | generate::Layout::TextV3Np) {
                recipe3::run_text3(&a, &chunks, &arts)?
            } else if layout == generate::Layout::Figure {
                let mut figs: Vec<figure::FigureSample> = read_jsonl(&sample)?;
                if let Some(n) = limit { figs.truncate(n); }
                generate::run_figures(&a, &figs)?
            } else {
                generate::run(&a, &chunks, &arts)?
            };
            write_jsonl(&out.join("generations.jsonl"), &gens)?;
            write_json(&out.join("summary.json"), &summary)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Cmd::Baseline { sample, arm, out } => {
            let chunks: Vec<SampleChunk> = read_jsonl(&sample)?;
            let gens: Vec<Generation> = chunks.iter().map(|c| generate::Generation {
                chunk_id: c.chunk_id.clone(), article_id: c.article_id.clone(), arm: arm.clone(), ok: true,
                queries: generate::copy_baseline(c), ..Default::default()
            }).collect();
            write_jsonl(&out.join("generations.jsonl"), &gens)?;
            write_json(&out.join("summary.json"), &json!({"arm": arm, "requests": gens.len(), "ok": gens.len()}))?;
        }
        Cmd::FigSample { manifest, records, atlas, items_root, xml_cache, vectors, per_type, max_side, seed, out } => {
            // figures that have an EG2 image vector (the round-trip corpus)
            let mut image_ids = std::collections::HashSet::new();
            for e in std::fs::read_dir(vectors.join("gemma"))? {
                let d = e?.path();
                if !d.join("meta.json").exists() { continue; }
                let ids: Value = serde_json::from_str(&std::fs::read_to_string(d.join("ids.json"))?)?;
                for (u, m) in ids["unit_id"].as_array().into_iter().flatten().zip(ids["modality"].as_array().into_iter().flatten()) {
                    if m == "image" { if let Some(u) = u.as_str() { image_ids.insert(u.to_string()); } }
                }
            }
            let image_dir = out.join("figs");
            let (figs, pairs) = figure::run(&figure::FigArgs { manifest: &manifest, records: &records, atlas: &atlas,
                items_root: &items_root, xml_cache: &xml_cache, image_dir: &image_dir, image_ids: &image_ids, per_type, max_side, seed: &seed })?;
            let mut by_type: std::collections::BTreeMap<String, usize> = Default::default();
            for f in &figs { *by_type.entry(f.section.clone()).or_default() += 1; }
            write_jsonl(&out.join("figsample.jsonl"), &figs)?;
            write_jsonl(&out.join("fig2text_pairs.jsonl"), &pairs)?;
            println!("{}", json!({"figures": figs.len(), "by_type": by_type, "fig2text_pairs": pairs.len(), "image_corpus": image_ids.len()}));
        }
        Cmd::Fig2text { pairs, vectors, model, out } => {
            let text = filter::load_corpus(&vectors, &model, "text")?;
            let img = filter::load_corpus(&vectors, &model, "image")?;
            let rows: Vec<Value> = read_jsonl(&pairs)?;
            let (mut metas, mut q) = (vec![], vec![]);
            for r in &rows {
                let (Some(f), Some(c)) = (r["query_figure"].as_str(), r["positive_chunk"].as_str()) else { continue };
                let Some(&i) = img.index.get(f) else { continue };
                q.extend_from_slice(&img.vecs[i * img.dim..(i + 1) * img.dim]);
                metas.push(QueryMeta { qid: format!("fig2text-{f}"), arm: "fig2text".into(), qtype: r["figure_type"].as_str().unwrap_or("").into(),
                    chunk_id: c.to_string(), article_id: r["article_id"].as_str().unwrap_or("").into(), text: String::new(), task: String::new() });
            }
            let ranks = filter::ranks(&text, &q, &metas, 100);
            let share = |f: &dyn Fn(&Ranks) -> bool| ranks.iter().filter(|r| r.chunk_rank > 0 && f(r)).count() as f64 / ranks.len().max(1) as f64;
            let s = json!({"pairs": ranks.len(), "chunk_top10_corpus": share(&|r| r.chunk_rank <= 10), "chunk_top100_corpus": share(&|r| r.chunk_rank <= 100),
                "within_article_top3": share(&|r| r.within_rank <= 3), "article_top3": share(&|r| r.article_rank <= 3)});
            write_jsonl(&out.join("fig2text_ranks.jsonl"), &ranks)?;
            write_json(&out.join("fig2text.json"), &s)?;
            println!("{s}");
        }
        Cmd::Queries { gen, out } => {
            let mut metas = vec![];
            for g in gen {
                for x in read_jsonl::<Generation>(&g.join("generations.jsonl"))?.into_iter().filter(|x| x.ok) {
                    for (t, v) in &x.queries {
                        if let Some(q) = v.as_str() {
                            let sm = &x.meta["slots"][t.as_str()];
                            metas.push(QueryMeta { qid: format!("{}-{}-{}", x.arm, t, x.chunk_id), arm: x.arm.clone(),
                                qtype: sm["intent"].as_str().unwrap_or(t).to_string(), chunk_id: x.chunk_id.clone(),
                                article_id: x.article_id.clone(), text: q.trim().to_string(),
                                task: sm["task"].as_str().unwrap_or("").to_string() });
                        }
                    }
                }
            }
            let fe: Vec<Value> = metas.iter().map(|m| json!({"id": m.qid, "text": format!("task: search result | query: {}", m.text)})).collect();
            write_jsonl(&out.join("queries.jsonl"), &metas)?;
            write_jsonl(&out.join("flashembed.jsonl"), &fe)?;
            println!("{}", json!({"queries": metas.len()}));
        }
        Cmd::Judge { gen, sample, figs, endpoint, model, judge, concurrency, max_tokens, temperature, top_p, extra, out } => {
            let chunks: std::collections::HashMap<String, SampleChunk> = read_jsonl::<SampleChunk>(&sample)?.into_iter().map(|c| (c.chunk_id.clone(), c)).collect();
            let figs: std::collections::HashMap<String, figure::FigureSample> = read_jsonl::<figure::FigureSample>(&figs)?.into_iter().map(|f| (f.chunk_id.clone(), f)).collect();
            let extra: Value = serde_json::from_str(&extra).context("--extra")?;
            std::fs::create_dir_all(&out)?;
            for g in gen {
                let gens: Vec<Generation> = read_jsonl(&g.join("generations.jsonl"))?;
                let arm = gens.first().map(|x| x.arm.clone()).unwrap_or_default();
                let a = generate::GenArgs { endpoint: endpoint.clone(), model: model.clone(), arm: arm.clone(), concurrency, max_tokens,
                    temperature, top_p, extra: extra.clone(), layout: generate::Layout::Abstract, gpus: 1 };
                let (rows, summary) = recipe::judge(&a, &judge, &gens, &chunks, &figs);
                write_jsonl(&out.join(format!("{arm}.jsonl")), &rows)?;
                write_json(&out.join(format!("{arm}.summary.json")), &summary)?;
                println!("{}", serde_json::to_string(&summary)?);
            }
        }
        Cmd::Export { filters, gen_roots, generators, manifest, records, figs, out } => {
            let table = export::load_table(&generators)?;
            let mut articles: std::collections::HashMap<String, export::ArticleInfo> = std::collections::HashMap::new();
            for v in read_jsonl::<Value>(&manifest)? {
                if let Some(id) = v["article_id"].as_str() {
                    articles.insert(id.to_string(), export::ArticleInfo { split: v["split"].as_str().unwrap_or("").into(),
                        license: String::new(), commercial: v["commercial"].as_bool().unwrap_or(false) });
                }
            }
            for v in read_jsonl::<Value>(&records)? {
                if let Some(x) = v["article_id"].as_str().and_then(|id| articles.get_mut(id)) {
                    x.license = v["license_code"].as_str().unwrap_or("").into();
                }
            }
            let mut figmap: std::collections::HashMap<String, figure::FigureSample> = std::collections::HashMap::new();
            for f in &figs {
                for x in read_jsonl::<figure::FigureSample>(f)? {
                    figmap.insert(x.chunk_id.clone(), x);
                }
            }
            let gens = export::load_gens(&gen_roots)?;
            let inp = export::Inputs { table: &table, articles: &articles, figs: &figmap, gens: &gens };
            std::fs::create_dir_all(&out)?;
            let mut skipped = std::collections::BTreeMap::new();
            let mut written = serde_json::Map::new();
            for d in &filters {
                for (arm, batch) in export::rows_by_arm(d, &inp, &mut skipped)? {
                    anyhow::ensure!(!written.contains_key(&arm), "arm {arm} appears in more than one filter dir");
                    export::write_parquet(&out.join(format!("{arm}.parquet")), &batch)?;
                    if written.is_empty() {
                        eprintln!("--- first row of {arm}\n{}", export::show_row(&batch, 0));
                    }
                    written.insert(arm, json!(batch.num_rows()));
                }
            }
            let summary = json!({"arms": written, "skipped_arms": skipped});
            write_json(&out.join("export_summary.json"), &summary)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Cmd::Metrics { gen, sample, figs, out } => {
            let chunks: std::collections::HashMap<String, SampleChunk> = read_jsonl::<SampleChunk>(&sample)?.into_iter().map(|c| (c.chunk_id.clone(), c)).collect();
            let figs: std::collections::HashMap<String, figure::FigureSample> = read_jsonl::<figure::FigureSample>(&figs)?.into_iter().map(|f| (f.chunk_id.clone(), f)).collect();
            let mut all: Vec<Generation> = vec![];
            for g in gen {
                all.extend(read_jsonl::<Generation>(&g.join("generations.jsonl"))?);
            }
            let m = recipe::metrics(&all, &chunks, &figs);
            write_json(&out, &m)?;
            println!("{}", serde_json::to_string_pretty(&m)?);
        }
        Cmd::Filter { queries, qvec, vectors, model, modality, cap, out } => {
            let metas: Vec<QueryMeta> = read_jsonl(&queries.join("queries.jsonl"))?;
            let t0 = std::time::Instant::now();
            let corpus = filter::load_corpus(&vectors, &model, &modality)?;
            eprintln!("corpus {} text units, {} articles, dim {} ({:.0}s)", corpus.unit_ids.len(), corpus.articles.len(), corpus.dim, t0.elapsed().as_secs_f64());
            let (rows, cols, mut qv) = filter::read_npy(&qvec.join("embeddings.dense.npy"))?;
            let ids: Vec<String> = serde_json::from_str(&std::fs::read_to_string(qvec.join("embeddings.ids.json"))?)?;
            anyhow::ensure!(rows == ids.len() && cols == corpus.dim, "query vectors {rows}x{cols} vs {} ids, corpus dim {}", ids.len(), corpus.dim);
            filter::normalize(&mut qv, cols);
            // align query vectors with metas
            let pos: std::collections::HashMap<&str, usize> = ids.iter().enumerate().map(|(i, x)| (x.as_str(), i)).collect();
            let metas: Vec<QueryMeta> = metas.into_iter().filter(|m| pos.contains_key(m.qid.as_str())).collect();
            let mut q = Vec::with_capacity(metas.len() * cols);
            for m in &metas {
                let i = pos[m.qid.as_str()];
                q.extend_from_slice(&qv[i * cols..(i + 1) * cols]);
            }
            let r: Vec<Ranks> = filter::ranks(&corpus, &q, &metas, cap);
            eprintln!("ranked {} queries ({:.0}s)", r.len(), t0.elapsed().as_secs_f64());
            let dups = filter::duplicate_queries(&q, cols, &metas, 0.95);
            let dup_rows: Vec<Value> = dups.iter().map(|(i, j, x)| json!({"a": metas[*i].qid, "b": metas[*j].qid, "cos": x})).collect();
            write_jsonl(&out.join("ranks.jsonl"), &r)?;
            write_jsonl(&out.join("duplicates.jsonl"), &dup_rows)?;
            write_json(&out.join("filter.json"), &json!({"corpus_units": corpus.unit_ids.len(), "queries": r.len(),
                "duplicates_cos95": dups.len(), "seconds": t0.elapsed().as_secs_f64()}))?;
        }
        Cmd::Report { sample, queries, filter, gen, review, out } => {
            let chunks: Vec<SampleChunk> = read_jsonl(&sample)?;
            let metas: Vec<QueryMeta> = read_jsonl(&queries.join("queries.jsonl"))?;
            let ranks: Vec<Ranks> = read_jsonl(&filter.join("ranks.jsonl"))?;
            let idx: std::collections::HashMap<&str, usize> = metas.iter().enumerate().map(|(i, m)| (m.qid.as_str(), i)).collect();
            let dups: Vec<(usize, usize, f32)> = read_jsonl::<Value>(&filter.join("duplicates.jsonl"))?.iter().filter_map(|d| {
                Some((*idx.get(d["a"].as_str()?)?, *idx.get(d["b"].as_str()?)?, d["cos"].as_f64()? as f32))
            }).collect();
            let sums: Vec<Value> = gen.iter().filter_map(|g| serde_json::from_str(&std::fs::read_to_string(g.join("summary.json")).ok()?).ok()).collect();
            let (rep, md, blind, key) = report::build(&chunks, &metas, &ranks, &dups, &sums, review);
            write_json(&out.join("report.json"), &rep)?;
            std::fs::write(out.join("review.md"), md)?;
            std::fs::write(out.join("review_blind.md"), blind)?;
            write_json(&out.join("review_blind_key.json"), &key)?;
            println!("{}", serde_json::to_string_pretty(&rep)?);
        }
        Cmd::AnnotateArticles { articles, records, out } => {
            let mut arts: Vec<sample::Article> = read_jsonl(&articles)?;
            let want: std::collections::HashMap<String, usize> = arts.iter().enumerate().map(|(i, a)| (a.article_id.clone(), i)).collect();
            let mut found = 0;
            for line in io::reader(&records)?.lines() {
                let line = line?;
                // cheap prefilter before parsing: the id is the first field
                let Some(id) = line.get(15..).and_then(|x| x.split('"').next()) else { continue };
                let Some(&i) = want.get(id) else { continue };
                let v: Value = serde_json::from_str(&line)?;
                let a = &mut arts[i];
                a.year = v["dates"].as_array().into_iter().flatten().filter_map(|d| d.as_str()?.get(..4)?.parse::<i32>().ok()).min();
                a.article_type = v["article_type"].as_str().unwrap_or("").to_string();
                a.pub_types = v["pub_types"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
                a.license_code = v["license_code"].as_str().unwrap_or("").to_string();
                a.commercial = v["commercial"].as_bool();
                found += 1;
            }
            write_jsonl(&out, &arts)?;
            let commercial = arts.iter().filter(|a| a.commercial == Some(true)).count();
            let with_cons = arts.iter().filter(|a| !recipe3::constraints(a).is_empty()).count();
            println!("{}", json!({"articles": arts.len(), "annotated": found, "commercial": commercial, "with_constraints": with_cons}));
        }
        Cmd::ApiSpend { root, key_file } => {
            let key = std::fs::read_to_string(&key_file).context("key file")?.trim().to_string();
            let ag = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(30)).build();
            let mut out = json!({"ok": false});
            for path in ["/key/info", "/v1/key/info"] {
                match ag.get(&format!("{}{path}", root.trim_end_matches('/'))).set("Authorization", &format!("Bearer {key}")).call() {
                    Ok(r) => {
                        let v: Value = r.into_json()?;
                        let i = if v["info"].is_object() { &v["info"] } else { &v };
                        // only spend / budget fields: never the key, its hash or its alias
                        out = json!({"ok": true, "path": path, "spend": i["spend"], "max_budget": i["max_budget"],
                            "budget_reset_at": i["budget_reset_at"], "budget_duration": i["budget_duration"],
                            "tpm_limit": i["tpm_limit"], "rpm_limit": i["rpm_limit"], "models": i["models"]});
                        break;
                    }
                    Err(ureq::Error::Status(c, _)) => out = json!({"ok": false, "path": path, "status": c}),
                    Err(e) => out = json!({"ok": false, "path": path, "error": e.to_string().replace(&key, "***")}),
                }
            }
            println!("{out}");
        }
        Cmd::Personas { n, out } => {
            let l = persona::library(recipe3::LIBRARY_SEED, n);
            let rows: Vec<Value> = l.iter().map(|p| { let mut v = serde_json::to_value(p).unwrap(); v["card"] = json!(p.card()); v }).collect();
            write_jsonl(&out, &rows)?;
            println!("{}", json!({"personas": rows.len()}));
        }
        Cmd::Judge3 { gen, endpoint, model, concurrency, max_tokens, temperature, top_p, per_call, extra, out } => {
            let extra: Value = serde_json::from_str(&extra).context("--extra")?;
            std::fs::create_dir_all(&out)?;
            for g in gen {
                let gens: Vec<Generation> = read_jsonl(&g.join("generations.jsonl"))?;
                let arm = gens.first().map(|x| x.arm.clone()).unwrap_or_default();
                let a = generate::GenArgs { endpoint: endpoint.clone(), model: model.clone(), arm: arm.clone(), concurrency, max_tokens,
                    temperature, top_p, extra: extra.clone(), layout: generate::Layout::Abstract, gpus: 1 };
                let (rows, summary) = recipe3::judge3(&a, &gens, per_call);
                write_jsonl(&out.join(format!("{arm}.jsonl")), &rows)?;
                write_json(&out.join(format!("{arm}.summary.json")), &summary)?;
                println!("{}", serde_json::to_string(&summary)?);
            }
        }
        Cmd::Pilot3 { queries, filter, qvec, gen, judge, refs, out } => {
            let metas: Vec<QueryMeta> = read_jsonl(&queries.join("queries.jsonl"))?;
            let ranks: Vec<Ranks> = read_jsonl(&filter.join("ranks.jsonl"))?;
            let qemb = pilot3::load_emb(&qvec)?;
            let mut gens: Vec<Generation> = vec![];
            for g in &gen {
                gens.extend(read_jsonl::<Generation>(&g.join("generations.jsonl"))?);
            }
            let judge = match judge { Some(j) => pilot3::load_judge(&j)?, None => Default::default() };
            let mut rs = vec![];
            for r in refs {
                let (name, dir) = r.split_once('=').context("--refs name=dir")?;
                rs.push((name.to_string(), pilot3::load_emb(std::path::Path::new(dir))?));
            }
            let (rep, md, blind, key) = pilot3::analyse(&pilot3::Inputs { metas: &metas, ranks: &ranks, qemb: &qemb, gens: &gens, judge: &judge, refs: &rs });
            std::fs::create_dir_all(&out)?;
            write_json(&out.join("pilot3.json"), &rep)?;
            std::fs::write(out.join("sample40.md"), md)?;
            std::fs::write(out.join("handread_blind.md"), blind)?;
            write_json(&out.join("handread_key.json"), &key)?;
            println!("{}", serde_json::to_string_pretty(&rep)?);
        }
        Cmd::PlsShard { chunks, manifest, seed, out } => {
            let held = pls::heldout(&manifest)?;
            let rows = pls::shard(&chunks, &held, &seed)?;
            write_jsonl(&out, &rows)?;
            println!("{}", json!({"heldout_articles": held.len(), "queries": rows.len()}));
        }
        Cmd::ClaudeJudge { queries, filter, gen, sample, per_arm, key_file, ledger, cap_usd, label, dry_run, poll_s, max_wait_s, out } => {
            let metas: Vec<QueryMeta> = read_jsonl(&queries.join("queries.jsonl"))?;
            let ranks: Vec<Ranks> = read_jsonl(&filter.join("ranks.jsonl"))?;
            let mut gens: Vec<Generation> = vec![];
            for g in &gen {
                gens.extend(read_jsonl::<Generation>(&g.join("generations.jsonl"))?);
            }
            let items = claude::select(&metas, &ranks, &gens, per_arm);
            let (reqs, map) = claude::requests(&items, &claude::load_chunks(&sample)?);
            std::fs::create_dir_all(&out)?;
            let est = claude::estimate(&reqs, &map);
            write_json(&out.join("plan.json"), &json!({"items": items.len(), "requests": reqs.len(), "estimate_usd": est,
                "ledger_usd": claude::ledger_spent(&ledger)?, "cap_usd": cap_usd, "model": claude::MODEL}))?;
            if dry_run {
                println!("{}", json!({"items": items.len(), "requests": reqs.len(), "estimate_usd": est}));
                return Ok(());
            }
            let (rows, cost) = claude::run(reqs, &map, &key_file, &ledger, cap_usd, &label, poll_s, max_wait_s)?;
            write_jsonl(&out.join("ratings.jsonl"), &rows)?;
            let sum = json!({"cost_usd": cost, "ledger_usd": claude::ledger_spent(&ledger)?, "arms": claude::summarise(&items, &rows)});
            write_json(&out.join("summary.json"), &sum)?;
            println!("{}", serde_json::to_string_pretty(&sum)?);
        }
    }
    Ok(())
}
