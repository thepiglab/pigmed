//! Identifier normalisation, licences, and the stable hash shared with `src/pigmed/evaluation.py::assign_split`.

use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// Licences that allow commercial use and redistribution (user decision 2026-11-14). ND allows commercial use.
pub const COMMERCIAL_LICENSES: [&str; 4] = ["CC0", "CC-BY", "CC-BY-SA", "CC-BY-ND"];

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

/// Port of `common.normal_license`.
pub fn normal_license(value: Option<&str>) -> String {
    static WS: OnceLock<Regex> = OnceLock::new();
    static VER: OnceLock<Regex> = OnceLock::new();
    let v = value.unwrap_or("UNKNOWN").trim().to_uppercase().replace('_', "-");
    let v = re(&WS, r"\s+").replace_all(&v, "-").to_string();
    let v = re(&VER, r"-(?:[1-4](?:\.0)?)$").replace(&v, "").to_string();
    match v.as_str() {
        "CC-0" | "CC0" | "PUBLIC-DOMAIN" => "CC0".into(),
        "BY" => "CC-BY".into(),
        _ => v,
    }
}

pub fn commercial_use(license: Option<&str>) -> bool {
    COMMERCIAL_LICENSES.contains(&normal_license(license).as_str())
}

pub fn norm_doi(value: &str) -> String {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    static DOI: OnceLock<Regex> = OnceLock::new();
    let v = value.trim().to_lowercase();
    let v = re(&PREFIX, r"^(?:https?://(?:dx\.)?doi\.org/|doi:\s*)").replace(&v, "").to_string();
    let v = v.trim_end_matches(['.', ';', ',', ' ']).to_string();
    if re(&DOI, r"^10\.\d{4,9}/\S+$").is_match(&v) { v } else { String::new() }
}

fn all_digits_positive(v: &str) -> bool {
    !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) && v.bytes().any(|b| b != b'0')
}

pub fn norm_pmid(value: &str) -> String {
    let v = value.trim();
    let v = v.strip_prefix("PMID:").or_else(|| v.strip_prefix("pmid:")).unwrap_or(v).trim();
    if all_digits_positive(v) { v.to_string() } else { String::new() }
}

pub fn norm_pmcid(value: &str) -> String {
    let v = value.trim().to_uppercase();
    let v = v.split('.').next().unwrap_or("");
    let v = v.strip_prefix("PMC").unwrap_or(v);
    if all_digits_positive(v) { format!("PMC{}", v.trim_start_matches('0')) } else { String::new() }
}

/// Typed graph node, e.g. `pmcid:PMC123`; empty when the value does not normalise.
pub fn id_node(id_type: &str, value: &str) -> String {
    let t: String = id_type.to_lowercase().chars().filter(|c| *c != '-' && *c != '_').collect();
    match t.as_str() {
        "pmcid" | "pmc" | "pmcidversion" => prefixed("pmcid:", norm_pmcid(value)),
        "pmid" | "pubmed" | "medline" => prefixed("pmid:", norm_pmid(value)),
        "doi" => prefixed("doi:", norm_doi(value)),
        _ => String::new(),
    }
}

fn prefixed(p: &str, v: String) -> String {
    if v.is_empty() { v } else { format!("{p}{v}") }
}

/// `common.stable_id(*parts)` for string parts: sha256 of the compact JSON list, first 32 hex chars.
pub fn stable_id(parts: &[&str]) -> String {
    let json = serde_json::to_string(parts).expect("strings serialise");
    hex::encode(Sha256::digest(json.as_bytes()))[..32].to_string()
}

/// Uniform value in [0, 1) exactly as Python computes `int(stable_id(...), 16) / 16**32`.
pub fn unit_hash(parts: &[&str]) -> f64 {
    let n = u128::from_str_radix(&stable_id(parts), 16).expect("hex");
    (n as f64) / 2f64.powi(128)
}

/// `evaluation.article_group` for a record without duplicate_cluster.
pub fn article_group(pmcid: &str, doi: &str, pmid: &str, article_id: &str) -> String {
    if !pmcid.is_empty() {
        return pmcid.split('.').next().unwrap_or("").to_string();
    }
    if !doi.is_empty() {
        let d = doi.to_lowercase();
        return d.strip_prefix("https://doi.org/").unwrap_or(&d).to_string();
    }
    if !pmid.is_empty() {
        return format!("PMID:{pmid}");
    }
    article_id.split('.').next().unwrap_or("").to_string()
}

/// ISO date from JATS/PubMed parts; port of `splits.iso_date`.
pub fn iso_date(year: Option<&str>, month: Option<&str>, day: Option<&str>) -> Option<String> {
    let y: i64 = year?.trim().chars().take(4).collect::<String>().parse().ok()?;
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let num = |v: Option<&str>, default: i64| -> i64 {
        let s = v.unwrap_or("").trim().to_lowercase();
        let head: String = s.chars().take(3).collect();
        if let Some(i) = MONTHS.iter().position(|m| *m == head) {
            return i as i64 + 1;
        }
        s.parse::<i64>().map(|x| x.clamp(1, 31)).unwrap_or(default)
    };
    Some(format!("{:04}-{:02}-{:02}", y, num(month, 1).min(12), num(day, 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers() {
        assert_eq!(norm_doi("https://doi.org/10.1000/ABC."), "10.1000/abc");
        assert_eq!(norm_doi("doi: 10.1000/x"), "10.1000/x");
        assert_eq!(norm_doi("not a doi"), "");
        assert_eq!(norm_doi("10.1/short"), "");
        assert_eq!(norm_pmcid("PMC0123.2"), "PMC123");
        assert_eq!(norm_pmcid("123"), "PMC123");
        assert_eq!(norm_pmcid("x"), "");
        assert_eq!(id_node("pubmed", "42"), "pmid:42");
        assert_eq!(id_node("pmid", "0"), "");
        assert_eq!(id_node("ext", "1"), "");
    }

    #[test]
    fn licences() {
        assert!(commercial_use(Some("CC BY")) && commercial_use(Some("cc_by-nd")) && commercial_use(Some("CC0")));
        assert!(commercial_use(Some("CC BY 4.0")));
        assert!(!commercial_use(Some("CC BY-NC")) && !commercial_use(None) && !commercial_use(Some("TDM")));
    }

    #[test]
    fn stable_id_matches_python() {
        // python -c "from pigmed.common import stable_id; print(stable_id('pigmed-v1','PMC123'))"
        assert_eq!(stable_id(&["pigmed-v1", "PMC123"]), PY_STABLE_ID_PMC123);
    }

    // Filled from the Python reference (see stable_id_matches_python).
    const PY_STABLE_ID_PMC123: &str = include_str!("../tests/py_stable_id_pmc123.txt");

    #[test]
    fn dates() {
        assert_eq!(iso_date(Some("2024"), Some("Mar"), None).unwrap(), "2024-03-01");
        assert_eq!(iso_date(Some("2024"), Some("11"), Some("31")).unwrap(), "2024-11-31");
        assert_eq!(iso_date(None, None, None), None);
        assert_eq!(iso_date(Some("2024"), None, None).unwrap(), "2024-01-01");
    }
}
