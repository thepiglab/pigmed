//! `pigmed-rs`: Rust acquisition path for PigMed (plan + download + canonical bundles).
//! Bundles are read by the Python tools (`pigmed publish|verify|reembed|search`) unchanged.
mod audit;
mod bundle;
mod census;
mod chunk;
mod common;
mod convert;
mod guidelines;
mod jats;
mod media;
mod net;
mod pdfbench;
mod pipeline;
mod probe;
mod pubmed;
mod sources;
mod supp;
mod supplements;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pigmed-rs", version, about = "PigMed bulk PMC/PubMed acquisition (Rust)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Immutable PMC plan from the current S3 inventory (latest version per PMCID, all years).
    PlanPmc {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        inventory_manifest: Option<String>,
        #[arg(long, default_value_t = 2000)]
        per_part: usize,
        #[arg(long, default_value_t = 1)]
        sample_mod: u128,
        #[arg(long, default_value_t = 0)]
        sample_rem: u128,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        all_versions: bool,
        /// Drop PMCIDs whose hash % EXCLUDE_MOD is in EXCLUDE_REM (e.g. samples already processed): --exclude-mod 37
        /// --exclude-rem 0,1. Applied after the sample filter.
        #[arg(long, default_value_t = 1)]
        exclude_mod: u128,
        #[arg(long, value_delimiter = ',')]
        exclude_rem: Vec<u128>,
        #[arg(long, default_value = "")]
        contact: String,
    },
    /// One partition per PubMed baseline/update archive.
    PlanPubmed {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        updates: bool,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Ingest one plan partition into immutable bundles under ROOT.
    Ingest {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Articles processed concurrently (committed in manifest order).
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// Simultaneous HTTP requests from this process.
        #[arg(long, default_value_t = 16)]
        http_concurrency: usize,
        /// Minimum spacing between request starts in this process (ms). 0 = no pacing.
        #[arg(long, default_value_t = 5)]
        request_interval_ms: u64,
        /// Abort the partition on any content error (Python behaviour).
        #[arg(long)]
        strict: bool,
        /// JSON object of RunConfig overrides, e.g. '{"render_pages":false}'.
        #[arg(long, default_value = "{}")]
        set: String,
    },
    /// Measure PMC supplementary files in existing bundles: formats, sizes, conversion yield.
    ProbeSupplements {
        /// Ingest root whose bundles carry `kind: media` rows.
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Files sampled per extension (deterministic).
        #[arg(long, default_value_t = 40)]
        per_ext: usize,
        #[arg(long, default_value_t = 268435456)]
        max_bytes: u64,
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
    },
    /// Time supplementary-PDF conversion (pdf-inspector stages, per page, pdfium text) on the slowest
    /// and a sample of ordinary PDFs recorded in existing bundles. Prints one JSON line per file.
    PdfBench {
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value_t = 12)]
        slow: usize,
        #[arg(long, default_value_t = 40)]
        sample: usize,
        #[arg(long, default_value_t = 60)]
        max_pages: u32,
    },
    /// Metadata-only media census over plan partitions; writes a plan of articles carrying WANT.
    MediaCensus {
        #[arg(long, num_args = 1.., required = true)]
        manifests: Vec<PathBuf>,
        /// Modality class to collect (image, video, audio, pdf, spreadsheet, ...).
        #[arg(long, default_value = "audio")]
        want: String,
        #[arg(long, default_value_t = 40)]
        limit: usize,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 64)]
        concurrency: usize,
    },
    /// Supplement extraction fidelity: re-convert failed + sampled ok files, compare to a reference.
    SuppAudit {
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value_t = 40)]
        per_ext_ok: usize,
        #[arg(long, default_value_t = 0)]
        ocr_pages: usize,
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
        /// Write ours/reference texts (and the PDF) for low-coverage PDFs here.
        #[arg(long)]
        dump: Option<PathBuf>,
    },
    /// Re-chunk committed bundles' body text with a chunk recipe (balanced-c = scripts/chunk_variants.py recipe C)
    /// into OUT_ROOT/runs/RUN/ready/<bundle>. Pass bundles directly, or --src-root to do every ready bundle.
    Rechunk {
        #[arg(long)]
        out_root: PathBuf,
        #[arg(long, default_value = "balanced-c")]
        run: String,
        #[arg(long)]
        src_root: Option<PathBuf>,
        #[arg(long, default_value_t = 8)]
        jobs: usize,
        #[arg(long, default_value_t = 384)]
        target: usize,
        #[arg(long, default_value_t = 100)]
        min: usize,
        #[arg(long, default_value_t = 512)]
        max: usize,
        #[arg(long, default_value_t = 48)]
        overlap: usize,
        /// Top-level heading in the model input (`embed_text`).
        #[arg(long)]
        heading_prefix: bool,
        /// Keep only the re-chunked body-text rows.
        #[arg(long)]
        body_only: bool,
        /// Also write one late-chunking `late_doc` row per article (spans = packed chunk ids) to a same-named bundle
        /// under LATE_OUT/runs/<run>/ready/ (written before the naive bundle).
        #[arg(long)]
        late_out: Option<PathBuf>,
        bundles: Vec<PathBuf>,
    },
    /// List every S3 object (key, size) of the articles in plan partitions (one ListObjectsV2 per article
    /// prefix). Writes TSV `key<TAB>size` per partition into OUT (skips partitions already listed).
    ListSizes {
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 64)]
        concurrency: usize,
        manifests: Vec<PathBuf>,
    },
    /// Check every file of a committed bundle against its manifest.
    Verify { bundle: PathBuf },
    /// Multimodal backfill pilot: download + ffprobe (+ one 32 s clip) for each JSONL row (`url`, `md5`); JSONL out.
    ProbeMedia {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
        #[arg(long)]
        clip_test: bool,
    },
    /// Multimodal backfill pilot: download + supplement converter (pdfium text, OCR) (+ page renders) per PDF row.
    ProbePdf {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        #[arg(long, default_value_t = 30)]
        ocr_pages: usize,
        #[arg(long, default_value_t = 0)]
        render_pages: usize,
        #[arg(long, default_value_t = 1024)]
        longest: u32,
    },
    /// Debug: run the supplement converter on one local file and print backend, detected format and output size.
    ConvertFile {
        path: PathBuf,
        /// Extension as PMC lists it (default: from the file name), e.g. .xls
        #[arg(long)]
        ext: Option<String>,
        #[arg(long, default_value_t = 0)]
        ocr_pages: usize,
        /// Spreadsheets: skip anydoc and use the calamine path directly.
        #[arg(long)]
        calamine_only: bool,
        /// Write the full Markdown here (used by the isolated spreadsheet converter).
        #[arg(long)]
        md_out: Option<PathBuf>,
    },
    /// Clinical practice guidelines: list / plan / probe / ingest (see docs/GUIDELINES.md).
    Guidelines {
        #[command(subcommand)]
        cmd: guidelines::Cmd,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let res: Result<Value> = async {
        match cli.cmd {
            Cmd::PlanPmc { out, inventory_manifest, per_part, sample_mod, sample_rem, limit, all_versions, exclude_mod, exclude_rem, contact } => {
                let http = net::Http::new(&contact, 16, 0)?;
                let scratch = tempfile::Builder::new().prefix("pigmed-inventory-").tempdir()?;
                sources::plan_pmc(&out, &http, sources::PlanArgs { manifest_url: inventory_manifest, per_part, sample_mod, sample_rem, limit, all_versions, exclude_mod, exclude_rem }, scratch.path()).await
            }
            Cmd::PlanPubmed { out, updates, limit } => {
                let http = net::Http::new("", 4, 100)?;
                sources::plan_pubmed(&out, &http, updates, limit).await
            }
            Cmd::Ingest { manifest, root, config, concurrency, http_concurrency, request_interval_ms, strict, set } => {
                let overrides: Map<String, Value> = serde_json::from_str(&set)?;
                let cfg = pipeline::load_config(config.as_deref(), &overrides)?;
                pipeline::run(&manifest, &root, cfg, concurrency, http_concurrency, request_interval_ms, strict).await
            }
            Cmd::PdfBench { root, slow, sample, max_pages } => pdfbench::run(&root, slow, sample, max_pages).await,
            Cmd::MediaCensus { manifests, want, limit, out, concurrency } => census::run(&manifests, &want, limit, &out, concurrency).await,
            Cmd::SuppAudit { root, per_ext_ok, ocr_pages, concurrency, dump } => audit::run(&root, per_ext_ok, ocr_pages, concurrency, dump).await,
            Cmd::Rechunk { out_root, run, src_root, jobs, target, min, max, overlap, heading_prefix, body_only, late_out, mut bundles } => {
                let r = chunk::Recipe { target, min, max, overlap, heading_prefix };
                if let Some(src) = src_root {
                    for e in std::fs::read_dir(src.join("runs"))? {
                        let ready = e?.path().join("ready");
                        if let Ok(rd) = std::fs::read_dir(&ready) {
                            for b in rd {
                                let p = b?.path();
                                if p.join("manifest.json").exists() && !p.to_string_lossy().ends_with(".partial") {
                                    bundles.push(p);
                                }
                            }
                        }
                    }
                }
                bundles.sort();
                let next = std::sync::atomic::AtomicUsize::new(0);
                let results = std::sync::Mutex::new(Vec::new());
                std::thread::scope(|s| {
                    for _ in 0..jobs.max(1) {
                        s.spawn(|| loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            if i >= bundles.len() {
                                break;
                            }
                            let v = chunk::rechunk_bundle(&bundles[i], &out_root, &run, r, body_only, late_out.as_deref())
                                .unwrap_or_else(|e| json!({"bundle": bundles[i].display().to_string(), "error": format!("{e:#}")}));
                            results.lock().unwrap().push(v);
                        });
                    }
                });
                let res = results.into_inner().unwrap();
                let errors: Vec<&Value> = res.iter().filter(|v| v.get("error").is_some()).collect();
                let sum = |k: &str| res.iter().filter_map(|v| v[k].as_u64()).sum::<u64>();
                let out = json!({"bundles": res.len(), "errors": errors, "text_in": sum("text_in"), "packed_out": sum("packed_out"),
                                 "recipe": r.tag()});
                if !errors.is_empty() {
                    anyhow::bail!("rechunk errors: {}", out);
                }
                Ok(out)
            }
            Cmd::ListSizes { out, concurrency, manifests } => {
                use futures::StreamExt;
                std::fs::create_dir_all(&out)?;
                let http = net::Http::new("", concurrency, 0)?;
                let (mut objects, mut bytes) = (0u64, 0u64);
                for m in manifests {
                    let dest = out.join(format!("{}.tsv", m.file_stem().unwrap().to_string_lossy()));
                    if dest.exists() {
                        continue;
                    }
                    let rows: Vec<Value> = common::read_jsonl(&m)?;
                    let res: Vec<Result<String>> = futures::stream::iter(rows.into_iter().map(|r| {
                        let http = http.clone();
                        async move {
                            let pre = format!("{}.{}/", r["pmcid"].as_str().unwrap_or(""), r["version"]);
                            let mut sizes = Vec::new();
                            let keys = http.list_s3_sized(&pre, None, Some(&mut sizes)).await?;
                            let mut s = String::new();
                            for ((k, _), z) in keys.iter().zip(sizes) {
                                if let Some(k) = k {
                                    s.push_str(&format!("{k}\t{z}\n"));
                                }
                            }
                            Ok::<_, anyhow::Error>(s)
                        }
                    }))
                    .buffer_unordered(concurrency)
                    .collect()
                    .await;
                    let mut text = String::new();
                    for r in res {
                        text.push_str(&r?);
                    }
                    for l in text.lines() {
                        objects += 1;
                        bytes += l.rsplit('\t').next().and_then(|z| z.parse::<u64>().ok()).unwrap_or(0);
                    }
                    let tmp = dest.with_extension("tsv.partial");
                    std::fs::write(&tmp, text)?;
                    std::fs::rename(&tmp, &dest)?;
                }
                Ok(json!({"objects": objects, "bytes": bytes}))
            }
            Cmd::Verify { bundle } => bundle::verify_bundle(&bundle),
            Cmd::ProbeMedia { input, out, concurrency, clip_test } => {
                let scratch = tempfile::Builder::new().prefix("pigmed-probe-media-").tempdir()?;
                probe::probe_media(&input, &out, concurrency, clip_test, scratch.path()).await
            }
            Cmd::ProbePdf { input, out, concurrency, ocr_pages, render_pages, longest } => {
                let scratch = tempfile::Builder::new().prefix("pigmed-probe-pdf-").tempdir()?;
                probe::probe_pdf(&input, &out, concurrency, ocr_pages, render_pages, longest, scratch.path()).await
            }
            Cmd::ConvertFile { path, ext, ocr_pages, calamine_only, md_out } => {
                let ext = ext.unwrap_or_else(|| crate::media::extension(&path.to_string_lossy()));
                let t0 = std::time::Instant::now();
                let r = if calamine_only { convert::excel_calamine_pub(&path, &ext) } else { convert::convert(&path, &ext, convert::Opts { ocr_pages }) };
                if let (Ok(c), Some(p)) = (&r, &md_out) {
                    std::fs::write(p, &c.md)?;
                }
                Ok(match r {
                    Ok(c) => json!({"ok": true, "tool": c.tool, "detected": c.detected, "md_bytes": c.md.len(), "pages": c.pages,
                                    "seconds": t0.elapsed().as_secs_f64(), "head": c.md.chars().take(200).collect::<String>()}),
                    Err(e) => json!({"ok": false, "error": e, "seconds": t0.elapsed().as_secs_f64()}),
                })
            }
            Cmd::Guidelines { cmd } => guidelines::run(cmd).await,
            Cmd::ProbeSupplements { root, out, per_ext, max_bytes, concurrency } => supp::probe(&root, &out, per_ext, max_bytes, concurrency).await,
        }
    }
    .await;
    match res {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        Err(e) => {
            let code = if e.downcast_ref::<bundle::Backpressure>().is_some() { 75 } else { 1 };
            eprintln!("{}", json!({"error": format!("{e:#}")}));
            std::process::exit(code);
        }
    }
}
