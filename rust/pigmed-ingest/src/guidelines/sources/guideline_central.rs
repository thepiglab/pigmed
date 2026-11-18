//! Guideline Central catalog and recommendation markup. Terminal extraction remains gated until a
//! stable authenticated response and automated-use rights are independently validated.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{Block, BlockKind, DateObservation, GuidelineDoc, Relation, Rights};
use crate::guidelines::fetch::{Fetcher, Req};
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::Html;
use serde_json::{json, Value};
use std::collections::HashSet;
use url::Url;

#[derive(Default)]
pub struct GuidelineCentral {
    session: tokio::sync::OnceCell<Fetcher>,
}
const ORIGIN: &str = "https://www.guidelinecentral.com";
fn state(html: &str) -> Result<Value> {
    let d = Html::parse_document(html);
    let raw = d
        .select(&sel("[data-page]"))
        .next()
        .and_then(|e| e.value().attr("data-page"))
        .context("Guideline Central data-page missing")?;
    Ok(serde_json::from_str(raw)?)
}
// Component names and login state are server-controlled; report only fixed categories.
fn auth_diagnostic(status: u16, parsed: Option<&Value>) -> String {
    let category = match parsed {
        None => "state_unavailable",
        Some(value) => match value.get("component") {
            None => "component_missing",
            Some(Value::String(component)) if component == "auth/Login" => "login",
            Some(Value::String(component)) if component == "public/Guidelines/ShowGuideline" => {
                "guideline_summary"
            }
            Some(_) => "other",
        },
    };
    format!("HTTP {status}; component_category={category}")
}
fn decode_cookie(s: &str) -> Result<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                bail!("invalid XSRF cookie encoding")
            };
            let hex = std::str::from_utf8(&b[i + 1..i + 3])
                .map_err(|_| anyhow::anyhow!("invalid XSRF cookie encoding"))?;
            out.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| anyhow::anyhow!("invalid XSRF cookie encoding"))?,
            );
            i += 3
        } else {
            out.push(b[i]);
            i += 1
        }
    }
    String::from_utf8(out).map_err(|_| anyhow::anyhow!("invalid XSRF cookie encoding"))
}
fn saved(src: &SourceEntry) -> Result<bool> {
    match src.config.get("authentication").and_then(Value::as_str) {
        None | Some("public") => Ok(false),
        Some("saved_account") => Ok(true),
        Some(_) => bail!("unsupported Guideline Central authentication setting"),
    }
}
impl GuidelineCentral {
    async fn session<'a>(&'a self, f: &Fetcher) -> Result<&'a Fetcher> {
        self.session
            .get_or_try_init(|| async {
                let username = std::env::var("GUIDELINE_CENTRAL_EMAIL")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        std::env::var("GUIDELINE_CENTRAL_USERNAME")
                            .ok()
                            .filter(|s| !s.is_empty())
                    })
                    .context("Guideline Central account username missing")?;
                let password = std::env::var("GUIDELINE_CENTRAL_PASSWORD")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .context("Guideline Central account password missing")?;
                let session = f.with_origin_session(ORIGIN, &username)?;
                let mut req = Req::get(&format!("{ORIGIN}/login/"));
                req.uncached = true;
                let page = session
                    .request(&req)
                    .await
                    .map_err(|_| anyhow::anyhow!("Guideline Central login page request failed"))?;
                let login_state = state(&page.text()).ok();
                if page.record.status != 200
                    || login_state
                        .as_ref()
                        .and_then(|value| value["component"].as_str())
                        != Some("auth/Login")
                {
                    bail!(
                        "Guideline Central login page unavailable or unrecognized ({})",
                        auth_diagnostic(page.record.status, login_state.as_ref())
                    )
                }
                let token = decode_cookie(
                    &session
                        .session_cookie_value_for(
                            "https://www.guidelinecentral.com/login/",
                            "XSRF-TOKEN",
                        )
                        .context("Guideline Central XSRF cookie missing")?,
                )?;
                let body = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("username", &username)
                    .append_pair("password", &password)
                    .append_pair("remember", "0")
                    .finish();
                let req = Req {
                    method: "POST".into(),
                    url: format!("{ORIGIN}/login/do"),
                    headers: vec![
                        (
                            "content-type".into(),
                            "application/x-www-form-urlencoded".into(),
                        ),
                        ("X-XSRF-TOKEN".into(), token),
                    ],
                    body: Some(body.into_bytes()),
                    uncached: true,
                    ..Default::default()
                };
                let response = session
                    .request(&req)
                    .await
                    .map_err(|_| anyhow::anyhow!("Guideline Central sign-in request failed"))?;
                let response_state = state(&response.text()).ok();
                if response.record.status != 200
                    || response_state
                        .as_ref()
                        .and_then(|value| value.pointer("/props/isAuthorized"))
                        .and_then(Value::as_bool)
                        != Some(true)
                {
                    bail!(
                        "Guideline Central sign-in did not establish an authorized session ({})",
                        auth_diagnostic(response.record.status, response_state.as_ref())
                    )
                }
                Ok(session)
            })
            .await
    }
}
fn catalog(html: &str, base: &Url) -> Result<Vec<(String, String, String)>> {
    let d = Html::parse_document(html);
    let state = d
        .select(&sel("[data-page]"))
        .next()
        .and_then(|e| e.value().attr("data-page"))
        .context("Guideline Central data-page missing")?;
    let v: Value = serde_json::from_str(state)?;
    let docs = v
        .pointer("/props/documents")
        .context("catalog documents missing")?;
    let mut seen = HashSet::new();
    let mut rows = vec![];
    for group in ["featured", "recent"] {
        let items = docs[group]
            .as_array()
            .context("catalog group is not an array")?;
        for item in items {
            let id = item["id"]
                .as_u64()
                .context("catalog item ID missing")?
                .to_string();
            let title = item["title"]
                .as_str()
                .context("catalog item title missing")?;
            let href = item["default_link"]
                .as_str()
                .context("catalog default link missing")?;
            let url = common::link(base, href).context("catalog link invalid")?;
            let u = Url::parse(&url)?;
            if u.origin() != base.origin() || !u.path().starts_with(&format!("/guideline/{id}/")) {
                bail!("catalog item route does not match source ID");
            }
            if seen.insert(url.clone()) {
                rows.push((id, title.into(), url));
            }
        }
    }
    if rows.is_empty() {
        bail!("catalog groups empty");
    }
    Ok(rows)
}
fn layout_html(layout: &Value) -> Result<String> {
    fn escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn walk(nodes: &[Value], out: &mut String, count: &mut usize) -> Result<()> {
        for node in nodes {
            let r = &node["rendered"];
            let typ = r["element_type"]
                .as_str()
                .context("layout element type missing")?;
            if !matches!(typ, "heading" | "Heading" | "Recommendation" | "Text") {
                bail!("unrecognized Guideline Central layout element type")
            }
            let content = r["content"].as_object().context("layout content missing")?;
            let order = r["keyOrder"]
                .as_array()
                .context("layout content order missing")?;
            if order.len() != content.len() {
                bail!("layout content order is incomplete")
            }
            let mut seen = HashSet::new();
            let mut fragments = String::new();
            for k in order {
                let key = if let Some(n) = k.as_u64() {
                    n.to_string()
                } else if let Some(s) = k.as_str() {
                    s.to_owned()
                } else {
                    bail!("invalid layout content key")
                };
                if !seen.insert(key.clone()) {
                    bail!("duplicate layout content key")
                };
                fragments.push_str(
                    content
                        .get(&key)
                        .and_then(Value::as_str)
                        .context("layout fragment missing")?,
                )
            }
            if typ == "Recommendation" {
                *count += 1;
                out.push_str("<div data-element-type='rec-statement'><div class='rec-content'>");
                out.push_str(&fragments);
                if let Some(grades) = r.pointer("/meta/Grading").and_then(Value::as_object) {
                    out.push_str("<span class='rec-grading-wrapper'>");
                    for label in grades.keys() {
                        out.push_str("<span class='rec-grading'>");
                        out.push_str(&escape(label));
                        out.push_str("</span>")
                    }
                    out.push_str("</span>")
                }
                out.push_str("</div></div>");
            } else {
                out.push_str(&fragments)
            }
            if let Some(children) = node.get("children") {
                if !children.is_null() {
                    walk(
                        children.as_array().context("layout children invalid")?,
                        out,
                        count,
                    )?
                }
            }
        }
        Ok(())
    }
    let nodes = layout.as_array().context("summary layout missing")?;
    let mut out = String::from("<div id='guide-content'>");
    let mut count = 0;
    walk(nodes, &mut out, &mut count)?;
    out.push_str("</div>");
    if count == 0 {
        bail!("summary layout has no recommendations")
    };
    Ok(out)
}
// Build recommendation blocks from each original layout node. Markers preserve their position
// among headings and other elements without asking the generic walker to flatten a multi-block node.
fn layout_blocks(layout: &Value, base: &Url) -> Result<(Vec<Block>, Vec<String>)> {
    fn walk(
        nodes: &[Value],
        html: &mut String,
        recs: &mut Vec<(Block, Vec<Block>)>,
        base: &Url,
    ) -> Result<()> {
        for node in nodes {
            let r = &node["rendered"];
            let typ = r["element_type"].as_str().context("layout type missing")?;
            let content = r["content"].as_object().context("layout content missing")?;
            let order = r["keyOrder"].as_array().context("layout order missing")?;
            let mut fragment = String::new();
            for key in order {
                let key = if let Some(n) = key.as_u64() {
                    n.to_string()
                } else {
                    key.as_str().context("invalid layout key")?.to_owned()
                };
                fragment.push_str(
                    content
                        .get(&key)
                        .and_then(Value::as_str)
                        .context("layout fragment missing")?,
                )
            }
            if typ == "Recommendation" {
                let parsed = Html::parse_fragment(&fragment);
                let text = text_of(&parsed.root_element());
                if text.is_empty() {
                    bail!("empty recommendation")
                }
                let labels = r
                    .pointer("/meta/Grading")
                    .and_then(Value::as_object)
                    .map(|o| o.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                let id = node["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("ordinal-{}", recs.len() + 1));
                let rec = Block {
                    block_id: format!("gc-{id}"),
                    kind: BlockKind::Recommendation {
                        label: None,
                        strength: None,
                        certainty: None,
                        population: None,
                        grading_system: None,
                        qualifier: (!labels.is_empty()).then(|| labels.join("; ")),
                    },
                    text,
                    heading_path: vec![],
                    page: None,
                };
                let (children, _) = blocks_from_html(&fragment, Some(base), None);
                let assets = children
                    .into_iter()
                    .filter(|b| {
                        matches!(b.kind, BlockKind::Table { .. } | BlockKind::Figure { .. })
                    })
                    .collect();
                html.push_str(&format!("<p>GC_LAYOUT_MARKER_{:08}</p>", recs.len()));
                recs.push((rec, assets));
            } else {
                html.push_str(&fragment)
            }
            if let Some(children) = node.get("children") {
                if !children.is_null() {
                    walk(
                        children.as_array().context("layout children invalid")?,
                        html,
                        recs,
                        base,
                    )?
                }
            }
        }
        Ok(())
    }
    let mut html = String::new();
    let mut recs = vec![];
    walk(
        layout.as_array().context("layout missing")?,
        &mut html,
        &mut recs,
        base,
    )?;
    let (blocks, warnings) = blocks_from_html(&html, Some(base), None);
    let mut out = vec![];
    let mut matched = HashSet::new();
    for block in blocks {
        if let Some(n) = block
            .text
            .strip_prefix("GC_LAYOUT_MARKER_")
            .and_then(|n| n.parse::<usize>().ok())
        {
            if !matched.insert(n) {
                bail!("duplicate recommendation marker")
            };
            let (mut rec, assets) = recs
                .get(n)
                .cloned()
                .context("invalid recommendation marker")?;
            rec.heading_path = block.heading_path.clone();
            out.push(rec);
            for (i, mut asset) in assets.into_iter().enumerate() {
                asset.block_id = format!("gc-{n}-asset-{i}");
                asset.heading_path = block.heading_path.clone();
                out.push(asset)
            }
        } else {
            out.push(block)
        }
    }
    if matched.len() != recs.len() {
        bail!("Guideline Central recommendation count mismatch")
    };
    Ok((out, warnings))
}
// Preserve the complete source grading object separately from textual labels. The block id
// joins native grading back to canonical recommendation blocks without guessing its semantics.
fn native_grading(layout: &Value) -> Result<Vec<Value>> {
    fn walk(nodes: &[Value], out: &mut Vec<Value>) -> Result<()> {
        for node in nodes {
            if node["rendered"]["element_type"] == "Recommendation" {
                let id = node["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("ordinal-{}", out.len() + 1));
                out.push(json!({"block_id":format!("gc-{id}"),"source_node_id":node.get("id"),"grading":node.pointer("/rendered/meta/Grading")}));
            }
            if let Some(children) = node.get("children") {
                if !children.is_null() {
                    walk(children.as_array().context("layout children invalid")?, out)?
                }
            }
        }
        Ok(())
    }
    let mut out = vec![];
    walk(layout.as_array().context("layout missing")?, &mut out)?;
    Ok(out)
}
fn summary_snapshot(raw: &str) -> Result<(Value, String, String, Option<String>)> {
    let v = state(raw)?;
    let props = &v["props"];
    if v["component"] != "public/Guidelines/ShowGuideline"
        || props["isAuthorized"].as_bool() != Some(true)
        || props["gated"].as_bool() != Some(false)
        || props["summaryGated"].as_bool() != Some(false)
        || props["preview"].as_bool() != Some(false)
    {
        bail!("Guideline Central summary authorization or completeness markers missing")
    }
    let mut body = if props["layout"].is_array() {
        layout_html(&props["layout"])?
    } else {
        let d = Html::parse_document(raw);
        d.select(&sel("#guide-content"))
            .next()
            .context("Guideline Central summary layout and HTML body unavailable")?
            .html()
    };
    recommendations(&body)?;
    if let Some(tables) = props["document"]["grading_table_data"].as_array() {
        for table in tables {
            let html = table
                .pointer("/table/content")
                .and_then(Value::as_str)
                .context("grading table content missing")?;
            let parsed = Html::parse_fragment(html);
            if parsed.select(&sel("table")).next().is_none() {
                bail!("unrecognized grading table markup")
            };
            body.push_str(html);
        }
    }
    let live = match &props["summary"]["live_version"] {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    // Include full clinical layout so grade-value-only changes cannot reuse an immutable ID.
    // Account/session props are excluded; source metadata and rendition links are explicit.
    let document = &props["document"];
    let stable = json!({"version":live,"body":body,"layout":props.get("layout"),"grading_tables":document.get("grading_table_data"),
  "title":document.get("title"),"source_document_url":document.get("document_url"),
  "published_at_readable":document.get("published_at_readable"),"updated_at_readable":document.get("updated_at_readable")});
    let hash = crate::common::sha256_hex(&serde_json::to_vec(&stable)?);
    Ok((v, body, hash, live))
}
fn recommendations(html: &str) -> Result<Vec<Block>> {
    let d = Html::parse_document(html);
    let root = d
        .select(&sel("#guide-content"))
        .next()
        .context("summary root missing")?;
    let text = text_of(&root).to_lowercase();
    if text.contains("create a free account to view this content")
        || text.contains("log in to view this content")
    {
        bail!("Guideline Central account gate detected; content is incomplete");
    }
    let mut out = vec![];
    for (i, rec) in root
        .select(&sel("[data-element-type='rec-statement']"))
        .enumerate()
    {
        let content = rec
            .select(&sel(".rec-content"))
            .next()
            .map(|x| text_of(&x))
            .filter(|x| !x.is_empty())
            .context("empty recommendation")?;
        let grades = rec
            .select(&sel(".rec-grading-wrapper .rec-grading"))
            .map(|x| text_of(&x))
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>();
        out.push(Block {
            block_id: format!("gc-rec-{}", i + 1),
            kind: BlockKind::Recommendation {
                label: None,
                strength: None,
                certainty: None,
                population: None,
                grading_system: None,
                qualifier: (!grades.is_empty()).then(|| grades.join("; ")),
            },
            text: content,
            heading_path: vec![],
            page: None,
        });
    }
    if out.is_empty() {
        bail!("no recognized recommendation statements");
    }
    Ok(out)
}
#[async_trait::async_trait]
impl Adapter for GuidelineCentral {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let account = saved(src)?;
        let catalog_url = src.catalog_url.as_deref().context("catalog URL missing")?;
        let page = if account {
            self.session(f).await?.get_ok(catalog_url).await?
        } else {
            f.get_ok(catalog_url).await?
        };
        let base = Url::parse(&page.record.final_url)?;
        let mut rows = vec![];
        for (id, title, url) in catalog(&page.text(), &base)? {
            if account {
                let session = self.session(f).await?;
                let p = session
                    .get(&url)
                    .await
                    .map_err(|_| anyhow::anyhow!("Guideline Central summary request failed"))?;
                if p.record.status != 200 {
                    bail!(
                        "Guideline Central summary unavailable (HTTP {})",
                        p.record.status
                    )
                }
                let (_, _, hash, live) = summary_snapshot(&p.text())?;
                rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{id}-summary-{}",&hash[..16]),url,title:Some(title),rendition:Some("html".into()),version:live,extra:json!({"work_key":id,"catalog_incomplete":true,"clinical_snapshot":hash})});
            } else {
                rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{id}-summary-unresolved"),url,title:Some(title),rendition:Some("html".into()),version:None,extra:json!({"work_key":id,"discovery_error":"Public catalog only; authorized summary extraction is disabled."})});
            }
            if limit.is_some_and(|n| rows.len() >= n) {
                break;
            }
        }
        Ok(rows)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if !saved(src)? {
            bail!("Guideline Central public mode is catalog-only")
        }
        let url = Url::parse(&row.url)?;
        if url.origin() != Url::parse(ORIGIN)?.origin() || !url.path().starts_with("/guideline/") {
            bail!("Guideline Central terminal origin or route invalid")
        }
        let session = self.session(f).await?;
        let p = session
            .get(&row.url)
            .await
            .map_err(|_| anyhow::anyhow!("Guideline Central summary request failed"))?;
        if p.record.status != 200 {
            bail!(
                "Guideline Central summary unavailable (HTTP {})",
                p.record.status
            )
        }
        let raw = p.text();
        let (v, _body, hash, live) = summary_snapshot(&raw)?;
        let props = &v["props"];
        if row.extra["clinical_snapshot"].as_str() != Some(hash.as_str()) || row.version != live {
            bail!("Guideline Central summary changed; re-plan")
        }
        let (blocks, mut warnings) = if props["layout"].is_array() {
            let (mut b, w) = layout_blocks(&props["layout"], &url)?;
            if let Some(tables) = props["document"]["grading_table_data"].as_array() {
                for table in tables {
                    let html = table
                        .pointer("/table/content")
                        .and_then(Value::as_str)
                        .context("grading table content missing")?;
                    let (tb, _) = blocks_from_html(html, Some(&url), None);
                    b.extend(tb);
                }
            }
            (b, w)
        } else {
            bail!("Guideline Central serialized clinical layout is required")
        };
        let document = &props["document"];
        let title = document["title"]
            .as_str()
            .context("summary title missing")?
            .to_owned();
        let mut dates = vec![];
        for (key, label) in [
            ("published_at_readable", "Document publication date"),
            ("updated_at_readable", "Document updated date"),
        ] {
            if let Some(raw) = document[key].as_str().filter(|s| !s.is_empty()) {
                dates.push(DateObservation {
                    label: label.into(),
                    raw: raw.into(),
                    normalized: None,
                    source_url: Some(row.url.clone()),
                })
            }
        }
        let mut relations = vec![];
        if let Some(target) = document["document_url"]
            .as_str()
            .and_then(|s| Url::parse(s).ok())
            .filter(|u| u.scheme() == "https")
        {
            relations.push(Relation {
                rel: "summary_of".into(),
                target: target.to_string(),
                scope: None,
                evidence_url: Some(row.url.clone()),
            })
        }
        warnings.push("Guideline Central derivative summary; full source guideline is a separate work. Catalog enumeration and summary completeness are not independently verified.".into());
        Ok(GuidelineDoc {
            source_id: src.id.clone(),
            doc_key: row.doc_key.clone(),
            work_key: row.extra["work_key"].as_str().map(str::to_owned),
            item_type: Some("guideline_summary".into()),
            title,
            url: row.url.clone(),
            status: "unknown".into(),
            version: live,
            date_observations: dates,
            relations,
            search_metadata: json!({"native_grading":native_grading(&props["layout"])? ,"native_grading_evidence":row.url,"derivative_type":"guideline_summary","metadata_mapping":"Country, specialty and population remain unknown until observed source field shapes are validated."}),
            rights: Rights {
                status: "unknown".into(),
                note: Some("Account access does not establish reuse rights.".into()),
                ..Default::default()
            },
            blocks,
            fetches: vec![p.record],
            warnings,
            ..Default::default()
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clinical_snapshot_excludes_account_state_and_tracks_version_and_body() {
        fn fixture(version: u32, user: &str, rec: &str) -> String {
            let v = json!({"component":"public/Guidelines/ShowGuideline","props":{"isAuthorized":true,"gated":false,"summaryGated":false,"preview":false,"summary":{"live_version":version},"user":user}});
            format!("<div data-page='{}'></div><div id='guide-content'><div data-element-type='rec-statement'><div class='rec-content'>{rec}</div></div></div>",v)
        }
        let first = summary_snapshot(&fixture(1, "account-a", "Synthetic recommendation"))
            .unwrap()
            .2;
        assert_eq!(
            first,
            summary_snapshot(&fixture(1, "account-b", "Synthetic recommendation"))
                .unwrap()
                .2
        );
        assert_ne!(
            first,
            summary_snapshot(&fixture(2, "account-a", "Synthetic recommendation"))
                .unwrap()
                .2
        );
        assert_ne!(
            first,
            summary_snapshot(&fixture(1, "account-a", "Changed synthetic recommendation"))
                .unwrap()
                .2
        );
    }
    #[test]
    fn serialized_layout_preserves_order_and_native_grade() {
        let v = json!([{"id":"1","rendered":{"element_type":"heading","content":{"82":"<h2>Synthetic section</h2>"},"keyOrder":[82]},"children":[{"id":"2","rendered":{"element_type":"Recommendation","content":{"83":"<div>Synthetic <em>statement</em>.</div>"},"keyOrder":[83],"meta":{"Grading":{"B-NR":1,"1":1}}}}]}]);
        let body = layout_html(&v).unwrap();
        let r = recommendations(&body).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].text.contains("Synthetic"));
        let grades = native_grading(&v).unwrap();
        assert_eq!(grades[0]["block_id"], "gc-2");
        assert_eq!(grades[0]["grading"]["B-NR"], 1);
        let wrap = |layout: &Value| {
            let state = json!({"component":"public/Guidelines/ShowGuideline","props":{"isAuthorized":true,"gated":false,"summaryGated":false,"preview":false,"layout":layout,"summary":{"live_version":1}}});
            format!("<div data-page='{}'></div>", state)
        };
        let hash = summary_snapshot(&wrap(&v)).unwrap().2;
        let mut changed = v.clone();
        changed[0]["children"][0]["rendered"]["meta"]["Grading"]["B-NR"] =
            json!({"native_value":"changed"});
        assert_eq!(
            native_grading(&changed).unwrap()[0]["grading"]["B-NR"]["native_value"],
            "changed"
        );
        assert_ne!(hash, summary_snapshot(&wrap(&changed)).unwrap().2);
        let (blocks, _) = layout_blocks(&v, &Url::parse(ORIGIN).unwrap()).unwrap();
        assert!(blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Heading { .. })));
        assert!(blocks
            .iter()
            .any(|b| matches!(b.kind, BlockKind::Recommendation { .. })));
    }
    #[test]
    fn layout_recommendation_preserves_structures_and_separate_grade() {
        let v = json!([{"id":"10","rendered":{"element_type":"heading","content":{"82":"<h2>Synthetic section</h2>"},"keyOrder":[82]},"children":[{"id":"20","rendered":{"element_type":"Recommendation","content":{"83":"<div><p>First synthetic paragraph.</p><p>Second synthetic paragraph.</p><ul><li>Synthetic list item</li></ul><table><tr><td>Synthetic cell</td></tr></table><figure><img src='/synthetic.png' alt='Synthetic figure'></figure></div>"},"keyOrder":[83],"meta":{"Grading":{"Grade X":1}}}}]}]);
        let (b, _) = layout_blocks(&v, &Url::parse(ORIGIN).unwrap()).unwrap();
        let rec = b
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .unwrap();
        assert_eq!(rec.block_id, "gc-20");
        assert_eq!(rec.heading_path, vec!["Synthetic section"]);
        assert!(rec.text.contains("Second synthetic paragraph"));
        assert!(!rec.text.contains("Grade X"));
        assert!(
            matches!(&rec.kind,BlockKind::Recommendation{qualifier:Some(g),..} if g=="Grade X")
        );
        assert!(b.iter().any(|b| matches!(b.kind, BlockKind::Table { .. })));
        assert!(b.iter().any(|b| matches!(b.kind, BlockKind::Figure { .. })));
    }
    #[test]
    fn login_failure_diagnostic_never_echoes_server_component_or_state_values() {
        let untrusted = json!({"component":"untrusted-private-component-token","props":{"password":"untrusted-private-password","csrf":"untrusted-private-csrf"}});
        let diagnostic = auth_diagnostic(403, Some(&untrusted));
        assert_eq!(diagnostic, "HTTP 403; component_category=other");
        assert!(!diagnostic.contains("untrusted-private"));
        assert_eq!(
            auth_diagnostic(
                200,
                Some(
                    &json!({"component":"auth/Login","props":{"token":"untrusted-private-token"}})
                )
            ),
            "HTTP 200; component_category=login"
        );
        assert_eq!(
            auth_diagnostic(502, None),
            "HTTP 502; component_category=state_unavailable"
        );
    }
    #[test]
    fn cookie_decoding_preserves_plus_and_rejects_bad_escape() {
        assert_eq!(decode_cookie("a+b%2Bc%3D").unwrap(), "a+b+c=");
        assert!(decode_cookie("%Q1").is_err());
    }
    #[test]
    fn bounded_catalog_and_gate() {
        let state = json!({"props":{"documents":{"featured":[{"id":42,"title":"Test","default_link":"/guideline/42/"}],"recent":[{"id":42,"title":"Test","default_link":"/guideline/42/"}]}}});
        let escaped = state.to_string().replace('"', "&quot;");
        let html = format!("<div data-page=\"{escaped}\"></div>");
        let rows = catalog(
            &html,
            &Url::parse("https://www.guidelinecentral.com/guidelines/").unwrap(),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        let rec="<div id='guide-content'><div data-element-type='rec-statement'><div class='rec-content'>Consider treatment.</div><div class='rec-grading-wrapper'><span class='rec-grading'>COR I</span></div></div></div>";
        let b = recommendations(rec).unwrap();
        assert_eq!(b[0].text, "Consider treatment.");
        assert!(matches!(&b[0].kind,BlockKind::Recommendation{qualifier:Some(s),..} if s=="COR I"));
        assert!(recommendations(&rec.replace(
            "</div></div></div>",
            "</div></div>Create a Free Account to View This Content</div>"
        ))
        .is_err());
    }
}
