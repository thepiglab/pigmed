//! `pigmed-export`: turn embedded PigMed bundles into Parquet shards for a Hugging Face dataset.
//!
//! One `batch` call reads N bundles (`items.jsonl.gz` + `manifest.json`) and their EmbeddingGemma 2 vector shards
//! (`<vectors>/<model>/<bundle>/{dense.npy,ids.json,meta.json}`), checks that each vector shard was made from exactly
//! this bundle (meta.manifest_sha256 == sha256(manifest.json)) and that every vector id is a row of the bundle, and
//! writes three zstd Parquet files plus a receipt:
//!   chunks/<name>.parquet   text units (body chunks, tables, captions, supplement text, table cards)
//!   vectors/<name>.parquet  unit_id, article_id, modality, embedding (768 x float16)
//!   media/<name>.parquet    figures, media files and A/V clips: S3 URL, md5, bytes (from `list-sizes` TSVs), type
//!   <name>.json             receipt: bundles, row counts, file sizes and sha256
use anyhow::{bail, ensure, Context, Result};
use arrow::array::{
    ArrayRef, BooleanBuilder, FixedSizeListArray, Float16Array, Float64Builder, Int64Builder, ListBuilder, RecordBatch,
    StringBuilder,
};
use arrow::datatypes::{DataType, Field, Schema};
use clap::{Parser, Subcommand};
use flate2::read::MultiGzDecoder;
use half::f16;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const TEXT_KINDS: [&str; 5] = ["text", "table_text", "caption", "supplement_text", "table_card"];
const MEDIA_KINDS: [&str; 4] = ["figure", "media", "video", "audio"];

#[derive(Parser)]
#[command(name = "pigmed-export", about = "PigMed bundles + vectors -> Parquet shards")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Export bundles (paths to bundle dirs) into one shard per config named NAME under OUT.
    Batch {
        #[arg(long)]
        vectors: PathBuf,
        #[arg(long, default_value = "gemma")]
        model: String,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        name: String,
        /// Directory of `key<TAB>size` TSVs (pigmed-rs list-sizes) for media byte sizes.
        #[arg(long)]
        sizes: Option<PathBuf>,
        #[arg(long, default_value_t = 768)]
        dim: usize,
        /// Late-chunking vector shards (same layout; span ids = unit ids). Each naive vector with a late twin is
        /// replaced by normalize(naive + BETA * late).
        #[arg(long)]
        late_vectors: Option<PathBuf>,
        #[arg(long, default_value_t = 1.0)]
        late_beta: f32,
        /// Recipe id written to every row (`recipe` column), e.g. v2.
        #[arg(long)]
        recipe: Option<String>,
        bundles: Vec<PathBuf>,
    },
    /// Re-write an already exported shard (IN/{chunks,vectors,media}/NAME.parquet + optional IN/NAME.json receipt) with
    /// the licence columns of the current schema: chunks + commercial_use_ok; media + license_code, commercial_use_ok;
    /// vectors + license, license_code, commercial_use_ok (by article, from the shard's chunks, else its media rows).
    /// Writes OUT/... with the same NAME, plus pending/supplement_pdfs (article PDF rows) and a new receipt.
    /// Title-only article units from exported chunk shards (one bundle per input file, named after its stem):
    /// unit_id "art|<article_id>", kind text, text = whitespace-collapsed title (or "none"),
    /// embed_text "title: none | text: <title>" (title in the TEXT slot), first title of each article's kind=text rows,
    /// licence columns carried. Port of scripts/chunk3_arms.py artvec --title-only. Writes OUT/runs/RUN/ready/<stem>.
    Titles {
        #[arg(long)]
        out_root: PathBuf,
        #[arg(long, default_value = "pmcv2t")]
        run: String,
        inputs: Vec<PathBuf>,
    },
    Relicense {
        #[arg(long = "in")]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        name: String,
    },
}

fn sha256_file(p: &Path) -> Result<String> {
    let mut h = Sha256::new();
    let mut f = File::open(p)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

fn read_rows(bundle: &Path) -> Result<Vec<Map<String, Value>>> {
    let man: Value = serde_json::from_slice(&fs::read(bundle.join("manifest.json"))?)?;
    let items = bundle.join("items.jsonl.gz");
    if let Some(want) = man["files"].as_array().and_then(|f| f.iter().find(|x| x["path"] == "items.jsonl.gz")).and_then(|x| x["sha256"].as_str()) {
        ensure!(sha256_file(&items)? == want, "{}: items.jsonl.gz sha256 mismatch", bundle.display());
    }
    let mut rows = Vec::new();
    for line in BufReader::new(MultiGzDecoder::new(File::open(&items)?)).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Value::Object(m) = serde_json::from_str(&line)? {
            rows.push(m);
        }
    }
    Ok(rows)
}

/// Minimal .npy reader for little-endian float16 2-D arrays.
fn read_npy_f16(p: &Path, dim: usize) -> Result<(usize, Vec<f16>)> {
    let data = fs::read(p)?;
    ensure!(data.len() > 10 && &data[..6] == b"\x93NUMPY", "{}: not an npy file", p.display());
    let (hlen, off) = if data[6] == 1 { (u16::from_le_bytes([data[8], data[9]]) as usize, 10) } else { (u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize, 12) };
    let header = std::str::from_utf8(&data[off..off + hlen])?;
    ensure!(header.contains("'descr': '<f2'") && header.contains("'fortran_order': False"), "{}: unexpected npy header {header}", p.display());
    let shape = header.split("'shape': (").nth(1).and_then(|s| s.split(')').next()).context("shape")?;
    let dims: Vec<usize> = shape.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    let body = &data[off + hlen..];
    let n = dims.first().copied().unwrap_or(0);
    if n == 0 {
        return Ok((0, vec![]));
    }
    ensure!(dims.len() == 2 && dims[1] == dim, "{}: shape {shape} != (n, {dim})", p.display());
    ensure!(body.len() == n * dim * 2, "{}: truncated npy body", p.display());
    let v = body.chunks_exact(2).map(|b| f16::from_bits(u16::from_le_bytes([b[0], b[1]]))).collect();
    Ok((n, v))
}

fn s<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(|v| v.as_str())
}

fn loc<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a Value> {
    m.get("locator").and_then(|l| l.get(k))
}

fn str_list(v: Option<&Value>) -> Option<Vec<Option<String>>> {
    v.and_then(|x| x.as_array()).map(|a| a.iter().map(|x| x.as_str().map(String::from)).collect())
}

/// s3://bucket/key?md5=... -> (key, md5)
/// Commercial-use subset of PMC OA (same rule as pigmed-ingest common::commercial_license): CC0, CC BY, CC BY-SA,
/// CC BY-ND, after normalising case, separators and a trailing version ("cc by-nd 4.0" -> "CC-BY-ND").
fn commercial_code(code: Option<&str>) -> bool {
    let v = code.unwrap_or("").trim().to_uppercase().replace('_', "-");
    let mut v = v.split_whitespace().collect::<Vec<_>>().join("-");
    for suf in ["-1.0", "-2.0", "-3.0", "-4.0", "-1", "-2", "-3", "-4"] {
        if let Some(x) = v.strip_suffix(suf) {
            v = x.to_string();
            break;
        }
    }
    matches!(v.as_str(), "CC0" | "CC-0" | "PUBLIC-DOMAIN" | "CC-BY" | "BY" | "CC-BY-SA" | "CC-BY-ND")
}

fn commercial_use_ok(r: &Map<String, Value>) -> bool {
    commercial_code(s(r, "license_code").or_else(|| s(r, "license")))
}

/// Held-out media file rows (not downloaded/embedded in this release): video + audio files, and PDFs (article and
/// supplementary). Returns the pending list name.
fn pending_of(r: &Map<String, Value>) -> Option<usize> {
    if s(r, "kind") != Some("media") {
        return None;
    }
    if matches!(s(r, "modality"), Some("video") | Some("audio")) {
        return Some(0);
    }
    let ext = s(r, "extension").unwrap_or("");
    if ext.trim_start_matches('.').eq_ignore_ascii_case("pdf") || s(r, "role") == Some("article_pdf") {
        return Some(1);
    }
    None
}

/// Pipeline recipe id written to every chunks/vectors/media row (`--recipe`, e.g. "v2").
static RECIPE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
fn recipe() -> Option<&'static str> {
    RECIPE.get().and_then(|r| r.as_deref())
}

const PENDING: [&str; 2] = ["pending/videos", "pending/supplement_pdfs"];

fn s3_key_md5(url: &str) -> (Option<String>, Option<String>) {
    let (base, q) = url.split_once('?').unwrap_or((url, ""));
    let key = base.strip_prefix("s3://").and_then(|r| r.split_once('/')).map(|(_, k)| k.to_string());
    let md5 = q.split('&').find_map(|kv| kv.strip_prefix("md5=")).map(String::from);
    (key, md5)
}

struct Chunks {
    unit_id: StringBuilder, article_id: StringBuilder, pmcid: StringBuilder, pmid: StringBuilder, doi: StringBuilder,
    title: StringBuilder, kind: StringBuilder, text: StringBuilder, section: ListBuilder<StringBuilder>,
    section_ids: ListBuilder<StringBuilder>, char_start: Int64Builder, char_end: Int64Builder, is_abstract: BooleanBuilder,
    token_count: Int64Builder, license: StringBuilder, license_code: StringBuilder, chunk_recipe: StringBuilder,
    source_url: StringBuilder, supplement_file: StringBuilder, figure_id: StringBuilder, source_drift: BooleanBuilder,
    commercial_use_ok: BooleanBuilder, recipe: StringBuilder,
}

impl Chunks {
    fn new() -> Self {
        Chunks { unit_id: StringBuilder::new(), article_id: StringBuilder::new(), pmcid: StringBuilder::new(), pmid: StringBuilder::new(),
            doi: StringBuilder::new(), title: StringBuilder::new(), kind: StringBuilder::new(), text: StringBuilder::new(),
            section: ListBuilder::new(StringBuilder::new()), section_ids: ListBuilder::new(StringBuilder::new()),
            char_start: Int64Builder::new(), char_end: Int64Builder::new(), is_abstract: BooleanBuilder::new(),
            token_count: Int64Builder::new(), license: StringBuilder::new(), license_code: StringBuilder::new(),
            chunk_recipe: StringBuilder::new(), source_url: StringBuilder::new(), supplement_file: StringBuilder::new(),
            figure_id: StringBuilder::new(), source_drift: BooleanBuilder::new(), commercial_use_ok: BooleanBuilder::new(), recipe: StringBuilder::new() }
    }
    fn schema() -> Arc<Schema> {
        let u = |n: &str, nullable: bool| Field::new(n, DataType::Utf8, nullable);
        let list = |n: &str| Field::new(n, DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))), true);
        Arc::new(Schema::new(vec![u("unit_id", false), u("article_id", false), u("pmcid", true), u("pmid", true), u("doi", true),
            u("title", true), u("kind", false), u("text", false), list("section"), list("section_ids"),
            Field::new("char_start", DataType::Int64, true), Field::new("char_end", DataType::Int64, true),
            Field::new("is_abstract", DataType::Boolean, true), Field::new("token_count", DataType::Int64, true),
            u("license", true), u("license_code", true), u("chunk_recipe", true), u("source_url", true),
            u("supplement_file", true), u("figure_id", true), Field::new("source_drift", DataType::Boolean, false),
            Field::new("commercial_use_ok", DataType::Boolean, false), u("recipe", true)]))
    }
    fn push(&mut self, r: &Map<String, Value>) {
        self.unit_id.append_value(s(r, "unit_id").unwrap_or(""));
        self.article_id.append_value(s(r, "article_id").unwrap_or(""));
        self.pmcid.append_option(s(r, "pmcid"));
        self.pmid.append_option(s(r, "pmid").filter(|x| !x.is_empty()));
        self.doi.append_option(s(r, "doi").filter(|x| !x.is_empty()));
        self.title.append_option(s(r, "title"));
        self.kind.append_value(s(r, "kind").unwrap_or(""));
        self.text.append_value(s(r, "text").unwrap_or(""));
        match str_list(loc(r, "section")) {
            Some(v) => self.section.append_value(v),
            None => self.section.append_null(),
        }
        let sids = str_list(loc(r, "section_ids")).or_else(|| loc(r, "section_id").and_then(|v| v.as_str()).map(|x| vec![Some(x.to_string())]));
        match sids {
            Some(v) => self.section_ids.append_value(v),
            None => self.section_ids.append_null(),
        }
        self.char_start.append_option(loc(r, "char_start").and_then(|v| v.as_i64()));
        self.char_end.append_option(loc(r, "char_end").and_then(|v| v.as_i64()));
        self.is_abstract.append_option(r.get("is_abstract").and_then(|v| v.as_bool()));
        self.token_count.append_option(r.get("token_count").and_then(|v| v.as_i64()));
        self.license.append_option(s(r, "license"));
        self.license_code.append_option(s(r, "license_code"));
        self.chunk_recipe.append_option(r.get("chunk_recipe").filter(|v| !v.is_null()).map(|v| v.to_string()));
        self.source_url.append_option(s(r, "source_url"));
        self.supplement_file.append_option(s(r, "supplement_file"));
        self.figure_id.append_option(loc(r, "figure_id").and_then(|v| v.as_str()));
        self.source_drift.append_value(r.get("source_drift").and_then(|v| v.as_bool()).unwrap_or(false));
        self.commercial_use_ok.append_value(commercial_use_ok(r));
        self.recipe.append_option(recipe());
    }
    fn finish(&mut self) -> Result<RecordBatch> {
        let cols: Vec<ArrayRef> = vec![Arc::new(self.unit_id.finish()), Arc::new(self.article_id.finish()), Arc::new(self.pmcid.finish()),
            Arc::new(self.pmid.finish()), Arc::new(self.doi.finish()), Arc::new(self.title.finish()), Arc::new(self.kind.finish()),
            Arc::new(self.text.finish()), Arc::new(self.section.finish()), Arc::new(self.section_ids.finish()),
            Arc::new(self.char_start.finish()), Arc::new(self.char_end.finish()), Arc::new(self.is_abstract.finish()),
            Arc::new(self.token_count.finish()), Arc::new(self.license.finish()), Arc::new(self.license_code.finish()),
            Arc::new(self.chunk_recipe.finish()), Arc::new(self.source_url.finish()), Arc::new(self.supplement_file.finish()),
            Arc::new(self.figure_id.finish()), Arc::new(self.source_drift.finish()), Arc::new(self.commercial_use_ok.finish()), Arc::new(self.recipe.finish())];
        Ok(RecordBatch::try_new(Self::schema(), cols)?)
    }
}

struct Media {
    unit_id: StringBuilder, article_id: StringBuilder, pmcid: StringBuilder, kind: StringBuilder, role: StringBuilder,
    modality: StringBuilder, extension: StringBuilder, file_name: StringBuilder, s3_url: StringBuilder, md5: StringBuilder,
    bytes: Int64Builder, caption: StringBuilder, figure_id: StringBuilder, start_seconds: Float64Builder,
    end_seconds: Float64Builder, license: StringBuilder, license_code: StringBuilder, commercial_use_ok: BooleanBuilder, recipe: StringBuilder,
}

impl Media {
    fn new() -> Self {
        Media { unit_id: StringBuilder::new(), article_id: StringBuilder::new(), pmcid: StringBuilder::new(), kind: StringBuilder::new(),
            role: StringBuilder::new(), modality: StringBuilder::new(), extension: StringBuilder::new(), file_name: StringBuilder::new(),
            s3_url: StringBuilder::new(), md5: StringBuilder::new(), bytes: Int64Builder::new(), caption: StringBuilder::new(),
            figure_id: StringBuilder::new(), start_seconds: Float64Builder::new(), end_seconds: Float64Builder::new(),
            license: StringBuilder::new(), license_code: StringBuilder::new(), commercial_use_ok: BooleanBuilder::new(), recipe: StringBuilder::new() }
    }
    fn schema() -> Arc<Schema> {
        let u = |n: &str, nullable: bool| Field::new(n, DataType::Utf8, nullable);
        Arc::new(Schema::new(vec![u("unit_id", false), u("article_id", false), u("pmcid", true), u("kind", false), u("role", true),
            u("modality", true), u("extension", true), u("file_name", true), u("s3_url", true), u("md5", true),
            Field::new("bytes", DataType::Int64, true), u("caption", true), u("figure_id", true),
            Field::new("start_seconds", DataType::Float64, true), Field::new("end_seconds", DataType::Float64, true), u("license", true),
            u("license_code", true), Field::new("commercial_use_ok", DataType::Boolean, false), u("recipe", true)]))
    }
    fn push(&mut self, r: &Map<String, Value>, sizes: &HashMap<String, u64>) {
        let url = s(r, "source_url").unwrap_or("");
        let (key, qmd5) = s3_key_md5(url);
        let kind = s(r, "kind").unwrap_or("");
        let modality = s(r, "modality").map(String::from).or_else(|| match kind {
            "figure" => Some("image".into()),
            "video" | "audio" => Some(kind.into()),
            _ => None,
        });
        self.unit_id.append_value(s(r, "unit_id").unwrap_or(""));
        self.article_id.append_value(s(r, "article_id").unwrap_or(""));
        self.pmcid.append_option(s(r, "pmcid"));
        self.kind.append_value(kind);
        self.role.append_option(s(r, "role"));
        self.modality.append_option(modality);
        self.extension.append_option(s(r, "extension"));
        self.file_name.append_option(s(r, "file_name").or_else(|| loc(r, "href").and_then(|v| v.as_str())));
        self.s3_url.append_option(key.as_ref().map(|k| format!("s3://pmc-oa-opendata/{k}")));
        self.md5.append_option(s(r, "source_md5").map(String::from).or(qmd5));
        self.bytes.append_option(key.as_ref().and_then(|k| sizes.get(k)).map(|&b| b as i64));
        self.caption.append_option(s(r, "caption"));
        self.figure_id.append_option(loc(r, "figure_id").and_then(|v| v.as_str()));
        self.start_seconds.append_option(loc(r, "start_seconds").and_then(|v| v.as_f64()));
        self.end_seconds.append_option(loc(r, "end_seconds").and_then(|v| v.as_f64()));
        self.license.append_option(s(r, "license"));
        self.license_code.append_option(s(r, "license_code"));
        self.commercial_use_ok.append_value(commercial_use_ok(r));
        self.recipe.append_option(recipe());
    }
    fn finish(&mut self) -> Result<RecordBatch> {
        let cols: Vec<ArrayRef> = vec![Arc::new(self.unit_id.finish()), Arc::new(self.article_id.finish()), Arc::new(self.pmcid.finish()),
            Arc::new(self.kind.finish()), Arc::new(self.role.finish()), Arc::new(self.modality.finish()), Arc::new(self.extension.finish()),
            Arc::new(self.file_name.finish()), Arc::new(self.s3_url.finish()), Arc::new(self.md5.finish()), Arc::new(self.bytes.finish()),
            Arc::new(self.caption.finish()), Arc::new(self.figure_id.finish()), Arc::new(self.start_seconds.finish()),
            Arc::new(self.end_seconds.finish()), Arc::new(self.license.finish()), Arc::new(self.license_code.finish()),
            Arc::new(self.commercial_use_ok.finish()), Arc::new(self.recipe.finish())];
        Ok(RecordBatch::try_new(Self::schema(), cols)?)
    }
}

fn vec_schema(dim: usize) -> Arc<Schema> {
    Arc::new(Schema::new(vec![Field::new("unit_id", DataType::Utf8, false), Field::new("article_id", DataType::Utf8, false),
        Field::new("modality", DataType::Utf8, false),
        Field::new("embedding", DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float16, false)), dim as i32), false),
        Field::new("license", DataType::Utf8, true), Field::new("license_code", DataType::Utf8, true),
        Field::new("commercial_use_ok", DataType::Boolean, false), Field::new("recipe", DataType::Utf8, true)]))
}

fn props(dictionary: bool) -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        .set_dictionary_enabled(dictionary)
        .set_max_row_group_size(65536)
        .build()
}

fn load_sizes(dir: &Path) -> Result<HashMap<String, u64>> {
    let mut m = HashMap::new();
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        if p.extension().map(|x| x == "tsv").unwrap_or(false) {
            for line in BufReader::new(File::open(&p)?).lines() {
                let line = line?;
                if let Some((k, z)) = line.rsplit_once('\t') {
                    if let Ok(z) = z.parse() {
                        m.insert(k.to_string(), z);
                    }
                }
            }
        }
    }
    Ok(m)
}

/// normalize(a + beta * b), computed in f32.
fn mix_into(a: &mut [f16], b: &[f16], beta: f32) {
    let v: Vec<f32> = a.iter().zip(b).map(|(x, y)| x.to_f32() + beta * y.to_f32()).collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    for (o, x) in a.iter_mut().zip(v) {
        *o = f16::from_f32(x / n);
    }
}

#[allow(clippy::too_many_arguments)]
fn batch(vectors: &Path, model: &str, out: &Path, name: &str, sizes: Option<&Path>, dim: usize, bundles: &[PathBuf],
         late: Option<(&Path, f32)>) -> Result<Value> {
    ensure!(!bundles.is_empty(), "no bundles");
    let sizes = match sizes {
        Some(d) => load_sizes(d)?,
        None => HashMap::new(),
    };
    let configs = ["chunks", "vectors", "media", PENDING[0], PENDING[1]];
    for c in configs {
        fs::create_dir_all(out.join(c))?;
    }
    let paths: Vec<PathBuf> = configs.iter().map(|c| out.join(c).join(format!("{name}.parquet"))).collect();
    let tmp: Vec<PathBuf> = paths.iter().map(|p| p.with_extension("parquet.partial")).collect();
    let mut wc = ArrowWriter::try_new(File::create(&tmp[0])?, Chunks::schema(), Some(props(true)))?;
    let mut wv = ArrowWriter::try_new(File::create(&tmp[1])?, vec_schema(dim), Some(props(false)))?;
    let mut wm = ArrowWriter::try_new(File::create(&tmp[2])?, Media::schema(), Some(props(true)))?;
    let mut wp = [ArrowWriter::try_new(File::create(&tmp[3])?, Media::schema(), Some(props(true)))?,
                  ArrowWriter::try_new(File::create(&tmp[4])?, Media::schema(), Some(props(true)))?];
    let (mut n_chunks, mut n_vec, mut n_media) = (0usize, 0usize, 0usize);
    let mut n_pending = [0usize; 2];
    let mut articles: HashSet<String> = HashSet::new();
    let mut by_modality: HashMap<String, usize> = HashMap::new();
    let mut done = Vec::new();
    let mut late_mixed = 0usize;
    for b in bundles {
        let bname = b.file_name().context("bundle name")?.to_string_lossy().to_string();
        let man_sha = sha256_file(&b.join("manifest.json"))?;
        let vd = vectors.join(model).join(&bname);
        let meta: Value = serde_json::from_slice(&fs::read(vd.join("meta.json")).with_context(|| format!("{bname}: no vector meta"))?)?;
        if meta["manifest_sha256"].as_str() != Some(man_sha.as_str()) {
            bail!("{bname}: vector shard was made from a different bundle manifest");
        }
        let rows = read_rows(b)?;
        let ids: Value = serde_json::from_slice(&fs::read(vd.join("ids.json"))?)?;
        let uids: Vec<&str> = ids["unit_id"].as_array().context("ids.unit_id")?.iter().map(|x| x.as_str().unwrap_or("")).collect();
        let mods: Vec<&str> = ids["modality"].as_array().context("ids.modality")?.iter().map(|x| x.as_str().unwrap_or("")).collect();
        let (n, mut vals) = read_npy_f16(&vd.join("dense.npy"), dim)?;
        ensure!(n == uids.len() && n == mods.len(), "{bname}: {n} vectors vs {} ids", uids.len());
        if let Some((lroot, beta)) = late {
            let ld = lroot.join(model).join(&bname);
            // A bundle without body text has no late docs (no late bundle, no late shard): nothing to mix.
            let lids: Value = match fs::read(ld.join("ids.json")) {
                Ok(b) => serde_json::from_slice(&b)?,
                Err(_) => json!({"unit_id": []}),
            };
            let lids: Vec<&str> = lids["unit_id"].as_array().context("late ids")?.iter().map(|x| x.as_str().unwrap_or("")).collect();
            let (ln, lv) = if lids.is_empty() { (0, vec![]) } else { read_npy_f16(&ld.join("dense.npy"), dim)? };
            ensure!(ln == lids.len(), "{bname}: late shard size");
            let li: HashMap<&str, usize> = lids.iter().enumerate().map(|(i, u)| (*u, i)).collect();
            for (i, u) in uids.iter().enumerate() {
                if let Some(&j) = li.get(u) {
                    mix_into(&mut vals[i * dim..(i + 1) * dim], &lv[j * dim..(j + 1) * dim], beta);
                    late_mixed += 1;
                }
            }
        }
        let aid_of: HashMap<&str, &str> = rows.iter().filter_map(|r| Some((s(r, "unit_id")?, s(r, "article_id").unwrap_or("")))).collect();
        let lic_of: HashMap<&str, &Map<String, Value>> = rows.iter().filter_map(|r| Some((s(r, "unit_id")?, r))).collect();
        let (mut va, mut vl, mut vlc, mut vok, mut vr) = (StringBuilder::new(), StringBuilder::new(), StringBuilder::new(), BooleanBuilder::new(), StringBuilder::new());
        for u in &uids {
            va.append_value(aid_of.get(u).with_context(|| format!("{bname}: vector id {u} not in bundle"))?);
            let r = lic_of[u];
            vl.append_option(s(r, "license"));
            vlc.append_option(s(r, "license_code"));
            vok.append_value(commercial_use_ok(r));
            vr.append_option(recipe());
        }
        let mut chunks = Chunks::new();
        let mut media = Media::new();
        let mut pending = [Media::new(), Media::new()];
        for r in &rows {
            let k = s(r, "kind").unwrap_or("");
            if TEXT_KINDS.contains(&k) && s(r, "text").map(|t| !t.trim().is_empty()).unwrap_or(false) {
                chunks.push(r);
                n_chunks += 1;
                articles.insert(s(r, "article_id").unwrap_or("").to_string());
            } else if MEDIA_KINDS.contains(&k) {
                media.push(r, &sizes);
                n_media += 1;
                if let Some(i) = pending_of(r) {
                    pending[i].push(r, &sizes);
                    n_pending[i] += 1;
                }
            }
        }
        wc.write(&chunks.finish()?)?;
        wm.write(&media.finish()?)?;
        for (w, p) in wp.iter_mut().zip(pending.iter_mut()) {
            w.write(&p.finish()?)?;
        }
        if n > 0 {
            let values = Float16Array::from(vals);
            let field = Arc::new(Field::new("item", DataType::Float16, false));
            let emb = FixedSizeListArray::try_new(field, dim as i32, Arc::new(values), None)?;
            let mut u = StringBuilder::new();
            let mut m = StringBuilder::new();
            for (i, id) in uids.iter().enumerate() {
                u.append_value(id);
                m.append_value(mods[i]);
                *by_modality.entry(mods[i].to_string()).or_default() += 1;
            }
            let rb = RecordBatch::try_new(vec_schema(dim), vec![Arc::new(u.finish()), Arc::new(va.finish()), Arc::new(m.finish()), Arc::new(emb),
                Arc::new(vl.finish()), Arc::new(vlc.finish()), Arc::new(vok.finish()), Arc::new(vr.finish())])?;
            wv.write(&rb)?;
            n_vec += n;
        }
        done.push(json!({"bundle": bname, "manifest_sha256": man_sha, "vector_meta_finished_at": meta["finished_at"]}));
    }
    wc.close()?;
    wv.close()?;
    wm.close()?;
    for w in wp {
        w.close()?;
    }
    let mut files = Vec::new();
    for (i, c) in configs.iter().enumerate() {
        fs::rename(&tmp[i], &paths[i])?;
        let rows = [n_chunks, n_vec, n_media, n_pending[0], n_pending[1]][i];
        files.push(json!({"config": c, "path": format!("{c}/{name}.parquet"), "rows": rows,
                          "bytes": fs::metadata(&paths[i])?.len(), "sha256": sha256_file(&paths[i])?}));
    }
    let receipt = json!({"name": name, "model": model, "dim": dim, "bundles": done, "articles": articles.len(),
                         "chunks": n_chunks, "vectors": n_vec, "vectors_by_modality": by_modality, "media": n_media,
                         "pending_videos": n_pending[0], "pending_supplement_pdfs": n_pending[1], "files": files,
                         "late_mixed": late_mixed, "late_beta": late.map(|x| x.1)});
    let rp = out.join(format!("{name}.json"));
    fs::write(rp.with_extension("json.partial"), serde_json::to_vec_pretty(&receipt)?)?;
    fs::rename(rp.with_extension("json.partial"), &rp)?;
    Ok(receipt)
}

fn read_parquet(p: &Path) -> Result<Vec<RecordBatch>> {
    let f = File::open(p).with_context(|| format!("open {}", p.display()))?;
    let rd = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f)?.build()?;
    Ok(rd.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn str_col<'a>(b: &'a RecordBatch, n: &str) -> Result<&'a arrow::array::StringArray> {
    b.column_by_name(n).with_context(|| format!("column {n}"))?.as_any().downcast_ref::<arrow::array::StringArray>().with_context(|| format!("{n} not utf8"))
}

fn opt(a: &arrow::array::StringArray, i: usize) -> Option<&str> {
    use arrow::array::Array;
    if a.is_null(i) { None } else { Some(a.value(i)) }
}

fn with_cols(b: &RecordBatch, schema: &Arc<Schema>, extra: Vec<ArrayRef>) -> Result<RecordBatch> {
    let mut cols: Vec<ArrayRef> = b.columns().to_vec();
    cols.extend(extra);
    Ok(RecordBatch::try_new(schema.clone(), cols)?)
}

fn relicense(input: &Path, out: &Path, name: &str) -> Result<Value> {
    let p = |dir: &Path, c: &str| dir.join(c).join(format!("{name}.parquet"));
    let chunks = read_parquet(&p(input, "chunks"))?;
    let media = read_parquet(&p(input, "media"))?;
    let vectors = read_parquet(&p(input, "vectors"))?;
    // article -> (license, license_code)
    let mut lic: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    for b in &chunks {
        let (a, l, c) = (str_col(b, "article_id")?, str_col(b, "license")?, str_col(b, "license_code")?);
        for i in 0..b.num_rows() {
            lic.entry(a.value(i).to_string()).or_insert((opt(l, i).map(String::from), opt(c, i).map(String::from)));
        }
    }
    for b in &media {
        let (a, l) = (str_col(b, "article_id")?, str_col(b, "license")?);
        for i in 0..b.num_rows() {
            lic.entry(a.value(i).to_string()).or_insert((opt(l, i).map(String::from), None));
        }
    }
    let ok_of = |l: Option<&str>, c: Option<&str>| commercial_code(c.or(l));
    for c in ["chunks", "vectors", "media", PENDING[1]] {
        fs::create_dir_all(out.join(c))?;
    }
    let (mut n_c, mut n_v, mut n_m, mut n_p) = (0usize, 0usize, 0usize, 0usize);
    // chunks
    let mut wc = ArrowWriter::try_new(File::create(p(out, "chunks"))?, Chunks::schema(), Some(props(true)))?;
    for b in &chunks {
        ensure!(b.num_columns() + 1 == Chunks::schema().fields().len(), "chunks: unexpected column count {}", b.num_columns());
        let (l, c) = (str_col(b, "license")?, str_col(b, "license_code")?);
        let ok: arrow::array::BooleanArray = (0..b.num_rows()).map(|i| Some(ok_of(opt(l, i), opt(c, i)))).collect();
        wc.write(&with_cols(b, &Chunks::schema(), vec![Arc::new(ok)])?)?;
        n_c += b.num_rows();
    }
    wc.close()?;
    // media (+ pending article PDFs)
    let mut wm = ArrowWriter::try_new(File::create(p(out, "media"))?, Media::schema(), Some(props(true)))?;
    let mut wp = ArrowWriter::try_new(File::create(p(out, PENDING[1]))?, Media::schema(), Some(props(true)))?;
    for b in &media {
        ensure!(b.num_columns() + 2 == Media::schema().fields().len(), "media: unexpected column count {}", b.num_columns());
        let (a, l, role) = (str_col(b, "article_id")?, str_col(b, "license")?, str_col(b, "role")?);
        let mut lc = StringBuilder::new();
        let mut ok = BooleanBuilder::new();
        for i in 0..b.num_rows() {
            let code = lic.get(a.value(i)).and_then(|x| x.1.as_deref());
            lc.append_option(code);
            ok.append_value(ok_of(opt(l, i), code));
        }
        let nb = with_cols(b, &Media::schema(), vec![Arc::new(lc.finish()), Arc::new(ok.finish())])?;
        let mask: arrow::array::BooleanArray = (0..b.num_rows()).map(|i| Some(opt(role, i) == Some("article_pdf"))).collect();
        let pb = arrow::compute::filter_record_batch(&nb, &mask)?;
        n_p += pb.num_rows();
        wp.write(&pb)?;
        wm.write(&nb)?;
        n_m += b.num_rows();
    }
    wm.close()?;
    wp.close()?;
    // vectors
    let dim = match vectors.first().map(|b| b.schema().field_with_name("embedding").map(|f| f.data_type().clone())) {
        Some(Ok(DataType::FixedSizeList(_, d))) => d as usize,
        _ => 768,
    };
    let mut wv = ArrowWriter::try_new(File::create(p(out, "vectors"))?, vec_schema(dim), Some(props(false)))?;
    for b in &vectors {
        ensure!(b.num_columns() + 3 == vec_schema(dim).fields().len(), "vectors: unexpected column count {}", b.num_columns());
        let a = str_col(b, "article_id")?;
        let (mut l, mut lc, mut ok) = (StringBuilder::new(), StringBuilder::new(), BooleanBuilder::new());
        for i in 0..b.num_rows() {
            let (x, y) = lic.get(a.value(i)).map(|x| (x.0.as_deref(), x.1.as_deref())).with_context(|| format!("vectors: article {} has no licence in this shard", a.value(i)))?;
            l.append_option(x);
            lc.append_option(y);
            ok.append_value(ok_of(x, y));
        }
        wv.write(&with_cols(b, &vec_schema(dim), vec![Arc::new(l.finish()), Arc::new(lc.finish()), Arc::new(ok.finish())])?)?;
        n_v += b.num_rows();
    }
    wv.close()?;
    let mut files = Vec::new();
    for (c, rows) in [("chunks", n_c), ("vectors", n_v), ("media", n_m), (PENDING[1], n_p)] {
        let f = p(out, c);
        files.push(json!({"config": c, "path": format!("{c}/{name}.parquet"), "rows": rows, "bytes": fs::metadata(&f)?.len(), "sha256": sha256_file(&f)?}));
    }
    let mut receipt = match fs::read(input.join(format!("{name}.json"))) {
        Ok(b) => serde_json::from_slice::<Value>(&b)?,
        Err(_) => json!({"name": name}),
    };
    receipt["files"] = json!(files);
    receipt["relicensed"] = json!({"from_files": receipt.get("files_before").cloned().unwrap_or(Value::Null),
        "added": ["chunks.commercial_use_ok", "media.license_code", "media.commercial_use_ok", "vectors.license",
                  "vectors.license_code", "vectors.commercial_use_ok", "pending/supplement_pdfs (article PDFs)"]});
    receipt["pending_supplement_pdfs"] = json!(n_p);
    let rp = out.join(format!("{name}.json"));
    fs::write(&rp, serde_json::to_vec_pretty(&receipt)?)?;
    Ok(json!({"name": name, "chunks": n_c, "vectors": n_v, "media": n_m, "pending_pdfs": n_p, "articles": lic.len()}))
}

/// Python `re.sub(r"\s+", " ", t).strip()`: Rust's Unicode White_Space plus the ASCII separators \x1c-\x1f that
/// Python's `\s` also matches.
fn ws_collapse(t: &str) -> String {
    t.split(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

fn title_rows(chunks: &[RecordBatch]) -> Result<Vec<Value>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for b in chunks {
        let (aid, kind, title) = (str_col(b, "article_id")?, str_col(b, "kind")?, str_col(b, "title")?);
        let (pmcid, lic, code) = (str_col(b, "pmcid")?, str_col(b, "license")?, str_col(b, "license_code")?);
        for i in 0..b.num_rows() {
            if kind.value(i) != "text" || seen.contains(aid.value(i)) {
                continue;
            }
            seen.insert(aid.value(i).to_string());
            let t = ws_collapse(opt(title, i).unwrap_or(""));
            let t = if t.is_empty() { "none".to_string() } else { t };
            out.push(json!({"unit_id": format!("art|{}", aid.value(i)), "article_id": aid.value(i), "pmcid": opt(pmcid, i),
                "kind": "text", "text": t, "embed_text": format!("title: none | text: {t}"), "title": opt(title, i),
                "license": opt(lic, i), "license_code": opt(code, i),
                "chunk_recipe": {"name": "article_title", "slot": "text"}}));
        }
    }
    Ok(out)
}

fn titles(input: &Path, out_root: &Path, run: &str) -> Result<Value> {
    use std::io::Write;
    let name = input.file_stem().and_then(|s| s.to_str()).context("input name")?.to_string();
    let rows = title_rows(&read_parquet(input)?)?;
    let ready = out_root.join("runs").join(run).join("ready");
    let (tmp, dst) = (ready.join(format!("{name}.partial")), ready.join(&name));
    ensure!(!dst.exists(), "{} exists", dst.display());
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    let mut gz = flate2::write::GzEncoder::new(File::create(tmp.join("items.jsonl.gz"))?, flate2::Compression::new(6));
    for r in &rows {
        writeln!(gz, "{r}")?;
    }
    gz.finish()?;
    let sha = sha256_file(&tmp.join("items.jsonl.gz"))?;
    fs::write(tmp.join("manifest.json"), serde_json::to_vec_pretty(&json!({"bundle": name, "rows": rows.len(), "source": input.file_name().and_then(|s| s.to_str()),
        "unit": "article_title", "files": [{"path": "items.jsonl.gz", "sha256": sha}]}))?)?;
    fs::rename(&tmp, &dst)?;
    Ok(json!({"bundle": name, "articles": rows.len()}))
}

fn main() {
    let cli = Cli::parse();
    let res = match cli.cmd {
        Cmd::Batch { vectors, model, out, name, sizes, dim, late_vectors, late_beta, recipe, bundles } => {
            RECIPE.set(recipe).ok();
            batch(&vectors, &model, &out, &name, sizes.as_deref(), dim, &bundles, late_vectors.as_deref().map(|p| (p, late_beta)))
        }
        Cmd::Relicense { input, out, name } => relicense(&input, &out, &name),
        Cmd::Titles { out_root, run, inputs } => inputs.iter().map(|i| titles(i, &out_root, &run)).collect::<Result<Vec<_>>>().map(|v| json!(v)),
    };
    match res {
        Ok(v) => println!("{}", serde_json::to_string(&v).unwrap()),
        Err(e) => {
            eprintln!("{}", json!({"error": format!("{e:#}")}));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_mix_normalizes() {
        let mut a: Vec<f16> = [1.0f32, 0.0].iter().map(|x| f16::from_f32(*x)).collect();
        let b: Vec<f16> = [0.0f32, 1.0].iter().map(|x| f16::from_f32(*x)).collect();
        mix_into(&mut a, &b, 1.0);
        assert!((a[0].to_f32() - 0.7071).abs() < 1e-3 && (a[1].to_f32() - 0.7071).abs() < 1e-3);
    }

    #[test]
    fn title_units_follow_artvec() {
        let mut b = Chunks::new();
        let rows = [
            json!({"unit_id": "c1", "article_id": "PMC1.1", "pmcid": "PMC1", "kind": "caption", "text": "cap", "title": "Cap  first", "license": "CC BY", "license_code": "cc by"}),
            json!({"unit_id": "c2", "article_id": "PMC1.1", "pmcid": "PMC1", "kind": "text", "text": "body", "title": " A\u{a0}study\n of\x1fX ", "license": "CC BY", "license_code": "cc by"}),
            json!({"unit_id": "c3", "article_id": "PMC1.1", "pmcid": "PMC1", "kind": "text", "text": "more", "title": "ignored", "license": "CC BY", "license_code": "cc by"}),
            json!({"unit_id": "c4", "article_id": "PMC2.1", "pmcid": "PMC2", "kind": "text", "text": "x", "title": null, "license": "CC BY-NC", "license_code": "cc by-nc"}),
        ];
        for r in &rows {
            b.push(r.as_object().unwrap());
        }
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("pmcv2-c0-00001.parquet");
        let mut w = ArrowWriter::try_new(File::create(&f).unwrap(), Chunks::schema(), Some(props(true))).unwrap();
        w.write(&b.finish().unwrap()).unwrap();
        w.close().unwrap();
        let got = titles(&f, &d.path().join("out"), "pmcv2t").unwrap();
        assert_eq!(got["articles"], 2);
        let r = read_rows(&d.path().join("out/runs/pmcv2t/ready/pmcv2-c0-00001")).unwrap();
        assert_eq!(r[0]["unit_id"], "art|PMC1.1");
        assert_eq!(r[0]["embed_text"], "title: none | text: A study of X");
        assert_eq!(r[1]["embed_text"], "title: none | text: none");
        assert_eq!(r[1]["license_code"], "cc by-nc");
    }

    #[test]
    fn commercial_codes() {
        for c in ["CC BY", "cc by-nd 4.0", "CC0", "CC BY-SA", "cc_by"] {
            assert!(commercial_code(Some(c)), "{c}");
        }
        for c in ["CC BY-NC", "CC BY-NC-SA", "CC BY-NC-ND", "NO-CC CODE", "", "other"] {
            assert!(!commercial_code(Some(c)), "{c}");
        }
        assert!(!commercial_code(None));
    }

    #[test]
    fn s3_url_parts() {
        let (k, m) = s3_key_md5("s3://pmc-oa-opendata/PMC1.1/a b.jpg?md5=abc");
        assert_eq!(k.as_deref(), Some("PMC1.1/a b.jpg"));
        assert_eq!(m.as_deref(), Some("abc"));
    }

    #[test]
    fn end_to_end_batch() {
        let d = tempfile::tempdir().unwrap();
        let b = d.path().join("root/runs/r/ready/b1");
        fs::create_dir_all(&b).unwrap();
        let rows = [
            json!({"article_id": "PMC1.1", "pmcid": "PMC1", "kind": "article", "unit_id": "a0"}),
            json!({"article_id": "PMC1.1", "pmcid": "PMC1", "kind": "text", "unit_id": "t1", "text": "hello world", "license": "CC-BY",
                   "locator": {"section_ids": ["s1"], "section": ["Intro"], "char_start": 0, "char_end": 11},
                   "chunk_recipe": {"name": "balanced-c"}}),
            json!({"article_id": "PMC1.1", "pmcid": "PMC1", "kind": "figure", "unit_id": "f1", "caption": "cap", "license": "CC-BY",
                   "source_url": "s3://pmc-oa-opendata/PMC1.1/f.jpg?md5=dd", "locator": {"figure_id": "F1"}}),
            json!({"article_id": "PMC1.1", "pmcid": "PMC1", "kind": "media", "unit_id": "m1", "role": "media_file", "modality": "video",
                   "extension": "mp4", "source_url": "s3://pmc-oa-opendata/PMC1.1/v.mp4", "license_code": "CC BY-NC"}),
            json!({"article_id": "PMC1.1", "pmcid": "PMC1", "kind": "media", "unit_id": "m2", "role": "media_file", "modality": "document",
                   "extension": ".PDF", "source_url": "s3://pmc-oa-opendata/PMC1.1/s1.pdf", "license_code": "CC BY"}),
        ];
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        for r in &rows {
            use std::io::Write;
            writeln!(gz, "{}", r).unwrap();
        }
        let gzb = gz.finish().unwrap();
        fs::write(b.join("items.jsonl.gz"), &gzb).unwrap();
        fs::write(b.join("manifest.json"), serde_json::to_vec(&json!({"files": [{"path": "items.jsonl.gz", "sha256": hex::encode(Sha256::digest(&gzb))}]})).unwrap()).unwrap();
        let vd = d.path().join("vec/gemma/b1");
        fs::create_dir_all(&vd).unwrap();
        let dim = 4;
        let header = format!("{{'descr': '<f2', 'fortran_order': False, 'shape': (2, {dim}), }}");
        let pad = 64 - (10 + header.len() + 1) % 64;
        let header = format!("{header}{}\n", " ".repeat(pad));
        let mut npy = b"\x93NUMPY\x01\x00".to_vec();
        npy.extend((header.len() as u16).to_le_bytes());
        npy.extend(header.as_bytes());
        for x in 0..8 {
            npy.extend(f16::from_f32(x as f32).to_bits().to_le_bytes());
        }
        fs::write(vd.join("dense.npy"), npy).unwrap();
        fs::write(vd.join("ids.json"), r#"{"unit_id": ["t1", "f1"], "modality": ["text", "image"]}"#).unwrap();
        fs::write(vd.join("meta.json"), serde_json::to_vec(&json!({"manifest_sha256": sha256_file(&b.join("manifest.json")).unwrap()})).unwrap()).unwrap();
        let sizes = d.path().join("sizes");
        fs::create_dir_all(&sizes).unwrap();
        fs::write(sizes.join("part-00000.tsv"), "PMC1.1/f.jpg\t1234\n").unwrap();
        let r = batch(&d.path().join("vec"), "gemma", &d.path().join("out"), "part-x", Some(&sizes), dim, &[b.clone()], None).unwrap();
        assert_eq!((r["chunks"].as_u64(), r["vectors"].as_u64(), r["media"].as_u64()), (Some(1), Some(2), Some(3)));
        assert_eq!((r["pending_videos"].as_u64(), r["pending_supplement_pdfs"].as_u64()), (Some(1), Some(1)));
        let f = File::open(d.path().join("out/pending/videos/part-x.parquet")).unwrap();
        let rb = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f).unwrap().build().unwrap().next().unwrap().unwrap();
        let ok = rb.column_by_name("commercial_use_ok").unwrap().as_any().downcast_ref::<arrow::array::BooleanArray>().unwrap();
        assert_eq!((rb.num_rows(), ok.value(0)), (1, false));
        let f = File::open(d.path().join("out/chunks/part-x.parquet")).unwrap();
        let rb = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f).unwrap().build().unwrap().next().unwrap().unwrap();
        let ok = rb.column_by_name("commercial_use_ok").unwrap().as_any().downcast_ref::<arrow::array::BooleanArray>().unwrap();
        assert!(ok.value(0));
        // read back
        let f = File::open(d.path().join("out/media/part-x.parquet")).unwrap();
        let mut rd = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f).unwrap().build().unwrap();
        let rb = rd.next().unwrap().unwrap();
        let bytes = rb.column_by_name("bytes").unwrap().as_any().downcast_ref::<arrow::array::Int64Array>().unwrap();
        assert_eq!(bytes.value(0), 1234);
        // a vector shard from another manifest is refused
        fs::write(vd.join("meta.json"), r#"{"manifest_sha256": "x"}"#).unwrap();
        assert!(batch(&d.path().join("vec"), "gemma", &d.path().join("out"), "part-y", None, dim, &[b], None).is_err());
    }
}
