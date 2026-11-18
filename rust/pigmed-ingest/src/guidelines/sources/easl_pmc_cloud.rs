//! Official PMC Cloud/JATS and separate PDF rendition of one patient guideline.
//! Processing version 1 is not a clinical edition. No HTML, reference-graph,
//! account, OCR or vision requests are made. Real runs belong on a batch cluster.
use super::{Adapter, PlanRow};
use crate::guidelines::{
    doc::{Block, BlockKind, DateObservation, FetchRecord, GuidelineDoc, Relation, Rights},
    fetch::{Fetched, Fetcher, Req},
    registry::SourceEntry,
};
use anyhow::{ensure, Context, Result};
use md5::{Digest, Md5};
use roxmltree::{Document, Node, ParsingOptions};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use url::Url;

pub struct EaslPmcCloud;
const PMCID: &str = "PMC8514420";
const PMID: u64 = 34693236;
const DOI: &str = "10.1016/j.jhepr.2021.100322";
const TITLE: &str = "Non-alcoholic fatty liver disease: A patient guideline";
const ARTICLE: &str = "https://pmc.ncbi.nlm.nih.gov/articles/PMC8514420/";
const META: &str = "https://pmc-oa-opendata.s3.amazonaws.com/metadata/PMC8514420.1.json";
const ORIGIN: &str = "https://pmc-oa-opendata.s3.amazonaws.com";
const OBJECT_PREFIX: &str = "/PMC8514420.1/";
const XLINK: &str = "http://www.w3.org/1999/xlink";
const ALI: &str = "http://www.niso.org/schemas/ali/1.0/";
const CC: &str = "https://creativecommons.org/licenses/by-nc-nd/4.0/";
const FILE_CAP: usize = 16 * 1024 * 1024;
const TOTAL_CAP: usize = 192 * 1024 * 1024;
const VERSION: u64 = 1;
const PDF_PROCESSING: &str = "pmc-cloud-pdfium-physical-pages-v1";
const PDF_PAGE_CAP: usize = 64;
const PDF_PAGE_TEXT_CAP: usize = 1024 * 1024;
const PDF_TEXT_CAP: usize = 16 * 1024 * 1024;

fn sha(bytes: &[u8]) -> String {
    crate::common::sha256_hex(bytes)
}
fn norm(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn name<'a>(node: Node<'a, '_>) -> &'a str {
    node.tag_name().name()
}
fn child<'a, 'i>(node: Node<'a, 'i>, tag: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|n| n.is_element() && name(*n) == tag)
}
fn one<'a, 'i>(node: Node<'a, 'i>, tag: &str) -> Result<Node<'a, 'i>> {
    let found: Vec<_> = node
        .children()
        .filter(|n| n.is_element() && name(*n) == tag)
        .collect();
    ensure!(found.len() == 1, "PMC own {tag} missing or ambiguous");
    Ok(found[0])
}
fn text(node: Node<'_, '_>) -> String {
    crate::jats::clean_text(Some(node))
}
fn label(node: Node<'_, '_>) -> String {
    [child(node, "label"), child(node, "title")]
        .into_iter()
        .flatten()
        .map(text)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
fn location(node: Node<'_, '_>) -> String {
    let mut parts = vec![];
    let mut current = Some(node);
    while let Some(n) = current.filter(|n| n.is_element()) {
        let same: Vec<_> = n
            .parent()
            .map(|p| {
                p.children()
                    .filter(|c| c.is_element() && c.tag_name() == n.tag_name())
                    .collect()
            })
            .unwrap_or_default();
        let index = same.iter().position(|c| *c == n).unwrap_or(0) + 1;
        parts.push(format!("{}[{index}]", name(n)));
        current = n.parent();
    }
    parts.reverse();
    format!("/{}", parts.join("/"))
}
fn native_id(node: Node<'_, '_>) -> String {
    node.attribute("id")
        .map(str::to_owned)
        .unwrap_or_else(|| location(node))
}
fn href<'a, 'i>(node: Node<'a, 'i>) -> Result<Option<&'a str>> {
    let values: Vec<_> = node
        .attributes()
        .filter(|a| a.name() == "href" && matches!(a.namespace(), None | Some(XLINK)))
        .map(|a| a.value())
        .collect();
    ensure!(values.len() <= 1, "Ambiguous native href attributes");
    Ok(values.first().copied())
}
fn filename(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// Unsupported/userinfo-bearing URIs stay hashes; never strip credentials or
/// turn a malformed contact URI into a usable request target.
fn uri_descriptor(raw: &str) -> Value {
    let shape =
        json!({"sha256":sha(raw.as_bytes()),"bytes":raw.len(),"characters":raw.chars().count()});
    let mut reason = None;
    let mut scheme = "relative_or_fragment";
    let mut normalized = None;
    if raw.len() > 8192 || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        reason = Some("unresolved_unsafe_native_href");
    } else {
        match Url::parse(raw) {
            Ok(url) => {
                scheme = match url.scheme() {
                    "http" => "http",
                    "https" => "https",
                    "mailto" => "mailto",
                    _ => "other_scheme",
                };
                if !url.username().is_empty() || url.password().is_some() {
                    reason = Some("unresolved_unsafe_native_href");
                } else if matches!(url.scheme(), "http" | "https") {
                    normalized = Some(url.to_string());
                } else {
                    reason = Some("unresolved_unsupported_native_uri_scheme");
                }
            }
            Err(url::ParseError::RelativeUrlWithoutBase)
                if filename(raw) || raw.strip_prefix('#').is_some_and(filename) => {}
            Err(_) => {
                reason = Some("unresolved_unsafe_native_href");
            }
        }
    }
    json!({"native_href":if reason.is_none(){Some(raw)}else{None},"normalized_uri":normalized,
        "shape":shape,"scheme_class":scheme,"unresolved_reason":reason,"target_fetched":false,
        "usable_replacement_uri_created":false})
}
fn attributes(node: Node<'_, '_>) -> Value {
    Value::Array(
        node.attributes()
            .map(|a| {
                let key = a
                    .namespace()
                    .map(|ns| format!("{{{ns}}}{}", a.name()))
                    .unwrap_or_else(|| a.name().into());
                if a.name() == "href" {
                    json!({"name":key,"uri":uri_descriptor(a.value())})
                } else {
                    json!({"name":key,"value":a.value()})
                }
            })
            .collect(),
    )
}
fn typed_identifier(node: Node<'_, '_>) -> Value {
    let value = text(node);
    let scheme = node.attribute("pub-id-type");
    let native = match scheme {
        Some("doi") => value.starts_with("10.") && !value.chars().any(char::is_whitespace),
        Some("pmid") => !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit()),
        _ => filename(&value),
    };
    json!({"scheme":scheme,"value":if native{Some(value.as_str())}else{None},"path":location(node),
        "native_value_sha256":sha(value.as_bytes()),"unresolved_reason":if native{None}else{Some("unsupported_native_identifier_representation")},
        "target_fetched":false})
}
fn native_tree(node: Node<'_, '_>) -> Value {
    if name(node) == "ref-list" {
        return json!({"tag":"ref-list","path":location(node),"native_id":node.attribute("id"),
            "bibliographic_prose_excluded":true,"reference_count":node.children().filter(|n|n.is_element()&&name(*n)=="ref").count()});
    }
    let children: Vec<Value> = node
        .children()
        .filter_map(|c| {
            if c.is_text() {
                Some(json!({"text":c.text().unwrap_or("")}))
            } else if c.is_element() {
                Some(native_tree(c))
            } else {
                None
            }
        })
        .collect();
    json!({"tag":name(node),"namespace":node.tag_name().namespace(),"path":location(node),
        "native_id":node.attribute("id"),"attributes":attributes(node),"children":children})
}

#[derive(Clone, Debug)]
struct Object {
    native_uri: String,
    url: String,
    basename: String,
    md5: String,
    role: &'static str,
}
impl Object {
    fn descriptor(&self) -> Value {
        json!({"role":self.role,"native_uri":self.native_uri,"url":self.url,"basename":self.basename,
            "provider_md5":self.md5,"bytes_acquired":false,"parsed":false})
    }
}
fn object(raw: &str, role: &'static str, extension: &str) -> Result<Object> {
    ensure!(
        raw.len() <= 2048 && !raw.chars().any(|c| c.is_whitespace() || c.is_control()),
        "Invalid Cloud object URI"
    );
    let mapped = if let Some(path) = raw.strip_prefix("s3://pmc-oa-opendata/") {
        format!("{ORIGIN}/{path}")
    } else {
        raw.to_string()
    };
    // Validate the raw namespace before URL parsing can normalize dot segments.
    let rest = mapped
        .strip_prefix(&format!("{ORIGIN}{OBJECT_PREFIX}"))
        .context("Object outside official expression namespace")?;
    let (base, query) = rest
        .split_once('?')
        .context("Provider object MD5 query missing")?;
    ensure!(
        filename(base) && base.ends_with(extension),
        "Invalid or unexpected Cloud object basename"
    );
    let expected = query
        .strip_prefix("md5=")
        .context("Provider object MD5 query changed")?;
    ensure!(
        expected.len() == 32 && expected.bytes().all(|c| c.is_ascii_hexdigit()),
        "Provider object MD5 missing or ambiguous"
    );
    let url = Url::parse(&mapped)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("pmc-oa-opendata.s3.amazonaws.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.path() == format!("{OBJECT_PREFIX}{base}"),
        "Unsafe Cloud object target"
    );
    Ok(Object {
        native_uri: raw.into(),
        url: mapped.clone(),
        basename: base.into(),
        md5: expected.to_ascii_lowercase(),
        role,
    })
}
#[derive(Clone)]
struct Metadata {
    raw: Value,
    xml: Object,
    txt: Object,
    pdf: Object,
    media: Vec<Object>,
}
fn metadata(raw: &[u8]) -> Result<Metadata> {
    ensure!(raw.len() <= 1024 * 1024, "PMC metadata exceeds source cap");
    let value: Value = serde_json::from_slice(raw).context("Native PMC metadata JSON")?;
    ensure!(
        value.is_object()
            && value["pmcid"].as_str() == Some(PMCID)
            && value["version"].as_u64() == Some(VERSION)
            && value["pmid"].as_u64() == Some(PMID)
            && value["doi"].as_str() == Some(DOI)
            && value["title"].as_str() == Some(TITLE),
        "Known patient expression metadata mismatch"
    );
    for (key, expected) in [
        ("is_pmc_openaccess", true),
        ("is_manuscript", false),
        ("is_historical_ocr", false),
        ("is_retracted", false),
    ] {
        ensure!(
            value[key].as_bool() == Some(expected),
            "PMC dataset flag {key} missing or changed; rediscover/review"
        );
    }
    ensure!(
        value["license_code"].as_str() == Some("CC BY-NC-ND")
            && value["citation"].as_str().is_some(),
        "PMC license/citation metadata mismatch"
    );
    let get = |key: &str| -> Result<&str> {
        value[key]
            .as_str()
            .with_context(|| format!("Native metadata {key} string missing"))
    };
    let xml = object(get("xml_url")?, "article_xml", ".xml")?;
    let txt = object(get("text_url")?, "xml_derived_text", ".txt")?;
    let pdf = object(get("pdf_url")?, "article_pdf", ".pdf")?;
    let entries = value["media_urls"]
        .as_array()
        .context("Native metadata media string array missing")?;
    ensure!(
        !entries.is_empty() && entries.len() <= 64,
        "PMC media inventory outside source bound"
    );
    let mut media = vec![];
    let mut seen = HashSet::from([
        xml.basename.clone(),
        txt.basename.clone(),
        pdf.basename.clone(),
    ]);
    for entry in entries {
        let raw = entry
            .as_str()
            .context("Native media inventory item is not a string")?;
        let extension = if raw.split('?').next().unwrap_or("").ends_with(".jpg") {
            ".jpg"
        } else {
            ".pdf"
        };
        let item = object(
            raw,
            if extension == ".jpg" {
                "figure_asset"
            } else {
                "supplementary_pdf"
            },
            extension,
        )?;
        ensure!(
            seen.insert(item.basename.clone()),
            "Duplicate or ambiguous PMC object inventory"
        );
        media.push(item);
    }
    Ok(Metadata {
        raw: value,
        xml,
        txt,
        pdf,
        media,
    })
}
fn config(src: &SourceEntry) -> Result<()> {
    ensure!(
        src.access == "public"
            && src.adapter.as_deref() == Some("easl_pmc_cloud")
            && src.catalog_url.as_deref() == Some(META),
        "Cloud adapter requires exact public metadata catalog"
    );
    ensure!(
        src.config["pmcid"].as_str() == Some(PMCID)
            && src.config["processing_version"].as_u64() == Some(VERSION),
        "Explicit known PMCID/processing version required"
    );
    ensure!(
        src.max_bytes > 0 && src.max_bytes <= FILE_CAP as u64,
        "Source max_bytes exceeds Cloud cap"
    );
    Ok(())
}
fn response(got: &Fetched, expected: &str, cap: usize) -> Result<()> {
    ensure!(
        got.record.url == expected
            && got.record.final_url == expected
            && got.record.redirects.is_empty(),
        "Redirected or changed Cloud response target"
    );
    ensure!(
        got.record.status == 200
            && got.body.len() <= cap
            && got.record.bytes == got.body.len() as u64
            && got.record.sha256 == sha(&got.body),
        "Cloud response failed status/size/SHA receipt binding"
    );
    ensure!(!got.is_html(), "HTML/interstitial is not a Cloud object");
    Ok(())
}
fn verify(got: &Fetched, object: &Object) -> Result<Value> {
    response(got, &object.url, FILE_CAP)?;
    ensure!(
        format!("{:x}", Md5::digest(&got.body)) == object.md5,
        "Published PMC MD5 mismatch"
    );
    let mime = got
        .record
        .content_type
        .as_deref()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let generic = matches!(
        mime.as_str(),
        "application/octet-stream" | "binary/octet-stream"
    );
    match object.role {
        "article_xml" => ensure!(
            generic || matches!(mime.as_str(), "application/xml" | "text/xml"),
            "Unexpected PMC XML MIME"
        ),
        "figure_asset" => ensure!(
            (generic || mime == "image/jpeg") && got.body.starts_with(b"\xff\xd8\xff"),
            "PMC figure is not verified JPEG bytes"
        ),
        "supplementary_pdf" | "article_pdf" => ensure!(
            (generic || mime == "application/pdf") && got.body.starts_with(b"%PDF-"),
            "PMC attachment is not verified PDF bytes"
        ),
        _ => ensure!(generic || mime == "text/plain", "Unexpected PMC text MIME"),
    }
    let mut descriptor = object.descriptor();
    descriptor["bytes_acquired"] = json!(true);
    descriptor["provider_md5_verified"] = json!(true);
    descriptor["sha256"] = json!(sha(&got.body));
    descriptor["fetch"] = serde_json::to_value(&got.record)?;
    descriptor["asset_specific_rights_verified"] = json!(false);
    descriptor["OCR_or_visual_content_verified"] = json!(false);
    Ok(descriptor)
}

fn date(node: Node<'_, '_>, date_label: String, xml: &str, source_url: &str) -> DateObservation {
    let part = |tag| child(node, tag).map(text).unwrap_or_default();
    let (year, month, day) = (part("year"), part("month"), part("day"));
    let mut normalized = None;
    if let Ok(y) = year.parse::<u32>() {
        if (1000..=9999).contains(&y) {
            normalized = Some(format!("{y:04}"));
            if !month.is_empty() {
                normalized = None;
                if let Ok(m) = month.parse::<u32>() {
                    if (1..=12).contains(&m) {
                        normalized = Some(format!("{y:04}-{m:02}"));
                        if !day.is_empty() {
                            normalized = None;
                            let max = match m {
                                4 | 6 | 9 | 11 => 30,
                                2 => {
                                    if y % 400 == 0 || y % 4 == 0 && y % 100 != 0 {
                                        29
                                    } else {
                                        28
                                    }
                                }
                                _ => 31,
                            };
                            if let Ok(d) = day.parse::<u32>() {
                                if (1..=max).contains(&d) {
                                    normalized = Some(format!("{y:04}-{m:02}-{d:02}"));
                                }
                            }
                        }
                    }
                }
            } else if !day.is_empty() {
                normalized = None;
            }
        }
    }
    DateObservation {
        label: date_label,
        raw: xml[node.range()].to_string(),
        normalized,
        source_url: Some(source_url.into()),
    }
}
fn dates(am: Node<'_, '_>, xml: &str, source_url: &str) -> Vec<DateObservation> {
    let mut out = vec![];
    for n in am.children().filter(Node::is_element) {
        match name(n) {
            "pub-date" => out.push(date(
                n,
                format!(
                    "pub-date:{}",
                    n.attribute("pub-type").unwrap_or("unlabelled")
                ),
                xml,
                source_url,
            )),
            "history" => {
                for d in n
                    .children()
                    .filter(|c| c.is_element() && name(*c) == "date")
                {
                    out.push(date(
                        d,
                        format!(
                            "history:{}",
                            d.attribute("date-type").unwrap_or("unlabelled")
                        ),
                        xml,
                        source_url,
                    ));
                }
            }
            "pub-history" => {
                for event in n
                    .children()
                    .filter(|c| c.is_element() && name(*c) == "event")
                {
                    for d in event
                        .children()
                        .filter(|c| c.is_element() && name(*c) == "date")
                    {
                        out.push(date(
                            d,
                            format!(
                                "pub-history:{}:{}",
                                event.attribute("event-type").unwrap_or("unlabelled"),
                                d.attribute("date-type").unwrap_or("unlabelled")
                            ),
                            xml,
                            source_url,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    out
}
fn rights(am: Node<'_, '_>, source_url: &str) -> Result<(Rights, Value)> {
    let permissions = one(am, "permissions")?;
    let license = one(permissions, "license")?;
    let reference: Vec<_> = license
        .children()
        .filter(|c| {
            c.is_element() && name(*c) == "license_ref" && c.tag_name().namespace() == Some(ALI)
        })
        .collect();
    ensure!(
        reference.len() == 1 && text(reference[0]) == CC,
        "Own ALI license_ref missing or changed"
    );
    let paragraph = one(license, "license-p")?;
    let notice = text(paragraph);
    ensure!(
        notice.contains("CC BY-NC-ND")
            && notice.contains("http://creativecommons.org/licenses/by-nc-nd/4.0/"),
        "Own license notice missing or changed"
    );
    let copyright = child(permissions, "copyright-statement").map(text);
    Ok((Rights {license:Some("CC BY-NC-ND 4.0".into()),status:"restricted".into(),terms_url:Some(CC.into()),note:Some(notice.clone()),
        restrictions:vec!["Noncommercial; no derivatives. Third-party and asset-specific rights require independent review.".into()],
        evidence_urls:vec![source_url.into()],..Default::default()},
        json!({"source_url":source_url,"owner_path":location(permissions),"native_ALI_license_ref":CC,"native_license_notice":notice,
            "copyright_statement":copyright,"native_permissions":native_tree(permissions),"scope":"own front/article-meta only; body/back/reference links do not grant article rights"})))
}

struct Walker<'m> {
    metadata: &'m Metadata,
    blocks: Vec<Block>,
    provenance: Vec<Value>,
    figures: Vec<Value>,
    tables: Vec<Value>,
    supplements: Vec<Value>,
    warnings: Vec<String>,
    ids: HashSet<String>,
}
impl<'m> Walker<'m> {
    fn push(
        &mut self,
        node: Node<'_, '_>,
        key: String,
        kind: BlockKind,
        value: String,
        path: &[String],
        owner: Option<&str>,
    ) -> Result<()> {
        let value = if matches!(kind, BlockKind::Table { .. }) {
            value.trim().to_string()
        } else {
            norm(&value)
        };
        if value.is_empty() && !matches!(kind, BlockKind::Figure { .. }) {
            return Ok(());
        }
        ensure!(
            self.ids.insert(key.clone()),
            "Canonical block identity collision"
        );
        let part = if node
            .ancestors()
            .any(|n| n.is_element() && name(n) == "abstract")
        {
            "abstract"
        } else if node
            .ancestors()
            .any(|n| n.is_element() && name(n) == "back")
        {
            "back"
        } else {
            "body"
        };
        let role = if node.ancestors().any(|n| n.is_element() && name(n) == "ack") {
            "acknowledgements_nonclinical"
        } else if node.ancestors().any(|n| n.is_element() && name(n) == "fn") {
            "source_footnote"
        } else if part == "back" {
            "back_section_or_resource"
        } else {
            "source_content"
        };
        self.provenance.push(json!({"block_id":key,"source_path":location(node),"native_id":node.attribute("id"),
            "source_tag":name(node),"source_part":part,"content_role":role,"owner":owner,"page_coordinates":null,"native_styles_interpreted":false,
            "ancestors_nearest_first":node.ancestors().skip(1).filter(Node::is_element).map(|n|json!({"tag":name(n),"native_id":n.attribute("id"),"path":location(n)})).collect::<Vec<_>>()}));
        self.blocks.push(Block {
            block_id: key,
            kind,
            text: value,
            heading_path: path.to_vec(),
            page: None,
        });
        Ok(())
    }
    fn flush(
        &mut self,
        node: Node<'_, '_>,
        kind: &BlockKind,
        path: &[String],
        owner: Option<&str>,
        buffer: &mut String,
        part: &mut usize,
    ) -> Result<()> {
        if !norm(buffer).is_empty() {
            *part += 1;
            let key = if *part == 1 {
                native_id(node)
            } else {
                format!("{}~part{part}", native_id(node))
            };
            self.push(node, key, kind.clone(), std::mem::take(buffer), path, owner)?;
        } else {
            buffer.clear();
        }
        Ok(())
    }
    fn mixed(
        &mut self,
        node: Node<'_, '_>,
        kind: BlockKind,
        path: &[String],
        owner: Option<&str>,
        prefix: String,
    ) -> Result<()> {
        let mut buffer = prefix;
        let mut part = 0;
        self.mixed_children(node, node, &kind, path, owner, &mut buffer, &mut part)?;
        self.flush(node, &kind, path, owner, &mut buffer, &mut part)
    }
    fn mixed_children(
        &mut self,
        parent: Node<'_, '_>,
        owner_node: Node<'_, '_>,
        kind: &BlockKind,
        path: &[String],
        owner: Option<&str>,
        buffer: &mut String,
        part: &mut usize,
    ) -> Result<()> {
        for c in parent.children() {
            if c.is_text() {
                buffer.push_str(c.text().unwrap_or(""));
            } else if c.is_element() {
                if matches!(
                    name(c),
                    "sec"
                        | "p"
                        | "fig"
                        | "boxed-text"
                        | "list"
                        | "list-item"
                        | "table-wrap"
                        | "fn"
                        | "fn-group"
                        | "supplementary-material"
                        | "graphic"
                        | "inline-graphic"
                ) {
                    self.flush(owner_node, kind, path, owner, buffer, part)?;
                    self.walk(c, path, Some(&native_id(owner_node)))?;
                } else {
                    if !matches!(
                        name(c),
                        "xref"
                            | "sup"
                            | "sub"
                            | "bold"
                            | "italic"
                            | "ext-link"
                            | "named-content"
                            | "styled-content"
                            | "underline"
                            | "sc"
                            | "monospace"
                            | "break"
                            | "label"
                    ) {
                        self.warnings.push(format!(
                            "Unreviewed inline tag {} at {}; native tree retained",
                            name(c),
                            location(c)
                        ));
                    }
                    if name(c) == "break" {
                        buffer.push(' ');
                    } else {
                        self.mixed_children(c, owner_node, kind, path, owner, buffer, part)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn figure(&mut self, node: Node<'_, '_>, path: &[String], owner: Option<&str>) -> Result<()> {
        let images: Vec<_> = node
            .children()
            .filter(|c| c.is_element() && matches!(name(*c), "graphic" | "inline-graphic"))
            .collect();
        ensure!(
            !images.is_empty(),
            "Native figure has no direct graphic; unsupported structure"
        );
        let caption = child(node, "caption").map(text).filter(|s| !s.is_empty());
        for (i, image) in images.iter().enumerate() {
            let raw = href(*image)?.context("Graphic href missing")?;
            ensure!(
                filename(raw),
                "Unsupported graphic href; no secondary request"
            );
            let found: Vec<_> = self
                .metadata
                .media
                .iter()
                .filter(|o| o.basename == raw && o.role == "figure_asset")
                .collect();
            ensure!(
                found.len() == 1,
                "Graphic does not uniquely resolve to actual metadata image entry"
            );
            let item = found[0];
            let key = if images.len() == 1 {
                native_id(node)
            } else {
                format!("{}~graphic{}", native_id(node), i + 1)
            };
            self.figures.push(json!({"canonical_block_id":key,"native_figure_id":node.attribute("id"),"figure_path":location(node),
                "graphic_path":location(*image),"native_href":raw,"owner":owner,"native_label":child(node,"label").map(text),
                "caption":caption,"source_kind":if node.attribute("id").is_some_and(|s|s.starts_with("undfig")){"inline_graphical_figure_wrapper"}else{"figure_wrapper"},
                "alt_text":child(*image,"alt-text").map(text).or_else(||child(node,"alt-text").map(text)),
                "long_description":child(*image,"long-desc").map(text).or_else(||child(node,"long-desc").map(text)),
                "native_attributes":attributes(*image),"object":item.descriptor(),"pixel_geometry":null,
                "ancestors_nearest_first":node.ancestors().skip(1).filter(Node::is_element).map(|n|json!({"tag":name(n),"native_id":n.attribute("id"),"path":location(n)})).collect::<Vec<_>>(),
                "caption_is_full_graphic_content":false,"OCR_or_visual_content_verified":false,"asset_specific_rights_verified":false}));
            self.push(
                node,
                key,
                BlockKind::Figure {
                    src_url: Some(item.url.clone()),
                    caption: caption.clone(),
                },
                String::new(),
                path,
                owner,
            )?;
        }
        Ok(())
    }
    fn table(&mut self, node: Node<'_, '_>, path: &[String], owner: Option<&str>) -> Result<()> {
        let table = one(node, "table")?;
        let rows: Vec<_> = table
            .descendants()
            .filter(|n| n.is_element() && name(*n) == "tr")
            .collect();
        ensure!(
            !rows.is_empty() && rows.len() <= 4096,
            "Native table row bound"
        );
        let mut native = vec![];
        let mut markdown = vec![];
        let mut occupied = BTreeMap::<(usize, usize), (usize, usize)>::new();
        for (r, row) in rows.iter().enumerate() {
            ensure!(
                row.ancestors()
                    .skip(1)
                    .find(|n| n.is_element() && name(*n) == "table")
                    == Some(table),
                "Nested table geometry unsupported"
            );
            let cells: Vec<_> = row
                .children()
                .filter(|c| c.is_element() && matches!(name(*c), "td" | "th"))
                .collect();
            ensure!(
                !cells.is_empty() && cells.len() <= 64,
                "Native table cell bound"
            );
            let mut data = vec![];
            let mut view = vec![];
            let mut column = 0;
            for (c, cell) in cells.iter().enumerate() {
                for attr in ["rowspan", "colspan"] {
                    if let Some(value) = cell.attribute(attr) {
                        let span = value
                            .parse::<usize>()
                            .context("Malformed native table span")?;
                        ensure!((1..=64).contains(&span), "Native table span outside bound");
                    }
                }
                while occupied.contains_key(&(r, column)) {
                    column += 1;
                }
                let down = cell.attribute("rowspan").unwrap_or("1").parse::<usize>()?;
                let across = cell.attribute("colspan").unwrap_or("1").parse::<usize>()?;
                ensure!(
                    r + down <= rows.len() && column + across <= 64,
                    "Native table span exceeds geometry"
                );
                for y in r..r + down {
                    for x in column..column + across {
                        ensure!(
                            occupied.insert((y, x), (r, c)).is_none(),
                            "Native table spans overlap"
                        );
                    }
                }
                let value = text(*cell);
                view.push(value.replace('|', "\\|"));
                data.push(json!({"row":r+1,"physical_cell":c+1,"origin_column":column+1,"tag":name(*cell),"text":value,"path":location(*cell),
                    "rowspan":cell.attribute("rowspan"),"colspan":cell.attribute("colspan"),"headers":cell.attribute("headers"),
                    "scope":cell.attribute("scope"),"native_cell_id":cell.attribute("id"),"native_attributes":attributes(*cell)}));
                column += across;
            }
            markdown.push(format!("| {} |", view.join(" | ")));
            if r == 0 {
                markdown.push(format!("| {} |", vec!["---"; cells.len()].join(" | ")));
            }
            native.push(json!({"row":r+1,"row_group":row.parent().filter(Node::is_element).map(name),"path":location(*row),"cells":data}));
        }
        let caption = child(node, "caption").map(text).filter(|s| !s.is_empty());
        let notes: Vec<_> = child(node, "table-wrap-foot")
            .map(|foot| foot.children().filter(Node::is_element).collect())
            .unwrap_or_default();
        self.tables.push(json!({"canonical_block_id":native_id(node),"native_table_id":node.attribute("id"),"path":location(node),
            "caption":caption,"label":child(node,"label").map(text),"rows":native,"native_attributes":attributes(table),
            "footnotes":notes.iter().map(|n|json!({"path":location(*n),"native_id":n.attribute("id"),"owner_table":native_id(node),"text":text(*n)})).collect::<Vec<_>>(),
            "Markdown_is_retrieval_view_only":true,"native_geometry_retained":true,"coordinates":null}));
        self.push(
            node,
            native_id(node),
            BlockKind::Table { caption },
            markdown.join("\n"),
            path,
            owner,
        )?;
        for note in notes {
            self.push(
                note,
                native_id(note),
                BlockKind::Footnote,
                text(note),
                path,
                Some(&native_id(node)),
            )?;
        }
        Ok(())
    }
    fn walk(&mut self, node: Node<'_, '_>, path: &[String], owner: Option<&str>) -> Result<()> {
        match name(node) {
            "ref-list" | "ref" => {}
            "sec" | "abstract" | "ack" | "boxed-text" => {
                let mut nested = path.to_vec();
                let heading = if name(node) == "boxed-text" {
                    [
                        child(node, "label").map(text),
                        child(node, "caption").map(text),
                    ]
                    .into_iter()
                    .flatten()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
                } else {
                    label(node)
                };
                if !heading.is_empty() {
                    self.push(
                        node,
                        format!("{}~heading", native_id(node)),
                        BlockKind::Heading {
                            level: (path.len() + 1).min(6) as u8,
                        },
                        heading.clone(),
                        path,
                        owner,
                    )?;
                    nested.push(heading);
                }
                for c in node.children().filter(Node::is_element) {
                    if !matches!(name(c), "label" | "title" | "caption") {
                        self.walk(c, &nested, Some(&native_id(node)))?;
                    }
                }
            }
            "p" => self.mixed(node, BlockKind::Paragraph, path, owner, String::new())?,
            "list-item" => {
                let mut prefix = child(node, "label")
                    .map(text)
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("{s} "))
                    .unwrap_or_default();
                for c in node.children().filter(Node::is_element) {
                    if name(c) == "p" {
                        self.mixed(
                            c,
                            BlockKind::ListItem,
                            path,
                            Some(&native_id(node)),
                            std::mem::take(&mut prefix),
                        )?;
                    } else if name(c) != "label" {
                        self.walk(c, path, Some(&native_id(node)))?;
                    }
                }
                if !prefix.is_empty() {
                    self.push(
                        node,
                        native_id(node),
                        BlockKind::ListItem,
                        prefix,
                        path,
                        owner,
                    )?;
                }
            }
            "fig" => self.figure(node, path, owner)?,
            "table-wrap" => self.table(node, path, owner)?,
            "fn" => self.push(
                node,
                native_id(node),
                BlockKind::Footnote,
                text(node),
                path,
                owner,
            )?,
            "supplementary-material" => {
                let media: Vec<_> = node
                    .descendants()
                    .filter(|n| n.is_element() && name(*n) == "media")
                    .collect();
                for m in media {
                    let raw = href(m)?.context("Supplement href missing")?;
                    ensure!(
                        filename(raw),
                        "Unsafe supplement href; no secondary request"
                    );
                    let items: Vec<_> = self
                        .metadata
                        .media
                        .iter()
                        .filter(|o| o.basename == raw && o.role == "supplementary_pdf")
                        .collect();
                    ensure!(
                        items.len() == 1,
                        "Supplement does not uniquely resolve to actual metadata inventory"
                    );
                    self.supplements.push(json!({"native_id":node.attribute("id"),"path":location(node),"media_path":location(m),
                        "native_href":raw,"object":items[0].descriptor(),"caption":child(node,"caption").map(text),"parsed":false,"owner":owner}));
                }
                if let Some(cap) = child(node, "caption") {
                    self.push(
                        cap,
                        format!("{}~caption", native_id(node)),
                        BlockKind::Paragraph,
                        text(cap),
                        path,
                        owner,
                    )?;
                }
            }
            "body" | "back" | "list" | "fn-group" => {
                for c in node.children().filter(Node::is_element) {
                    self.walk(c, path, Some(&native_id(node)))?;
                }
            }
            _ => {
                self.warnings.push(format!(
                    "Unreviewed clinical-bearing tag {} at {}; native mixed tree retained",
                    name(node),
                    location(node)
                ));
                self.mixed(node, BlockKind::Paragraph, path, owner, String::new())?;
            }
        }
        Ok(())
    }
}

fn content(bytes: &[u8], meta: &Metadata) -> Result<GuidelineDoc> {
    ensure!(bytes.len() <= FILE_CAP, "XML source cap");
    let xml = std::str::from_utf8(bytes).context("JATS must be UTF-8")?;
    ensure!(
        !xml.to_ascii_uppercase().contains("<!ENTITY"),
        "JATS entity declarations not accepted"
    );
    if let Some((_, tail)) = xml.split_once("<!DOCTYPE") {
        ensure!(
            !tail.split('>').next().unwrap_or("").contains('['),
            "Internal DTD subset not accepted"
        );
    }
    let parsed = Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: true,
            nodes_limit: 100000,
        },
    )?;
    let root = parsed.root_element();
    ensure!(
        name(root) == "article" && root.tag_name().namespace().is_none(),
        "Known native JATS article root required"
    );
    let am = one(one(root, "front")?, "article-meta")?;
    let mut ids = HashSet::new();
    for n in root.descendants().filter(Node::is_element) {
        ensure!(
            n.ancestors().count() <= 128,
            "XML nesting exceeds source bound"
        );
        if let Some(id) = n.attribute("id") {
            ensure!(ids.insert(id), "Duplicate native XML id");
        }
    }
    let id = |scheme: &str| -> Result<String> {
        let values: Vec<_> = am
            .children()
            .filter(|n| {
                n.is_element()
                    && name(*n) == "article-id"
                    && n.attribute("pub-id-type") == Some(scheme)
            })
            .map(text)
            .collect();
        ensure!(
            values.len() == 1,
            "Own article identifier missing or ambiguous"
        );
        Ok(values[0].clone())
    };
    ensure!(
        id("pmcid")? == PMCID
            && id("pmcid-ver")? == "PMC8514420.1"
            && id("doi")? == DOI
            && id("pmid")? == PMID.to_string(),
        "Native JATS expression identity mismatch"
    );
    ensure!(
        text(one(one(am, "title-group")?, "article-title")?) == TITLE,
        "Native JATS title mismatch"
    );
    let (rights, legal) = rights(am, &meta.xml.url)?;
    let dates = dates(am, xml, &meta.xml.url);
    let published: Vec<_> = dates
        .iter()
        .filter(|d| d.label == "pub-date:epub")
        .collect();
    ensure!(
        published.len() == 1 && published[0].normalized.as_deref() == Some("2021-09-17"),
        "Own electronic publication date missing or changed"
    );
    let body = one(root, "body")?;
    ensure!(
        body.children().any(|n| n.is_element() && name(n) == "sec"),
        "JATS body section root empty or missing"
    );
    let mut walker = Walker {
        metadata: meta,
        blocks: vec![],
        provenance: vec![],
        figures: vec![],
        tables: vec![],
        supplements: vec![],
        warnings: vec![],
        ids: HashSet::new(),
    };
    let abstracts: Vec<_> = am
        .children()
        .filter(|n| n.is_element() && name(*n) == "abstract")
        .collect();
    for abstract_ in &abstracts {
        walker.walk(*abstract_, &[], None)?;
    }
    walker.walk(body, &[], None)?;
    let back = child(root, "back");
    if let Some(back) = back {
        walker.walk(back, &[], None)?;
    }
    ensure!(
        walker.blocks.iter().any(
            |b| matches!(b.kind, BlockKind::Paragraph | BlockKind::ListItem) && !b.text.is_empty()
        ),
        "Empty clinical document"
    );
    let references:Vec<_>=back.into_iter().flat_map(|b|b.descendants()).filter(|n|n.is_element()&&name(*n)=="ref").map(|r| {
        let links=r.descendants().filter(|n|n.is_element()&&name(*n)=="ext-link").map(|n|href(n).map(|h|h.map(uri_descriptor))).collect::<Result<Vec<_>>>()?;
        Ok(json!({"native_ref_id":r.attribute("id"),"path":location(r),"role":"bibliographic_reference_descriptor",
            "identifiers":r.descendants().filter(|n|n.is_element()&&name(*n)=="pub-id").map(typed_identifier).collect::<Vec<_>>(),
            "links":links,"bibliographic_prose_in_clinical_blocks":false,"external_graph_fetched":false}))
    }).collect::<Result<_>>()?;
    let scope = |n: Node<'_, '_>| {
        n.ancestors()
            .any(|a| a == body || abstracts.contains(&a) || back == Some(a))
            && !n
                .ancestors()
                .any(|a| a.is_element() && matches!(name(a), "ref-list" | "ref"))
    };
    let links:Vec<_>=root.descendants().filter(|n|n.is_element()&&scope(*n)&&matches!(name(*n),"xref"|"ext-link")).map(|n| {
        let rids:Vec<_>=n.attribute("rid").unwrap_or("").split_whitespace().map(|rid|{
            let target=root.descendants().find(|t|t.is_element()&&t.attribute("id")==Some(rid));
            json!({"native_target_id":rid,"target_tag":target.map(name),"target_in_retained_scope":target.map(scope)})
        }).collect();
        Ok(json!({"tag":name(n),"path":location(n),"native_id":n.attribute("id"),"ref_type":n.attribute("ref-type"),
            "native_link_text":text(n),"targets":rids,"href":href(n)?.map(uri_descriptor),"target_fetched":false}))
    }).collect::<Result<_>>()?;
    let mut relations = vec![];
    let related:Vec<_>=am.children().filter(|n|n.is_element()&&name(*n)=="related-article").map(|n|{
        let raw=href(n)?;
        if raw==Some("PMC11228441") {
            relations.push(Relation{rel:"erratum".into(),target:"https://pmc.ncbi.nlm.nih.gov/articles/PMC11228441/".into(),scope:None,evidence_url:Some(meta.xml.url.clone())});
        }
        Ok(json!({"path":location(n),"native_attributes":attributes(n),"href":raw.map(uri_descriptor),
            "native_relation_type":n.attribute("related-article-type"),"identifiers":n.descendants().filter(|c|c.is_element()&&name(*c)=="pub-id").map(typed_identifier).collect::<Vec<_>>(),
            "target_fetched":false,"clinical_currentness_inferred":false}))
    }).collect::<Result<_>>()?;
    let native_scopes: Vec<_> = abstracts
        .iter()
        .copied()
        .chain(std::iter::once(body))
        .chain(back)
        .map(native_tree)
        .collect();
    let mut warnings = walker.warnings;
    warnings.extend([
        "Exactly one configured PMC processing expression; version history and EASL professional CPG corpus are incomplete. Processing sequence is not clinical edition/currentness.",
        "Image captions/headings are not graphic clinical content. All figure text OCR/vision and image algorithms remain unverified, including undfig1/undfig2 text-as-image.",
        "Article rights are restricted CC BY-NC-ND; asset-specific/third-party permissions remain unverified. No formal graded recommendations inferred.",
        "Native mixed-content tree preserves inline markup/list/boxed/paragraph boundaries; Markdown/plain text are retrieval views, without inferred coordinates/styles/GRADE.",
        "Shared Fetcher can follow redirects before this adapter rejects redirected receipts; this adapter does not provide a pre-request no-follow guarantee.",
    ].into_iter().map(str::to_string));
    let publisher = child(one(root, "front")?, "journal-meta")
        .and_then(|n| child(n, "publisher"))
        .and_then(|n| child(n, "publisher-name"))
        .map(text);
    Ok(GuidelineDoc {
        source_id: String::new(),
        doc_key: String::new(),
        work_key: Some(format!("doi:{DOI}")),
        item_type: Some("patient_guideline".into()),
        title: TITLE.into(),
        url: ARTICLE.into(),
        publisher,
        issuers: vec![],
        version: None,
        status: "unknown".into(),
        published: published[0].normalized.clone(),
        updated: None,
        date_observations: dates,
        relations,
        language: root
            .attribute(("http://www.w3.org/XML/1998/namespace", "lang"))
            .map(str::to_owned),
        country: None,
        doi: Some(DOI.into()),
        pmid: Some(PMID.to_string()),
        rights,
        blocks: walker.blocks,
        search_metadata: json!({"expression_id":PMCID,"dataset_processing_version":VERSION,"clinical_edition":null,"source_role":"patient_guideline",
            "scope_complete":false,"clinical_completeness_accepted":false,"processing_version_history_complete":false,
            "order_authority":"JATS XML preorder","rendered_html_order_equivalence_verified":false,
            "source_metadata":meta.raw,"source_xml_sha256":sha(bytes),"native_legal_metadata":legal,
            "native_own_date_metadata":am.children().filter(|n|n.is_element()&&matches!(name(*n),"pub-date"|"history"|"pub-history")).map(native_tree).collect::<Vec<_>>(),
            "native_scopes":native_scopes,"native_block_provenance":walker.provenance,"native_figures":walker.figures,"native_tables":walker.tables,
            "native_supplements":walker.supplements,"native_links":links,"bibliographic_reference_descriptors":references,"native_related_articles":related,
            "coverage":{"abstract_and_body":"ordered canonical blocks and native mixed tree","back_sections_and_resources":"ordered after body, preserving native section identities",
                "acknowledgements":"retained with nonclinical provenance role","footnotes":"owned source blocks","bibliography":"typed identifiers and safe/hash-only links; prose excluded",
                "supplementary_pdf":"descriptor; acquired only during extract; unparsed","main_pdf":"unacquired descriptor"},
            "asset_OCR_or_vision_verified":false,"native_coordinates":null,"styles_interpreted":false}),
        attachments: vec![
            meta.xml.descriptor(),
            meta.txt.descriptor(),
            meta.pdf.descriptor(),
        ],
        fetches: vec![],
        warnings,
    })
}
fn snapshot(doc: &GuidelineDoc) -> Result<String> {
    Ok(sha(&serde_json::to_vec(
        &json!({"title":doc.title,"work_key":doc.work_key,"item_type":doc.item_type,"doi":doc.doi,"pmid":doc.pmid,
        "version":doc.version,"published":doc.published,"dates":doc.date_observations,"rights":doc.rights,"relations":doc.relations,
        "blocks":doc.blocks,"source_native_evidence":doc.search_metadata,"attachments":doc.attachments}),
    )?))
}
async fn load(f: &Fetcher) -> Result<(Fetched, Fetched, Metadata, GuidelineDoc)> {
    let got = f
        .request(&Req {
            cache_max_age_secs: Some(0),
            ..Req::get(META)
        })
        .await?;
    response(&got, META, 1024 * 1024)?;
    let meta = metadata(&got.body)?;
    let xml = f.get_ok(&meta.xml.url).await?;
    verify(&xml, &meta.xml)?;
    let doc = content(&xml.body, &meta)?;
    Ok((got, xml, meta, doc))
}

// The PDF is a distinct source-advertised rendition, not a replacement or an
// inferred layout/figure interpretation of the existing JATS expression.
#[derive(Clone, serde::Deserialize)]
struct PdfPage {
    text: String,
    width_points: f32,
    height_points: f32,
}

fn pdf_first_page_doi(text: &str) -> bool {
    regex::Regex::new(
        r"(?i)(?:^|[^a-z0-9_])10\.1016/j\.jhepr\.2021\.100322(?:$|\s|[.,;:!?()\[\]{}]+(?:$|\s))",
    )
    .expect("constant patient-work DOI regex")
    .is_match(text)
}

fn pdf_frontmatter(text: &str) -> Value {
    // These are observations, not self-DOI ownership or source equivalence.
    // A citation-rich first page can legitimately mention another work's DOI.
    let regex = regex::Regex::new(r"(?i)\b10\.[0-9]{4,9}/[-._;()/:a-z0-9]+")
        .expect("constant bounded DOI-looking token regex");
    let mut candidates = vec![];
    let mut truncated = false;
    for found in regex.find_iter(text) {
        if candidates.len() == 32 {
            truncated = true;
            break;
        }
        candidates.push(json!({"raw_token_sha256":sha(found.as_str().as_bytes()),
            "raw_token_utf8_bytes":found.as_str().len(),"first_page_utf8_byte_start":found.start(),
            "self_identity_ownership_verified":false}));
    }
    json!({"expected_DOI_literal_present":text.contains(DOI),
        "expected_DOI_bounded_case_insensitive_present":pdf_first_page_doi(text),
        "exact_native_title_literal_present":text.contains(TITLE),
        "whitespace_folded_native_title_present":norm(text).contains(TITLE),
        "title_comparison_method":"source-native title, exact case/punctuation/hyphens; whitespace folding only; no fuzzy, Unicode or OCR repair",
        "title_expected_sha256":sha(TITLE.as_bytes()),"unowned_DOI_looking_candidates":candidates,
        "DOI_candidate_cap":32,"DOI_candidate_scan_truncated":truncated,
        "native_self_DOI_ownership_verified":false,"unmatched_candidates_are_not_work_mismatch":true,
        "content_equivalence_or_clinical_currency_verified":false})
}

fn collect_pdf_pages(
    count: usize,
    mut get: impl FnMut(usize) -> Result<PdfPage>,
) -> Result<Vec<PdfPage>> {
    ensure!(
        (1..=PDF_PAGE_CAP).contains(&count),
        "PMC PDF physical page count outside 1..={PDF_PAGE_CAP}; not truncated"
    );
    let mut pages = Vec::with_capacity(count);
    let mut total = 0usize;
    for index in 0..count {
        let page = get(index)?;
        total = total
            .checked_add(page.text.len())
            .context("PMC PDF text byte overflow")?;
        ensure!(
            page.text.len() <= PDF_PAGE_TEXT_CAP && total <= PDF_TEXT_CAP,
            "PMC PDF physical page {} text exceeds finite bounds; not truncated",
            index + 1
        );
        pages.push(page);
    }
    ensure!(
        pages.len() == count,
        "PMC PDF declared/loaded physical page count mismatch"
    );
    Ok(pages)
}

fn pdf_pages(bytes: &[u8]) -> Result<Vec<PdfPage>> {
    ensure!(
        bytes.len() <= FILE_CAP && bytes.starts_with(b"%PDF-"),
        "PMC PDF signature/byte bound failed"
    );
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("patient-rendition.pdf");
    std::fs::write(&path, bytes)?;
    let _gate = crate::media::PDFIUM_GATE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let pdf = crate::media::pdfium()?
        .load_pdf_from_file(&path, None)
        .map_err(|e| anyhow::anyhow!("PMC PDFium load: {e}"))?;
    let count = pdf.pages().len() as usize;
    // PdfPages::iter() can stop at a get() error. Index every declared page and
    // propagate load/text errors so an incomplete prefix can never be accepted.
    collect_pdf_pages(count, |index| {
        let page = pdf
            .pages()
            .get(index as u16)
            .map_err(|e| anyhow::anyhow!("PMC PDFium physical page {} load: {e}", index + 1))?;
        let text = page
            .text()
            .map_err(|e| anyhow::anyhow!("PMC PDFium physical page {} text: {e}", index + 1))?
            .all();
        Ok(PdfPage {
            text,
            width_points: page.width().value,
            height_points: page.height().value,
        })
    })
}

fn pdf_document(
    meta: &Metadata,
    source: &GuidelineDoc,
    metadata_sha: &str,
    xml_sha: &str,
    pdf_sha: &str,
    pdf_bytes: usize,
    pages: Vec<PdfPage>,
) -> Result<GuidelineDoc> {
    ensure!(
        source.doi.as_deref() == Some(DOI)
            && source.pmid.as_deref() == Some(PMID.to_string().as_str())
            && source.work_key.as_deref() == Some(format!("doi:{DOI}").as_str())
            && source.url == ARTICLE
            && source.item_type.as_deref() == Some("patient_guideline"),
        "PMC PDF source work/role identity mismatch"
    );
    for digest in [metadata_sha, xml_sha, pdf_sha] {
        ensure!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "PMC PDF input SHA identity invalid"
        );
    }
    ensure!(
        pdf_bytes > 0 && pdf_bytes <= FILE_CAP && (1..=PDF_PAGE_CAP).contains(&pages.len()),
        "PMC PDF byte/page bounds invalid"
    );
    ensure!(
        pages[0].text.len() <= PDF_PAGE_TEXT_CAP,
        "PMC PDF first-page text exceeds bound"
    );
    let frontmatter = pdf_frontmatter(&pages[0].text);
    let mut blocks = Vec::with_capacity(pages.len());
    let mut evidence = Vec::with_capacity(pages.len());
    let mut total = 0usize;
    for (index, page) in pages.into_iter().enumerate() {
        total = total
            .checked_add(page.text.len())
            .context("PMC PDF text byte overflow")?;
        ensure!(!page.text.trim().is_empty() && page.text.len() <= PDF_PAGE_TEXT_CAP && total <= PDF_TEXT_CAP
            && page.width_points.is_finite() && page.height_points.is_finite()
            && page.width_points > 0.0 && page.height_points > 0.0,
            "PMC PDF physical page {} blank/unreadable/invalid or over bound; no OCR or silent omission", index + 1);
        let number = (index + 1) as u32;
        let id = format!("pdf-page-{number:04}");
        evidence.push(json!({"block_id":id,"physical_page":number,"text_utf8_bytes":page.text.len(),
            "exact_text_sha256":sha(page.text.as_bytes()),"width_points":page.width_points,"height_points":page.height_points,
            "row_section_leading_whitespace_chars":page.text.chars().take_while(|c| c.is_whitespace()).count(),
            "row_section_trimmed_chars":page.text.trim().chars().count()}));
        blocks.push(Block {
            block_id: id,
            kind: BlockKind::Paragraph,
            text: page.text,
            heading_path: vec![format!(
                "PDF physical page {number} (unclassified text layer)"
            )],
            page: Some(number),
        });
    }
    // Stable content evidence only: actual response timestamps/cache flags live in
    // doc.fetches, not the content fingerprint or these attachment descriptors.
    let native_pdf = json!({"role":"article_pdf_rendition_source","native_uri":meta.pdf.native_uri,
        "url":meta.pdf.url,"basename":meta.pdf.basename,"provider_md5":meta.pdf.md5,
        "provider_md5_verified":true,"sha256":pdf_sha,"bytes":pdf_bytes,"bytes_acquired":true,"parsed":true,
        "asset_specific_rights_verified":false,"OCR_or_visual_content_verified":false});
    let source_evidence = json!({"role":"source_JATS_metadata_and_rights_evidence","url":meta.xml.url,
        "metadata_url":META,"metadata_sha256":metadata_sha,"source_xml_sha256":xml_sha,
        "native_metadata_license_code":meta.raw["license_code"],"JATS_article_rights":source.rights,
        "rights_scope":"Provider metadata and own JATS article only; PDF-specific notices and third-party credits unverified",
        "native_JATS_related_work_evidence":source.relations});
    Ok(GuidelineDoc {
        source_id:String::new(), doc_key:String::new(), work_key:Some(format!("doi:{DOI}")),
        title:source.title.clone(), url:meta.pdf.url.clone(), item_type:Some("patient_guideline".into()),
        publisher:source.publisher.clone(), issuers:vec![], version:None, status:"unknown".into(),
        published:source.published.clone(), updated:None, date_observations:source.date_observations.clone(),
        language:source.language.clone(), country:None, doi:Some(DOI.into()), pmid:Some(PMID.to_string()),
        rights:Rights { status:"unknown".into(), note:Some("PDF-specific rights and third-party credit lines have not been mapped. The provider license_code and JATS article permissions are retained as scoped evidence; neither is inherited as this PDF asset's reuse licence.".into()),
            evidence_urls:vec![meta.pdf.url.clone(),meta.xml.url.clone(),META.into()], ..Default::default() },
        relations:vec![Relation { rel:"rendition_of".into(), target:ARTICLE.into(),
            scope:Some("Source-advertised main PDF of this patient guideline/PMC processing expression; physical pages are not JATS figure ordinals".into()), evidence_url:Some(META.into()) }],
        blocks,
        attachments:vec![native_pdf.clone(),source_evidence],
        search_metadata:json!({"expression_id":PMCID,"dataset_processing_version":VERSION,"clinical_edition":null,
            "source_role":"patient_guideline","rendition":"pdf","pdf_processing_revision":PDF_PROCESSING,
            "scope_complete":false,"clinical_completeness_accepted":false,"processing_version_history_complete":false,
            "source_metadata_url":META,"source_metadata_sha256":metadata_sha,"source_metadata":meta.raw,
            "source_xml_sha256":xml_sha,"metadata_provenance":"Known provider metadata plus own JATS article; native source URLs retained",
            "native_pdf":native_pdf,"main_pdf_acquired":true,"native_first_page_identity_observations":frontmatter,
            "work_binding":"source-attested by known provider metadata, advertised native PDF URL, published MD5 and complete received byte SHA256",
            "physical_page_count":evidence.len(),"physical_pages":evidence,
            "text_method":"PDFium indexed physical pages, page.text().all(); exact returned UTF-8 preserved",
            "order_authority":"physical PDF page order; within-page PDFium text order unreviewed",
            "page_numbering":"one-based physical PDF pages; printed labels not inferred",
            "heading_path_method":"synthetic physical-page context for grouping/provenance, not clinical headings",
            "row_text_method":"Shared writer trims page-edge whitespace and chunks words; row char offsets address trimmed page text; exact untrimmed text stays canonical",
            "PDF_byte_cap":FILE_CAP,"physical_page_cap":PDF_PAGE_CAP,"page_text_byte_cap":PDF_PAGE_TEXT_CAP,"total_text_byte_cap":PDF_TEXT_CAP,
            "OCR_performed":false,"table_count":null,"table_geometry_verified":false,"figure_ownership_verified":false,
            "footnote_ownership_verified":false,"recommendation_grades_inferred":false,"clinical_reading_order_verified":false,
            "nine_anchor_presence_is_not_geometry_or_clinical_fidelity":true,"clinical_currentness_verified":false}),
        fetches:vec![],
        warnings:vec!["Separate patient-guideline PDF rendition. Every physical page must have extracted text; blank/unreadable or over-bound pages reject it. PDFium text is unclassified, not inferred table cells, figure labels, footnote ownership, grades or clinical reading order. OCR/rendering are disabled.".into(),
            "PMC dataset processing version 1 and text-extraction revision are not a clinical edition or proof of currency. This patient work does not establish EASL professional CPG corpus coverage.".into(),
            "PDF-specific reuse rights/third-party credits remain unknown; provider and JATS notices keep their own evidence scope. Nine selected strings somewhere in PDF text do not establish illustration ownership or visual fidelity.".into(),
            "PDF work/rendition association is source-attested. Printed first-page DOI/title markers are separate observations; missing or unmatched unowned DOI candidates do not identify the wrong work. Native self-DOI ownership and cross-rendition content equivalence remain unresolved.".into(),
            "Shared Fetcher may follow redirects before exact native response validation rejects them; no pre-request no-follow guarantee is claimed.".into()],
    })
}

fn pdf_snapshot(doc: &GuidelineDoc) -> Result<String> {
    // serde_json/preserve_order is the producer contract, as for JATS snapshot().
    Ok(sha(&serde_json::to_vec(
        &json!({"content_snapshot":snapshot(doc)?,"url":doc.url,
        "publisher":doc.publisher,"language":doc.language,"status":doc.status}),
    )?))
}

async fn load_pdf(
    f: &Fetcher,
    metadata: &Fetched,
    xml: &Fetched,
    meta: &Metadata,
    source: &GuidelineDoc,
) -> Result<GuidelineDoc> {
    let got = f
        .request(&Req {
            cache_max_age_secs: Some(0),
            ..Req::get(&meta.pdf.url)
        })
        .await?;
    verify(&got, &meta.pdf)?;
    ensure!(
        metadata.body.len() + xml.body.len() + got.body.len() <= TOTAL_CAP,
        "PMC PDF aggregate source cap exceeded"
    );
    let digest = sha(&got.body);
    let size = got.body.len();
    let Fetched { record, body } = got;
    let pages = tokio::task::spawn_blocking(move || pdf_pages(&body)).await??;
    let mut doc = pdf_document(
        meta,
        source,
        &metadata.record.sha256,
        &xml.record.sha256,
        &digest,
        size,
        pages,
    )?;
    doc.fetches = vec![metadata.record.clone(), xml.record.clone(), record];
    Ok(doc)
}

fn pdf_plan(src: &SourceEntry, doc: &GuidelineDoc) -> Result<PlanRow> {
    let hash = pdf_snapshot(doc)?;
    Ok(PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("pmc8514420-processing1-pdf-{}", &hash[..16]),
        url: doc.url.clone(),
        title: Some(doc.title.clone()),
        rendition: Some("pdf".into()),
        version: None,
        extra: json!({"expression_id":PMCID,"processing_version":VERSION,"source_role":"patient_guideline","clinical_version":null,
            "metadata_url":META,"metadata_sha256":doc.search_metadata["source_metadata_sha256"],
            "xml_sha256":doc.search_metadata["source_xml_sha256"],"pdf_snapshot_sha256":hash,
            "native_pdf":doc.search_metadata["native_pdf"],"pdf_processing_revision":PDF_PROCESSING,
            "work_key":format!("doi:{DOI}"),"scope_complete":false,"version_history_complete":false}),
    })
}

fn validate_pdf_plan(src: &SourceEntry, row: &PlanRow, doc: &GuidelineDoc) -> Result<()> {
    let expected = pdf_plan(src, doc)?;
    ensure!(
        row.source_id == expected.source_id
            && row.doc_key == expected.doc_key
            && row.url == expected.url
            && row.title == expected.title
            && row.rendition == expected.rendition
            && row.version == expected.version
            && row.extra == expected.extra,
        "PMC PDF source/content/PlanRow identity changed; rediscover"
    );
    Ok(())
}

async fn extract_pdf(src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
    ensure!(
        row.source_id == src.id && row.rendition.as_deref() == Some("pdf") && row.version.is_none(),
        "Invalid PMC PDF plan source/rendition/version"
    );
    ensure!(
        row.extra["discovery_error"].is_null(),
        "PMC PDF discovery unresolved; rediscover"
    );
    let (metadata, xml, meta, source) = load(f).await?;
    ensure!(
        row.url == meta.pdf.url,
        "PMC PDF target no longer equals native advertised URL; rediscover"
    );
    let mut doc = load_pdf(f, &metadata, &xml, &meta, &source).await?;
    validate_pdf_plan(src, row, &doc)?;
    doc.source_id = src.id.clone();
    doc.doc_key = row.doc_key.clone();
    Ok(doc)
}
#[async_trait::async_trait]
impl Adapter for EaslPmcCloud {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        config(src)?;
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let (metadata, xml, meta, doc) = load(f).await?;
        let hash = snapshot(&doc)?;
        let mut rows = vec![PlanRow {
            source_id: src.id.clone(),
            doc_key: format!("pmc8514420-processing1-{}", &hash[..16]),
            url: ARTICLE.into(),
            title: Some(doc.title.clone()),
            rendition: Some("xml".into()),
            version: None,
            extra: json!({"expression_id":PMCID,"processing_version":VERSION,"source_role":"patient_guideline","clinical_version":null,
                "metadata_url":META,"metadata_sha256":metadata.record.sha256,"xml_sha256":xml.record.sha256,"clinical_snapshot_sha256":hash,
                "work_key":format!("doi:{DOI}"),"scope_complete":false,"version_history_complete":false}),
        }];
        if !limit.is_some_and(|n| n <= 1) {
            match load_pdf(f, &metadata, &xml, &meta, &doc).await {
                Ok(pdf) => rows.push(pdf_plan(src, &pdf)?),
                Err(error) => rows.push(PlanRow {
                    source_id:src.id.clone(), doc_key:format!("pmc8514420-processing1-pdf-unresolved-{}", &sha(meta.pdf.url.as_bytes())[..12]),
                    url:meta.pdf.url.clone(), title:Some(doc.title.clone()), rendition:Some("pdf".into()), version:None,
                    extra:json!({"discovery_error":format!("{error:#}"),"expression_id":PMCID,"processing_version":VERSION,
                        "source_role":"patient_guideline","clinical_version":null,"metadata_url":META,
                        "metadata_sha256":metadata.record.sha256,"xml_sha256":xml.record.sha256,
                        "native_advertised_pdf":meta.pdf.descriptor(),"work_key":format!("doi:{DOI}"),
                        "scope_complete":false,"PDF_rendition_complete":false}),
                }),
            }
        }
        // Fix group H2: other EASL documents distributed through the PMC Open Access subset, enumerated through
        // Europe PMC (`config.oa_enumeration`) and read through the shared OA crosswalk; this Cloud expression is
        // never planned twice.
        if src.config.get("oa_enumeration").is_some() && !limit.is_some_and(|n| rows.len() >= n) {
            let skip: HashSet<String> = [PMCID.to_string()].into();
            let room = limit.map(|n| n - rows.len());
            rows.extend(super::pmc_crosswalk::enumerate(src, f, "easl-oa", room, &skip).await?);
        }
        Ok(rows)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        config(src)?;
        if super::pmc_crosswalk::is_crosswalk_row(row) {
            return super::pmc_crosswalk::extract(src, f, row).await;
        }
        if row.rendition.as_deref() == Some("pdf") {
            return extract_pdf(src, f, row).await;
        }
        ensure!(
            row.source_id == src.id
                && row.url == ARTICLE
                && row.version.is_none()
                && row.extra["expression_id"].as_str() == Some(PMCID)
                && row.extra["processing_version"].as_u64() == Some(VERSION)
                && row.extra["source_role"].as_str() == Some("patient_guideline")
                && row.extra["metadata_url"].as_str() == Some(META),
            "Invalid patient Cloud plan identity"
        );
        let (metadata, xml, meta, mut doc) = load(f).await?;
        let hash = snapshot(&doc)?;
        ensure!(
            row.extra["metadata_sha256"].as_str() == Some(metadata.record.sha256.as_str())
                && row.extra["xml_sha256"].as_str() == Some(xml.record.sha256.as_str())
                && row.extra["clinical_snapshot_sha256"].as_str() == Some(hash.as_str())
                && row.doc_key == format!("pmc8514420-processing1-{}", &hash[..16]),
            "PMC objects/native semantics changed within processing version; rediscover"
        );
        let mut acquired = vec![];
        let mut bytes = metadata.body.len() + xml.body.len();
        let mut xml_descriptor = verify(&xml, &meta.xml)?;
        xml_descriptor["parsed"] = json!(true);
        let mut fetches: Vec<FetchRecord> = vec![metadata.record, xml.record];
        for item in &meta.media {
            // Every request target originates in validated exact official metadata;
            // native references/contact links are never request targets.
            ensure!(
                bytes.checked_add(FILE_CAP).is_some_and(|v| v <= TOTAL_CAP),
                "Aggregate cap cannot admit another worst-case object"
            );
            let got = f.get_ok(&item.url).await?;
            bytes = bytes
                .checked_add(got.body.len())
                .context("Aggregate Cloud byte overflow")?;
            ensure!(
                bytes <= TOTAL_CAP,
                "Aggregate Cloud source byte cap exceeded"
            );
            acquired.push(verify(&got, item)?);
            fetches.push(got.record);
        }
        doc.source_id = src.id.clone();
        doc.doc_key = row.doc_key.clone();
        doc.fetches = fetches;
        doc.attachments = vec![xml_descriptor, meta.txt.descriptor(), meta.pdf.descriptor()];
        doc.attachments.extend(acquired);
        doc.search_metadata["planned_clinical_snapshot_sha256"] = json!(hash);
        doc.search_metadata["media_inventory_objects_verified"] = json!(meta.media.len());
        doc.search_metadata["main_pdf_acquired"] = json!(false);
        doc.search_metadata["supplementary_pdf_parsed"] = json!(false);
        Ok(doc)
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests;

#[cfg(all(test, feature = "guideline-fixtures"))]
mod pdf_rendition_tests {
    use super::*;
    const XML: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/easl_pmc_cloud/patient-synthetic.xml"
    ));
    const NATIVE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/easl_pmc_cloud/metadata-synthetic.json"
    ));
    const PDF: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/easl_pmc_cloud/physical-pages-synthetic.pdf"
    ));
    const JPEG: &[u8] = b"\xff\xd8\xffsynthetic-signature-only";
    const SUPPLEMENT: &[u8] = b"%PDF-1.4\nSynthetic unparsed fixture only\n%%EOF";

    fn native(pdf: &[u8]) -> Value {
        let mut value: Value = serde_json::from_str(NATIVE).unwrap();
        value["xml_url"] = json!(format!(
            "s3://pmc-oa-opendata/PMC8514420.1/PMC8514420.1.xml?md5={:x}",
            Md5::digest(XML.as_bytes())
        ));
        // A deliberately different native advertised basename proves the adapter
        // uses the provider field, not a reconstructed PMCID + '.pdf' filename.
        value["pdf_url"] = json!(format!(
            "s3://pmc-oa-opendata/PMC8514420.1/provider-advertised-patient.pdf?md5={:x}",
            Md5::digest(pdf)
        ));
        value
    }
    fn source() -> SourceEntry {
        serde_json::from_value(
            json!({"id":"easl_public_pmc","access":"public","enabled":true,
            "adapter":"easl_pmc_cloud","catalog_url":META,"max_bytes":FILE_CAP,
            "config":{"pmcid":PMCID,"processing_version":VERSION}}),
        )
        .unwrap()
    }
    fn pages() -> Vec<PdfPage> {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guidelines/easl_pmc_cloud/pdf-pages-synthetic.json"
        )))
        .unwrap()
    }
    fn fixture_doc() -> GuidelineDoc {
        let raw = serde_json::to_vec(&native(PDF)).unwrap();
        let meta = metadata(&raw).unwrap();
        let jats = content(XML.as_bytes(), &meta).unwrap();
        pdf_document(
            &meta,
            &jats,
            &sha(&raw),
            &sha(XML.as_bytes()),
            &sha(PDF),
            PDF.len(),
            pages(),
        )
        .unwrap()
    }
    fn seed(f: &Fetcher, value: &Value, pdf: Option<&[u8]>) {
        let raw = serde_json::to_vec(value).unwrap();
        let meta = metadata(&raw).unwrap();
        f.seed_cache_for_tests(META, &raw, "binary/octet-stream");
        f.seed_cache_for_tests(&meta.xml.url, XML.as_bytes(), "binary/octet-stream");
        for item in &meta.media {
            f.seed_cache_for_tests(
                &item.url,
                if item.role == "figure_asset" {
                    JPEG
                } else {
                    SUPPLEMENT
                },
                "binary/octet-stream",
            );
        }
        if let Some(bytes) = pdf {
            f.seed_cache_for_tests(&meta.pdf.url, bytes, "binary/octet-stream");
        }
    }

    #[test]
    fn source_advertised_pdf_md5_and_patient_work_are_authoritative_not_filename_or_page1_guess() {
        let raw = serde_json::to_vec(&native(PDF)).unwrap();
        let meta = metadata(&raw).unwrap();
        assert!(meta
            .pdf
            .url
            .contains("/provider-advertised-patient.pdf?md5="));
        let mut got = Fetched {
            record: FetchRecord {
                url: meta.pdf.url.clone(),
                final_url: meta.pdf.url.clone(),
                status: 200,
                content_type: Some("binary/octet-stream".into()),
                bytes: PDF.len() as u64,
                sha256: sha(PDF),
                ..Default::default()
            },
            body: PDF.to_vec(),
        };
        verify(&got, &meta.pdf).unwrap();
        got.body.push(b' ');
        got.record.bytes = got.body.len() as u64;
        got.record.sha256 = sha(&got.body);
        assert!(verify(&got, &meta.pdf).is_err()); // A valid SHA receipt cannot waive provider MD5.
        for bad in [
            "https://example.org/copied.pdf?md5=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "s3://pmc-oa-opendata/PMC8514420.1/../other.pdf?md5=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let mut value = native(PDF);
            value["pdf_url"] = json!(bad);
            assert!(metadata(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        let mut value = native(PDF);
        value.as_object_mut().unwrap().remove("pdf_url");
        assert!(metadata(&serde_json::to_vec(&value).unwrap()).is_err());
        value = native(PDF);
        value["doi"] = json!("10.1000/wrong-work");
        assert!(metadata(&serde_json::to_vec(&value).unwrap()).is_err());
        let doc = fixture_doc();
        assert_eq!(doc.work_key.as_deref(), Some(format!("doi:{DOI}").as_str()));
        assert_eq!(doc.item_type.as_deref(), Some("patient_guideline"));
        assert!(doc.version.is_none());
        assert_eq!(doc.rights.status, "unknown");
        assert!(doc.rights.license.is_none());
        assert_eq!(
            doc.attachments[1]["JATS_article_rights"]["status"],
            "restricted"
        );
        // Missing printed DOI/title is explicitly unverified, not a wrong-work finding.
        let meta = metadata(&raw).unwrap();
        let jats = content(XML.as_bytes(), &meta).unwrap();
        let mut no_marker = pages();
        no_marker[0].text = "Synthetic preface citing 10.1000/unrelated-work.\n".into();
        let doc = pdf_document(
            &meta,
            &jats,
            &sha(&raw),
            &sha(XML.as_bytes()),
            &sha(PDF),
            PDF.len(),
            no_marker,
        )
        .unwrap();
        let front = &doc.search_metadata["native_first_page_identity_observations"];
        assert_eq!(front["expected_DOI_literal_present"], false);
        assert_eq!(front["whitespace_folded_native_title_present"], false);
        assert_eq!(
            front["unowned_DOI_looking_candidates"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(front["native_self_DOI_ownership_verified"], false);
        assert_eq!(
            pdf_frontmatter(&format!("{DOI}.1"))["expected_DOI_bounded_case_insensitive_present"],
            false
        );
        assert_eq!(
            pdf_frontmatter(&TITLE.replace(' ', "\n"))["whitespace_folded_native_title_present"],
            true
        );
        assert_eq!(
            pdf_frontmatter(&TITLE.replace("Non-alcoholic", "Non alcoholic"))
                ["whitespace_folded_native_title_present"],
            false
        );
    }

    #[test]
    fn complete_pdf_text_and_physical_page_spans_survive_writer_without_inferred_table_or_figure() {
        use crate::guidelines::doc::{to_rows, ChunkCfg};
        let expected = pages();
        let mut doc = fixture_doc();
        let src = source();
        let plan = pdf_plan(&src, &doc).unwrap();
        doc.source_id = src.id;
        doc.doc_key = plan.doc_key;
        let rows = to_rows(
            &doc,
            &ChunkCfg {
                chunk_tokens: 8,
                overlap_tokens: 0,
                context_prefix: false,
                ..Default::default()
            },
            false,
            "canonical.json",
            &Default::default(),
        );
        assert!(rows
            .iter()
            .all(|r| matches!(r["kind"].as_str(), Some("article" | "text" | "media"))));
        assert_eq!(rows.iter().filter(|r| r["kind"] == "media").count(), 2);
        assert!(doc
            .blocks
            .iter()
            .all(|b| matches!(b.kind, BlockKind::Paragraph)));
        let text: Vec<_> = rows.iter().filter(|r| r["kind"] == "text").collect();
        assert!(text.len() > 2);
        for (index, page) in expected.iter().enumerate() {
            let number = index + 1;
            let id = format!("pdf-page-{number:04}");
            let body = page.text.trim();
            assert_eq!(doc.blocks[index].text, page.text);
            assert_eq!(doc.blocks[index].page, Some(number as u32));
            assert_eq!(
                doc.search_metadata["physical_pages"][index]["exact_text_sha256"],
                sha(page.text.as_bytes())
            );
            let scoped: Vec<_> = text
                .iter()
                .filter(|r| r["locator"]["page"] == number)
                .collect();
            assert!(!scoped.is_empty());
            let mut covered = vec![false; body.chars().count()];
            for row in scoped {
                assert_eq!(row["locator"]["page_end"], number);
                assert_eq!(row["locator"]["block_ids"], json!([id]));
                assert_eq!(row["doc_id"], doc.doc_id());
                assert_eq!(row["article_id"], doc.doc_id());
                let start = row["locator"]["char_start"].as_u64().unwrap() as usize;
                let end = row["locator"]["char_end"].as_u64().unwrap() as usize;
                assert_eq!(
                    row["text"],
                    body.chars()
                        .skip(start)
                        .take(end - start)
                        .collect::<String>()
                );
                for seen in &mut covered[start..end] {
                    *seen = true;
                }
            }
            assert!(body
                .chars()
                .zip(covered)
                .all(|(c, seen)| c.is_whitespace() || seen));
        }
        assert_eq!(doc.search_metadata["table_count"], Value::Null);
        assert_eq!(doc.search_metadata["figure_ownership_verified"], false);
        assert_eq!(doc.search_metadata["footnote_ownership_verified"], false);
    }

    #[test]
    fn indexed_pdf_reader_rejects_missing_middle_tail_blank_and_overbound_pages() {
        for failure in [1, 2] {
            let mut visited = vec![];
            let result = collect_pdf_pages(3, |index| {
                visited.push(index);
                ensure!(index != failure, "synthetic page-load failure");
                Ok(pages().remove(0))
            });
            assert!(result.is_err());
            assert_eq!(visited, (0..=failure).collect::<Vec<_>>());
        }
        let mut visited = vec![];
        assert_eq!(
            collect_pdf_pages(3, |index| {
                visited.push(index);
                Ok(pages().remove(0))
            })
            .unwrap()
            .len(),
            3
        );
        assert_eq!(visited, [0, 1, 2]);
        assert!(collect_pdf_pages(0, |_| unreachable!()).is_err());
        assert!(collect_pdf_pages(PDF_PAGE_CAP + 1, |_| unreachable!()).is_err());
        let raw = serde_json::to_vec(&native(PDF)).unwrap();
        let meta = metadata(&raw).unwrap();
        let jats = content(XML.as_bytes(), &meta).unwrap();
        for invalid in ["blank", "dimensions", "page_bytes", "total_bytes"] {
            let mut values = pages();
            match invalid {
                "blank" => values[1].text = " \r\n ".into(),
                "dimensions" => values[1].height_points = f32::NAN,
                "page_bytes" => values[1].text = "x".repeat(PDF_PAGE_TEXT_CAP + 1),
                _ => {
                    let mut page = values.remove(0);
                    page.text
                        .push_str(&"x".repeat(PDF_PAGE_TEXT_CAP - page.text.len()));
                    values = vec![page; 17];
                }
            }
            assert!(
                pdf_document(
                    &meta,
                    &jats,
                    &sha(&raw),
                    &sha(XML.as_bytes()),
                    &sha(PDF),
                    PDF.len(),
                    values
                )
                .is_err(),
                "{invalid}"
            );
        }
        assert!(pdf_pages(b"<html>PDF download wrapper</html>").is_err());
        let mut bytes = b"%PDF-1.4\n".to_vec();
        bytes.resize(FILE_CAP + 1, 0);
        assert!(pdf_pages(&bytes).is_err());
    }

    #[test]
    fn pdf_snapshot_binds_all_source_evidence_and_plan_identity_while_excluding_fetch_time() {
        let src = source();
        let original = fixture_doc();
        let row = pdf_plan(&src, &original).unwrap();
        validate_pdf_plan(&src, &row, &original).unwrap();
        let before = pdf_snapshot(&original).unwrap();
        let mut changed = original.clone();
        changed.fetches.push(Default::default());
        assert_eq!(before, pdf_snapshot(&changed).unwrap());
        for field in [
            "source_id",
            "doc_key",
            "url",
            "title",
            "rendition",
            "version",
            "work_key",
            "pdf_snapshot_sha256",
        ] {
            let mut bad = serde_json::to_value(&row).unwrap();
            if matches!(field, "work_key" | "pdf_snapshot_sha256") {
                bad["extra"][field] = json!("wrong");
            } else {
                bad[field] = json!("wrong");
            }
            assert!(
                validate_pdf_plan(&src, &serde_json::from_value(bad).unwrap(), &original).is_err(),
                "{field}"
            );
        }
        for field in ["bytes", "page", "rights", "metadata", "JATS_rights"] {
            let mut changed = original.clone();
            match field {
                "bytes" => changed.search_metadata["native_pdf"]["sha256"] = json!("a".repeat(64)),
                "page" => changed.blocks[1].text.push_str(" changed"),
                "rights" => changed.rights.status = "restricted".into(),
                "metadata" => {
                    changed.search_metadata["source_metadata_sha256"] = json!("b".repeat(64))
                }
                _ => {
                    changed.attachments[1]["JATS_article_rights"]["note"] =
                        json!("changed source notice")
                }
            }
            assert!(validate_pdf_plan(&src, &row, &changed).is_err(), "{field}");
        }
    }

    #[test]
    fn genuine_synthetic_pdf_decodes_all_indexed_pages_without_semantic_promotion() {
        let extracted = pdf_pages(PDF).unwrap();
        assert_eq!(extracted.len(), 2);
        assert!(pdf_first_page_doi(&extracted[0].text));
        assert!(extracted[1]
            .text
            .contains("Synthetic patient final tail 2042"));
        let raw = serde_json::to_vec(&native(PDF)).unwrap();
        let meta = metadata(&raw).unwrap();
        let jats = content(XML.as_bytes(), &meta).unwrap();
        let doc = pdf_document(
            &meta,
            &jats,
            &sha(&raw),
            &sha(XML.as_bytes()),
            &sha(PDF),
            PDF.len(),
            extracted,
        )
        .unwrap();
        assert_eq!(doc.blocks.len(), 2);
        assert!(doc
            .blocks
            .iter()
            .all(|b| matches!(b.kind, BlockKind::Paragraph)));
        assert_eq!(
            doc.search_metadata["native_first_page_identity_observations"]
                ["expected_DOI_literal_present"],
            true
        );
    }

    #[tokio::test]
    async fn distinct_pdf_discovery_and_revalidation_keep_original_JATS_plan_and_extract_identical()
    {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, FILE_CAP as u64, "").unwrap();
        let src = source();
        let value = native(PDF);
        seed(&f, &value, None);
        assert!(EaslPmcCloud
            .discover(&src, &f, Some(0))
            .await
            .unwrap()
            .is_empty());
        let first = EaslPmcCloud.discover(&src, &f, Some(1)).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].rendition.as_deref(), Some("xml"));
        let old_doc = EaslPmcCloud.extract(&src, &f, &first[0]).await.unwrap();
        let missing = EaslPmcCloud.discover(&src, &f, Some(2)).await.unwrap();
        assert_eq!(missing.len(), 2);
        assert!(missing[1].extra["discovery_error"].is_string());
        assert_eq!(missing[1].rendition.as_deref(), Some("pdf"));
        assert!(EaslPmcCloud.extract(&src, &f, &missing[1]).await.is_err());
        seed(&f, &value, Some(PDF));
        let rows = EaslPmcCloud.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            serde_json::to_value(&rows[0]).unwrap(),
            serde_json::to_value(&first[0]).unwrap()
        );
        assert_eq!(
            serde_json::to_vec(&EaslPmcCloud.extract(&src, &f, &rows[0]).await.unwrap()).unwrap(),
            serde_json::to_vec(&old_doc).unwrap()
        );
        assert_eq!(old_doc.search_metadata["main_pdf_acquired"], false);
        let doc = EaslPmcCloud.extract(&src, &f, &rows[1]).await.unwrap();
        assert_eq!(doc.source_id, src.id);
        assert_eq!(doc.doc_key, rows[1].doc_key);
        assert_eq!(doc.work_key, old_doc.work_key);
        assert_ne!(doc.doc_key, old_doc.doc_key);
        assert_eq!(doc.fetches.len(), 3);
        assert_eq!(doc.fetches[0].url, META);
        assert_eq!(doc.search_metadata["main_pdf_acquired"], true);
        let mut changed = PDF.to_vec();
        changed.extend_from_slice(b"\n% new source bytes\n");
        let meta = metadata(&serde_json::to_vec(&value).unwrap()).unwrap();
        f.seed_cache_for_tests(&meta.pdf.url, &changed, "binary/octet-stream");
        assert!(EaslPmcCloud.extract(&src, &f, &rows[1]).await.is_err());
        let updated = native(&changed);
        seed(&f, &updated, Some(&changed));
        assert!(EaslPmcCloud.extract(&src, &f, &rows[1]).await.is_err());
        let refreshed = EaslPmcCloud.discover(&src, &f, Some(2)).await.unwrap();
        assert_ne!(rows[1].doc_key, refreshed[1].doc_key);
        assert_eq!(rows[1].extra["work_key"], refreshed[1].extra["work_key"]);
    }

    /// Fix group H2: the registry's Europe PMC enumeration adds other EASL OA documents after the two Cloud rows,
    /// never re-planning PMC8514420, and those rows are read through the shared crosswalk.
    #[tokio::test]
    async fn oa_enumeration_follows_cloud_rows_and_skips_the_cloud_expression() {
        let dir = tempfile::tempdir().unwrap();
        let f = Fetcher::new(dir.path(), true, 500, FILE_CAP as u64, "").unwrap();
        let mut src = source();
        let q = "(TITLE:\"EASL\") AND (OPEN_ACCESS:y)";
        src.config["oa_enumeration"] = json!({"query": q, "max": 10, "item_type": "clinical_practice_guideline", "scope": "test"});
        seed(&f, &native(PDF), Some(PDF));
        let answer = json!({"hitCount": 2, "nextCursorMark": "*", "resultList": {"result": [
            {"id":"34712929","source":"MED","pmcid":PMCID,"doi":"10.1016/j.jhepr.2021.100322","title":"Non-alcoholic fatty liver disease: A patient guideline","isOpenAccess":"Y","inPMC":"Y","firstPublicationDate":"2021-08-01"},
            {"id":"35647308","source":"MED","pmid":"35647308","pmcid":"PMC9134774","doi":"10.1177/23969873221078696","title":"stand-in OA guideline","isOpenAccess":"Y","inPMC":"Y","firstPublicationDate":"2022-03-29"}]}});
        f.seed_cache_for_tests(&super::super::pmc_crosswalk::epmc_search_url(q, 10, "*"), answer.to_string().as_bytes(), "application/json");
        let oai = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/pmc_crosswalk/oai-PMC9134774.xml"));
        f.seed_cache_for_tests(&super::super::pmc_crosswalk::oai_url("PMC9134774").unwrap(), oai.as_bytes(), "application/xml");
        let rows = EaslPmcCloud.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].doc_key.starts_with("pmc8514420-processing1-") && rows[1].rendition.as_deref() == Some("pdf"));
        assert!(rows[2].doc_key.starts_with("easl-oa-pmc9134774-"));
        assert_eq!(EaslPmcCloud.discover(&src, &f, Some(2)).await.unwrap().len(), 2);
        let d = EaslPmcCloud.extract(&src, &f, &rows[2]).await.unwrap();
        assert!(d.relations.iter().any(|r| r.rel == "rendition_of" && r.target == "pmc:PMC9134774"));
        assert_eq!(d.item_type.as_deref(), Some("clinical_practice_guideline"));
    }
}
