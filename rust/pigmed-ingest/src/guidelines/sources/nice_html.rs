//! Public source-linked NICE HTML. Separate from the licensed/API source entry.
//! No API, credentials, guessed pages, synthesized chapter URLs or gate bypass.
use super::{common, Adapter, PlanRow};
use crate::guidelines::{
    doc::{Block, BlockKind, DateObservation, GuidelineDoc, Relation, Rights},
    fetch::Fetcher,
    html::{blocks_from_html, sel, text_of},
    registry::SourceEntry,
};
use anyhow::{bail, Context, Result};
use scraper::{ElementRef, Html, Node};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use url::Url;

pub struct NiceHtml;
const CATALOG: &str = "https://www.nice.org.uk/guidance/published?sp=on";
fn norm(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn raw_text(e: ElementRef<'_>) -> String {
    e.text().collect()
}
fn node_key(e: ElementRef<'_>) -> String {
    format!("{:?}", e.id())
}
fn class(e: ElementRef<'_>, name: &str) -> bool {
    e.value().classes().any(|c| c == name)
}
fn first_party(u: &Url) -> bool {
    u.scheme() == "https"
        && u.host_str() == Some("www.nice.org.uk")
        && u.username().is_empty()
        && u.password().is_none()
        && u.port_or_known_default() == Some(443)
}
fn product_id(u: &Url) -> Option<String> {
    if !first_party(u) || u.query().is_some() {
        return None;
    }
    let re = regex::Regex::new(r"(?i)^/guidance/(?:terminated/)?([a-z]+\d+)/?$").unwrap();
    re.captures(u.path()).map(|c| c[1].to_ascii_lowercase())
}
fn chapter_product(u: &Url) -> Option<String> {
    if !first_party(u) || u.query().is_some() {
        return None;
    }
    regex::Regex::new(r"(?i)^/guidance/(?:terminated/)?([a-z]+\d+)/chapter/[^/]+/?$")
        .unwrap()
        .captures(u.path())
        .map(|c| c[1].to_ascii_lowercase())
}
fn catalog_route(u: &Url) -> bool {
    first_party(u) && u.path() == "/guidance/published"
}
fn route_key(raw: &str) -> String {
    let Ok(u) = Url::parse(raw) else {
        return raw.into();
    };
    format!(
        "{}{}?{}",
        u.origin().ascii_serialization(),
        u.path().to_ascii_lowercase(),
        u.query().unwrap_or_default()
    )
}
fn gate(raw: &str) -> Result<Html> {
    let d = Html::parse_document(raw);
    let text = norm(&raw_text(d.root_element())).to_lowercase();
    if [
        "temporary service interruption",
        "0x8133",
        "performing security verification",
        "verify you are human",
        "access denied",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        bail!("NICE source access/service gate; not an empty terminal");
    }
    Ok(d)
}
fn source_link(base: &Url, href: &str) -> Option<String> {
    common::link(base, href).filter(|s| Url::parse(s).ok().is_some_and(|u| first_party(&u)))
}
fn links_in(e: ElementRef<'_>, base: &Url) -> Vec<Value> {
    e.select(&sel("a[href]"))
        .filter_map(|a| {
            let href = a.value().attr("href")?;
            let u = base.join(href).ok()?;
            if !matches!(u.scheme(), "http" | "https")
                || !u.username().is_empty()
                || u.password().is_some()
            {
                return None;
            }
            Some(json!({"href":href,"url":u.as_str(),"label":text_of(&a)}))
        })
        .collect()
}
fn source_date(label: &str, raw: &str, url: &str) -> DateObservation {
    let mut d = common::date(label, raw, url);
    let re=regex::Regex::new(r"^(\d{1,2})\s+(January|February|March|April|May|June|July|August|September|October|November|December)\s+((?:19|20)\d{2})$").unwrap();
    if let Some(c) = re.captures(&d.raw) {
        d.normalized =
            common::date(label, &format!("{} {}, {}", &c[2], &c[1], &c[3]), url).normalized;
    }
    d
}
fn date_fields(main: ElementRef<'_>, base: &Url) -> (Vec<DateObservation>, Vec<Value>) {
    let labels =
        regex::Regex::new(r"(?i)^(Published|Last updated|Last reviewed|Terminated):?$").unwrap();
    let line =
        regex::Regex::new(r"(?i)^(Published|Last updated|Last reviewed|Terminated):\s*(.+)$")
            .unwrap();
    let mut dates = vec![];
    let mut evidence = vec![];
    let mut seen = HashSet::new();
    let mut add = |label: String, raw: String, e: ElementRef<'_>| {
        if !raw.is_empty() && seen.insert((label.to_lowercase(), raw.clone())) {
            dates.push(source_date(&label, &raw, base.as_str()));
            evidence.push(json!({"label":label,"raw":raw,"source_id":e.value().id(),"source_html":e.html(),"scope":"source product header/date field; no inferred chapter revision date"}));
        }
    };
    for dt in main.select(&sel("dt")) {
        let label = text_of(&dt);
        if !labels.is_match(&label) {
            continue;
        }
        if let Some(dd) = dt
            .next_siblings()
            .filter_map(ElementRef::wrap)
            .next()
            .filter(|e| e.value().name() == "dd")
        {
            add(label.trim_end_matches(':').into(), text_of(&dd), dd);
        }
    }
    // The observed terminated-appraisal header uses LI + TIME, on both the
    // product overview and advice chapter. A URL/title/company withdrawal alone
    // is not a termination event, and a Published label must never be relabelled.
    let terminated_title = regex::Regex::new(r"(?i)\(\s*terminated\s+appraisal\s*\)").unwrap();
    let terminated_appraisal_header = main
        .select(&sel("h1.page-header__heading--alt"))
        .any(|h| terminated_title.is_match(&text_of(&h)));
    if terminated_appraisal_header {
        for li in main.select(&sel("li")) {
            let label = li
                .children()
                .filter_map(|n| match n.value() {
                    Node::Text(t) => Some(t.to_string()),
                    _ => None,
                })
                .collect::<String>();
            let children = li
                .children()
                .filter_map(ElementRef::wrap)
                .collect::<Vec<_>>();
            if label.trim() == "Terminated:"
                && children.len() == 1
                && children[0].value().name() == "time"
                && children[0].value().attr("datetime").is_some()
            {
                let raw = text_of(&children[0]);
                if !raw.is_empty() {
                    add("Terminated".into(), raw, li);
                }
            }
        }
    }
    for p in main.select(&sel("p")) {
        let text = text_of(&p);
        if let Some(c) = line.captures(&text) {
            let raw = c[2].trim();
            if regex::Regex::new(r"^(?:\d{1,2}\s+[A-Za-z]+\s+\d{4}|\d{4}-\d{2}-\d{2})$")
                .unwrap()
                .is_match(raw)
            {
                add(c[1].into(), raw.into(), p);
            }
        }
    }
    (dates, evidence)
}
fn chapter_links(main: ElementRef<'_>, base: &Url, id: &str) -> Vec<Value> {
    let mut out = vec![];
    let mut seen = HashSet::new();
    for a in main.select(&sel("a[href]")) {
        let Some(url) = a.value().attr("href").and_then(|h| source_link(base, h)) else {
            continue;
        };
        let Ok(u) = Url::parse(&url) else {
            continue;
        };
        if chapter_product(&u).as_deref() != Some(id) || !seen.insert(route_key(&url)) {
            continue;
        }
        let raw_label = text_of(&a);
        let lower = raw_label.to_lowercase();
        let control = if lower.starts_with("next page") {
            Some("next")
        } else if lower.starts_with("previous page") {
            Some("previous")
        } else {
            None
        };
        let label = match control {
            Some("next") => raw_label["next page".len()..].trim().to_string(),
            Some("previous") => raw_label["previous page".len()..].trim().to_string(),
            _ => raw_label.clone(),
        };
        out.push(json!({"url":url,"label":label,"raw_link_label":raw_label,"source_navigation_control":control,"source_page_url":base.as_str(),"source_anchor_html":a.html(),"source_rel":a.value().attr("rel"),"source_aria_label":a.value().attr("aria-label")}));
    }
    out
}
fn asset_links(main: ElementRef<'_>, base: &Url) -> Vec<Value> {
    let mut out = vec![];
    let mut seen = HashSet::new();
    for a in main.select(&sel("a[href]")) {
        let Some(url) = a.value().attr("href").and_then(|h| common::link(base, h)) else {
            continue;
        };
        let Ok(u) = Url::parse(&url) else {
            continue;
        };
        let label = text_of(&a);
        let low = label.to_lowercase();
        let path = u.path().to_lowercase();
        let pdf = path.ends_with(".pdf")
            || low.contains("pdf")
            || a.value().attr("type") == Some("application/pdf");
        let resource_role = if pdf {
            "source_linked_asset_descriptor"
        } else if path.ends_with("/evidence") {
            "source_linked_evidence_index"
        } else if path.ends_with("/history") {
            "source_linked_version_history"
        } else {
            continue;
        };
        if u.query().is_some() || !seen.insert(url.clone()) {
            continue;
        }
        out.push(json!({"url":url,"label":label,"role":resource_role,"bytes_verified":false,"rights":"unknown; API/public HTML visibility is not a reuse grant"}));
    }
    out
}
fn state(
    main: ElementRef<'_>,
    base: &Url,
    dates: &[DateObservation],
) -> (String, Vec<Relation>, Vec<Value>) {
    let mut status = "unknown".to_string();
    let mut relations = vec![];
    let mut evidence = vec![];
    let id = product_id(base).or_else(|| chapter_product(base));
    for p in main.select(&sel("p")) {
        let raw = text_of(&p);
        let low = raw.to_lowercase();
        if low.contains("updated and replaced") && low.contains("guidance") {
            status = "updated and replaced".into();
            let mut targets = HashSet::new();
            for a in p.select(&sel("a[href]")) {
                let Some(target) = a.value().attr("href").and_then(|h| source_link(base, h)) else {
                    continue;
                };
                let Ok(u) = Url::parse(&target) else {
                    continue;
                };
                if product_id(&u).is_some_and(|v| Some(v) != id) && targets.insert(target.clone()) {
                    relations.push(Relation {
                        rel: "superseded_by".into(),
                        target,
                        scope: Some(raw.clone()),
                        evidence_url: Some(base.to_string()),
                    });
                }
            }
            evidence.push(json!({"raw":raw,"source_html":p.html(),"scope":"guidance product","linked_replacement_verified":!relations.is_empty()}));
        } else if regex::Regex::new(r"(?i)^This guidance (?:has been|was) withdrawn\b")
            .unwrap()
            .is_match(&raw)
        {
            status = "withdrawn".into();
            evidence.push(json!({"raw":raw,"source_html":p.html(),"scope":"guidance product"}));
        } else if low.contains("replaces") || low.contains("withdraw") {
            evidence.push(json!({"raw":raw,"source_html":p.html(),"role":"source_statement_only; no inferred target/state/edge"}));
        }
    }
    if dates
        .iter()
        .any(|d| d.label.eq_ignore_ascii_case("Terminated"))
    {
        status = "terminated".into();
        evidence.push(json!({"event":"product_termination","scope":"guidance product; not company withdrawal or inferred chapter revision","source_dates":dates.iter().filter(|d|d.label.eq_ignore_ascii_case("Terminated")).collect::<Vec<_>>(),"inferred_from_url_or_title":false}));
    }
    (status, relations, evidence)
}
fn source_rights(url: &str) -> Rights {
    Rights{status:"restricted".into(),license:None,terms_url:Some("https://www.nice.org.uk/reusing-our-content/nice-uk-open-content-licence".into()),
        restrictions:vec!["Observed NICE UK open content licence excludes automated website scraping/harvesting and organisational AI use outside the separately licensed syndication service; no item-specific grant or separate permission is established here.".into(),"Observed syndication terms require API access for AI end-user systems and exclude training, fine-tuning or weighting generative AI/large language models. Personal research, third-party material and product/international use have distinct conditions.".into()],
        note:Some("Source-wide restrictions are retained from the 2026-11-14 browser observation, not a newly fetched licence/entitlement check; applicability of separate user permission and individual asset rights remains unverified. No API credential or licence entitlement is inferred.".into()),
        evidence_urls:vec![url.into(),"https://www.nice.org.uk/reusing-our-content/nice-uk-open-content-licence".into(),"https://www.nice.org.uk/reusing-our-content/nice-syndication-api".into()],..Default::default()}
}
fn snapshot(doc: &mut GuidelineDoc) -> Result<String> {
    let hash = crate::common::sha256_hex(&serde_json::to_vec(
        &json!({"title":doc.title,"item_type":doc.item_type,"status":doc.status,"dates":doc.date_observations,"relations":doc.relations,"rights":doc.rights,"blocks":doc.blocks,"attachments":doc.attachments,"metadata":doc.search_metadata}),
    )?);
    doc.search_metadata["source_snapshot_sha256"] = json!(hash);
    Ok(hash)
}
fn overview(raw: &str, base: &Url) -> Result<GuidelineDoc> {
    let id = product_id(base).context("NICE source product URL is unrecognized")?;
    let d = gate(raw)?;
    let main = d
        .select(&sel("main"))
        .next()
        .context("NICE product main root missing")?;
    let title = main
        .select(&sel("h1"))
        .next()
        .map(|e| text_of(&e))
        .filter(|s| !s.is_empty())
        .context("NICE printed product title missing")?;
    let (dates, date_evidence) = date_fields(main, base);
    let (status, relations, state_evidence) = state(main, base, &dates);
    let chapters = chapter_links(main, base, &id);
    let assets = asset_links(main, base);
    let identity_pattern = format!(r"(?i)\b{}\b", regex::escape(&id));
    if !regex::Regex::new(&identity_pattern)?.is_match(&text_of(&main))
        || (dates.is_empty() && chapters.is_empty() && status == "unknown")
    {
        bail!("NICE product lacks native reference/date/navigation/state identity structure; no shell fallback");
    }

    let types = [
        "NICE guideline",
        "HealthTech guidance",
        "Technology appraisal guidance",
        "Technology appraisal",
        "Clinical guideline",
        "Public health guideline",
        "Quality standard",
        "Diagnostics guidance",
        "Medical technologies guidance",
        "Highly specialised technologies guidance",
    ];
    let mut native_types = HashSet::new();
    for p in main.select(&sel("p")) {
        let raw = text_of(&p);
        for t in types {
            let rest = raw.strip_prefix(t).unwrap_or("not-a-native-type").trim();
            if raw == t
                || (raw.starts_with(t)
                    && rest.trim_matches(&['[', ']'][..]).eq_ignore_ascii_case(&id))
            {
                native_types.insert(t.to_string());
            }
        }
    }
    let native_type =
        (native_types.len() == 1).then(|| native_types.iter().next().unwrap().clone());
    let (blocks, mut warnings) = blocks_from_html(&main.html(), Some(base), Some("main"));
    warnings.push("NICE product overview/state notice is not the full recommendation chapters. Public HTML is separate from the licensed API; access does not establish reuse rights or complete catalog coverage.".into());
    let item = if status == "updated and replaced" {
        "guidance_replacement_notice"
    } else if status == "terminated" {
        "terminated_guidance_product"
    } else {
        "guidance_product_overview"
    };
    let published = dates
        .iter()
        .filter(|d| d.label.eq_ignore_ascii_case("Published"))
        .collect::<Vec<_>>();
    let pubdate = (published.len() == 1)
        .then(|| published[0].normalized.clone())
        .flatten();
    let updated_dates = dates
        .iter()
        .filter(|d| d.label.eq_ignore_ascii_case("Last updated"))
        .collect::<Vec<_>>();
    let updated = (updated_dates.len() == 1)
        .then(|| updated_dates[0].normalized.clone())
        .flatten();
    let mut doc = GuidelineDoc {
        title,
        url: base.to_string(),
        work_key: Some(id.clone()),
        item_type: Some(item.into()),
        status,
        published: pubdate,
        updated,
        date_observations: dates,
        relations,
        rights: source_rights(base.as_str()),
        blocks,
        attachments: assets,
        search_metadata: json!({"source_product_id":id,"source_product_type":native_type,"source_date_evidence":date_evidence,"source_state_evidence":state_evidence,"source_chapter_links":chapters,"source_main_sha256":crate::common::sha256_hex(main.html().as_bytes()),"coverage":"Source product overview and observed href graph only; no full catalog/archive/state inventory claim"}),
        warnings,
        ..Default::default()
    };
    snapshot(&mut doc)?;
    Ok(doc)
}
struct Render {
    ids: HashMap<String, String>,
    used: HashSet<String>,
}
impl Render {
    fn write(&mut self, e: ElementRef<'_>, out: &mut String) {
        if class(e, "recommendation__number")
            || class(e, "research-recommendation__number")
            || matches!(
                e.value().name(),
                "script" | "style" | "nav" | "button" | "form" | "template"
            )
        {
            return;
        }
        fn esc(s: &str) -> String {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        }
        let mut id = e
            .value()
            .id()
            .map(String::from)
            .unwrap_or_else(|| format!("nice-node-{}", self.ids.len() + 1));
        if !self.used.insert(id.clone()) {
            id = format!("{id}-nice-node-{}", self.ids.len() + 1);
            self.used.insert(id.clone());
        }
        self.ids.insert(node_key(e), id.clone());
        let tag = if e.value().attr("role") == Some("doc-footnote") {
            "aside"
        } else {
            e.value().name()
        };
        out.push('<');
        out.push_str(tag);
        out.push_str(" id=\"");
        out.push_str(&esc(&id));
        out.push('"');
        for (name, value) in e.value().attrs() {
            if name != "id" {
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                out.push_str(&esc(value));
                out.push('"');
            }
        }
        out.push('>');
        for child in e.children() {
            match child.value() {
                Node::Text(t) => out.push_str(&esc(t)),
                Node::Element(_) => {
                    if let Some(c) = ElementRef::wrap(child) {
                        self.write(c, out)
                    }
                }
                _ => {}
            }
        }
        if !matches!(
            tag,
            "img" | "br" | "hr" | "input" | "source" | "wbr" | "meta" | "link"
        ) {
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
}
fn chapter(raw: &str, base: &Url, parent: &GuidelineDoc, label: &str) -> Result<GuidelineDoc> {
    let id = chapter_product(base).context("NICE source chapter route invalid")?;
    if parent.search_metadata["source_product_id"] != id {
        bail!("NICE chapter/parent identity differs");
    }
    let d = gate(raw)?;
    let main = d
        .select(&sel("main"))
        .next()
        .context("NICE chapter main missing")?;
    let root = main
        .select(&sel("#ng245-recommendations-for-research"))
        .next()
        .or_else(|| main.select(&sel(".chapter")).next())
        .context("NICE source clinical chapter root absent; no main/body fallback")?;
    let research_only = label
        .to_lowercase()
        .contains("recommendations for research")
        || root.value().id() == Some("ng245-recommendations-for-research");
    let mut render = Render {
        ids: HashMap::new(),
        used: HashSet::new(),
    };
    let mut html = String::new();
    render.write(root, &mut html);
    let (mut blocks, mut warnings) = blocks_from_html(&html, Some(base), None);
    // Restore decoded inline adjacency for native leaf prose (e.g. 5<sup>th</sup>)
    // while retaining the shared walker's block/list boundaries and source IDs.
    for element in std::iter::once(root).chain(root.descendants().filter_map(ElementRef::wrap)) {
        let leaf = matches!(element.value().name(), "p" | "li" | "aside")
            && !element
                .descendants()
                .filter(|node| node.id() != element.id())
                .filter_map(ElementRef::wrap)
                .any(|e| {
                    matches!(
                        e.value().name(),
                        "p" | "div" | "ul" | "ol" | "table" | "figure" | "img"
                    )
                });
        if leaf {
            if let Some(id) = render.ids.get(&node_key(element)) {
                if let Some(block) = blocks.iter_mut().find(|b| {
                    &b.block_id == id
                        && matches!(
                            b.kind,
                            BlockKind::Paragraph | BlockKind::ListItem | BlockKind::Footnote
                        )
                }) {
                    block.text = norm(&raw_text(element));
                }
            }
        }
    }
    let mut native = vec![];
    for a in root.select(&sel("article.numbered-paragraph.recommendation,article.numbered-paragraph.research-recommendation")){
        let research=class(a,"research-recommendation");let source_id=a.value().id().context("NICE numbered statement native ID missing")?;
        let body=a.select(&sel(if research{".research-recommendation__body"}else{".recommendation__body"})).next().context("NICE numbered statement source body missing")?;
        let number=a.select(&sel(".recommendation__number,.research-recommendation__number")).next().map(|n|text_of(&n)).filter(|s|!s.is_empty());if !research&&number.is_none(){bail!("NICE clinical source number absent");}
        let ids=std::iter::once(body).chain(body.descendants().filter_map(ElementRef::wrap)).filter_map(|e|render.ids.get(&node_key(e)).cloned()).collect::<HashSet<_>>();
        let positions=blocks.iter().enumerate().filter(|(_,b)|ids.contains(&b.block_id)&&matches!(b.kind,BlockKind::Paragraph|BlockKind::ListItem|BlockKind::Footnote)).map(|(i,_)|i).collect::<Vec<_>>();if positions.is_empty(){bail!("NICE numbered source statement has no preserved prose/list blocks");}
        let source_blocks=blocks.iter().filter(|b|ids.contains(&b.block_id)).cloned().collect::<Vec<_>>();let statement=positions.iter().map(|i|blocks[*i].text.clone()).collect::<Vec<_>>().join("\n\n");
        if blocks.iter().enumerate().any(|(i,b)|b.block_id==source_id&&i!=positions[0]){bail!("NICE source statement ID collides with another canonical block");}
        let first=positions[0];blocks[first].block_id=source_id.into();blocks[first].text=statement.clone();blocks[first].kind=BlockKind::Recommendation{label:number.clone(),strength:None,certainty:None,population:None,grading_system:None,qualifier:research.then(||"research-recommendation".into())};for position in positions.iter().skip(1).rev(){blocks.remove(*position);}
        native.push(json!({"block_id":source_id,"source_id":source_id,"source_number":number,"source_role":if research{"research_recommendation"}else{"clinical_recommendation"},"source_qualifier_evidence":research.then_some("article class=research-recommendation"),"source_html":a.html(),"source_body_html":body.html(),"source_body_blocks":source_blocks,"source_links":links_in(body,base),"statement":statement,"grade_semantics":"No strength/certainty inferred from NICE wording; source role is distinct from clinical grade"}));
    }
    let research_sections = if research_only {
        root.children().filter_map(ElementRef::wrap).filter(|e|class(*e,"section")).map(|s|json!({"source_id":s.value().id(),"source_html":s.html(),"source_role":"research_section_without_numbered_article"})).collect::<Vec<_>>()
    } else {
        vec![]
    };
    if research_only && native.is_empty() && research_sections.is_empty() {
        bail!("NICE research chapter has no observed source section boundary");
    }
    if blocks.is_empty() {
        bail!("NICE chapter yielded no clinical/context blocks");
    }
    let mut tables = vec![];
    for t in root.select(&sel("table")) {
        let canonical = render
            .ids
            .get(&node_key(t))
            .context("NICE native table map missing")?;
        let rows=t.select(&sel("tr")).filter(|r|r.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().name()=="table").is_some_and(|a|a.id()==t.id())).map(|r|json!({"source_id":r.value().id(),"section":r.ancestors().filter_map(ElementRef::wrap).find(|e|matches!(e.value().name(),"thead"|"tbody"|"tfoot")).map(|e|e.value().name()),"cells":common::cells(r).into_iter().map(|c|json!({"tag":c.value().name(),"source_id":c.value().id(),"text":raw_text(c),"rowspan":c.value().attr("rowspan"),"colspan":c.value().attr("colspan"),"headers":c.value().attr("headers"),"scope":c.value().attr("scope"),"source_html":c.html(),"source_links":links_in(c,base)})).collect::<Vec<_>>()})).collect::<Vec<_>>();
        let canonical_present = blocks
            .iter()
            .any(|b| &b.block_id == canonical && matches!(b.kind, BlockKind::Table { .. }));
        tables.push(json!({"block_id":if canonical_present {Some(canonical)}else{None},"source_node_id":canonical,"canonical_table_emitted":canonical_present,"source_id":t.value().id(),"caption":t.select(&sel("caption")).next().map(|c|text_of(&c)),"source_html":t.html(),"rows":rows,"association":"Source order retained; sibling contextual paragraphs are not inferred table footnotes or owned recommendations"}));
    }
    let figures=root.select(&sel("figure,img")).map(|e|json!({"source_id":e.value().id(),"source_tag":e.value().name(),"canonical_source_node_id":render.ids.get(&node_key(e)),"source_html":e.html()})).collect::<Vec<_>>();
    let own_dates = date_fields(main, base);
    let termination_dates = own_dates
        .0
        .iter()
        .filter(|d| d.label.eq_ignore_ascii_case("Terminated"))
        .cloned()
        .collect::<Vec<_>>();
    let chapter_status = if termination_dates.is_empty() {
        "unknown"
    } else {
        "terminated"
    };
    let links = chapter_links(main, base, &id);
    let has_next = links
        .iter()
        .any(|l| l["source_navigation_control"] == "next" || l["source_rel"] == "next");
    let has_previous = main
        .select(&sel("a[href]"))
        .any(|a| text_of(&a).to_lowercase().starts_with("previous page"));
    warnings.push("NICE source numbered statements retain ordered native prose/list blocks, IDs and links; tables/figures remain separate in canonical order. Grouped statement source_body_blocks preserve native structure when a statement spans a visual object. Research classes/sections are explicit source roles, not inferred clinical strength. No catalog/archive/rights/visual-fidelity acceptance.".into());
    let title = if label.trim().is_empty() {
        main.select(&sel("h1"))
            .next()
            .map(|e| text_of(&e))
            .context("NICE source chapter title absent")?
    } else {
        format!("{} — {}", parent.title, label)
    };
    let slug = base
        .path()
        .split("/chapter/")
        .nth(1)
        .unwrap_or("")
        .trim_matches('/');
    let mut doc=GuidelineDoc{title,url:base.to_string(),work_key:Some(format!("{}-{}",id,common::slug(slug))),item_type:Some(if research_only{"research_recommendations_chapter"}else{"guideline_chapter"}.into()),status:chapter_status.into(),date_observations:termination_dates,relations:vec![Relation{rel:"part_of".into(),target:parent.url.clone(),scope:Some("Source-linked chapter of this guidance product; product-level state/dates remain separately scoped".into()),evidence_url:Some(parent.url.clone())}],rights:source_rights(base.as_str()),blocks,attachments:asset_links(main,base),search_metadata:json!({"source_product_id":id,"source_product_type":parent.search_metadata["source_product_type"],"source_product_metadata":{"url":parent.url,"snapshot":parent.search_metadata["source_snapshot_sha256"],"status":parent.status,"dates":parent.date_observations,"state_evidence":parent.search_metadata["source_state_evidence"]},"source_chapter_label":label,"source_recommendations":native,"source_research_sections":research_sections,"source_tables":tables,"source_figures":figures,"source_page_date_observations":own_dates.0,"source_page_date_evidence":own_dates.1,"source_chapter_links":links,"source_next_present":has_next,"source_previous_present":has_previous,"source_navigation_end_observation":"Absence of Next is an observation, not proof of full product/catalog completeness","clinical_root_sha256":crate::common::sha256_hex(root.html().as_bytes())}),warnings,..Default::default()};
    if chapter_status == "terminated" {
        doc.search_metadata["source_termination_scope"] = json!({"scope":"guidance product termination explicitly repeated in this chapter header; applies to this advice chapter, not a chapter publication/revision date","evidence_url":base.as_str(),"status_inferred_from_url_or_company_withdrawal":false,"parent_product_status":parent.status});
    }
    snapshot(&mut doc)?;
    Ok(doc)
}
#[derive(Debug)]
struct CatalogPage {
    records: Vec<Value>,
    next: Option<String>,
    terminal: bool,
    signature: String,
}
fn catalog(raw: &str, base: &Url) -> Result<CatalogPage> {
    if !catalog_route(base) {
        bail!("NICE catalog left the observed published route");
    }
    let d = gate(raw)?;
    let main = d
        .select(&sel("main"))
        .next()
        .context("NICE catalog main missing")?;
    let table = main
        .select(&sel("table#results"))
        .next()
        .context("NICE catalog results table absent")?;
    let headers = table
        .select(&sel("thead th"))
        .map(|e| text_of(&e))
        .collect::<Vec<_>>();
    if headers != ["Title", "Reference number", "Published", "Last updated"] {
        bail!("NICE source catalog column contract changed");
    }
    let mut records = vec![];
    let mut seen = HashSet::new();
    for row in table.select(&sel("tbody > tr")) {
        let cells = common::cells(row);
        if cells.len() != 4 {
            bail!("NICE catalog source row cell geometry changed");
        }
        let links = cells[0]
            .select(&sel("a[href]"))
            .filter_map(|a| {
                a.value()
                    .attr("href")
                    .and_then(|h| source_link(base, h))
                    .map(|u| (a, u))
            })
            .filter(|(_, u)| Url::parse(u).ok().and_then(|u| product_id(&u)).is_some())
            .collect::<Vec<_>>();
        if links.len() != 1 {
            bail!("NICE catalog row lacks a unique source product href");
        }
        let (a, url) = &links[0];
        let id = product_id(&Url::parse(url)?).unwrap();
        let reference = text_of(&cells[1]);
        if reference
            .split_whitespace()
            .collect::<String>()
            .to_ascii_lowercase()
            != id
        {
            bail!("NICE catalog native reference disagrees with source href");
        }
        if !seen.insert(id.clone()) {
            bail!("NICE catalog repeated native product identity on one page");
        }
        records.push(json!({"url":url,"title":text_of(a),"reference":reference,"source_published":text_of(&cells[2]),"source_last_updated":text_of(&cells[3]),"source_row_html":row.html(),"source_catalog_url":base.as_str()}));
    }
    if records.is_empty() {
        bail!("NICE catalog zero rows is not a verified empty/end marker");
    }
    let mut next = vec![];
    for a in main.select(&sel("a[href]")) {
        let text = text_of(&a).to_lowercase();
        let next_marker = text.starts_with("next")
            || a.value().attr("rel") == Some("next")
            || a.value()
                .attr("aria-label")
                .is_some_and(|s| s.to_lowercase().starts_with("next"));
        if !next_marker {
            continue;
        }
        let Some(url) = a.value().attr("href").and_then(|h| source_link(base, h)) else {
            continue;
        };
        if catalog_route(&Url::parse(&url)?) {
            next.push(url);
        }
    }
    next.sort();
    next.dedup();
    if next.len() > 1 {
        bail!("NICE catalog ambiguous source Next links");
    }
    let text = text_of(&main);
    let range = regex::Regex::new(
        r"(?i)([0-9][0-9,]*)\s*(?:to|[-–])\s*([0-9][0-9,]*)\s+of\s+([0-9][0-9,]*)",
    )?
    .captures(&text)
    .and_then(|c| {
        let end = c[2].replace(',', "").parse::<usize>().ok()?;
        let total = c[3].replace(',', "").parse::<usize>().ok()?;
        Some(end == total && total > 0)
    })
    .unwrap_or(false);
    let signature = crate::common::sha256_hex(&serde_json::to_vec(
        &records.iter().map(|r| &r["reference"]).collect::<Vec<_>>(),
    )?);
    let terminal = range && next.is_empty();
    Ok(CatalogPage {
        records,
        next: next.pop(),
        terminal,
        signature,
    })
}
/// One page of NICE's "Browse guidance by topic" tree (`/guidance/conditions-and-diseases`, and the other
/// "All categories" roots it links). Branch pages link child topics with `a.menu` / `a.topic` or as plain list links
/// below their own path; a leaf topic page
/// carries a call-to-action "Published and terminated products on this topic (N)" whose href is the topic's
/// `/products` listing with a status filter.
struct TopicPage {
    children: Vec<String>,
    /// (listing route, source-declared count): the CTA href's own path (`.../<topic>/products`) without its filter
    /// query. Every filtered listing (and every query on /guidance/published after page 1) answers this agent with
    /// HTTP 403 "Temporary Service Interruption 0x8133"; the unfiltered path answers 200 and lists the same products
    /// plus in-development records, which are not product overviews and are skipped.
    products: Option<(String, usize)>,
}
fn topic_route(u: &Url) -> bool {
    first_party(u)
        && u.query().is_none()
        && u.path().starts_with("/guidance/")
        && product_id(u).is_none()
        && !u.path().ends_with("/products")
        && !u.path().starts_with("/guidance/published")
}
fn topic_page(raw: &str, base: &Url) -> Result<TopicPage> {
    if !topic_route(base) {
        bail!("NICE topic page left the browse-by-topic routes");
    }
    let d = gate(raw)?;
    let main = d.select(&sel("main")).next().context("NICE topic main missing")?;
    let mut children = vec![];
    let mut seen = HashSet::new();
    let below = format!("{}/", base.path().trim_end_matches('/'));
    for a in main.select(&sel("a[href]")) {
        let Some(url) = source_link(base, a.value().attr("href").unwrap_or_default()) else { continue };
        let u = Url::parse(&url)?;
        // sub-topics of this page (plain list links below its own path) and the tree's own menu/topic links
        let tree_link = u.path().starts_with(&below) || class(a, "menu") || class(a, "topic");
        if tree_link && topic_route(&u) && u.path() != base.path() && seen.insert(u.path().to_string()) {
            children.push(url);
        }
    }
    let mut products = None;
    let count = regex::Regex::new(r"(?i)^published and terminated products on this topic \((\d+)\)$").unwrap();
    for a in main.select(&sel("a.btn--cta[href]")) {
        let label = norm(&text_of(&a));
        let Some(c) = count.captures(&label) else { continue };
        let Some(url) = source_link(base, a.value().attr("href").unwrap_or_default()) else { continue };
        let mut u = Url::parse(&url)?;
        if u.path() != format!("{}/products", base.path().trim_end_matches('/')) {
            bail!("NICE topic products CTA does not point at this topic's products listing");
        }
        u.set_query(None);
        if products.replace((u.to_string(), c[1].parse::<usize>()?)).is_some() {
            bail!("NICE topic page has more than one products CTA");
        }
    }
    if children.is_empty() && products.is_none() {
        bail!("NICE topic page has neither child topics nor a products listing");
    }
    Ok(TopicPage { children, products })
}
/// Product overview links on a topic's unfiltered `/products` listing, in source order, plus the number of listed
/// published or terminated product cards (the population the topic page's CTA counts). Cards for NICE advice
/// (`/advice/mib…`) or with a non-overview route are counted as listed but planned by no row here.
fn topic_products(raw: &str, base: &Url) -> Result<Vec<Value>> {
    Ok(topic_listing(raw, base)?.0)
}
fn topic_listing(raw: &str, base: &Url) -> Result<(Vec<Value>, usize)> {
    if !first_party(base) || base.query().is_some() || !base.path().ends_with("/products") {
        bail!("NICE topic products listing route mismatch");
    }
    let d = gate(raw)?;
    let main = d.select(&sel("main")).next().context("NICE topic products main missing")?;
    let mut out = vec![];
    let mut seen = HashSet::new();
    for a in main.select(&sel("a[href]")) {
        let Some(url) = source_link(base, a.value().attr("href").unwrap_or_default()) else { continue };
        let Some(id) = Url::parse(&url).ok().and_then(|u| product_id(&u)) else { continue };
        if seen.insert(id.clone()) {
            out.push(json!({"url":url,"title":norm(&text_of(&a)),"reference":id,"source_topic_products_url":base.as_str(),"route_scope":"unfiltered topic products listing (path of the topic page's own products CTA); filtered variants are gated for this agent"}));
        }
    }
    let mut listed = 0;
    for card in main.select(&sel("article.card")) {
        let pairs = card.select(&sel(".card__metadatum")).map(|m| {
            let dt = m.select(&sel("dt")).next().map(|e| norm(&text_of(&e))).unwrap_or_default();
            let dd = m.select(&sel("dd")).next().map(|e| norm(&text_of(&e))).unwrap_or_default();
            (dt.trim_end_matches(':').trim().to_string(), dd)
        }).collect::<Vec<_>>();
        if pairs.iter().any(|(dt, dd)| dt == "Product type" || (dt == "Status" && dd == "Terminated")) {
            listed += 1;
        }
    }
    Ok((out, listed))
}
/// Walk the browse-by-topic tree breadth-first from the configured roots and return the distinct product records
/// in first-seen order, plus honest issue rows for unreadable pages and listings whose product count falls short
/// of the topic page's own declared count.
async fn topic_tree(src: &SourceEntry, f: &Fetcher, roots: &[String], known: &HashSet<String>) -> (Vec<Value>, Vec<PlanRow>, usize) {
    let page_bound = src.config["max_topic_pages"].as_u64().unwrap_or(3000) as usize;
    let mut queue: VecDeque<String> = roots.iter().cloned().collect();
    let mut visited = HashSet::new();
    let mut records = vec![];
    let mut ids: HashSet<String> = known.clone();
    let mut issues = vec![];
    while let Some(url) = queue.pop_front() {
        let Ok(u) = Url::parse(&url) else { continue };
        if !visited.insert(u.path().to_string()) {
            continue;
        }
        if visited.len() > page_bound {
            issues.push(issue(src, url, "NICE topic tree page bound reached; topic inventory incomplete".into()));
            break;
        }
        let page = async {
            let p = f.get_ok(&url).await?;
            topic_page(&p.text(), &Url::parse(&p.record.final_url)?)
        }
        .await;
        let page = match page {
            Ok(p) => p,
            Err(e) => {
                issues.push(issue(src, url, format!("NICE topic page unresolved: {e:#}")));
                continue;
            }
        };
        for c in page.children {
            if !visited.contains(Url::parse(&c).map(|u| u.path().to_string()).unwrap_or_default().as_str()) {
                queue.push_back(c);
            }
        }
        let Some((listing, declared)) = page.products else { continue };
        let found = async {
            let p = f.get_ok(&listing).await?;
            topic_listing(&p.text(), &Url::parse(&p.record.final_url)?)
        }
        .await;
        match found {
            Ok((list, listed)) => {
                if listed < declared {
                    issues.push(issue(src, listing.clone(), format!("NICE topic listing shows {listed} published/terminated product cards but the topic page declares {declared}; topic inventory incomplete")));
                } else if list.len() < listed {
                    // advice (MIB) cards and cards routed to an in-development page have no /guidance/ overview
                    eprintln!("NICE topic listing {listing}: {} of {listed} listed published/terminated cards have no /guidance/ product overview link (advice or in-development route); not planned", listed - list.len());
                }
                for mut r in list {
                    let id = r["reference"].as_str().unwrap_or_default().to_string();
                    if ids.insert(id) {
                        r["source_topic_url"] = json!(url);
                        records.push(r);
                    }
                }
            }
            Err(e) => issues.push(issue(src, listing, format!("NICE topic products listing unresolved: {e:#}"))),
        }
    }
    (records, issues, visited.len())
}
fn issue(src: &SourceEntry, url: String, why: String) -> PlanRow {
    PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("nice-html-unresolved-{}", common::short_hash(&url)),
        url,
        title: None,
        rendition: None,
        version: None,
        extra: json!({"discovery_error":why,"catalog_complete":false}),
    }
}
fn row(
    src: &SourceEntry,
    doc: &GuidelineDoc,
    parent_url: &str,
    parent_snapshot: &str,
    label: &str,
    evidence: &Value,
) -> PlanRow {
    let snapshot = doc.search_metadata["source_snapshot_sha256"]
        .as_str()
        .unwrap();
    PlanRow {
        source_id: src.id.clone(),
        doc_key: format!("{}-{}", doc.work_key.as_deref().unwrap(), &snapshot[..12]),
        url: doc.url.clone(),
        title: Some(doc.title.clone()),
        rendition: Some("html".into()),
        version: None,
        extra: json!({"source_snapshot":snapshot,"product_url":parent_url,"product_snapshot":parent_snapshot,"chapter_label":label,"source_item_role":doc.item_type,"source_link_evidence":evidence,"catalog_complete":false}),
    }
}
async fn product_rows(
    src: &SourceEntry,
    f: &Fetcher,
    url: &str,
    evidence: &Value,
    room: Option<usize>,
) -> Vec<PlanRow> {
    let result = async {
        let p = f.get_ok(url).await?;
        let base = Url::parse(&p.record.final_url)?;
        if product_id(&base) != product_id(&Url::parse(url)?) {
            bail!("NICE product redirected outside source identity");
        }
        let parent = overview(&p.text(), &base)?;
        let parent_hash = parent.search_metadata["source_snapshot_sha256"]
            .as_str()
            .unwrap();
        let mut out = vec![row(src, &parent, &parent.url, parent_hash, "", evidence)];
        if room.is_some_and(|n| out.len() >= n) {
            return Ok::<_, anyhow::Error>(out);
        }
        let mut queue = VecDeque::from(
            parent.search_metadata["source_chapter_links"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
        let mut seen = HashSet::new();
        let bound = src.config["max_chapters_per_product"]
            .as_u64()
            .unwrap_or(60) as usize;
        while let Some(link) = queue.pop_front() {
            let url = link["url"]
                .as_str()
                .context("NICE source chapter URL missing")?;
            if !seen.insert(route_key(url)) {
                continue;
            }
            if seen.len() > bound {
                out.push(issue(
                    src,
                    url.into(),
                    "NICE chapter href bound reached; product coverage incomplete".into(),
                ));
                break;
            }
            let item = async {
                let p = f.get_ok(url).await?;
                let base = Url::parse(&p.record.final_url)?;
                let expected = chapter_product(&Url::parse(url)?);
                if chapter_product(&base) != expected {
                    bail!("NICE source chapter identity changed on redirect");
                }
                let doc = chapter(
                    &p.text(),
                    &base,
                    &parent,
                    link["label"].as_str().unwrap_or_default(),
                )?;
                for child in doc.search_metadata["source_chapter_links"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if !seen.contains(&route_key(child["url"].as_str().unwrap_or_default())) {
                        queue.push_back(child.clone());
                    }
                }
                Ok::<_, anyhow::Error>(row(
                    src,
                    &doc,
                    &parent.url,
                    parent_hash,
                    link["label"].as_str().unwrap_or_default(),
                    &link,
                ))
            }
            .await;
            out.push(match item {
                Ok(r) => r,
                Err(e) => issue(
                    src,
                    url.into(),
                    format!("NICE source chapter unresolved: {e:#}; no alternative route"),
                ),
            });
            if room.is_some_and(|n| out.len() >= n) {
                break;
            }
        }
        Ok(out)
    }
    .await;
    match result {
        Ok(rows) => rows,
        Err(e) => vec![issue(
            src,
            url.into(),
            format!("NICE source product unresolved: {e:#}; no alternative route"),
        )],
    }
}
#[async_trait::async_trait]
impl Adapter for NiceHtml {
    async fn discover(
        &self,
        src: &SourceEntry,
        f: &Fetcher,
        limit: Option<usize>,
    ) -> Result<Vec<PlanRow>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }
        let catalog_url = src.catalog_url.as_deref().unwrap_or(CATALOG);
        if !catalog_route(&Url::parse(catalog_url)?) {
            bail!("NICE HTML requires the observed public published catalog route");
        }
        let mut out = vec![];
        let mut products = HashSet::new();
        if let Some(seeds) = src.config["source_linked_documents"].as_array() {
            if seeds.len() > 8 {
                bail!("NICE independently observed seed bound exceeds eight");
            }
            for seed in seeds {
                let url = seed["url"].as_str().context("NICE seed missing URL")?;
                let u = Url::parse(url)?;
                let id =
                    product_id(&u).context("NICE seed is not a source product overview URL")?;
                let evidence = seed["source_evidence_url"]
                    .as_str()
                    .context("NICE seed evidence URL missing")?;
                let e = Url::parse(evidence)?;
                if !first_party(&e) || !(catalog_route(&e) || product_id(&e).is_some()) {
                    bail!("NICE seed evidence is not a public first-party source observation");
                }
                if !products.insert(id) {
                    continue;
                }
                let remaining = limit.map(|n| n.saturating_sub(out.len()));
                out.extend(product_rows(src, f, url, seed, remaining).await);
                if limit.is_some_and(|n| out.len() >= n) {
                    return Ok(out);
                }
            }
        }
        if src.config["source_linked_only"].as_bool() == Some(true) {
            return Ok(out);
        }
        let mut url = catalog_url.to_string();
        let mut pages = HashSet::new();
        let mut signatures = HashSet::new();
        let bound = src.config["max_catalog_pages"].as_u64().unwrap_or(300) as usize;
        let product_bound = src.config["max_products"].as_u64().unwrap_or(4000) as usize;
        loop {
            if !pages.insert(route_key(&url)) || pages.len() > bound {
                out.push(issue(
                    src,
                    url,
                    "NICE source Next loop/page bound; catalog incomplete".into(),
                ));
                break;
            }
            let page = async {
                let p = f.get_ok(&url).await?;
                catalog(&p.text(), &Url::parse(&p.record.final_url)?)
            }
            .await;
            let page = match page {
                Ok(p) => p,
                Err(e) => {
                    out.push(issue(
                        src,
                        url,
                        format!("NICE source catalog unresolved: {e:#}"),
                    ));
                    break;
                }
            };
            if !signatures.insert(page.signature) {
                out.push(issue(
                    src,
                    url,
                    "NICE catalog repeated native reference inventory; no progress".into(),
                ));
                break;
            }
            for record in page.records {
                let target = record["url"].as_str().unwrap();
                let id = product_id(&Url::parse(target)?).unwrap();
                if !products.insert(id) {
                    continue;
                }
                if products.len() > product_bound {
                    out.push(issue(
                        src,
                        target.into(),
                        "NICE product bound; catalog incomplete".into(),
                    ));
                    return Ok(out);
                }
                let remaining = limit.map(|n| n.saturating_sub(out.len()));
                out.extend(product_rows(src, f, target, &record, remaining).await);
                if limit.is_some_and(|n| out.len() >= n) {
                    return Ok(out);
                }
            }
            match page.next {
                Some(next) => url = next,
                None => {
                    if !page.terminal {
                        out.push(issue(src,url,"NICE no Next link without a verified source range/end marker; coverage unknown".into()));
                    }
                    break;
                }
            }
        }
        // The published catalog's Next pages are gated for this agent (0x8133), so the browse-by-topic tree is the
        // inventory route for the rest of the catalog. Products already planned above keep their rows.
        if let Some(roots) = src.config["topic_roots"].as_array() {
            let roots = roots.iter().filter_map(|r| r.as_str().map(String::from)).collect::<Vec<_>>();
            let (records, issues, pages) = topic_tree(src, f, &roots, &products).await;
            eprintln!("NICE topic tree: {pages} topic pages, {} additional distinct products, {} unresolved topic pages/listings", records.len(), issues.len());
            for record in records {
                let target = record["url"].as_str().unwrap().to_string();
                let id = product_id(&Url::parse(&target)?).unwrap();
                if !products.insert(id) {
                    continue;
                }
                if products.len() > product_bound {
                    out.push(issue(src, target, "NICE product bound; catalog incomplete".into()));
                    return Ok(out);
                }
                let remaining = limit.map(|n| n.saturating_sub(out.len()));
                out.extend(product_rows(src, f, &target, &record, remaining).await);
                if limit.is_some_and(|n| out.len() >= n) {
                    return Ok(out);
                }
            }
            let remaining = limit.map(|n| n.saturating_sub(out.len())).unwrap_or(usize::MAX);
            out.extend(issues.into_iter().take(remaining));
        }
        Ok(out)
    }
    async fn extract(&self, src: &SourceEntry, f: &Fetcher, row: &PlanRow) -> Result<GuidelineDoc> {
        if let Some(error) = row.extra["discovery_error"].as_str() {
            bail!("{error}");
        }
        let product_url = row.extra["product_url"]
            .as_str()
            .context("NICE plan product evidence missing")?;
        let p = f.get_ok(product_url).await?;
        let parent = overview(&p.text(), &Url::parse(&p.record.final_url)?)?;
        if row.extra["product_snapshot"] != parent.search_metadata["source_snapshot_sha256"] {
            bail!("NICE product state/date/content changed; re-plan");
        }
        let mut doc = if product_id(&Url::parse(&row.url)?).is_some() {
            parent
        } else {
            if !parent.search_metadata["source_chapter_links"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|l| route_key(l["url"].as_str().unwrap_or_default()) == route_key(&row.url))
            {
                // A source Next may reach a chapter absent from overview navigation; retain its exact discovery evidence.
                if row.extra["source_link_evidence"]["url"].as_str() != Some(row.url.as_str()) {
                    bail!("NICE chapter no longer has source href evidence");
                }
            }
            let c = f.get_ok(&row.url).await?;
            let mut doc = chapter(
                &c.text(),
                &Url::parse(&c.record.final_url)?,
                &parent,
                row.extra["chapter_label"].as_str().unwrap_or_default(),
            )?;
            doc.fetches.push(c.record);
            doc
        };
        if row.extra["source_snapshot"] != doc.search_metadata["source_snapshot_sha256"] {
            bail!("NICE chapter/product source changed; re-plan");
        }
        doc.source_id = src.id.clone();
        doc.doc_key = row.doc_key.clone();
        doc.fetches.insert(0, p.record);
        doc.search_metadata["source_link_evidence"] = row.extra["source_link_evidence"].clone();
        Ok(doc)
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const OVERVIEW: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/nice_html/ng245-overview.html"
    ));
    const RECS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/nice_html/ng245-recommendations.html"
    ));
    const RESEARCH: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guidelines/nice_html/ng245-research.html"
    ));
    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/guidelines/nice_html")
                .join(name),
        )
        .unwrap()
    }
    #[test]
    fn clinical_numbered_bodies_and_research_without_articles_keep_source_roles_and_native_structure(
    ) {
        let parent = overview(
            OVERVIEW,
            &Url::parse("https://www.nice.org.uk/guidance/ng245").unwrap(),
        )
        .unwrap();
        assert_eq!(parent.status, "unknown");
        assert_eq!(parent.published.as_deref(), Some("2024-11-27"));
        assert_eq!(
            parent.search_metadata["source_product_type"],
            "NICE guideline"
        );
        assert!(parent.relations.is_empty());
        let doc = chapter(
            RECS,
            &Url::parse("https://www.nice.org.uk/guidance/ng245/chapter/Recommendations").unwrap(),
            &parent,
            "Recommendations",
        )
        .unwrap();
        let recs = doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
            .collect::<Vec<_>>();
        assert_eq!(recs.len(), 85);
        assert_eq!(recs[0].block_id, "ng245-1_1_1");
        assert!(
            recs[0].text.contains("Invented nested action A")
                && recs[0].text.contains("Invented nested action B")
        );
        assert!(
            matches!(&recs[0].kind,BlockKind::Recommendation{label:Some(n),strength:None,certainty:None,grading_system:None,..}if n=="1.1.1")
        );
        assert_eq!(
            doc.search_metadata["source_recommendations"][0]["source_body_blocks"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert!(doc.search_metadata["source_next_present"]
            .as_bool()
            .unwrap());
        assert_eq!(doc.attachments.len(), 1);
        assert!(doc.published.is_none());
        let research = chapter(
            RESEARCH,
            &Url::parse(
                "https://www.nice.org.uk/guidance/ng245/chapter/Recommendations-for-research",
            )
            .unwrap(),
            &parent,
            "Recommendations for research",
        )
        .unwrap();
        assert_eq!(
            research.item_type.as_deref(),
            Some("research_recommendations_chapter")
        );
        assert_eq!(
            research.search_metadata["source_research_sections"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            research
                .blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                .count(),
            0
        );
        assert!(research
            .blocks
            .iter()
            .any(|b| b.text.contains("Invented research question")));
        let p = overview(
            &fixture("htg786-overview.html"),
            &Url::parse("https://www.nice.org.uk/guidance/htg786").unwrap(),
        )
        .unwrap();
        let h = chapter(
            &fixture("htg786-recommendations.html"),
            &Url::parse("https://www.nice.org.uk/guidance/htg786/chapter/1-Recommendations")
                .unwrap(),
            &p,
            "1 Recommendations",
        )
        .unwrap();
        let native = h.search_metadata["source_recommendations"]
            .as_array()
            .unwrap();
        assert_eq!(
            native
                .iter()
                .filter(|r| r["source_role"] == "clinical_recommendation")
                .count(),
            2
        );
        assert_eq!(
            native
                .iter()
                .filter(|r| r["source_role"] == "research_recommendation")
                .count(),
            1
        );
        assert!(h.blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{label:None,qualifier:Some(q),certainty:None,..}if q=="research-recommendation")));
    }
    #[test]
    fn native_sibling_tables_and_context_are_preserved_in_source_order_without_inferred_footnote_ownership(
    ) {
        let parent = overview(
            &fixture("ng143-overview.html"),
            &Url::parse("https://www.nice.org.uk/guidance/NG143").unwrap(),
        )
        .unwrap();
        let doc = chapter(
            &fixture("ng143-table-context.html"),
            &Url::parse("https://www.nice.org.uk/guidance/NG143/chapter/recommendations").unwrap(),
            &parent,
            "Recommendations",
        )
        .unwrap();
        let tables = doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
            .collect::<Vec<_>>();
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0].block_id, "ID0EDBAC");
        assert!(
            matches!(&tables[0].kind,BlockKind::Table{caption:Some(c)}if c=="Invented clinical comparison")
        );
        let native = &doc.search_metadata["source_tables"];
        assert_eq!(native[0]["rows"].as_array().unwrap().len(), 4);
        assert_eq!(native[1]["rows"].as_array().unwrap().len(), 6);
        assert_eq!(native[1]["rows"][1]["cells"][0]["tag"], "th");
        assert_eq!(native[1]["rows"][1]["cells"][0]["scope"], "row");
        assert_eq!(native[1]["rows"][1]["cells"][1]["text"], "");
        let rec = doc
            .blocks
            .iter()
            .position(|b| b.block_id == "ng143-1_2_15")
            .unwrap();
        let table = doc
            .blocks
            .iter()
            .position(|b| b.block_id == "ID0EDBAC")
            .unwrap();
        let note = doc
            .blocks
            .iter()
            .position(|b| b.block_id == "context-note")
            .unwrap();
        assert!(rec < table && table < note);
        assert!(matches!(doc.blocks[note].kind, BlockKind::Paragraph));
    }
    #[test]
    fn explicit_replacement_and_termination_are_distinct_from_active_or_withdrawn_inference() {
        let replaced = overview(
            &fixture("htg575-replaced.html"),
            &Url::parse("https://www.nice.org.uk/guidance/htg575").unwrap(),
        )
        .unwrap();
        assert_eq!(replaced.status, "updated and replaced");
        assert_eq!(
            replaced.item_type.as_deref(),
            Some("guidance_replacement_notice")
        );
        assert_eq!(replaced.relations.len(), 1);
        assert_eq!(replaced.relations[0].rel, "superseded_by");
        assert_eq!(
            replaced.relations[0].target,
            "https://www.nice.org.uk/guidance/HTG786"
        );
        let terminated = overview(
            &fixture("ta1102-terminated.html"),
            &Url::parse("https://www.nice.org.uk/guidance/terminated/ta1102").unwrap(),
        )
        .unwrap();
        assert_eq!(terminated.status, "terminated");
        assert!(terminated.relations.is_empty());
        assert_eq!(
            terminated.date_observations[0].normalized.as_deref(),
            Some("2025-10-29")
        );
        assert_eq!(
            terminated
                .blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                .count(),
            0
        );
    }
    #[tokio::test]
    async fn catalog_pagination_follows_actual_next_hrefs_chapter_graph_and_honest_failure_end_rules(
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 8 << 20, "").unwrap();
        let pairs = [
            (CATALOG, "catalog-page-1.html"),
            (
                "https://www.nice.org.uk/guidance/published?sp=on&pa=2",
                "catalog-page-2.html",
            ),
            (
                "https://www.nice.org.uk/guidance/ng245",
                "ng245-overview.html",
            ),
            (
                "https://www.nice.org.uk/guidance/ng245/chapter/Recommendations",
                "ng245-recommendations.html",
            ),
            (
                "https://www.nice.org.uk/guidance/ng245/chapter/Recommendations-for-research",
                "ng245-research.html",
            ),
            (
                "https://www.nice.org.uk/guidance/htg786",
                "htg786-overview.html",
            ),
            (
                "https://www.nice.org.uk/guidance/htg786/chapter/1-Recommendations",
                "htg786-recommendations.html",
            ),
            (
                "https://www.nice.org.uk/guidance/htg786/chapter/2-Information-about-the-procedure",
                "htg786-procedure.html",
            ),
            (
                "https://www.nice.org.uk/guidance/htg575",
                "htg575-replaced.html",
            ),
            (
                "https://www.nice.org.uk/guidance/ta1206",
                "ta1206-overview.html",
            ),
            (
                "https://www.nice.org.uk/guidance/ta1206/chapter/3-Committee-discussion",
                "ta1206-committee.html",
            ),
        ];
        for (url, file) in pairs {
            f.seed_cache_for_tests(url, fixture(file).as_bytes(), "text/html");
        }
        let src: SourceEntry = serde_json::from_value(
            json!({"id":"nice_html","enabled":true,"access":"public","catalog_url":CATALOG}),
        )
        .unwrap();
        assert!(NiceHtml
            .discover(&src, &f, Some(0))
            .await
            .unwrap()
            .is_empty());
        let rows = NiceHtml.discover(&src, &f, None).await.unwrap();
        assert_eq!(rows.len(), 9);
        assert!(rows.iter().all(|r| r.extra["discovery_error"].is_null()));
        let research = rows
            .iter()
            .find(|r| r.url.ends_with("/Recommendations-for-research"))
            .unwrap();
        let doc = NiceHtml.extract(&src, &f, research).await.unwrap();
        assert_eq!(
            doc.item_type.as_deref(),
            Some("research_recommendations_chapter")
        );
        assert_eq!(doc.fetches.len(), 2);
        let mut changed = research.clone();
        changed.extra["product_snapshot"] = json!("wrong");
        assert!(NiceHtml
            .extract(&src, &f, &changed)
            .await
            .unwrap_err()
            .to_string()
            .contains("re-plan"));
        f.seed_cache_for_tests(
            "https://www.nice.org.uk/guidance/published?sp=on&pa=2",
            fixture("service-interruption.html").as_bytes(),
            "text/html",
        );
        let gated = NiceHtml.discover(&src, &f, None).await.unwrap();
        assert!(gated.last().unwrap().extra["discovery_error"]
            .as_str()
            .unwrap()
            .contains("gate"));
        assert!(gated.last().unwrap().url.contains("pa=2"));
        assert!(catalog("<main><table id='results'><thead><tr><th>Title</th><th>Reference number</th><th>Published</th><th>Last updated</th></tr></thead><tbody></tbody></table></main>",&Url::parse(CATALOG).unwrap()).unwrap_err().to_string().contains("zero rows"));
    }
    const TERMINATED_LI_OVERVIEW: &str = include_str!(
        "../../../tests/fixtures/guidelines/nice_html/ta1102-li-overview-synthetic.html"
    );
    const TERMINATED_LI_ADVICE: &str = include_str!(
        "../../../tests/fixtures/guidelines/nice_html/ta1102-li-advice-synthetic.html"
    );
    #[test]
    fn explicit_li_time_termination_is_product_scoped_on_overview_and_advice_not_published_or_withdrawn_inference(
    ) {
        let url = Url::parse("https://www.nice.org.uk/guidance/terminated/ta1102").unwrap();
        let parent = overview(TERMINATED_LI_OVERVIEW, &url).unwrap();
        assert_eq!(parent.status, "terminated");
        assert_eq!(
            parent.item_type.as_deref(),
            Some("terminated_guidance_product")
        );
        assert!(parent.relations.is_empty());
        assert!(parent.published.is_none());
        let term = parent
            .date_observations
            .iter()
            .find(|d| d.label == "Terminated")
            .unwrap();
        assert_eq!(term.normalized.as_deref(), Some("2025-10-29"));
        assert_eq!(term.source_url.as_deref(), Some(url.as_str()));
        assert!(parent.search_metadata["source_date_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["label"] == "Terminated"
                && v["source_html"]
                    .as_str()
                    .unwrap()
                    .contains("<time datetime=\"2025-10-29\"")));
        let chapter_url =
            Url::parse("https://www.nice.org.uk/guidance/terminated/ta1102/chapter/advice")
                .unwrap();
        let advice = chapter(TERMINATED_LI_ADVICE, &chapter_url, &parent, "Advice").unwrap();
        assert_eq!(advice.status, "terminated");
        assert!(advice.published.is_none() && advice.updated.is_none());
        assert_eq!(advice.date_observations.len(), 1);
        assert_eq!(advice.date_observations[0].label, "Terminated");
        assert_eq!(
            advice.date_observations[0].source_url.as_deref(),
            Some(chapter_url.as_str())
        );
        assert_eq!(
            advice.search_metadata["source_termination_scope"]
                ["status_inferred_from_url_or_company_withdrawal"],
            false
        );
        assert_eq!(
            advice.search_metadata["source_product_metadata"]["status"],
            "terminated"
        );
        assert_eq!(
            advice
                .blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                .count(),
            0
        );
        // Same date/title/URL and company withdrawal are insufficient without the
        // exact native Terminated label. Never reinterpret Published as this event.
        let published = TERMINATED_LI_OVERVIEW.replace("<li>Terminated:", "<li>Published:");
        let unproven = overview(&published, &url).unwrap();
        assert_eq!(unproven.status, "unknown");
        assert!(!unproven
            .date_observations
            .iter()
            .any(|d| d.label == "Terminated"));
        let missing_time = TERMINATED_LI_OVERVIEW.replace(" datetime=\"2025-10-29\"", "");
        assert_eq!(overview(&missing_time, &url).unwrap().status, "unknown");
        let changed = TERMINATED_LI_OVERVIEW
            .replace("2025-10-29", "2025-10-30")
            .replace("29 October 2025", "30 October 2025");
        assert_ne!(
            parent.search_metadata["source_snapshot_sha256"],
            overview(&changed, &url).unwrap().search_metadata["source_snapshot_sha256"]
        );
    }
    #[tokio::test]
    async fn source_linked_terminated_overview_advice_planning_pins_own_header_event_and_parent_snapshot(
    ) {
        let root = "https://www.nice.org.uk/guidance/terminated/ta1102";
        let advice_url = "https://www.nice.org.uk/guidance/terminated/ta1102/chapter/advice";
        let temp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(temp.path(), true, 500, 1 << 20, "").unwrap();
        f.seed_cache_for_tests(root, TERMINATED_LI_OVERVIEW.as_bytes(), "text/html");
        f.seed_cache_for_tests(advice_url, TERMINATED_LI_ADVICE.as_bytes(), "text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"nice_html","enabled":true,"access":"public","catalog_url":CATALOG,"config":{"source_linked_only":true,"source_linked_documents":[{"url":root,"source_evidence_url":root}]}})).unwrap();
        let rows = NiceHtml.discover(&src, &f, Some(2)).await.unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            let doc = NiceHtml.extract(&src, &f, row).await.unwrap();
            assert_eq!(doc.status, "terminated");
            assert_eq!(
                doc.blocks
                    .iter()
                    .filter(|b| matches!(b.kind, BlockKind::Recommendation { .. }))
                    .count(),
                0
            );
        }
        f.seed_cache_for_tests(
            root,
            TERMINATED_LI_OVERVIEW
                .replace("2025-10-29", "2025-10-30")
                .replace("29 October 2025", "30 October 2025")
                .as_bytes(),
            "text/html",
        );
        assert!(NiceHtml
            .extract(&src, &f, &rows[1])
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("re-plan"));
    }

    const TOPIC_ROOT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/nice_html/topic-root.html"));
    const TOPIC_CV: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/nice_html/topic-cardiovascular.html"));
    const TOPIC_HT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/nice_html/topic-hypertension.html"));
    const TOPIC_HT_PRODUCTS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/nice_html/topic-hypertension-products.html"));
    const ROOT_URL: &str = "https://www.nice.org.uk/guidance/conditions-and-diseases";
    const CV_URL: &str = "https://www.nice.org.uk/guidance/conditions-and-diseases/cardiovascular-conditions";
    const HT_URL: &str = "https://www.nice.org.uk/guidance/conditions-and-diseases/cardiovascular-conditions/hypertension";
    #[test]
    fn topic_tree_pages_expose_children_and_the_declared_products_listing() {
        let root = topic_page(TOPIC_ROOT, &Url::parse(ROOT_URL).unwrap()).unwrap();
        assert!(root.products.is_none());
        assert!(root.children.iter().any(|c| c == CV_URL));
        // the "All categories" roots are part of the tree
        for other in ["health-protection", "lifestyle-and-wellbeing", "population-groups", "health-and-social-care-delivery", "settings"] {
            assert!(root.children.iter().any(|c| c == &format!("https://www.nice.org.uk/guidance/{other}")), "{other}");
        }
        assert!(root.children.iter().all(|c| !c.contains('?')));
        let cv = topic_page(TOPIC_CV, &Url::parse(CV_URL).unwrap()).unwrap();
        assert!(cv.children.iter().any(|c| c == HT_URL));
        assert!(cv.children.iter().any(|c| c.ends_with("/cardiovascular-conditions/vasculitis")));
        let ht = topic_page(TOPIC_HT, &Url::parse(HT_URL).unwrap()).unwrap();
        assert_eq!(ht.products, Some((format!("{HT_URL}/products"), 11)));
        assert!(topic_page(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/nice_html/service-interruption.html")), &Url::parse(HT_URL).unwrap()).is_err());
        assert!(topic_page(TOPIC_HT, &Url::parse("https://www.nice.org.uk/guidance/published?sp=on").unwrap()).is_err());
    }
    #[test]
    fn topic_products_listing_keeps_product_overviews_and_skips_in_development_records() {
        let list = topic_products(TOPIC_HT_PRODUCTS, &Url::parse(&format!("{HT_URL}/products")).unwrap()).unwrap();
        let ids = list.iter().map(|r| r["reference"].as_str().unwrap()).collect::<Vec<_>>();
        assert_eq!(ids, ["ta1161", "ng136", "htg741", "ng133", "htg662", "htg630", "htg410", "htg387", "htg357", "qs35", "qs28"]);
        assert!(list.iter().all(|r| r["url"].as_str().unwrap().starts_with("https://www.nice.org.uk/guidance/")));
        assert!(topic_products(TOPIC_HT_PRODUCTS, &Url::parse(&format!("{HT_URL}/products?Status=Published")).unwrap()).is_err());
    }
    #[test]
    fn topic_listing_counts_published_and_terminated_cards_including_advice() {
        let (list, listed) = topic_listing(TOPIC_HT_PRODUCTS, &Url::parse(&format!("{HT_URL}/products")).unwrap()).unwrap();
        assert_eq!((list.len(), listed), (11, 11));
        // cystic fibrosis: CTA declares 9 = 7 /guidance/ overviews (incl. terminated TA1188) + 2 /advice/ MIBs
        let cf = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/nice_html/topic-cystic-fibrosis-products.html"));
        let (list, listed) = topic_listing(cf, &Url::parse("https://www.nice.org.uk/guidance/conditions-and-diseases/cystic-fibrosis/products").unwrap()).unwrap();
        assert_eq!(listed, 9);
        assert_eq!(list.len(), 7);
        assert!(list.iter().any(|r| r["reference"] == "ta1188"));
        assert!(list.iter().all(|r| !r["url"].as_str().unwrap().contains("/advice/")));
    }
    #[tokio::test]
    async fn discovery_adds_topic_tree_products_after_the_gated_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fetcher::new(tmp.path(), true, 500, 8 << 20, "").unwrap();
        // catalog page 1 is not cached here (an unreadable catalog stays an issue row), the tree supplies products
        f.seed_cache_for_tests(ROOT_URL, TOPIC_ROOT.as_bytes(), "text/html");
        f.seed_cache_for_tests(CV_URL, TOPIC_CV.as_bytes(), "text/html");
        f.seed_cache_for_tests(HT_URL, TOPIC_HT.as_bytes(), "text/html");
        f.seed_cache_for_tests(&format!("{HT_URL}/products"), TOPIC_HT_PRODUCTS.as_bytes(), "text/html");
        f.seed_cache_for_tests("https://www.nice.org.uk/guidance/ng245", OVERVIEW.as_bytes(), "text/html");
        let src: SourceEntry = serde_json::from_value(json!({"id":"nice_html","access":"public","enabled":true,"catalog_url":CATALOG,
            "config":{"topic_roots":[ROOT_URL],"max_chapters_per_product":0}})).unwrap();
        let rows = NiceHtml.discover(&src, &f, None).await.unwrap();
        assert!(rows[0].extra["discovery_error"].as_str().unwrap().contains("NICE source catalog unresolved"));
        let urls = rows.iter().map(|r| r.url.as_str()).collect::<Vec<_>>();
        for id in ["ta1161", "ng136", "qs28"] {
            assert!(urls.contains(&format!("https://www.nice.org.uk/guidance/{id}").as_str()), "{id} missing");
        }
        // each product is planned once, unreadable tree pages are explicit issue rows (offline: other categories)
        assert_eq!(urls.iter().filter(|u| u.ends_with("/guidance/ng136")).count(), 1);
        assert!(rows.iter().any(|r| r.url.ends_with("/guidance/health-protection") && r.extra["discovery_error"].as_str().unwrap().contains("NICE topic page unresolved")));
        // without topic_roots the adapter behaves as before
        let plain: SourceEntry = serde_json::from_value(json!({"id":"nice_html","access":"public","enabled":true,"catalog_url":CATALOG})).unwrap();
        assert_eq!(NiceHtml.discover(&plain, &f, None).await.unwrap().len(), 1);
    }
}
