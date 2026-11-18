//! figvec: the published v2 EG2 image vectors of a shard's figures, in flashembed layout for the S3 miner/teacher
//! (`<out>/embeddings.dense.npy` float16 [n, d] + `<out>/embeddings.ids.json`), in the order of `figures.jsonl.gz`.
//! The S1 parity check showed the published image vectors match the trainer's image path (no text prompt).

use anyhow::{anyhow, bail, Context, Result};
use arrow_array::{Array, FixedSizeListArray, Float16Array, StringArray};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ProjectionMask;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;

/// unit_id -> f16 bits of its image vector, for the wanted ids only.
pub fn read_vectors(path: &Path, want: &std::collections::HashSet<String>, modality: &str) -> Result<(HashMap<String, Vec<u16>>, usize)> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let b = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let mask = ProjectionMask::columns(b.parquet_schema(), ["unit_id", "modality", "embedding"]);
    let rd = b.with_projection(mask).with_batch_size(4096).build()?;
    let (mut out, mut dim) = (HashMap::new(), 0usize);
    for batch in rd {
        let b = batch?;
        let uid = b.column_by_name("unit_id").and_then(|c| c.as_any().downcast_ref::<StringArray>()).ok_or_else(|| anyhow!("unit_id"))?;
        let mo = b.column_by_name("modality").and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let emb = b.column_by_name("embedding").and_then(|c| c.as_any().downcast_ref::<FixedSizeListArray>()).ok_or_else(|| anyhow!("embedding"))?;
        dim = emb.value_length() as usize;
        let vals = emb.values().as_any().downcast_ref::<Float16Array>().ok_or_else(|| anyhow!("embedding is not float16"))?;
        for i in 0..b.num_rows() {
            let u = uid.value(i);
            if !want.contains(u) || mo.is_some_and(|m| !m.is_null(i) && m.value(i) != modality) {
                continue;
            }
            let off = emb.value_offset_at(i);
            out.insert(u.to_string(), (off..off + dim).map(|k| vals.value(k).to_bits()).collect());
        }
    }
    Ok((out, dim))
}

/// Minimal .npy writer: little-endian float16, C order, shape (n, d).
pub fn write_npy_f16(path: &Path, rows: &[&Vec<u16>], d: usize) -> Result<()> {
    let mut header = format!("{{'descr': '<f2', 'fortran_order': False, 'shape': ({}, {}), }}", rows.len(), d);
    while (10 + header.len() + 1) % 64 != 0 {
        header.push(' ');
    }
    header.push('\n');
    let tmp = path.with_extension("npy.tmp");
    let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    w.write_all(b"\x93NUMPY\x01\x00")?;
    w.write_all(&(header.len() as u16).to_le_bytes())?;
    w.write_all(header.as_bytes())?;
    for r in rows {
        for x in r.iter() {
            w.write_all(&x.to_le_bytes())?;
        }
    }
    w.flush()?;
    drop(w);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn figvec(figures: &Path, vectors: &Path, out: &Path) -> Result<Value> {
    let ids: Vec<String> = crate::io::reader(figures)?.lines().map(|l| {
        let v: Value = serde_json::from_str(&l?)?;
        Ok(v["id"].as_str().unwrap_or("").to_string())
    }).collect::<Result<_>>()?;
    let want: std::collections::HashSet<String> = ids.iter().cloned().collect();
    let (vecs, d) = read_vectors(vectors, &want, "image")?;
    let found: Vec<&String> = ids.iter().filter(|i| vecs.contains_key(*i)).collect();
    if !ids.is_empty() && found.is_empty() {
        bail!("no figure ids found in {}", vectors.display());
    }
    std::fs::create_dir_all(out)?;
    let rows: Vec<&Vec<u16>> = found.iter().map(|i| &vecs[*i]).collect();
    write_npy_f16(&out.join("embeddings.dense.npy"), &rows, d)?;
    std::fs::write(out.join("embeddings.ids.json"), serde_json::to_string(&found)?)?;
    Ok(json!({"figures": ids.len(), "with_vector": found.len(), "missing": ids.len() - found.len(), "dim": d}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::builder::{FixedSizeListBuilder, Float16Builder};
    use arrow_array::{ArrayRef, RecordBatch};
    use half::f16;
    use parquet::arrow::ArrowWriter;
    use std::sync::Arc;

    #[test]
    fn figvec_matches_ids_and_writes_npy() {
        let d = tempfile::tempdir().unwrap();
        let mut eb = FixedSizeListBuilder::new(Float16Builder::new(), 4);
        for k in 0..3 {
            for j in 0..4 {
                eb.values().append_value(f16::from_f32((k * 10 + j) as f32));
            }
            eb.append(true);
        }
        let b = RecordBatch::try_from_iter(vec![
            ("unit_id", Arc::new(StringArray::from(vec!["m1", "t1", "m2"])) as ArrayRef),
            ("modality", Arc::new(StringArray::from(vec!["image", "text", "image"])) as ArrayRef),
            ("embedding", Arc::new(eb.finish()) as ArrayRef),
        ]).unwrap();
        let vp = d.path().join("v.parquet");
        let mut w = ArrowWriter::try_new(std::fs::File::create(&vp).unwrap(), b.schema(), None).unwrap();
        w.write(&b).unwrap();
        w.close().unwrap();
        let f = d.path().join("figures.jsonl");
        std::fs::write(&f, "{\"id\":\"m2\"}\n{\"id\":\"m9\"}\n{\"id\":\"m1\"}\n").unwrap();
        let rep = figvec(&f, &vp, &d.path().join("figvec")).unwrap();
        assert_eq!((rep["with_vector"].as_u64(), rep["missing"].as_u64(), rep["dim"].as_u64()), (Some(2), Some(1), Some(4)));
        let ids: Vec<String> = serde_json::from_str(&std::fs::read_to_string(d.path().join("figvec/embeddings.ids.json")).unwrap()).unwrap();
        assert_eq!(ids, vec!["m2", "m1"]);
        let raw = std::fs::read(d.path().join("figvec/embeddings.dense.npy")).unwrap();
        let hl = u16::from_le_bytes([raw[8], raw[9]]) as usize;
        assert_eq!((10 + hl) % 64, 0);
        let first = u16::from_le_bytes([raw[10 + hl], raw[11 + hl]]);
        assert_eq!(f16::from_bits(first).to_f32(), 20.0, "m2's first value");
        assert_eq!(raw.len(), 10 + hl + 2 * 4 * 2);
    }
}
