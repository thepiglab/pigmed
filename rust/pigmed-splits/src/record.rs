//! The per-article record (one JSON line in records.jsonl.gz), shared by census, build and audit.

use crate::ids::id_node;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Ref {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "opt_stringish")]
    pub pmid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "opt_stringish")]
    pub pmcid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "opt_stringish")]
    pub doi: Option<String>,
}

impl Ref {
    pub fn is_empty(&self) -> bool {
        self.pmid.is_none() && self.pmcid.is_none() && self.doi.is_none()
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Related {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "stringish")]
    pub id_type: String,
    #[serde(default, deserialize_with = "stringish")]
    pub id: String,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct PubDate {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default, deserialize_with = "opt_stringish")]
    pub year: Option<String>,
    #[serde(default, deserialize_with = "opt_stringish")]
    pub month: Option<String>,
    #[serde(default, deserialize_with = "opt_stringish")]
    pub day: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Record {
    #[serde(deserialize_with = "stringish")]
    pub article_id: String,
    #[serde(deserialize_with = "stringish")]
    pub pmcid: String,
    #[serde(deserialize_with = "stringish")]
    pub pmid: String,
    #[serde(deserialize_with = "stringish")]
    pub doi: String,
    #[serde(deserialize_with = "stringish")]
    pub article_type: String,
    pub issns: Vec<String>,
    #[serde(deserialize_with = "stringish")]
    pub nlm_ta: String,
    #[serde(deserialize_with = "stringish")]
    pub journal: String,
    pub pub_dates: Vec<PubDate>,
    #[serde(deserialize_with = "stringish")]
    pub title: String,
    pub question_title: bool,
    pub has_abstract: bool,
    pub abstract_words: usize,
    pub words: usize,
    pub eg2_tokens: Option<usize>,
    pub related: Vec<Related>,
    pub references: Vec<Ref>,
    pub n_refs: usize,
    pub n_refs_resolvable: usize,
    pub citation_context_pairs: usize,
    pub citation_paragraphs: usize,
    pub fig_mentions: usize,
    pub n_figs: usize,
    #[serde(deserialize_with = "opt_stringish")]
    pub license_code: Option<String>,
    pub commercial: bool,
    #[serde(deserialize_with = "boolish")]
    pub is_retracted: bool,
    pub pub_types: Vec<String>,
    pub retraction_notice: bool,
    pub dates: Vec<String>,
}

impl Record {
    /// Typed id nodes of the article itself.
    pub fn nodes(&self) -> Vec<String> {
        let pmc = if self.pmcid.is_empty() { &self.article_id } else { &self.pmcid };
        let mut out: Vec<String> = vec![];
        for n in [id_node("pmcid", pmc), id_node("pmid", &self.pmid), id_node("doi", &self.doi)] {
            if !n.is_empty() && !out.contains(&n) {
                out.push(n);
            }
        }
        out
    }

    /// (id_type, id) of the first resolvable identifier per reference: PMID, then PMCID, then DOI.
    pub fn citation_ids(&self) -> impl Iterator<Item = (&'static str, String)> + '_ {
        self.references.iter().filter_map(|r| {
            for (t, v) in [("pmid", &r.pmid), ("pmcid", &r.pmcid), ("doi", &r.doi)] {
                if let Some(v) = v {
                    if !id_node(t, v).is_empty() {
                        return Some((t, v.clone()));
                    }
                }
            }
            None
        })
    }

    pub fn is_retracted_or_notice(&self) -> bool {
        self.is_retracted || self.retraction_notice
    }
}

fn stringish<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(opt_stringish(d)?.unwrap_or_default())
}

fn opt_stringish<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s),
        other => Some(other.to_string()),
    })
}

fn boolish<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::String(s) => s.eq_ignore_ascii_case("true"),
        _ => false,
    })
}
