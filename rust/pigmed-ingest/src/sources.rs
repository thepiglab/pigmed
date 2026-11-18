//! Immutable source plans. Output is row-for-row identical to `src/pigmed/sources.py`.
use crate::common::{atomic_json, stable_id, write_jsonl};
use crate::net::{s3_url, Http};
use anyhow::{bail, Context, Result};
use flate2::read::MultiGzDecoder;
use percent_encoding::percent_decode_str;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const INVENTORY: &str = "inventory-reports/pmc-oa-opendata/metadata/";

pub async fn latest_inventory(http: &Http) -> Result<String> {
    let day = Regex::new(r"/\d{4}-\d\d-\d\dT[^/]+/$").unwrap();
    let mut prefixes: Vec<String> = http
        .list_s3(INVENTORY, Some("/"))
        .await?
        .into_iter()
        .filter_map(|(_, p)| p)
        .filter(|p| day.is_match(p))
        .collect();
    if prefixes.is_empty() {
        bail!("No PMC inventory manifest found; check current PMC README");
    }
    prefixes.sort();
    for p in prefixes.iter().rev() {
        let url = s3_url(&format!("{p}manifest.json"));
        match http.json(&url).await {
            Ok(_) => return Ok(url),
            Err(e) if e.to_string().contains("HTTP 404") || e.to_string().contains("HTTP 403") => continue,
            Err(e) => return Err(e),
        }
    }
    bail!("No completed inventory found")
}

struct Row {
    pmcid: String,
    version: i64,
    key: String,
    etag: String,
    modified: String,
}

pub struct PlanArgs {
    pub manifest_url: Option<String>,
    pub per_part: usize,
    pub sample_mod: u128,
    pub sample_rem: u128,
    pub limit: Option<usize>,
    pub all_versions: bool,
    pub exclude_mod: u128,
    pub exclude_rem: Vec<u128>,
}

pub async fn plan_pmc(out: &Path, http: &Http, a: PlanArgs, scratch: &Path) -> Result<Value> {
    if a.per_part < 1 || a.sample_mod < 1 || a.sample_rem >= a.sample_mod || a.exclude_mod < 1 || a.exclude_rem.iter().any(|r| *r >= a.exclude_mod) {
        bail!("Invalid partition/sample setting");
    }
    if out.exists() {
        bail!("Plan directories are immutable: {}", out.display());
    }
    let tmp_out = out.with_file_name(format!("{}.partial", out.file_name().unwrap().to_string_lossy()));
    fs::create_dir_all(out.parent().unwrap())?;
    fs::create_dir(&tmp_out).with_context(|| format!("create {}", tmp_out.display()))?;
    let manifest_url = match a.manifest_url {
        Some(u) => u,
        None => latest_inventory(http).await?,
    };
    let manifest = http.json(&manifest_url).await?;
    let schema = manifest["fileSchema"].as_str().context("fileSchema")?;
    let fields: Vec<String> = schema
        .split(',')
        .map(|x| x.to_lowercase().chars().filter(|c| c.is_ascii_lowercase()).collect())
        .collect();
    for req in ["bucket", "key", "etag", "lastmodifieddate"] {
        if !fields.iter().any(|f| f == req) {
            bail!("Unexpected inventory schema: {schema}");
        }
    }
    let col = |n: &str| fields.iter().position(|f| f == n).unwrap();
    let (cb, ck, ce, cm) = (col("bucket"), col("key"), col("etag"), col("lastmodifieddate"));

    // Fetch all inventory parts concurrently (each bounded at 8 GiB, MD5-verified), then parse in parallel.
    let files = manifest["files"].as_array().context("files")?.clone();
    fs::create_dir_all(scratch)?;
    let mut tasks = Vec::new();
    for (i, part) in files.iter().enumerate() {
        let http = http.clone();
        let key = part["key"].as_str().context("key")?.to_string();
        let md5 = part.get("MD5checksum").or_else(|| part.get("md5Checksum")).and_then(|v| v.as_str()).map(String::from);
        let path = scratch.join(format!("inventory-{i}.csv.gz"));
        tasks.push(tokio::spawn(async move {
            http.download(&s3_url(&key), &path, 8 << 30, md5.as_deref()).await?;
            let parsed = tokio::task::spawn_blocking(move || -> Result<(Vec<Row>, usize)> {
                let re = Regex::new(r"^metadata/(PMC\d+)\.(\d+)\.json$").unwrap();
                let f = fs::File::open(&path)?;
                let mut rdr = csv::ReaderBuilder::new().has_headers(false).flexible(false).from_reader(MultiGzDecoder::new(f));
                let mut rows = Vec::new();
                let mut n = 0usize;
                for rec in rdr.records() {
                    let rec = rec?;
                    if rec.get(cb) != Some("pmc-oa-opendata") {
                        bail!("Unexpected inventory bucket");
                    }
                    let key = percent_decode_str(rec.get(ck).unwrap_or("")).decode_utf8()?.to_string();
                    if let Some(m) = re.captures(&key) {
                        n += 1;
                        rows.push(Row {
                            pmcid: m[1].to_string(),
                            version: m[2].parse()?,
                            etag: rec.get(ce).unwrap_or("").trim_matches('"').to_string(),
                            modified: rec.get(cm).unwrap_or("").to_string(),
                            key,
                        });
                    }
                }
                let _ = fs::remove_file(&path);
                Ok((rows, n))
            })
            .await??;
            Ok::<_, anyhow::Error>(parsed)
        }));
    }
    let mut scanned = 0usize;
    // id -> row; "latest" keeps the max version per PMCID (ties: later wins, like SQLite upsert >=).
    let mut keep: HashMap<String, Row> = HashMap::new();
    for t in tasks {
        let (rows, n) = t.await??;
        scanned += n;
        for r in rows {
            let h = &stable_id(&[json!(r.pmcid)]);
            let v = u128::from_str_radix(h, 16).unwrap();
            if v % a.sample_mod != a.sample_rem || a.exclude_rem.contains(&(v % a.exclude_mod)) {
                continue;
            }
            let id = if a.all_versions { r.key.clone() } else { r.pmcid.clone() };
            match keep.get(&id) {
                Some(old) if r.version < old.version => {}
                _ => {
                    keep.insert(id, r);
                }
            }
        }
    }
    let mut rows: Vec<Row> = keep.into_values().collect();
    rows.sort_by(|x, y| x.pmcid.cmp(&y.pmcid).then(x.version.cmp(&y.version)));
    if let Some(l) = a.limit {
        rows.truncate(l);
    }
    let count = rows.len();
    let mut n_parts = 0usize;
    for chunk in rows.chunks(a.per_part) {
        let batch: Vec<Value> = chunk
            .iter()
            .map(|r| {
                json!({"pmcid": r.pmcid, "version": r.version, "key": r.key, "etag": r.etag,
                       "modified": r.modified, "source": "pmc", "snapshot": manifest_url})
            })
            .collect();
        write_jsonl(&tmp_out.join(format!("part-{n_parts:05}.jsonl")), &batch)?;
        n_parts += 1;
    }
    let result = json!({"source": "pmc", "manifest_url": manifest_url, "rows": count, "parts": n_parts,
        "scanned_versions": scanned, "versions": if a.all_versions {"all"} else {"latest"},
        "sample_mod": a.sample_mod as u64, "sample_rem": a.sample_rem as u64,
        "exclude_mod": a.exclude_mod as u64, "exclude_rem": a.exclude_rem.iter().map(|r| *r as u64).collect::<Vec<_>>(),
        "limit": a.limit, "planner": "pigmed-rs"});
    atomic_json(&tmp_out.join("plan.json"), &result)?;
    fs::rename(&tmp_out, out)?;
    Ok(result)
}

pub async fn plan_pubmed(out: &Path, http: &Http, updates: bool, limit: Option<usize>) -> Result<Value> {
    if out.exists() {
        bail!("Plan directories are immutable: {}", out.display());
    }
    let base = format!("https://ftp.ncbi.nlm.nih.gov/pubmed/{}", if updates { "updatefiles/" } else { "baseline/" });
    let html = String::from_utf8(http.get(&base, 16 << 20).await?)?;
    let re = Regex::new(r#"href=["'](pubmed\d+n(\d+)\.xml\.gz)["']"#).unwrap();
    let mut names: Vec<(String, u64)> = re.captures_iter(&html).map(|c| (c[1].to_string(), c[2].parse().unwrap())).collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        bail!("No PubMed XML.gz files found; directory format may have changed");
    }
    fs::create_dir_all(out)?;
    let mut n = 0;
    for (name, seq) in names {
        if limit.map_or(false, |l| n >= l) {
            break;
        }
        let row = json!({"source": "pubmed", "url": format!("{base}{name}"), "md5_url": format!("{base}{name}.md5"),
                         "sequence": seq, "phase": if updates {"update"} else {"baseline"}, "name": name});
        write_jsonl(&out.join(format!("part-{n:05}.jsonl")), &[row])?;
        n += 1;
    }
    let r = json!({"source": "pubmed", "parts": n, "phase": if updates {"update"} else {"baseline"}});
    atomic_json(&out.join("plan.json"), &r)?;
    Ok(json!({"parts": n}))
}
