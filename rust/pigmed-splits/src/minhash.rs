//! MinHash near-duplicate detection on word 5-gram shingles, with banded LSH.

use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

pub const NUM_PERM: usize = 128;
const MERSENNE: u128 = (1u128 << 61) - 1;

fn words(text: &str) -> Vec<String> {
    static W: OnceLock<Regex> = OnceLock::new();
    let re = W.get_or_init(|| Regex::new(r"[a-z0-9]+").unwrap());
    let lower = text.to_lowercase();
    re.find_iter(&lower).map(|m| m.as_str().to_string()).collect()
}

fn fnv1a32(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h ^ (h >> 32)) & 0xFFFF_FFFF
}

/// Unique 32-bit hashes of lowercased word n-grams.
pub fn shingle_hashes(text: &str, n: usize) -> Vec<u64> {
    let w = words(text);
    if w.len() < n {
        return vec![];
    }
    let grams: HashSet<String> = (0..=w.len() - n).map(|i| w[i..i + n].join(" ")).collect();
    let mut out: Vec<u64> = grams.iter().map(|g| fnv1a32(g)).collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn perms(seed: u64) -> Vec<(u128, u128)> {
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    };
    (0..NUM_PERM).map(|_| (((next() >> 32) | 1) as u128, (next() >> 32) as u128)).collect()
}

/// (NUM_PERM,) signature; all-max when there are no shingles.
pub fn signature(hashes: &[u64]) -> Vec<u32> {
    static P: OnceLock<Vec<(u128, u128)>> = OnceLock::new();
    let p = P.get_or_init(|| perms(1));
    let mut sig = vec![u32::MAX; NUM_PERM];
    for &x in hashes {
        for (k, &(a, b)) in p.iter().enumerate() {
            let h = (((a * x as u128 + b) % MERSENNE) & 0xFFFF_FFFF) as u32;
            if h < sig[k] {
                sig[k] = h;
            }
        }
    }
    sig
}

pub fn jaccard_estimate(a: &[u32], b: &[u32]) -> f64 {
    a.iter().zip(b).filter(|(x, y)| x == y).count() as f64 / a.len() as f64
}

pub struct LshStats {
    pub candidates: usize,
    pub skipped_buckets: usize,
}

/// Near-duplicate pairs (i, j, estimate >= threshold). Buckets larger than `max_bucket` are templates and are
/// skipped; rows with `valid[i] == false` (too few shingles) never pair.
pub fn duplicates(sigs: &[Vec<u32>], valid: &[bool], threshold: f64, bands: usize, max_bucket: usize)
    -> (Vec<(usize, usize, f64)>, LshStats) {
    let rows = NUM_PERM / bands;
    let mut cand: HashSet<(usize, usize)> = HashSet::new();
    let mut skipped = 0;
    for band in 0..bands {
        let mut buckets: HashMap<&[u32], Vec<usize>> = HashMap::new();
        for (i, s) in sigs.iter().enumerate() {
            if valid[i] {
                buckets.entry(&s[band * rows..(band + 1) * rows]).or_default().push(i);
            }
        }
        for members in buckets.values() {
            if members.len() < 2 {
                continue;
            }
            if members.len() > max_bucket {
                skipped += 1;
                continue;
            }
            for x in 0..members.len() {
                for y in x + 1..members.len() {
                    cand.insert((members[x], members[y]));
                }
            }
        }
    }
    let mut out: Vec<(usize, usize, f64)> = cand
        .iter()
        .map(|&(i, j)| (i, j, jaccard_estimate(&sigs[i], &sigs[j])))
        .filter(|&(_, _, e)| e >= threshold)
        .collect();
    out.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    (out, LshStats { candidates: cand.len(), skipped_buckets: skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_near_duplicates_only() {
        let mut seed = 7u64;
        let mut word = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            format!("w{}", (seed >> 33) % 5000)
        };
        let base: Vec<String> = (0..400).map(|_| word()).collect();
        let mut near = base.clone();
        near[10] = "changed".into();
        let other: Vec<String> = (0..400).map(|_| word()).collect();
        let texts = [base.join(" "), near.join(" "), other.join(" "), "too short".to_string()];
        let hs: Vec<Vec<u64>> = texts.iter().map(|t| shingle_hashes(t, 5)).collect();
        let sigs: Vec<Vec<u32>> = hs.iter().map(|h| signature(h)).collect();
        let valid: Vec<bool> = hs.iter().map(|h| h.len() >= 50).collect();
        let (pairs, _) = duplicates(&sigs, &valid, 0.8, 16, 200);
        assert_eq!(pairs.len(), 1);
        assert_eq!((pairs[0].0, pairs[0].1), (0, 1));
        assert!(pairs[0].2 > 0.9);
        assert!(shingle_hashes("one two three four", 5).is_empty());
    }
}
