//! One source-emitted, version-specific public MAGIC export. This is not a global catalog adapter.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{
    Block, BlockKind, DateObservation, FetchRecord, GuidelineDoc, Relation, Rights,
};
use crate::guidelines::fetch::{Fetcher, Req};
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::Html;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};
use tokio::sync::OnceCell;
use url::Url;

pub const EXPORT_URL:&str="https://s3.amazonaws.com/files.magicapp.org/guideline/2a597afd-81bc-4c8a-b5e0-7bfff8b0da7f/guideline_11482-14_0.json";
const READER: &str = "https://app.magicapp.org/#/guideline/11482";
const MAX_BYTES: usize = 96 * 1024 * 1024;
#[derive(Default)]
pub struct MagicAppPublic {
    collection: OnceCell<Collection>,
}
struct Section {
    id: i64,
    pointer: String,
    parent: Option<usize>,
    headings: Vec<String>,
    source_index: usize,
}
struct Recommendation {
    id: i64,
    pointer: String,
    section: usize,
    source_index: usize,
}
struct Collection {
    root: Value,
    sections: Vec<Section>,
    records: Vec<Recommendation>,
    picos: BTreeMap<i64, Vec<String>>,
    fetch: FetchRecord,
}
fn hash(v: &Value) -> String {
    crate::common::sha256_hex(&serde_json::to_vec(v).expect("JSON value"))
}
fn id(v: &Value, key: &str) -> Result<i64> {
    v[key]
        .as_i64()
        .filter(|n| *n > 0)
        .with_context(|| format!("MAGIC native {key} missing/invalid"))
}
fn code(v: &Value, key: &str) -> Option<String> {
    v[key]
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 100
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .map(str::to_owned)
}
fn native(v: &Value, keys: &[&str]) -> Value {
    json!(keys
        .iter()
        .filter_map(|key| v.get(*key).map(|value| ((*key).to_string(), value.clone())))
        .collect::<Map<String, Value>>())
}
fn source_order(v: &Value, index: usize) -> (i64, usize) {
    (v["order"].as_i64().unwrap_or(i64::MAX), index)
}
fn pointer<'a>(root: &'a Value, path: &str) -> Result<&'a Value> {
    root.pointer(path)
        .context("MAGIC native source locator missing")
}
fn source_url(kind: &str, gid: i64, native_id: i64) -> String {
    format!("https://app.magicapp.org/#/guideline/{gid}/{kind}/{native_id}")
}
fn absolute(raw: &str) -> Option<String> {
    let u = Url::parse(raw).ok()?;
    if !matches!(u.scheme(), "https" | "http") || !u.username().is_empty() || u.password().is_some()
    {
        return None;
    }
    Some(u.to_string())
}
fn escaped(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn field_html(s: &str) -> String {
    if regex::Regex::new(
        r"(?i)</?(?:p|div|span|ul|ol|li|table|a|strong|b|u|em|br|h[1-6]|img|cite)\b",
    )
    .unwrap()
    .is_match(s)
    {
        s.to_string()
    } else {
        format!("<p>{}</p>", escaped(s))
    }
}
// Rich components may be stored as tag-free HTML character-reference text.
// Decode each input reference once, then escape the decoded value before wrapping:
// encoded markup remains text, and &amp;nbsp; does not become a second-pass NBSP.
// Plain/structured fields continue to use field_html/ plain without this adapter.
fn rich_field_html(s: &str) -> String {
    if regex::Regex::new(r"(?i)</?[a-z][^>]*>")
        .expect("static rich-component tag pattern")
        .is_match(s)
    {
        return field_html(s);
    }
    let references = regex::Regex::new(r"&(?:#[xX][0-9a-fA-F]+|#[0-9]+|[a-zA-Z][a-zA-Z0-9]*);?")
        .expect("static character-reference pattern");
    let decoded = references.replace_all(s, |capture: &regex::Captures<'_>| {
        // The matched token contains no angle brackets, so HTML parsing here
        // performs character-reference decoding without interpreting source tags.
        Html::parse_fragment(&capture[0])
            .root_element()
            .text()
            .collect::<String>()
    });
    format!("<p>{}</p>", escaped(&decoded))
}
fn plain(s: &str) -> String {
    text_of(&Html::parse_fragment(&field_html(s)).root_element())
}
fn config(src: &SourceEntry) -> Result<()> {
    if src.catalog_url.as_deref() != Some(READER)
        || src.config["export_url"].as_str() != Some(EXPORT_URL)
        || src.config["access_scope"].as_str() != Some("source_emitted_public_version")
    {
        bail!("MAGIC requires the exact observed Stroke publication11482 reader/export and explicit source_emitted_public_version scope")
    }
    Ok(())
}
impl Collection {
    fn build(root: Value, fetch: FetchRecord) -> Result<Self> {
        if id(&root, "guidelineId")? != 11482
            || id(&root, "publishedId")? != 11482
            || code(&root, "shortCode").as_deref() != Some("QnoKGn")
        {
            bail!("MAGIC export native identity differs from observed publication11482")
        }
        let mut out = Self {
            root,
            sections: vec![],
            records: vec![],
            picos: BTreeMap::new(),
            fetch,
        };
        let roots = out.root["sections"]
            .as_array()
            .context("MAGIC root sections missing")?
            .len();
        let mut positions = (0..roots).collect::<Vec<_>>();
        positions.sort_by_key(|n| source_order(&out.root["sections"][*n], *n));
        for n in positions {
            out.section(format!("/sections/{n}"), None, vec![], n, 0)?;
        }
        let mut recs = HashSet::new();
        let mut sections = HashSet::new();
        if out.records.iter().any(|r| !recs.insert(r.id))
            || out.sections.iter().any(|s| !sections.insert(s.id))
        {
            bail!("MAGIC duplicate native recommendation/section identity")
        }
        Ok(out)
    }
    fn section(
        &mut self,
        path: String,
        parent: Option<usize>,
        mut headings: Vec<String>,
        source_index: usize,
        depth: usize,
    ) -> Result<()> {
        if depth > 32 || self.sections.len() >= 10000 {
            bail!("MAGIC section count/depth guard exceeded")
        }
        let section = pointer(&self.root, &path)?;
        let sid = id(section, "sectionId")?;
        if id(section, "guidelineId")? != 11482 {
            bail!("MAGIC section guideline identity mismatch")
        }
        let heading = section["heading"].as_str().map(plain).unwrap_or_default();
        if !heading.is_empty() {
            headings.push(heading)
        }
        let section_index = self.sections.len();
        self.sections.push(Section {
            id: sid,
            pointer: path.clone(),
            parent,
            headings,
            source_index,
        });
        let section = pointer(&self.root, &path)?;
        let recs = section["recommendations"]
            .as_array()
            .context("MAGIC section recommendations missing")?;
        let mut positions = (0..recs.len()).collect::<Vec<_>>();
        positions.sort_by_key(|n| source_order(&recs[*n], *n));
        for n in positions {
            let recpath = format!("{path}/recommendations/{n}");
            let rec = pointer(&self.root, &recpath)?;
            if id(rec, "guidelineId")? != 11482 || id(rec, "sectionId")? != sid {
                bail!("MAGIC recommendation containment mismatch")
            }
            if self.records.len() >= 10000 {
                bail!("MAGIC recommendation count guard exceeded")
            }
            self.records.push(Recommendation {
                id: id(rec, "recommendationId")?,
                pointer: recpath.clone(),
                section: section_index,
                source_index: n,
            });
            // A hidden parent does not authorize exporting its private embedded components.
            if rec["hideInPublish"].as_bool() != Some(true) {
                self.index_picos(&format!("{recpath}/picos"))?;
            }
        }
        self.index_picos(&format!("{path}/picos"))?;
        let section = pointer(&self.root, &path)?;
        let children = section["subSections"]
            .as_array()
            .context("MAGIC section subSections missing")?;
        let mut positions = (0..children.len()).collect::<Vec<_>>();
        positions.sort_by_key(|n| source_order(&children[*n], *n));
        for n in positions {
            self.section(
                format!("{path}/subSections/{n}"),
                Some(section_index),
                self.sections[section_index].headings.clone(),
                n,
                depth + 1,
            )?;
        }
        Ok(())
    }
    fn index_picos(&mut self, path: &str) -> Result<()> {
        let items = pointer(&self.root, path)?
            .as_array()
            .context("MAGIC picos container missing")?;
        for (n, pico) in items.iter().enumerate() {
            let pid = id(pico, "picoId")?;
            self.picos
                .entry(pid)
                .or_default()
                .push(format!("{path}/{n}"));
        }
        if self.picos.len() > 10000 {
            bail!("MAGIC PICO count guard exceeded")
        }
        Ok(())
    }
    fn root_metadata(&self) -> Value {
        let mut data = native(
            &self.root,
            &[
                "name",
                "byName",
                "shortCode",
                "guidelineId",
                "publishedId",
                "language",
                "countryFlag",
                "institutionId",
                "institutionName",
                "institutionShortName",
                "institutionGradeStrength",
                "gradeStrengthLabel",
                "isLatestPublished",
                "isPublic",
                "status",
                "versionMajor",
                "versionMinor",
                "publishDate",
                "publishCreationTime",
                "publishLastSearchDate",
                "lastEdit",
                "startDate",
            ],
        );
        data["observed_ui_context"] = json!({"evidence_url":READER,"observation_date":"2026-11-14","publication_version":"v14.0","history_labels":["Most current version","PUBLIC"],"history_published_raw":"2026-09-16","header_published_raw":"9/15/2026","last_evidence_search_raw":"2026-04-01","scope":"independent operator browser observation of this exact publication; raw export flags retained without reconciliation"});
        data["currentness_conflict"] = json!(
            self.root["isLatestPublished"] == false
                || self.root["isPublic"] == false
                || self.root["status"] == "NOTSET"
        );
        data["canonical_currentness"] = json!(
            "unknown; no latest/historical/superseded inference from conflicting source flags"
        );
        data["country_flag_scope"] =
            json!("native publisher/platform field; clinical applicability unknown");
        data
    }
}
fn export_request() -> Req {
    Req {
        cache_max_age_secs: Some(0),
        ..Req::get(EXPORT_URL)
    }
}
impl MagicAppPublic {
    async fn load<'a>(&'a self, src: &SourceEntry, f: &Fetcher) -> Result<&'a Collection> {
        config(src)?;
        self.collection
            .get_or_try_init(|| async {
                let request = export_request();
                let page = f.request(&request).await?;
                if page.record.status != 200 || page.body.len() > MAX_BYTES {
                    bail!("MAGIC source export HTTP status/byte guard failed")
                }
                let actual = Url::parse(&page.record.final_url)?;
                let expected = Url::parse(EXPORT_URL)?;
                if actual != expected {
                    bail!("MAGIC export redirected away from exact source-emitted version URL")
                }
                let mime = page
                    .record
                    .content_type
                    .as_deref()
                    .unwrap_or("")
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim();
                if mime != "application/json" {
                    bail!("MAGIC export MIME is not verified application/json")
                }
                Collection::build(
                    serde_json::from_slice(&page.body)
                        .context("MAGIC source export JSON invalid")?,
                    page.record,
                )
            })
            .await
    }
}

fn omitted(value: &Value, path: &str, reason: &str, out: &mut Vec<Value>) {
    out.push(json!({"source_path":path,"reason":reason,"source_sha256":hash(value),"native_type":match value{Value::Null=>"null",Value::Bool(_)=>"boolean",Value::Number(_)=>"number",Value::String(_)=>"string",Value::Array(_)=>"array",Value::Object(_)=>"object"},"text_exported":false}));
}
fn private_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "password",
        "token",
        "secret",
        "credential",
        "contact",
        "email",
        "permission",
    ]
    .iter()
    .any(|word| key.contains(word))
        || matches!(
            key.as_str(),
            "owner" | "editor" | "createdby" | "modifiedby" | "users" | "user"
        )
}
// This projector is used only inside selected clinical components, never on the full export.
fn visible(value: &Value, path: &str, omissions: &mut Vec<Value>, depth: usize) -> Result<Value> {
    if depth > 32 {
        bail!("MAGIC clinical projection depth exceeded")
    }
    Ok(match value {
        Value::Object(object) => {
            if object.get("isHidden").and_then(Value::as_bool) == Some(true) {
                omitted(value, path, "native isHidden=true", omissions);
                return Ok(native(
                    value,
                    &["outcomeId", "picoId", "isHidden", "status"],
                ));
            }
            let mut out = Map::new();
            for (key, child) in object {
                let childpath = format!("{path}.{key}");
                let explicit = object
                    .get(&format!("{key}InPublic"))
                    .and_then(Value::as_bool);
                let supplementary =
                    key.ends_with("ResearchEvidence") || key.ends_with("AdditionalConsideration");
                let hidden_summary = object
                    .get("isSummaryOfJudgementPublic")
                    .and_then(Value::as_bool)
                    == Some(false)
                    && (key == "interventions" || key.ends_with("Strength"));
                if private_key(key)
                    || explicit == Some(false)
                    || (supplementary && explicit != Some(true))
                    || hidden_summary
                {
                    omitted(
                        child,
                        &childpath,
                        if explicit == Some(false) {
                            "native paired InPublic=false"
                        } else if hidden_summary {
                            "native isSummaryOfJudgementPublic=false"
                        } else if supplementary {
                            "supplementary component visibility not explicitly public"
                        } else {
                            "editor/contact/security field excluded"
                        },
                        omissions,
                    );
                    continue;
                }
                out.insert(
                    key.clone(),
                    visible(child, &childpath, omissions, depth + 1)?,
                );
            }
            Value::Object(out)
        }
        Value::Array(values) => {
            if values.len() > 20000 {
                bail!("MAGIC clinical array bound exceeded")
            }
            Value::Array(
                values
                    .iter()
                    .enumerate()
                    .map(|(n, value)| visible(value, &format!("{path}[{n}]"), omissions, depth + 1))
                    .collect::<Result<Vec<_>>>()?,
            )
        }
        _ => value.clone(),
    })
}
fn asset_projection(value: &Value) -> Value {
    native(
        value,
        &[
            "fileId",
            "uuid",
            "fileName",
            "displayName",
            "fileExtension",
            "fileLocation",
            "referenceId",
            "referenceFileId",
            "fullPath",
        ],
    )
}
fn reference_projection(value: &Value, omissions: &mut Vec<Value>) -> Result<Value> {
    let mut result = native(
        value,
        &[
            "referenceId",
            "referenceNumber",
            "titleText",
            "url",
            "doi",
            "pmid",
            "journal",
            "year",
            "volume",
            "issue",
            "pageStart",
            "pageEnd",
            "referenceType",
            "studyType",
            "shortName",
            "order",
            "results",
        ],
    );
    if let Some(authors) = value["authors"].as_array() {
        result["authors"] = json!(authors
            .iter()
            .map(|author| native(author, &["firstName", "lastName", "formattedName"]))
            .collect::<Vec<_>>());
    }
    for key in ["files", "attachedFiles"] {
        if let Some(files) = value[key].as_array() {
            result[key] = json!(files.iter().map(asset_projection).collect::<Vec<_>>());
        }
    }
    // The nested referencePicos study graph is not another clinical PICO inventory.
    if let Some(items) = value["referencePicos"].as_array() {
        result["reference_pico_links"] = json!(items
            .iter()
            .map(|item| native(item, &["picoId", "referenceId", "revManId"]))
            .collect::<Vec<_>>());
    }
    visible(&result, "reference", omissions, 0)
}
fn project_pico(value: &Value, path: &str) -> Result<(Value, Vec<Value>)> {
    let mut omissions = vec![];
    let mut projected = native(
        value,
        &[
            "picoId",
            "sectionId",
            "guidelineId",
            "order",
            "population",
            "populationShortName",
            "intervention",
            "interventionShortName",
            "comparator",
            "comparatorShortName",
            "setting",
            "showPicots",
            "shortCode",
            "summary",
            "picoCodes",
        ],
    );
    if let Some(outcomes) = value["outcomes"].as_object() {
        let mut groups = Map::new();
        for (group, items) in outcomes {
            if matches!(group.as_str(), "picoId" | "sectionId" | "guidelineId") {
                groups.insert(group.clone(), items.clone());
                continue;
            }
            if !matches!(
                group.as_str(),
                "dichotomousOutcomes"
                    | "continuousOutcomes"
                    | "nonPoolableOutcomes"
                    | "practicalIssues"
            ) {
                omitted(
                    items,
                    &format!("{path}.outcomes.{group}"),
                    "unrecognized outcome group; no clinical fallback",
                    &mut omissions,
                );
                continue;
            }
            let array = items
                .as_array()
                .context("MAGIC outcome group is not an array")?;
            let mut records = vec![];
            for (n, outcome) in array.iter().enumerate() {
                let mut selected = Map::new();
                for (key, field) in outcome
                    .as_object()
                    .context("MAGIC outcome is not an object")?
                {
                    if OUTCOME_FIELDS.contains(&key.as_str())
                        || (key.ends_with("InPublic") && field.is_boolean())
                    {
                        if matches!(
                            key.as_str(),
                            "interventionReference" | "comparatorReference"
                        ) && field.is_object()
                        {
                            selected
                                .insert(key.clone(), reference_projection(field, &mut omissions)?);
                        } else {
                            selected.insert(key.clone(), field.clone());
                        }
                    } else {
                        omitted(
                            field,
                            &format!("{path}.outcomes.{group}[{n}].{key}"),
                            "unrecognized outcome field",
                            &mut omissions,
                        );
                    }
                }
                records.push(visible(
                    &json!(selected),
                    &format!("{path}.outcomes.{group}[{n}]"),
                    &mut omissions,
                    0,
                )?);
            }
            groups.insert(group.clone(), json!(records));
        }
        projected["outcomes"] = json!(groups);
    }
    if let Some(references) = value["references"].as_array() {
        projected["references"] = json!(references
            .iter()
            .map(|reference| reference_projection(reference, &mut omissions))
            .collect::<Result<Vec<_>>>()?);
    }
    for (key, field) in value
        .as_object()
        .context("MAGIC PICO source object missing")?
    {
        if key.ends_with("InPublic") && field.is_boolean() {
            projected[key] = field.clone();
        } else if projected.get(key).is_none() {
            omitted(
                field,
                &format!("{path}.{key}"),
                "unrecognized PICO field",
                &mut omissions,
            );
        }
    }
    let projected = visible(&projected, path, &mut omissions, 0)?;
    Ok((projected, omissions))
}
fn element_identity(value: &Value, path: &str) -> Option<String> {
    let key = if [
        "dichotomousOutcomes",
        "continuousOutcomes",
        "nonPoolableOutcomes",
    ]
    .iter()
    .any(|field| path.ends_with(&format!(".{field}")))
    {
        "outcomeId"
    } else if path.ends_with(".references") {
        "referenceId"
    } else if path.ends_with(".files") || path.ends_with(".attachedFiles") {
        "fileId"
    } else {
        return None;
    };
    value[key]
        .as_i64()
        .filter(|n| *n > 0)
        .map(|id| format!("{key}:{id}"))
}
fn visibility_field(key: &str) -> bool {
    key.ends_with("InPublic") || key == "isSummaryOfJudgementPublic" || key == "isHidden"
}
fn visibility_observations(value: &Value, path: &str) -> Vec<Value> {
    let mut out = vec![];
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            if visibility_field(key) && value.is_boolean() {
                out.push(json!({"source_path":format!("{path}.{key}"),"native_boolean":value}));
            }
        }
        if let Some(groups) = object.get("outcomes").and_then(Value::as_object) {
            for (group, values) in groups {
                if let Some(values) = values.as_array() {
                    for (n, value) in values.iter().enumerate() {
                        out.extend(visibility_observations(
                            value,
                            &format!("{path}.outcomes.{group}[{n}]"),
                        ));
                    }
                }
            }
        }
    }
    out
}
fn blank(value: &Value) -> bool {
    value.is_null()
        || value.as_str() == Some("")
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(Map::is_empty)
}
// Compatible partial records enrich one another. Conflicting nonempty clinical values stay
// separate variants with provenance; no first/last-wins scalar assignment is allowed.
fn merge(left: &Value, right: &Value, path: &str) -> std::result::Result<Value, String> {
    if left == right || blank(right) {
        return Ok(left.clone());
    }
    if blank(left) {
        return Ok(right.clone());
    }
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            let mut out = a.clone();
            for (key, value) in b {
                if visibility_field(key) {
                    if let (Some(old), Some(new)) =
                        (out.get(key).and_then(Value::as_bool), value.as_bool())
                    {
                        out.insert(
                            key.clone(),
                            json!(if key == "isHidden" {
                                old || new
                            } else {
                                old && new
                            }),
                        );
                        continue;
                    }
                }
                let child = if let Some(old) = out.get(key) {
                    merge(old, value, &format!("{path}.{key}"))?
                } else {
                    value.clone()
                };
                out.insert(key.clone(), child);
            }
            Ok(json!(out))
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.iter().all(|value| b.contains(value)) {
                return Ok(right.clone());
            }
            if b.iter().all(|value| a.contains(value)) {
                return Ok(left.clone());
            }
            if a.iter()
                .chain(b.iter())
                .any(|value| element_identity(value, path).is_none())
            {
                return Err(path.to_string());
            }
            let mut out = a.clone();
            for value in b {
                if out.contains(value) {
                    continue;
                }
                let identity =
                    element_identity(value, path).expect("validated native container identity");
                if let Some(position) = out.iter().position(|item| {
                    element_identity(item, path).as_deref() == Some(identity.as_str())
                }) {
                    out[position] = merge(&out[position], value, &format!("{path}[{identity}]"))?;
                } else {
                    out.push(value.clone());
                }
            }
            Ok(json!(out))
        }
        _ => Err(path.to_string()),
    }
}
// Privacy flags are conservative across every alias of the same native PICO/outcome,
// including aliases whose conflicting clinical scalars remain separate variants.
fn apply_visibility_flags(target: &mut Value, source: &Value) {
    if !target.is_object() || !source.is_object() {
        return;
    }
    for (key, value) in source.as_object().unwrap() {
        if (key.ends_with("InPublic") || key == "isSummaryOfJudgementPublic") && value == false {
            target[key] = json!(false);
        }
        if key == "isHidden" && value == true {
            target[key] = json!(true);
        }
    }
    if let (Some(target_groups), Some(source_groups)) = (
        target.get_mut("outcomes").and_then(Value::as_object_mut),
        source.get("outcomes").and_then(Value::as_object),
    ) {
        for (group, target_values) in target_groups {
            if let (Some(target_values), Some(source_values)) = (
                target_values.as_array_mut(),
                source_groups.get(group).and_then(Value::as_array),
            ) {
                for target in target_values {
                    let oid = target["outcomeId"].as_i64();
                    if let Some(source) = source_values
                        .iter()
                        .find(|source| oid.is_some() && source["outcomeId"].as_i64() == oid)
                    {
                        apply_visibility_flags(target, source);
                    }
                }
            }
        }
    }
}
struct PicoVariant {
    value: Value,
    paths: Vec<String>,
    omissions: Vec<Value>,
    conflicts: Vec<String>,
}
impl Collection {
    fn variants(&self, pid: i64) -> Result<Vec<PicoVariant>> {
        let paths = self
            .picos
            .get(&pid)
            .context("MAGIC declared native picoId has no source object")?;
        let mut variants: Vec<PicoVariant> = vec![];
        for path in paths {
            let (value, omissions) = project_pico(pointer(&self.root, path)?, path)?;
            let mut matched = false;
            let mut conflicts = vec![];
            for variant in &mut variants {
                match merge(&variant.value, &value, "pico") {
                    Ok(merged) => {
                        let mut merge_omissions = vec![];
                        variant.value = visible(
                            &merged,
                            "pico.compatible_alias_projection",
                            &mut merge_omissions,
                            0,
                        )?;
                        variant.paths.push(path.clone());
                        variant.omissions.extend(omissions.clone());
                        variant.omissions.extend(merge_omissions);
                        matched = true;
                        break;
                    }
                    Err(path) => conflicts.push(path),
                }
            }
            if !matched {
                variants.push(PicoVariant {
                    value,
                    paths: vec![path.clone()],
                    omissions,
                    conflicts,
                });
            }
        }
        for variant in &mut variants {
            for path in paths {
                apply_visibility_flags(&mut variant.value, pointer(&self.root, path)?);
            }
            variant.value = visible(
                &variant.value,
                "pico.aggregate_alias_visibility",
                &mut variant.omissions,
                0,
            )?;
        }
        Ok(variants)
    }
    fn eligible_picos(&self) -> Result<HashSet<i64>> {
        let mut ids = HashSet::new();
        for record in &self.records {
            let item = pointer(&self.root, &record.pointer)?;
            if item["hideInPublish"] == true {
                continue;
            }
            for value in item["picoIds"]
                .as_array()
                .context("MAGIC recommendation picoIds missing")?
            {
                ids.insert(
                    value
                        .as_i64()
                        .filter(|n| *n > 0)
                        .context("MAGIC native picoId link invalid")?,
                );
            }
        }
        Ok(ids)
    }
    fn coverage(&self) -> Result<Value> {
        let linked = self.eligible_picos()?;
        let hidden=self.records.iter().filter_map(|record|{let value=self.root.pointer(&record.pointer)?;(value["hideInPublish"]==true).then(||json!({"recommendationId":record.id,"hideInPublish":true,"source_sha256":hash(value),"text_exported":false}))}).collect::<Vec<_>>();
        let unlinked=self.picos.iter().filter(|(pid,_)|!linked.contains(*pid)).map(|(pid,paths)|json!({"picoId":pid,"source_locations":paths,"source_variant_sha256":paths.iter().map(|path|self.root.pointer(path).map(hash)).collect::<Vec<_>>(),"visibility":"unresolved section-only/unattached profile; metadata only","clinical_text_exported":false})).collect::<Vec<_>>();
        Ok(
            json!({"source_scope":"one exact observed Stroke publication11482 export; not platform/catalog completeness","section_records":self.sections.len(),"recommendation_records":self.records.len(),"visible_recommendation_records":self.records.len()-hidden.len(),"hidden_recommendation_omissions":hidden,"unique_native_picos":self.picos.len(),"visible_linked_native_picos":linked.len(),"unattached_or_only_hidden_parent_picos":unlinked,"catalog_complete":false,"full_clinical_fidelity_verified":false}),
        )
    }
}

const OUTCOME_FIELDS: &[&str] = &[
    "absoluteDifference",
    "absoluteDifferenceConfidenceType",
    "absoluteDifferenceDirection",
    "absoluteDifferenceDirectionType",
    "absoluteDifferenceHigh",
    "absoluteDifferenceHighDirection",
    "absoluteDifferenceLow",
    "absoluteDifferenceLowDirection",
    "absoluteDifferenceType",
    "autoCalcInterventions",
    "comparatorEffect",
    "comparatorEffectConfidenceHigh",
    "comparatorEffectConfidenceLow",
    "comparatorEffectConfidenceType",
    "comparatorEffectType",
    "comparatorFollowUp",
    "comparatorReference",
    "comparatorReferenceType",
    "comparatorStudies",
    "comparatorStudyData",
    "comparatorStudyType",
    "comparatorTotalEvents",
    "comparatorTotalParticipants",
    "description",
    "directionOfEffectType",
    "estimatesDescription",
    "extraReferences",
    "guidelineId",
    "importanceLevel",
    "interventionComparison",
    "interventionComparisonId",
    "interventionComparisonRevManId",
    "interventionEffect",
    "interventionEffectConfidenceHigh",
    "interventionEffectConfidenceLow",
    "interventionEffectConfidenceType",
    "interventionEffectType",
    "interventionOutcome",
    "interventionOutcomeId",
    "interventionOutcomeResults",
    "interventionOutcomeRevManId",
    "interventionPrimaryComment",
    "interventionReference",
    "interventionReferenceType",
    "interventionStudies",
    "interventionStudyData",
    "interventionStudyFollowUp",
    "interventionStudyType",
    "interventionTotalParticipants",
    "isHidden",
    "measurementLabel",
    "meshId",
    "meshTerm",
    "outcome",
    "outcomeId",
    "picoId",
    "plainSummaryComment",
    "qualityImprecision",
    "qualityImprecisionComment",
    "qualityImprecisionIssues",
    "qualityInconsistency",
    "qualityInconsistencyComment",
    "qualityInconsistencyIssues",
    "qualityIndirectness",
    "qualityIndirectnessComment",
    "qualityIndirectnessIssues",
    "qualityInterventionStudyType",
    "qualityOfEvidenceComment",
    "qualityOfEvidenceLevel",
    "qualityPublicationBias",
    "qualityPublicationBiasComment",
    "qualityPublicationBiasIssues",
    "qualityRiskOfBias",
    "qualityRiskOfBiasComment",
    "qualityRiskOfBiasIssues",
    "qualityUpgrade",
    "qualityUpgradeComment",
    "relativeEffect",
    "relativeEffectConfidenceHigh",
    "relativeEffectConfidenceLow",
    "relativeEffectConfidenceType",
    "relativeEffectType",
    "scaleDirection",
    "scaleHigh",
    "scaleLow",
    "sectionId",
    "shadow_PARENT_ID_FOR_MERGING",
    "shortCode",
    "shortName",
    "sortOrder",
    "status",
    "timeFrame",
    "unit",
];

#[derive(Default)]
struct View {
    blocks: Vec<Block>,
    components: Vec<Value>,
    assets: Vec<Value>,
    omissions: Vec<Value>,
    warnings: Vec<String>,
    serial: usize,
}
impl View {
    fn heading(&mut self, title: &str, path: &[String]) {
        self.serial += 1;
        self.blocks.push(Block {
            block_id: format!("magic-heading-{}", self.serial),
            kind: BlockKind::Heading { level: 4 },
            text: title.into(),
            heading_path: path.to_vec(),
            page: None,
        });
    }
    fn asset(&mut self, source: Value, path: &str, role: &str) {
        if let Some(old) = self
            .assets
            .iter_mut()
            .find(|old| old["source_fields"] == source && old["role"] == role)
        {
            if let Some(paths) = old["source_paths"].as_array_mut() {
                paths.push(json!(path))
            }
            return;
        }
        let url = source["fullPath"]
            .as_str()
            .and_then(absolute)
            .or_else(|| source["url"].as_str().and_then(absolute));
        self.assets.push(json!({"url":url,"role":role,"source_fields":source,"source_paths":[path],"bytes_downloaded":false,"mime_verified":false,"relative_origin_unresolved":url.is_none()}));
    }
    fn component(
        &mut self,
        label: &str,
        raw: &str,
        source_path: &str,
        path: &[String],
        statement: Option<(&Value, bool)>,
    ) -> Result<()> {
        if raw.trim().is_empty() {
            return Ok(());
        }
        if raw.len() > 8 * 1024 * 1024 {
            bail!("MAGIC rich component size exceeded")
        }
        let html = rich_field_html(raw);
        let (mut parts, notes) = blocks_from_html(
            &format!("<article id='magic-component'>{html}</article>"),
            None,
            Some("#magic-component"),
        );
        self.warnings.extend(notes);
        let source_parts = parts.clone();
        let mut scope = path.to_vec();
        if statement.is_none() {
            scope.push(label.into());
            self.heading(label, &scope);
        }
        self.serial += 1;
        let prefix = format!("magic-component-{}", self.serial);
        for (n, block) in parts.iter_mut().enumerate() {
            block.block_id = format!("{prefix}-{n}-{}", block.block_id);
            let mut heading = scope.clone();
            heading.extend(block.heading_path.clone());
            block.heading_path = heading;
            if let BlockKind::Figure { src_url, .. } = &mut block.kind {
                if let Some(raw) = src_url.clone() {
                    if absolute(&raw).is_none() {
                        self.asset(
                            json!({"source_literal":raw}),
                            source_path,
                            "unresolved_relative_figure",
                        );
                        *src_url = None;
                        self.warnings.push("Relative figure URI has no verified source origin; native descriptor retained without inventing a usable URL".into());
                    }
                }
            }
        }
        let mut canonical_ids = vec![];
        if let Some((record, info)) = statement {
            if info {
                canonical_ids.extend(parts.iter().map(|b| b.block_id.clone()));
                self.blocks.extend(parts);
            } else {
                let text = parts
                    .iter()
                    .filter(|b| {
                        !matches!(b.kind, BlockKind::Table { .. } | BlockKind::Figure { .. })
                    })
                    .map(|b| b.text.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if text.is_empty() {
                    bail!("MAGIC visible recommendation text has no clinical statement")
                }
                let draft = draft_labels(raw);
                let mut qualifier = vec![];
                if let Some(state) = record["status"].as_str() {
                    qualifier.push(format!("Native status: {state}"));
                }
                qualifier.extend(draft);
                let strength = record["strength"]
                    .as_str()
                    .filter(|v| !matches!(*v, "NOTSET" | "PRACTICE" | "INFO"))
                    .map(str::to_owned);
                if let Some(value) = record["strength"]
                    .as_str()
                    .filter(|v| matches!(*v, "PRACTICE" | "NOTSET"))
                {
                    qualifier.push(format!("Native source strength field: {value}"));
                }
                let block_id = format!("magic-recommendation-{}", id(record, "recommendationId")?);
                canonical_ids.push(block_id.clone());
                self.blocks.push(Block {
                    block_id,
                    kind: BlockKind::Recommendation {
                        label: None,
                        strength,
                        certainty: None,
                        population: None,
                        grading_system: None,
                        qualifier: (!qualifier.is_empty()).then(|| qualifier.join("\n")),
                    },
                    text,
                    heading_path: scope.clone(),
                    page: None,
                });
                for block in parts.into_iter().filter(|b| {
                    matches!(b.kind, BlockKind::Table { .. } | BlockKind::Figure { .. })
                }) {
                    canonical_ids.push(block.block_id.clone());
                    self.blocks.push(block);
                }
            }
        } else {
            canonical_ids.extend(parts.iter().map(|b| b.block_id.clone()));
            self.blocks.extend(parts);
        }
        let fragment = Html::parse_fragment(&html);
        for link in fragment.select(&sel("a[href],iframe[src]")) {
            let attribute = if link.value().name() == "iframe" {
                "src"
            } else {
                "href"
            };
            let raw = link.value().attr(attribute).unwrap_or_default();
            if !raw.is_empty() {
                self.asset(json!({"url":absolute(raw),"source_literal":raw,"label":text_of(&link),"attribute":attribute}),source_path,if attribute=="src"{"unacquired_embedded_asset"}else{"source_linked_resource"});
            }
        }
        self.components.push(json!({"source_path":source_path,"source_label":label,"source_html":html,"source_value":raw,"source_value_sha256":hash(&json!(raw)),"source_blocks_in_native_order":source_parts,"canonical_block_ids":canonical_ids,"statement_boundary":"one native recommendation.text component; tables/figures retained as companions; source blocks/HTML preserve exact internal interleaving"}));
        Ok(())
    }
    fn structured(
        &mut self,
        label: &str,
        value: &Value,
        source_path: &str,
        path: &[String],
    ) -> Result<()> {
        let mut cells = vec![];
        flatten(value, "", &mut cells, 0)?;
        if cells.is_empty() {
            return Ok(());
        }
        let mut html=String::from("<table><thead><tr><th>Native source field</th><th>Native source value</th></tr></thead><tbody>");
        for (key, value) in cells {
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td></tr>",
                escaped(&key),
                escaped(&value)
            ));
        }
        html.push_str("</tbody></table>");
        self.component(label, &html, source_path, path, None)
    }
    fn references(&mut self, refs: &Value, source_path: &str, path: &[String]) -> Result<()> {
        let Some(values) = refs.as_array() else {
            return Ok(());
        };
        for (n, reference) in values.iter().enumerate() {
            let mut citation = vec![];
            if let Some(authors) = reference["authors"].as_array() {
                let names = authors
                    .iter()
                    .map(|a| {
                        a["formattedName"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                format!(
                                    "{} {}",
                                    a["firstName"].as_str().unwrap_or(""),
                                    a["lastName"].as_str().unwrap_or("")
                                )
                            })
                    })
                    .filter(|s| !s.trim().is_empty())
                    .collect::<Vec<_>>();
                if !names.is_empty() {
                    citation.push(names.join(", "));
                }
            }
            for key in [
                "titleText",
                "journal",
                "year",
                "volume",
                "issue",
                "pageStart",
                "pageEnd",
                "doi",
                "pmid",
                "url",
            ] {
                if let Some(value) = reference[key].as_str().filter(|s| !s.is_empty()) {
                    citation.push(format!("{key}: {value}"));
                }
            }
            self.component(
                "References",
                &citation.join(". "),
                &format!("{source_path}[{n}]"),
                path,
                None,
            )?;
            for key in ["files", "attachedFiles"] {
                if let Some(files) = reference[key].as_array() {
                    for file in files {
                        self.asset(
                            file.clone(),
                            &format!("{source_path}[{n}].{key}"),
                            "reference_file",
                        );
                    }
                }
            }
        }
        Ok(())
    }
    fn pico(&mut self, pid: i64, variant: &PicoVariant, n: usize, path: &[String]) -> Result<()> {
        let value = &variant.value;
        let mut scope = path.to_vec();
        scope.push(format!("PICO {pid}"));
        if n > 0 {
            scope.push(format!("Source variant {}", n + 1));
        }
        self.heading(&format!("PICO {pid}"), &scope);
        self.omissions.extend(variant.omissions.clone());
        for key in [
            "population",
            "intervention",
            "comparator",
            "setting",
            "summary",
        ] {
            if let Some(value) = value[key].as_str() {
                self.component(key, value, &format!("pico:{pid}.{key}"), &scope, None)?;
            }
        }
        if let Some(groups) = value["outcomes"].as_object() {
            for group in [
                "dichotomousOutcomes",
                "continuousOutcomes",
                "nonPoolableOutcomes",
                "practicalIssues",
            ] {
                let Some(items) = groups.get(group).and_then(Value::as_array) else {
                    continue;
                };
                let mut positions = (0..items.len()).collect::<Vec<_>>();
                positions.sort_by_key(|index| {
                    (
                        items[*index]["sortOrder"].as_i64().unwrap_or(i64::MAX),
                        *index,
                    )
                });
                for index in positions {
                    let outcome = &items[index];
                    if outcome["isHidden"] == true {
                        continue;
                    }
                    let oid = outcome["outcomeId"].as_i64();
                    let mut outcome_path = scope.clone();
                    outcome_path.push(
                        outcome["outcome"]
                            .as_str()
                            .map(plain)
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| format!("{group} native outcome {:?}", oid)),
                    );
                    self.heading(outcome_path.last().unwrap(), &outcome_path);
                    let source_path = format!("pico:{pid}.outcomes.{group}[{index}]");
                    let mut scalars = Map::new();
                    for (key, field) in outcome
                        .as_object()
                        .context("MAGIC projected outcome object missing")?
                    {
                        if matches!(key.as_str(), "outcome" | "shortName") {
                            continue;
                        }
                        if matches!(
                            key.as_str(),
                            "interventionReference" | "comparatorReference"
                        ) {
                            if field.is_object() {
                                self.references(
                                    &json!([field]),
                                    &format!("{source_path}.{key}"),
                                    &outcome_path,
                                )?;
                            }
                            continue;
                        }
                        if key.ends_with("Comment") || key == "description" {
                            if let Some(text) = field.as_str() {
                                self.component(
                                    key,
                                    text,
                                    &format!("{source_path}.{key}"),
                                    &outcome_path,
                                    None,
                                )?;
                            }
                            continue;
                        }
                        scalars.insert(key.clone(), field.clone());
                    }
                    self.structured("Native outcome data (importance, certainty and effects are separate fields)",&json!(scalars),&source_path,&outcome_path)?;
                }
            }
        }
        self.references(
            &value["references"],
            &format!("pico:{pid}.references"),
            &scope,
        )?;
        if !variant.conflicts.is_empty() {
            self.warnings.push(format!("Native PICO {pid} has incompatible source variants at {}; alternatives remain separate, not overwritten",variant.conflicts.join(", ")));
        }
        Ok(())
    }
}
fn flatten(value: &Value, path: &str, out: &mut Vec<(String, String)>, depth: usize) -> Result<()> {
    if depth > 24 || out.len() > 50000 {
        bail!("MAGIC native structured clinical data bound exceeded")
    }
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                flatten(
                    value,
                    &if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    },
                    out,
                    depth + 1,
                )?;
            }
        }
        Value::Array(values) => {
            for (n, value) in values.iter().enumerate() {
                flatten(value, &format!("{path}[{n}]"), out, depth + 1)?;
            }
        }
        Value::Null => {}
        Value::String(value) if value.is_empty() => {}
        Value::String(value) => out.push((path.into(), plain(value))),
        _ => out.push((path.into(), value.to_string())),
    }
    Ok(())
}
fn draft_labels(raw: &str) -> Vec<String> {
    let d = Html::parse_fragment(raw);
    d.select(&sel("strong u,u strong"))
        .map(|e| text_of(&e))
        .filter(|s| {
            s.to_ascii_uppercase().starts_with("DRAFT RECOMMENDATION")
                || s.to_ascii_uppercase().starts_with("DRAFT FOR CONSULTATION")
        })
        .collect()
}
fn state(record: &Value) -> &'static str {
    if record["status"] == "UNDER_REVIEW"
        || (record["textInPublic"] != false
            && record["text"]
                .as_str()
                .is_some_and(|raw| !draft_labels(raw).is_empty()))
    {
        "in_review"
    } else {
        "unknown"
    }
}
fn dates(record: &Value, url: &str, keys: &[&str]) -> Vec<DateObservation> {
    keys.iter()
        .filter_map(|key| {
            record[*key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|value| common::date(key, value, url))
        })
        .collect()
}

impl Collection {
    fn base(
        &self,
        src: &SourceEntry,
        title: String,
        url: String,
        work: String,
        item_type: &str,
    ) -> GuidelineDoc {
        GuidelineDoc{source_id:src.id.clone(),work_key:Some(work),title,url,version:Some("14.0".into()),status:"unknown".into(),item_type:Some(item_type.into()),language:self.root["language"].as_str().filter(|s|!s.is_empty()).map(str::to_owned),
        rights:Rights{status:"unknown".into(),note:Some("Platform access and issuer reuse rights are separate. Source UI states Stroke Foundation reproduction requires permission; this adapter accepts no agreement and requests no permission.".into()),restrictions:vec!["Source UI About dialog states reproduction requires Stroke Foundation permission; no reuse grant established.".into()],evidence_urls:vec![READER.into()],..Default::default()},
        search_metadata:json!({"platform":"MAGICapp","source_export_url":EXPORT_URL,"publication_scope":"fixed source-observed publication11482 / UIv14.0; no latest/publication-state inference","source_guideline":self.root_metadata(),"full_platform_catalog_complete":false}),fetches:vec![self.fetch.clone()],..Default::default()}
    }
    fn finish(&self, mut doc: GuidelineDoc, view: View) -> Result<GuidelineDoc> {
        doc.blocks = view.blocks;
        doc.attachments = view.assets;
        doc.search_metadata["source_components"] = json!(view.components);
        doc.search_metadata["visibility_omissions"] = json!(view.omissions);
        doc.warnings.extend(view.warnings);
        if doc.search_metadata["visibility_omissions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|entry| {
                entry["reason"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("unrecognized"))
            })
        {
            doc.warnings.push("Unrecognized source clinical fields/groups were omitted with source paths and hashes; full clinical fidelity is unresolved".into());
        }
        doc.warnings.push("One fixed source-emitted publication export only. Public render eligibility follows native component visibility; unattached section PICO visibility is unresolved and its clinical text is excluded. Native strength/status/importance/certainty/effect fields are separately scoped; source/API/UI conflicts are not reconciled by inference. Linked assets are descriptors, not verified downloaded content.".into());
        if doc.blocks.is_empty() {
            doc.warnings.push(
                "This is a source metadata/index document with no acquired clinical body".into(),
            );
        }
        let snapshot = hash(
            &json!({"work_key":doc.work_key,"title":doc.title,"url":doc.url,"version":doc.version,"status":doc.status,"item_type":doc.item_type,"dates":doc.date_observations,"relations":doc.relations,"metadata":doc.search_metadata,"rights":doc.rights,"blocks":doc.blocks,"assets":doc.attachments}),
        );
        doc.doc_key = format!(
            "{}-publication11482-{}",
            common::slug(doc.work_key.as_deref().unwrap_or("record")),
            &snapshot[..16]
        );
        doc.search_metadata["semantic_sha256"] = json!(snapshot);
        Ok(doc)
    }
    fn parent_doc(&self, src: &SourceEntry) -> Result<GuidelineDoc> {
        let title = self.root["name"]
            .as_str()
            .map(plain)
            .context("MAGIC guideline title missing")?;
        let mut doc = self.base(
            src,
            title,
            READER.into(),
            "magic-guideline-QnoKGn".into(),
            "guideline_version_index",
        );
        let mut view = View::default();
        for key in ["description", "sponsors", "disclaimer"] {
            if let Some(raw) = self.root[key].as_str() {
                view.component(key, raw, &format!("$.{key}"), &[], None)?;
            }
        }
        doc.date_observations = dates(
            &self.root,
            READER,
            &[
                "startDate",
                "lastEdit",
                "publishCreationTime",
                "publishDate",
                "publishLastSearchDate",
            ],
        );
        doc.date_observations.push(DateObservation {
            label: "Observed UI header published".into(),
            raw: "9/15/2026".into(),
            normalized: None,
            source_url: Some(READER.into()),
        });
        doc.date_observations.push(DateObservation {
            label: "Observed UI history published".into(),
            raw: "2026-09-16".into(),
            normalized: Some("2026-09-16".into()),
            source_url: Some(READER.into()),
        });
        doc.search_metadata["collection_coverage"] = self.coverage()?;
        for section in &self.sections {
            doc.relations.push(Relation {
                rel: "has_part".into(),
                target: source_url("section", 11482, section.id),
                scope: Some("native sectionId and observed versioned share-link pattern".into()),
                evidence_url: Some(READER.into()),
            });
        }
        if let Some(refs) = self.root["references"].as_array() {
            let projected = refs
                .iter()
                .map(|reference| reference_projection(reference, &mut view.omissions))
                .collect::<Result<Vec<_>>>()?;
            view.references(&json!(projected), "$.references", &[])?;
        }
        self.finish(doc, view)
    }
    fn section_doc(&self, src: &SourceEntry, index: usize) -> Result<GuidelineDoc> {
        let section = &self.sections[index];
        let source = pointer(&self.root, &section.pointer)?;
        let url = source_url("section", 11482, section.id);
        let work = code(source, "shortCode")
            .map(|code| format!("magic-section-{code}"))
            .unwrap_or_else(|| format!("magic-section-versioned-11482-{}", section.id));
        let mut doc = self.base(
            src,
            section.headings.join(" › "),
            url.clone(),
            work,
            "guideline_section",
        );
        let mut view = View::default();
        if let Some(text) = source["text"].as_str() {
            view.component(
                "Section introduction",
                text,
                &format!("{}.text", section.pointer),
                &section.headings,
                None,
            )?;
        }
        doc.search_metadata["native_section"] = native(
            source,
            &[
                "guidelineId",
                "sectionId",
                "parentId",
                "order",
                "sectionType",
                "shortCode",
                "sectionLabel",
                "sectionNumber",
                "isNumbered",
            ],
        );
        doc.search_metadata["source_array_index"] = json!(section.source_index);
        doc.search_metadata["native_recommendation_records"]=json!(self.records.iter().filter(|record|record.section==index).map(|record|{let source=&self.root.pointer(&record.pointer).unwrap();if source["hideInPublish"]==true{json!({"recommendationId":record.id,"hideInPublish":true,"source_sha256":hash(source),"text_exported":false})}else{native(source,&["recommendationId","shortCode","order","hideInPublish","status"])}}).collect::<Vec<_>>());
        doc.search_metadata["section_pico_records"]=json!(source["picos"].as_array().into_iter().flatten().map(|pico|json!({"picoId":pico["picoId"],"source_sha256":hash(pico),"clinical_text_exported":false,"visibility_scope":"section cache alone does not establish public evidence-profile visibility"})).collect::<Vec<_>>());
        doc.relations.push(Relation {
            rel: "part_of".into(),
            target: section
                .parent
                .map(|n| source_url("section", 11482, self.sections[n].id))
                .unwrap_or_else(|| READER.into()),
            scope: Some(
                "native section containment; declaration and source parentId retained separately"
                    .into(),
            ),
            evidence_url: Some(url),
        });
        self.finish(doc, view)
    }
    fn recommendation_doc(&self, src: &SourceEntry, index: usize) -> Result<GuidelineDoc> {
        let record = &self.records[index];
        let source = pointer(&self.root, &record.pointer)?;
        if source["hideInPublish"] == true {
            bail!(
                "MAGIC selected recommendation {} is native hideInPublish=true; body excluded",
                record.id
            )
        }
        let section = &self.sections[record.section];
        let own_heading = source["heading"].as_str().map(plain).unwrap_or_default();
        let identifier = code(source, "shortCode").unwrap_or_else(|| record.id.to_string());
        let title = if own_heading.is_empty() {
            format!("{} › {identifier}", section.headings.join(" › "))
        } else {
            own_heading.clone()
        };
        let info = source["strength"] == "INFO";
        let url = source_url("rec", 11482, record.id);
        let mut doc = self.base(
            src,
            title,
            url.clone(),
            code(source, "shortCode")
                .map(|value| format!("magic-recommendation-{value}"))
                .unwrap_or_else(|| format!("magic-recommendation-versioned11482-{}", record.id)),
            if info {
                "guideline_information"
            } else {
                "guideline_recommendation"
            },
        );
        doc.status = state(source).into();
        doc.date_observations = dates(source, &url, &["statusDate"]);
        let mut view = View::default();
        doc.search_metadata["native_recommendation"] = native(
            source,
            &[
                "recommendationId",
                "shortCode",
                "guidelineId",
                "sectionId",
                "order",
                "strength",
                "heading",
                "status",
                "statusDate",
                "statusComment",
                "hideInPublish",
                "textInPublic",
                "keyInfoType",
                "keyInfoDisplayMode",
                "showAdaptation",
                "referenceIds",
                "picoIds",
            ],
        );
        doc.search_metadata["source_locator"] = json!({"json_pointer":record.pointer,"source_array_index":record.source_index,"section_headings":section.headings,"native_section":native(pointer(&self.root,&section.pointer)?,&["sectionId","shortCode","parentId","order"])});
        doc.search_metadata["native_status_signals"] = json!({"export_status":source["status"],"explicit_draft_labels":if source["textInPublic"]==false{vec![]}else{source["text"].as_str().map(draft_labels).unwrap_or_default()},"parent_status_not_inherited":true,"statusDate_event":"native statusDate, not inferred approval/review/publication event"});
        doc.search_metadata["source_record_type"]=json!("recommendation object; INFO native code is source-verified Info Box and is not typed as a formal recommendation");
        doc.relations.push(Relation {
            rel: "part_of".into(),
            target: source_url("section", 11482, section.id),
            scope: Some("native recommendation.sectionId and source section containment".into()),
            evidence_url: Some(url.clone()),
        });
        if let Some(code) = code(source, "shortCode") {
            doc.relations.push(Relation{rel:"related".into(),target:format!("https://app.magicapp.org/#/guideline/QnoKGn/rec/{code}"),scope:Some("observed always-current share-link pattern; target not fetched, fixed source document does not assert latest".into()),evidence_url:Some(url.clone())});
        }
        let raw = source["text"]
            .as_str()
            .context("MAGIC recommendation text missing")?;
        if source["textInPublic"] == false {
            omitted(
                &source["text"],
                &format!("{}.text", record.pointer),
                "native paired InPublic=false",
                &mut view.omissions,
            );
        } else {
            view.component(
                "Recommendation",
                raw,
                &format!("{}.text", record.pointer),
                &section.headings,
                Some((source, info)),
            )?;
        }
        let mut visible_source = Map::new();
        for (key, label) in [
            ("remarks", "Remarks"),
            ("advice", "Practical information"),
            ("rational", "Rationale"),
            ("adaptation", "Adaptation"),
            ("implementation", "Implementation"),
            ("evaluation", "Evaluation"),
            ("research", "Research"),
        ] {
            if let Some(raw) = source[key].as_str() {
                if source[format!("{key}InPublic")] == false {
                    omitted(
                        &source[key],
                        &format!("{}.{key}", record.pointer),
                        "native paired InPublic=false",
                        &mut view.omissions,
                    );
                } else {
                    view.component(
                        label,
                        raw,
                        &format!("{}.{key}", record.pointer),
                        &section.headings,
                        None,
                    )?;
                    visible_source.insert(key.into(), json!(raw));
                }
            }
        }
        if source["keyInfo"].is_object() {
            let projected = visible(
                &source["keyInfo"],
                &format!("{}.keyInfo", record.pointer),
                &mut view.omissions,
                0,
            )?;
            for key in [
                "benefits",
                "evidence",
                "preferences",
                "resources",
                "acceptability",
                "equity",
                "feasibility",
            ] {
                if let Some(raw) = projected[key].as_str() {
                    view.component(
                        key,
                        raw,
                        &format!("{}.keyInfo.{key}", record.pointer),
                        &section.headings,
                        None,
                    )?;
                }
                for suffix in ["ResearchEvidence", "AdditionalConsideration"] {
                    let field = format!("{key}{suffix}");
                    if let Some(raw) = projected[&field].as_str() {
                        view.component(
                            &field,
                            raw,
                            &format!("{}.keyInfo.{field}", record.pointer),
                            &section.headings,
                            None,
                        )?;
                    }
                }
            }
            if projected["interventions"].is_array() {
                view.structured(
                    "Native public evidence-to-decision judgements",
                    &projected["interventions"],
                    &format!("{}.keyInfo.interventions", record.pointer),
                    &section.headings,
                )?;
            }
            doc.search_metadata["public_keyInfo"] = projected;
        }
        if source["interventions"].is_object() {
            omitted(
                &source["interventions"],
                &format!("{}.interventions", record.pointer),
                "EMR intervention component public visibility not established",
                &mut view.omissions,
            );
            doc.search_metadata["native_emr_intervention_identity"] = native(
                &source["interventions"],
                &["recommendationId", "shortCode", "guidelineId", "sectionId"],
            );
        }
        doc.search_metadata["public_recommendation_components"] = json!(visible_source);
        let declared = source["picoIds"]
            .as_array()
            .context("MAGIC recommendation picoIds missing")?;
        let mut used = HashSet::new();
        let mut evidence = vec![];
        for value in declared {
            let pid = value
                .as_i64()
                .filter(|n| *n > 0)
                .context("MAGIC native PICO relation invalid")?;
            if !used.insert(pid) {
                continue;
            }
            let variants = self.variants(pid)?;
            for (n, variant) in variants.iter().enumerate() {
                view.pico(pid, variant, n, &section.headings)?;
            }
            evidence.push(json!({"picoId":pid,"relation":"native recommendation.picoIds","all_source_visibility_observations":self.picos.get(&pid).into_iter().flatten().flat_map(|path|self.root.pointer(path).map(|value|visibility_observations(value,path)).unwrap_or_default()).collect::<Vec<_>>(),"source_variants":variants.iter().map(|variant|json!({"projection":variant.value,"source_paths":variant.paths,"source_record_sha256":variant.paths.iter().map(|path|json!({"source_path":path,"sha256":self.root.pointer(path).map(hash),"native_visibility_observations":self.root.pointer(path).map(|value|visibility_observations(value,path))})).collect::<Vec<_>>(),"alias_visibility_policy":"false InPublic / true isHidden dominate all native aliases, including conflicting clinical variants; source flags retained separately","conflict_paths":variant.conflicts,"visibility_omissions":variant.omissions,"semantic_sha256":hash(&variant.value)})).collect::<Vec<_>>(),"rendering_identity_not_used":["C_0","C_1"],"permalink":null}));
        }
        doc.search_metadata["native_pico_evidence"] = json!(evidence);
        if let Some(references) = source["references"].as_array() {
            let projected = references
                .iter()
                .map(|reference| reference_projection(reference, &mut view.omissions))
                .collect::<Result<Vec<_>>>()?;
            view.references(
                &json!(projected),
                &format!("{}.references", record.pointer),
                &section.headings,
            )?;
            doc.search_metadata["public_references"] = json!(projected);
        }
        for key in ["files", "images"] {
            if let Some(files) = source[key].as_array() {
                for file in files {
                    view.asset(
                        asset_projection(file),
                        &format!("{}.{key}", record.pointer),
                        if key == "images" {
                            "source_image_descriptor"
                        } else {
                            "recommendation_file"
                        },
                    );
                }
            }
        }
        self.finish(doc, view)
    }
}

fn selected(collection: &Collection, src: &SourceEntry) -> Result<Vec<usize>> {
    let filter = src
        .config
        .get("recommendation_ids")
        .map(|value| {
            value
                .as_array()
                .context("MAGIC recommendation_ids must be an array")
        })
        .transpose()?;
    let mut ids = HashSet::new();
    if let Some(values) = filter {
        for value in values {
            let native = value
                .as_i64()
                .filter(|n| *n > 0)
                .context("MAGIC selected recommendation ID invalid")?;
            if !ids.insert(native) {
                bail!("MAGIC duplicate selected recommendation ID")
            }
            let record = collection
                .records
                .iter()
                .find(|record| record.id == native)
                .context("MAGIC selected recommendation ID absent from exact source export")?;
            if pointer(&collection.root, &record.pointer)?["hideInPublish"] == true {
                bail!(
                    "MAGIC selected recommendation {native} is hideInPublish=true; output excluded"
                )
            }
        }
    }
    Ok(collection
        .records
        .iter()
        .enumerate()
        .filter(|(_, record)| {
            (filter.is_none() || ids.contains(&record.id))
                && collection
                    .root
                    .pointer(&record.pointer)
                    .is_some_and(|value| value["hideInPublish"] != true)
        })
        .map(|(n, _)| n)
        .collect())
}
fn plan(
    src: &SourceEntry,
    doc: GuidelineDoc,
    kind: &str,
    native_id: i64,
    scope: &Value,
) -> PlanRow {
    let extra = json!({"kind":kind,"native_id":native_id,"semantic_sha256":doc.search_metadata["semantic_sha256"],"selection_scope":scope});
    PlanRow {
        source_id: src.id.clone(),
        doc_key: doc.doc_key,
        url: doc.url,
        title: Some(doc.title),
        rendition: Some("json".into()),
        version: doc.version,
        extra,
    }
}
#[async_trait::async_trait]
impl Adapter for MagicAppPublic {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let collection = self.load(src, f).await?;
        let selected = selected(collection, src)?;
        let scope = json!({"publicationId":11482,"recommendation_ids":src.config["recommendation_ids"],"eligible_selected_records":selected.len(),"collection_coverage":collection.coverage()?,"source_export_sha256":collection.fetch.sha256,"catalog_complete":false});
        let include_indexes = src.config["include_indexes"]
            .as_bool()
            .unwrap_or(limit.is_none() && src.config.get("recommendation_ids").is_none());
        let mut rows = vec![];
        if include_indexes {
            rows.push(plan(
                src,
                collection.parent_doc(src)?,
                "parent_index",
                11482,
                &scope,
            ));
            for (n, section) in collection.sections.iter().enumerate() {
                rows.push(plan(
                    src,
                    collection.section_doc(src, n)?,
                    "section_index",
                    section.id,
                    &scope,
                ));
            }
        }
        for index in selected.into_iter().take(limit.unwrap_or(usize::MAX)) {
            let record = &collection.records[index];
            match collection.recommendation_doc(src,index) {
            Ok(doc)=>rows.push(plan(src,doc,"recommendation",record.id,&scope)),
            Err(error)=>rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("magic-recommendation-{}-unresolved-{}",record.id,&hash(pointer(&collection.root,&record.pointer)?)[..16]),url:source_url("rec",11482,record.id),title:None,rendition:Some("json".into()),version:Some("14.0".into()),extra:json!({"kind":"recommendation","native_id":record.id,"discovery_error":format!("{error:#}"),"selection_scope":scope})})
        }
        }
        Ok(rows)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if row.source_id != src.id {
            bail!("MAGIC plan source ID mismatch")
        }
        if row.extra["discovery_error"].is_string() {
            bail!("MAGIC source record unresolved in discovery")
        }
        let collection = self.load(src, f).await?;
        let native = row.extra["native_id"]
            .as_i64()
            .context("MAGIC plan native identity missing")?;
        let doc = match row.extra["kind"].as_str() {
            Some("parent_index") if native == 11482 => collection.parent_doc(src)?,
            Some("section_index") => {
                let index = collection
                    .sections
                    .iter()
                    .position(|section| section.id == native)
                    .context("MAGIC planned section absent")?;
                collection.section_doc(src, index)?
            }
            Some("recommendation") => {
                let index = collection
                    .records
                    .iter()
                    .position(|record| record.id == native)
                    .context("MAGIC planned recommendation absent")?;
                collection.recommendation_doc(src, index)?
            }
            _ => bail!("MAGIC plan kind/identity unsupported"),
        };
        if row.doc_key != doc.doc_key
            || row.url != doc.url
            || row.version != doc.version
            || row.extra["semantic_sha256"] != doc.search_metadata["semantic_sha256"]
        {
            bail!("MAGIC visible clinical/metadata/evidence/visibility/state/asset snapshot changed; replan")
        }
        Ok(doc)
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const FIXTURE: &str = include_str!(
        "../../../tests/fixtures/guidelines/magicapp_public/publication-synthetic.json"
    );
    fn root() -> Value {
        serde_json::from_str(FIXTURE).unwrap()
    }
    fn collection(value: Value) -> Collection {
        Collection::build(
            value,
            FetchRecord {
                url: EXPORT_URL.into(),
                final_url: EXPORT_URL.into(),
                status: 200,
                content_type: Some("application/json".into()),
                ..Default::default()
            },
        )
        .unwrap()
    }
    fn source() -> SourceEntry {
        serde_json::from_value(json!({"id":"magicapp_stroke_public","name":"Synthetic scope","enabled":true,"access":"public","adapter":"magicapp_public","catalog_url":READER,"max_bytes":MAX_BYTES,"config":{"export_url":EXPORT_URL,"access_scope":"source_emitted_public_version"}})).unwrap()
    }
    fn record(c: &Collection, rid: i64) -> usize {
        c.records
            .iter()
            .position(|record| record.id == rid)
            .unwrap()
    }
    fn native_pico(c: &Collection, pid: i64) -> String {
        c.picos[&pid]
            .iter()
            .find(|path| c.root.pointer(path).unwrap().get("outcomes").is_some())
            .unwrap()
            .clone()
    }
    fn normal(c: &Collection) -> GuidelineDoc {
        c.recommendation_doc(&source(), record(c, 246603)).unwrap()
    }

    #[test]
    fn false_visibility_dominates_missing_or_true_flag_partial_enrichment() {
        let mut value = root();
        let c = collection(value.clone());
        let recpath = c.records[record(&c, 246603)].pointer.clone();
        let full = native_pico(&c, 167531);
        value.pointer_mut(&full).unwrap()["population"] = json!("PRIVATE_FALSE_ALIAS_VALUE");
        value.pointer_mut(&full).unwrap()["populationInPublic"] = json!(false);
        value
            .pointer_mut(&format!("{recpath}/picos/0"))
            .map(|pico| {
                *pico = json!({"picoId":167531,"population":"PRIVATE_REINTRODUCED_BY_ALIAS"})
            })
            .unwrap();
        let doc = normal(&collection(value.clone()));
        let serialized = serde_json::to_string(&doc).unwrap();
        assert!(
            !serialized.contains("PRIVATE_FALSE_ALIAS_VALUE")
                && !serialized.contains("PRIVATE_REINTRODUCED_BY_ALIAS")
        );
        value.pointer_mut(&format!("{recpath}/picos/0")).unwrap()["populationInPublic"] =
            json!(true);
        let mut incompatible = value.pointer(&full).unwrap().clone();
        incompatible["populationInPublic"] = json!(true);
        incompatible["population"] = json!("PRIVATE_REINTRODUCED_BY_ALIAS");
        incompatible["intervention"] = json!("Different native clinical variant");
        *value.pointer_mut(&format!("{recpath}/picos/0")).unwrap() = incompatible;
        let doc = normal(&collection(value));
        assert!(!serde_json::to_string(&doc)
            .unwrap()
            .contains("PRIVATE_REINTRODUCED_BY_ALIAS"));
        let observations =
            &doc.search_metadata["native_pico_evidence"][0]["all_source_visibility_observations"];
        assert!(observations
            .as_array()
            .unwrap()
            .iter()
            .any(|flag| flag["native_boolean"] == false
                && flag["source_path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with(".populationInPublic"))));
    }
    #[test]
    fn unknown_study_arrays_cannot_merge_arms_by_shared_reference_or_pico_identity() {
        let left = json!([{"referenceId":1,"picoId":9,"participants":20,"arm":"A"}]);
        let right = json!([{"referenceId":1,"picoId":9,"events":3,"arm":"B"}]);
        assert!(merge(
            &left,
            &right,
            "pico.outcomes.dichotomousOutcomes[outcomeId:1].interventionStudyData"
        )
        .is_err());
        let left = json!([{"outcomeId":1,"picoId":9,"relativeEffect":0.5}]);
        let right = json!([{"outcomeId":2,"picoId":9,"relativeEffect":0.8}]);
        assert_eq!(
            merge(&left, &right, "pico.outcomes.dichotomousOutcomes")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn visibility_flags_survive_allowlisting_before_pico_and_outcome_projection() {
        let mut value = root();
        let base = collection(value.clone());
        let path = native_pico(&base, 167531);
        let pico = value.pointer_mut(&path).unwrap();
        pico["population"] = json!("PRIVATE_PICO_COMPONENT");
        pico["populationInPublic"] = json!(false);
        pico["outcomes"]["dichotomousOutcomes"][0]["qualityRiskOfBiasComment"] =
            json!("PRIVATE_OUTCOME_COMPONENT");
        pico["outcomes"]["dichotomousOutcomes"][0]["qualityRiskOfBiasCommentInPublic"] =
            json!(false);
        let doc = normal(&collection(value));
        let text = serde_json::to_string(&doc).unwrap();
        assert!(
            !text.contains("PRIVATE_PICO_COMPONENT") && !text.contains("PRIVATE_OUTCOME_COMPONENT")
        );
        assert!(doc.search_metadata["visibility_omissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["source_path"]
                .as_str()
                .is_some_and(|path| path.ends_with(".population"))));
        let mut value = root();
        let rec = value
            .pointer_mut(&base.records[record(&base, 246603)].pointer)
            .unwrap();
        rec["text"] =
            json!("<strong><u>DRAFT RECOMMENDATION PRIVATE_PRIMARY_STATEMENT</u></strong>");
        rec["textInPublic"] = json!(false);
        let doc = normal(&collection(value));
        assert!(!serde_json::to_string(&doc)
            .unwrap()
            .contains("PRIVATE_PRIMARY_STATEMENT"));
        assert_eq!(doc.status, "unknown");
    }
    #[test]
    fn native_order_and_own_draft_review_states_do_not_inherit_published_parent() {
        let c = collection(root());
        assert_eq!(
            c.sections.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![273469, 273470, 273475]
        );
        let normal = normal(&c);
        let draft = c.recommendation_doc(&source(), record(&c, 246626)).unwrap();
        let review = c.recommendation_doc(&source(), record(&c, 246627)).unwrap();
        assert_eq!(normal.status, "unknown");
        assert_eq!(draft.status, "in_review");
        assert_eq!(review.status, "in_review");
        assert_eq!(
            draft.search_metadata["native_recommendation"]["status"],
            "UPDATED"
        );
        assert_eq!(
            review.search_metadata["native_recommendation"]["status"],
            "UNDER_REVIEW"
        );
        let statements = draft
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .collect::<Vec<_>>();
        assert_eq!(statements.len(), 1);
        assert!(
            statements[0].text.contains("Synthetic first draft clause.")
                && statements[0]
                    .text
                    .contains("Synthetic second draft clause.")
        );
        assert!(
            matches!(&statements[0].kind,BlockKind::Recommendation{strength:Some(strength),certainty:None,..}if strength=="WEAK")
        );
        assert_eq!(c.parent_doc(&source()).unwrap().status, "unknown");
    }
    #[test]
    fn hidden_records_detailed_components_judgements_outcomes_and_unattached_picos_never_leak() {
        let c = collection(root());
        let mut docs = vec![c.parent_doc(&source()).unwrap()];
        for n in 0..c.sections.len() {
            docs.push(c.section_doc(&source(), n).unwrap());
        }
        for n in selected(&c, &source()).unwrap() {
            docs.push(c.recommendation_doc(&source(), n).unwrap());
        }
        let serialized = serde_json::to_string(&docs).unwrap();
        for forbidden in [
            "PRIVATE_HIDDEN_RECORD_",
            "PRIVATE_FALSE_COMPONENT",
            "PRIVATE_HIDDEN_JUDGEMENT",
            "PRIVATE_HIDDEN_OUTCOME",
            "PRIVATE_UNVERIFIED_EMR",
            "PRIVATE_AUTHOR_CONTACT",
            "PRIVATE_EDITOR_NAME",
            "PRIVATE_CONTACT",
            "UNATTACHED_UNVERIFIED_EVIDENCE_PRIVATE",
            "UNATTACHED_UNVERIFIED_OUTCOME_PRIVATE",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "hidden source component leaked: {forbidden}"
            );
        }
        assert!(
            serialized.contains("BASE_SUMMARY_PUBLIC")
                && serialized.contains("BASE_EVIDENCE_PUBLIC")
                && serialized.contains("DETAIL_EXPLICITLY_PUBLIC")
        );
        assert!(normal(&c).search_metadata["visibility_omissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["reason"] == "native paired InPublic=false"));
        assert_eq!(
            c.coverage().unwrap()["hidden_recommendation_omissions"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert!(
            c.coverage().unwrap()["unattached_or_only_hidden_parent_picos"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["picoId"] == 199999 && entry["clinical_text_exported"] == false)
        );
    }
    #[test]
    fn source_info_box_and_ungraded_notset_are_not_given_inferred_grades() {
        let c = collection(root());
        let info = c.recommendation_doc(&source(), record(&c, 246604)).unwrap();
        assert_eq!(info.item_type.as_deref(), Some("guideline_information"));
        assert!(!info
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Recommendation { .. })));
        let notset = c.recommendation_doc(&source(), record(&c, 246607)).unwrap();
        assert_eq!(notset.title, "Practice points");
        assert_eq!(
            notset.search_metadata["native_recommendation"]["strength"],
            "NOTSET"
        );
        assert!(notset.blocks.iter().any(|b| matches!(
            b.kind,
            BlockKind::Recommendation {
                strength: None,
                certainty: None,
                ..
            }
        )));
        assert_eq!(notset.status, "unknown");
    }
    #[test]
    fn partial_pico_aliases_enrich_without_losing_native_outcome_units_or_scope() {
        let c = collection(root());
        let variants = c.variants(167531).unwrap();
        assert_eq!(variants.len(), 1);
        assert!(variants[0].paths.len() >= 3);
        let doc = normal(&c);
        let evidence = doc.search_metadata["native_pico_evidence"]
            .as_array()
            .unwrap();
        assert_eq!(evidence.len(), 2);
        let value = &evidence[0]["source_variants"][0]["projection"];
        assert_eq!(value["picoId"], 167531);
        let outcomes = value["outcomes"]["dichotomousOutcomes"].as_array().unwrap();
        let outcome = outcomes.iter().find(|v| v["outcomeId"] == 9001).unwrap();
        assert_eq!(outcome["importanceLevel"], "LEVEL_9");
        assert_eq!(outcome["qualityOfEvidenceLevel"], "LOW");
        assert_eq!(outcome["relativeEffect"], 0.76);
        assert_eq!(outcome["comparatorEffectType"], "PER_1000");
        assert_eq!(outcome["absoluteDifference"], -6);
        assert!(evidence.iter().all(|p| p["permalink"].is_null()));
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Table { .. })
                && b.text.contains("relativeEffect")
                && b.text.contains("PER_1000")));
        assert!(doc
            .blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BlockKind::Recommendation { certainty, .. } => Some(certainty),
                _ => None,
            })
            .all(Option::is_none));
    }
    #[test]
    fn incompatible_nonempty_pico_values_stay_distinct_with_conflict_paths() {
        let mut value = root();
        let c = collection(value.clone());
        let path = c.records[record(&c, 246603)].pointer.clone();
        let full = native_pico(&c, 167531);
        let mut variant = value.pointer(&full).unwrap().clone();
        variant["outcomes"]["dichotomousOutcomes"][0]["relativeEffect"] = json!(0.90);
        value
            .pointer_mut(&format!("{path}/picos/0"))
            .map(|v| *v = variant)
            .unwrap();
        let c = collection(value);
        let variants = c.variants(167531).unwrap();
        assert_eq!(variants.len(), 2);
        assert!(variants.iter().any(|variant| variant
            .conflicts
            .iter()
            .any(|path| path.contains("relativeEffect"))));
        let doc = normal(&c);
        assert!(doc
            .warnings
            .iter()
            .any(|warning| warning.contains("incompatible source variants")));
        assert_eq!(
            doc.search_metadata["native_pico_evidence"][0]["source_variants"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(merge(&json!({"effect":1}), &json!({"effect":"1"}), "test").is_err());
    }
    #[test]
    fn rich_components_keep_tables_figures_native_html_and_absolute_asset_descriptors() {
        let c = collection(root());
        let doc = normal(&c);
        assert!(doc.blocks.iter().any(|b|matches!(b.kind,BlockKind::Figure{src_url:Some(ref u),..}if u.ends_with("clinical-figure.png"))));
        assert!(doc.blocks.iter().any(
            |b| matches!(b.kind, BlockKind::Table { .. }) && b.text.contains("Synthetic cell")
        ));
        let components = doc.search_metadata["source_components"].as_array().unwrap();
        let advice = components
            .iter()
            .find(|component| {
                component["source_path"]
                    .as_str()
                    .unwrap()
                    .ends_with(".advice")
            })
            .unwrap();
        assert!(advice["source_html"].as_str().unwrap().contains("<img"));
        assert!(advice["source_blocks_in_native_order"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block["kind"]["type"] == "figure"));
        let file = doc
            .attachments
            .iter()
            .filter(|asset| asset["source_fields"]["fileId"] == 108055)
            .collect::<Vec<_>>();
        assert!(!file.is_empty());
        assert!(file
            .iter()
            .all(|asset| asset["bytes_downloaded"] == false && asset["mime_verified"] == false));
        let mut ids = HashSet::new();
        assert!(doc
            .blocks
            .iter()
            .all(|block| ids.insert(block.block_id.clone())));
    }
    #[test]
    fn clinical_evidence_visibility_state_and_asset_changes_all_change_immutable_snapshot() {
        let base = collection(root());
        let original = normal(&base).doc_key;
        let recpath = base.records[record(&base, 246603)].pointer.clone();
        let pico = native_pico(&base, 167531);
        for (path, value) in [
            (
                format!("{recpath}/rational"),
                json!("Changed visible rationale"),
            ),
            (format!("{recpath}/status"), json!("UNDER_REVIEW")),
            (
                format!("{recpath}/keyInfo/benefitsResearchEvidenceInPublic"),
                json!(true),
            ),
            (
                format!("{recpath}/keyInfo/benefitsResearchEvidence"),
                json!("CHANGED_PRIVATE_FALSE_COMPONENT"),
            ),
            (
                format!("{recpath}/files/0/fullPath"),
                json!("https://files.magicapp.org/guideline/synthetic/files/revised.pdf"),
            ),
            (
                format!("{pico}/outcomes/dichotomousOutcomes/0/relativeEffect"),
                json!(0.5),
            ),
        ] {
            let mut value_root = root();
            *value_root.pointer_mut(&path).unwrap() = value;
            assert_ne!(
                normal(&collection(value_root)).doc_key,
                original,
                "snapshot failed to bind {path}"
            );
        }
        let mut private_actor = root();
        private_actor["contactEmail"] = json!("DIFFERENT_PRIVATE_CONTACT");
        assert_eq!(normal(&collection(private_actor)).doc_key, original);
    }
    #[test]
    fn selected_native_ids_reject_unknown_and_hidden_and_do_not_limit_inner_content() {
        let c = collection(root());
        let mut src = source();
        src.config["recommendation_ids"] = json!([246603, 246626, 246627]);
        let selection = selected(&c, &src).unwrap();
        assert_eq!(
            selection
                .iter()
                .map(|n| c.records[*n].id)
                .collect::<Vec<_>>(),
            vec![246603, 246626, 246627]
        );
        src.config["recommendation_ids"] = json!([999999]);
        assert!(selected(&c, &src).is_err());
        src.config["recommendation_ids"] = json!([246606]);
        assert!(selected(&c, &src)
            .unwrap_err()
            .to_string()
            .contains("hideInPublish"));
        assert!(c.variants(199999).is_ok());
        assert!(!c.eligible_picos().unwrap().contains(&199999));
    }
    #[tokio::test]
    async fn offline_plan_extract_scoped_three_and_once_cell_preserve_captured_source() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path(), true, 500, MAX_BYTES as u64, "").unwrap();
        fetcher.seed_cache_for_tests(EXPORT_URL, FIXTURE.as_bytes(), "application/json");
        let mut src = source();
        src.config["recommendation_ids"] = json!([246603, 246626, 246627]);
        let adapter = MagicAppPublic::default();
        let rows = adapter.discover(&src, &fetcher, Some(3)).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .map(|row| row.extra["native_id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![246603, 246626, 246627]
        );
        let docs = for_rows(&adapter, &src, &fetcher, &rows).await;
        assert_eq!(
            docs.iter()
                .map(|doc| doc.status.as_str())
                .collect::<Vec<_>>(),
            vec!["unknown", "in_review", "in_review"]
        );
        assert_eq!(docs[0].fetches.len(), 1);
        assert!(rows
            .iter()
            .all(|row| row.url.contains("#/guideline/11482/rec/")
                && row.rendition.as_deref() == Some("json")));
        let mut changed = root();
        changed["sections"][1]["recommendations"][0]["rational"] =
            json!("Changed public rationale");
        let bytes = serde_json::to_vec(&changed).unwrap();
        fetcher.seed_cache_for_tests(EXPORT_URL, &bytes, "application/json");
        assert_eq!(
            adapter
                .extract(&src, &fetcher, &rows[0])
                .await
                .unwrap()
                .doc_key,
            rows[0].doc_key
        ); // One immutable capture per adapter instance.
        assert!(MagicAppPublic::default()
            .extract(&src, &fetcher, &rows[0])
            .await
            .unwrap_err()
            .to_string()
            .contains("snapshot changed"));
    }
    async fn for_rows(
        adapter: &MagicAppPublic,
        src: &SourceEntry,
        fetcher: &Fetcher,
        rows: &[PlanRow],
    ) -> Vec<GuidelineDoc> {
        let mut out = vec![];
        for row in rows {
            out.push(adapter.extract(src, fetcher, row).await.unwrap());
        }
        out
    }
    #[tokio::test]
    async fn default_complete_fixed_export_includes_index_roles_and_excludes_four_hidden_records() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path(), true, 500, MAX_BYTES as u64, "").unwrap();
        fetcher.seed_cache_for_tests(EXPORT_URL, FIXTURE.as_bytes(), "application/json");
        let adapter = MagicAppPublic::default();
        assert!(adapter
            .discover(&source(), &fetcher, Some(0))
            .await
            .unwrap()
            .is_empty());
        let rows = adapter.discover(&source(), &fetcher, None).await.unwrap();
        let recs = rows
            .iter()
            .filter(|row| row.extra["kind"] == "recommendation")
            .collect::<Vec<_>>();
        assert_eq!(recs.len(), 5);
        assert_eq!(
            rows.iter()
                .filter(|row| row.extra["kind"] == "section_index")
                .count(),
            3
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.extra["kind"] == "parent_index")
                .count(),
            1
        );
        for hidden in [246606, 246619, 246633, 246644] {
            assert!(!recs.iter().any(|row| row.extra["native_id"] == hidden));
        }
        assert_eq!(export_request().cache_max_age_secs, Some(0));
    }
    #[test]
    fn tag_free_rich_components_decode_character_references_once_without_creating_markup() {
        let raw = "Synthetic A&nbsp;B &amp; C &#160; &#xA0; &lt;strong&gt;literal&lt;/strong&gt; &amp;nbsp; &syntheticunknown;";
        let html = rich_field_html(raw);
        assert!(html.contains("A\u{a0}B"));
        assert!(html.contains("C \u{a0} \u{a0}"));
        assert!(html.contains("&lt;strong&gt;literal&lt;/strong&gt;"));
        assert!(html.contains("&amp;nbsp;"));
        assert_eq!(
            Html::parse_fragment(&html).select(&sel("strong")).count(),
            0
        );
        let (blocks, _) = blocks_from_html(&html, None, None);
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].text,
            "Synthetic A B & C <strong>literal</strong> &nbsp; &syntheticunknown;"
        );
        assert!(matches!(blocks[0].kind, BlockKind::Paragraph));
    }
    #[test]
    fn rich_markup_plain_fields_and_literal_comparisons_keep_existing_decode_boundaries() {
        let markup = "<p>Synthetic&nbsp;markup &amp;nbsp; &lt; 7.</p>";
        assert_eq!(rich_field_html(markup), markup);
        let (blocks, _) = blocks_from_html(&rich_field_html(markup), None, None);
        assert_eq!(blocks[0].text, "Synthetic markup &nbsp; < 7.");
        let raw = "Synthetic x < y and y > z &amp;nbsp;";
        let (blocks, _) = blocks_from_html(&rich_field_html(raw), None, None);
        assert_eq!(blocks[0].text, "Synthetic x < y and y > z &nbsp;");
        assert_eq!(
            plain("Synthetic &nbsp; &amp; and < 7"),
            "Synthetic &nbsp; &amp; and < 7"
        );
        let mut view = View::default();
        view.structured(
            "Synthetic structured",
            &json!({"value":"Literal &nbsp; &amp; < 7"}),
            "$.synthetic",
            &[],
        )
        .unwrap();
        assert!(view
            .blocks
            .iter()
            .any(|block| block.text.contains("Literal &nbsp; &amp; < 7")));
        assert!(!view
            .blocks
            .iter()
            .any(|block| block.text.contains('\u{a0}')));
    }
    #[tokio::test]
    async fn offline_tag_free_rationale_preserves_raw_provenance_visibility_and_decoded_rows() {
        let seed = collection(root());
        let path = seed.records[record(&seed, 246627)].pointer.clone();
        let raw = "Synthetic public A&nbsp;B &amp; plain &amp;nbsp; and &lt; 7.";
        let expected = "Synthetic public A B & plain &nbsp; and < 7.";
        let mut value = root();
        value.pointer_mut(&path).unwrap()["rational"] = json!(raw);
        value.pointer_mut(&path).unwrap()["rationalInPublic"] = json!(true);
        value.pointer_mut(&path).unwrap()["heading"] = json!("Synthetic &nbsp; Heading");
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path(), true, 500, MAX_BYTES as u64, "").unwrap();
        fetcher.seed_cache_for_tests(
            EXPORT_URL,
            &serde_json::to_vec(&value).unwrap(),
            "application/json",
        );
        let mut src = source();
        src.config["recommendation_ids"] = json!([246627]);
        let adapter = MagicAppPublic::default();
        let rows = adapter.discover(&src, &fetcher, Some(1)).await.unwrap();
        assert_eq!(rows.len(), 1);
        let doc = adapter.extract(&src, &fetcher, &rows[0]).await.unwrap();
        let component = doc.search_metadata["source_components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| {
                component["source_path"]
                    .as_str()
                    .unwrap()
                    .ends_with(".rational")
            })
            .unwrap();
        assert_eq!(component["source_value"], raw);
        assert_eq!(component["source_value_sha256"], hash(&json!(raw)));
        assert!(component["source_html"]
            .as_str()
            .unwrap()
            .contains("A\u{a0}B"));
        assert!(doc.blocks.iter().any(|block| block.text == expected));
        assert_eq!(doc.title, "Synthetic &nbsp; Heading");
        assert_eq!(doc.status, "in_review");
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|block| matches!(block.kind, BlockKind::Recommendation { .. }))
                .count(),
            1
        );
        let persisted = crate::guidelines::doc::to_rows(
            &doc,
            &Default::default(),
            false,
            "synthetic/document.json",
            &Default::default(),
        );
        assert!(persisted.iter().any(|row| row
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains(expected))));
        value.pointer_mut(&path).unwrap()["rationalInPublic"] = json!(false);
        let hidden = collection(value);
        let hidden = hidden
            .recommendation_doc(&src, record(&hidden, 246627))
            .unwrap();
        let exported = serde_json::to_string(&hidden).unwrap();
        assert!(!exported.contains("Synthetic public A"));
        assert!(!hidden.search_metadata["source_components"]
            .as_array()
            .unwrap()
            .iter()
            .any(|component| component["source_path"]
                .as_str()
                .unwrap()
                .ends_with(".rational")));
    }
}
