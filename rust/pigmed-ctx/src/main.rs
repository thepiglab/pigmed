//! pigmed-ctx: retriever S2 (contextual) and S3 (multimodal) training data from the published v2 chunks / media
//! shards and the PMC JATS (formats match what the S1/S2 trainer in train/ reads).
//!
//!   shard          --chunks C.parquet [--media M.parquet] --keep keep.tsv [--exclude F]... [--xml-dir D] [--no-net]
//!                  [--tokenizer tokenizer.json] [--windows] --out DIR
//!   cite-resolve   --shards DIR --records records.jsonl.gz --keep keep.tsv [--exclude F]... --out DIR
//!   fetch-figs     --figures SHARD/figures.jsonl.gz --out IMAGES/part-NNNNN [--max-side 768] [--limit N] [--quality 90]
//!   abssent-ctx    --pairs abssent.jsonl.gz [--exclude F]... --out ctx_abssent.jsonl.gz
//!   xml-ctx        --xml FILE.xml[.gz] [--article-id ID]        (debug: print the parsed context as JSON)
mod cite;
mod fetch;
mod io;
mod shard;
mod units;
mod xmlctx;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// One published chunk shard -> articles, caption ctx_queries, figures, citation contexts (+ optional windows)
    Shard {
        #[arg(long)] chunks: PathBuf,
        #[arg(long)] media: Option<PathBuf>,
        /// keep.tsv from `pigmed-synth pairs-keep` (train && commercial && !retracted; article, group, journal, abs_ok)
        #[arg(long)] keep: PathBuf,
        /// article ids to hold out (e.g. the S3 figure test set), one per line
        #[arg(long)] exclude: Vec<PathBuf>,
        /// local JATS (<article_id>.xml[.gz]) before the public S3 bucket
        #[arg(long)] xml_dir: Option<PathBuf>,
        #[arg(long)] no_net: bool,
        #[arg(long)] tokenizer: Option<PathBuf>,
        #[arg(long, default_value_t = 1.5)] per_word: f64,
        #[arg(long)] windows: bool,
        /// articles.jsonl.gz without chunk text (trainer joins text from the chunk parquet)
        #[arg(long)] ids_only: bool,
        /// held-out shard: no figures / caption queries
        #[arg(long)] no_figures: bool,
        #[arg(long, default_value_t = 8064)] max_tokens: usize,
        #[arg(long, default_value_t = 768)] prefix_tokens: usize,
        #[arg(long, default_value_t = 2048)] overlap_tokens: usize,
        #[arg(long, default_value_t = 16)] threads: usize,
        #[arg(long)] limit: Option<usize>,
        #[arg(long)] out: PathBuf,
    },
    /// Citation context -> cited article pairs across all shards (S1-style source: queries, docs, pairs)
    CiteResolve {
        #[arg(long)] shards: PathBuf,
        #[arg(long)] records: PathBuf,
        #[arg(long)] keep: PathBuf,
        #[arg(long)] exclude: Vec<PathBuf>,
        #[arg(long, default_value_t = 3)] per_pair: usize,
        #[arg(long, default_value_t = 20)] per_article: usize,
        #[arg(long)] out: PathBuf,
    },
    /// Fetch, md5-check and resize one shard's figures (resumable)
    FetchFigs {
        #[arg(long)] figures: PathBuf,
        #[arg(long)] out: PathBuf,
        #[arg(long, default_value_t = 768)] max_side: u32,
        #[arg(long, default_value_t = 90)] quality: u8,
        #[arg(long, default_value_t = 32)] threads: usize,
        #[arg(long)] limit: Option<usize>,
        #[arg(long)] local: Option<PathBuf>,
    },
    /// Debug: parsed JATS context (figures, refs, cited sentences) of one file
    XmlCtx {
        #[arg(long)] xml: PathBuf,
        #[arg(long, default_value = "")] article_id: String,
    },
    /// Natural abstract-sentence pairs (pigmed-synth pairs-merge-stream) -> S2 ctx_queries rows (kind=abssent)
    AbssentCtx {
        #[arg(long)] pairs: PathBuf,
        #[arg(long)] exclude: Vec<PathBuf>,
        #[arg(long)] out: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Shard { chunks, media, keep, exclude, xml_dir, no_net, tokenizer, per_word, windows, ids_only, no_figures, max_tokens, prefix_tokens, overlap_tokens, threads, limit, out } => {
            let a = shard::ShardArgs { chunks, media, keep, exclude, xml: shard::XmlSource::new(xml_dir, !no_net), tokenizer, per_word,
                win: shard::WinCfg { max_tokens, prefix_tokens, overlap_tokens }, threads, limit, windows, ids_only, no_figures, out };
            println!("{}", serde_json::to_string_pretty(&shard::run(&a)?)?);
        }
        Cmd::CiteResolve { shards, records, keep, exclude, per_pair, per_article, out } => {
            let a = cite::CiteArgs { shards, records, keep, exclude, per_pair, per_article, out };
            println!("{}", serde_json::to_string_pretty(&cite::run(&a)?)?);
        }
        Cmd::FetchFigs { figures, out, max_side, quality, threads, limit, local } => {
            let a = fetch::FetchArgs { figures, out, max_side, quality, threads, limit, local };
            println!("{}", serde_json::to_string_pretty(&fetch::run(&a)?)?);
        }
        Cmd::XmlCtx { xml, article_id } => {
            use std::io::Read;
            let mut s = String::new();
            io::reader(&xml)?.read_to_string(&mut s)?;
            println!("{}", serde_json::to_string_pretty(&xmlctx::parse(&s, &article_id)?)?);
        }
        Cmd::AbssentCtx { pairs, exclude, out } => {
            use std::io::BufRead;
            let ex = shard::read_ids(&exclude)?;
            let mut w = io::JsonlOut::create(&out)?;
            let mut skipped = 0;
            for line in io::reader(&pairs)?.lines() {
                let r: serde_json::Value = serde_json::from_str(&line?)?;
                let aid = r["article_id"].as_str().unwrap_or("");
                if ex.contains(aid) || ex.contains(aid.split('.').next().unwrap_or("")) {
                    skipped += 1;
                    continue;
                }
                w.put(&serde_json::json!({"id": r["qid"], "text": r["query"], "article_id": aid, "pos": r["pos"], "kind": "abssent",
                    "group": r["group"], "pmcid": r["pmcid"], "doi": r["doi"], "license_code": r["license_code"]}))?;
            }
            println!("{}", serde_json::json!({"rows": w.finish()?, "excluded": skipped}));
        }
    }
    Ok(())
}
