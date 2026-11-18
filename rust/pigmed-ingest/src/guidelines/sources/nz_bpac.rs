//! bpacnz public articles and distinct linked full-guideline PDFs.
//! Public access does not authorize republication or retrieval-system reuse.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,GuidelineDoc,Relation,Rights};
use crate::guidelines::fetch::{Fetcher,Fetched};
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::json;
use std::collections::HashSet;
use url::Url;
pub struct NzBpac;
/// Article body column: the publisher uses several Bootstrap widths for the same article template.
const ARTICLE_BODY:&str=".article-wrapper .col-lg-8.col-lg-offset-1, .article-wrapper .col-lg-7.col-lg-offset-1, .article-wrapper .col-sm-8.col-sm-offset-1";

// `get_ok`'s sitewide login heuristic sees an optional Mybpac nav form. Use the
// fetcher unchanged but validate status and the selected content root ourselves.
async fn public_get(f:&Fetcher,url:&str)->Result<Fetched>{
    let got=f.get(url).await?;
    if got.record.status!=200 {bail!("bpac HTTP {} at {}",got.record.status,got.record.final_url);}
    if Url::parse(url)?.origin()!=Url::parse(&got.record.final_url)?.origin(){bail!("bpac redirect crosses origin; asset rights require independent review");}
    Ok(got)
}
fn reject_gate(root:ElementRef<'_>)->Result<()> {
    if root.select(&sel("input[type='password'], form[action*='login'], [class*='captcha'], iframe[src*='captcha']")).next().is_some() {bail!("bpac clinical content is behind an authentication/challenge form");}
    let t=text_of(&root).to_lowercase();
    if t.contains("sign in to continue reading") || t.contains("log in to continue reading") || t.contains("subscribe to continue reading") || t.contains("verify you are human") {bail!("bpac content is a login, paywall or challenge");}
    Ok(())
}
fn dmy(label:&str,raw:&str,url:&str)->DateObservation {
    let mut d=common::date(label,raw,url);
    let re=regex::Regex::new(r"(?i)^\s*(\d{1,2})\s*(?:st|nd|rd|th)?\s+([A-Za-z]+),?\s+(\d{4})\s*$").unwrap();
    if let Some(c)=re.captures(raw) {d.normalized=common::date(label,&format!("{} {}, {}",&c[2],&c[1],&c[3]),url).normalized;}
    d
}
fn article_dates(root:ElementRef<'_>,url:&str)->Vec<DateObservation> {
    let pattern=regex::Regex::new(r"(?i)(Published|Updated):\s*(\d{1,2}\s*(?:st|nd|rd|th)?\s+[A-Za-z]+,?\s+\d{4})").unwrap();
    let mut out=Vec::new();
    for p in root.select(&sel("p")) {
        let t=text_of(&p);
        for c in pattern.captures_iter(&t) {out.push(dmy(&c[1],&c[2],url));}
    }
    for m in root.select(&sel("#whatchanged mark")) {let raw=text_of(&m);if !raw.is_empty(){out.push(dmy("What's changed?",&raw,url));}}
    out
}
fn article(html:&str,base:&Url)->Result<(String,Vec<DateObservation>,Vec<Block>,Vec<String>,Vec<serde_json::Value>)>{
    let doc=Html::parse_document(html);
    let title=doc.select(&sel("h1#Gen2Content_h1Title")).next().map(|e|text_of(&e)).context("bpac full article title missing")?;
    let body=doc.select(&sel(ARTICLE_BODY)).next().context("bpac full article body missing")?;
    // Some articles (e.g. part 1 of a series) have the references block commented out in the source.
    let refs=doc.select(&sel(".row.refs")).next();
    reject_gate(body)?;if let Some(r)=refs{reject_gate(r)?;}
    if text_of(&body).split_whitespace().count()<200 {bail!("bpac page is a teaser, not a full clinical article");}
    let dates=article_dates(body,base.as_str());
    let combined=format!("<div id='bpac-clinical'>{}{}</div>",body.html(),refs.map(|r|r.html()).unwrap_or_default());
    let (mut blocks,mut warnings)=blocks_from_html(&combined,Some(base),Some("#bpac-clinical"));
    let tables=body.select(&sel("table")).chain(refs.into_iter().flat_map(|r|r.select(&sel("table")).collect::<Vec<_>>())).collect::<Vec<_>>();
    let table_ids=blocks.iter().filter_map(|b|matches!(b.kind,BlockKind::Table{..}).then(||b.block_id.clone())).collect::<Vec<_>>();
    if tables.len()!=table_ids.len(){warnings.push("Source/extracted table counts differ; figure and table scope need review.".into());}
    else {
        let mut additions=Vec::new();
        for (table,id) in tables.into_iter().zip(table_ids) {
            let mut figures=Vec::new();
            for (n,img) in table.select(&sel("img[src]")).enumerate() {
                let Some(url)=common::link(base,img.value().attr("src").unwrap()) else {continue};
                figures.push(Block {block_id:format!("{}-image-{}",id,n+1),kind:BlockKind::Figure{src_url:Some(url),caption:img.value().attr("alt").map(String::from)},text:String::new(),heading_path:vec![],page:None});
            }
            if !figures.is_empty(){additions.push((id,figures));}
        }
        for (id,mut figures) in additions.into_iter().rev() {
            if let Some(i)=blocks.iter().position(|b|b.block_id==id) {
                for figure in &mut figures {figure.heading_path=blocks[i].heading_path.clone();}
                blocks.splice(i+1..i+1,figures);
            }
        }
        if blocks.iter().any(|b|b.block_id.contains("-image-")){warnings.push("Images embedded in tables are retained separately; color legends and image semantics require visual review.".into());}
    }
    for i in 0..blocks.len().saturating_sub(1) {
        let caption=blocks[i+1].text.clone();
        if caption.starts_with("Figure ") && matches!(blocks[i+1].kind,BlockKind::Paragraph) {
            if let BlockKind::Figure {caption:existing,..}=&mut blocks[i].kind { *existing=Some(caption); }
        }
    }
    let mut seen=HashSet::new();
    let attachments=doc.select(&sel("a[href]"))
      .filter_map(|a|{let url=common::link(base,a.value().attr("href")?)?;
        let u=Url::parse(&url).ok()?;if u.origin()!=base.origin()||!u.path().to_lowercase().ends_with(".pdf")||!seen.insert(url.clone()){return None;}
        Some(json!({"url":url,"label":text_of(&a),"role":"linked_pdf","rights":"unknown; independent rendition not validated"}))}).collect();
    if refs.is_none(){warnings.push("Page has no (uncommented) references/acknowledgement section; article-specific copyright notice not available, site terms apply.".into());}
    warnings.push("Key practice points include facts as well as advice; individual formal recommendations and grades are not inferred. Nested-list context and linked PDF currency require review.".into());
    Ok((title,dates,blocks,warnings,attachments))
}
/// Publisher's Latest Articles carousel is a bounded, not historical, inventory.
fn latest(html:&str,base:&Url)->Result<Vec<(String,String)>> {
    let doc=Html::parse_document(html);
    if doc.select(&sel("#pics .carousel-inner")).next().is_none(){bail!("bpac latest-articles carousel missing");}
    let mut out=Vec::new();let mut seen=HashSet::new();
    for a in doc.select(&sel("#pics .carousel-caption h3 a[href]")) {
        let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};
        let u=Url::parse(&url)?;
        if u.origin()!=base.origin() || !(u.path().ends_with(".aspx")||u.path().ends_with(".asp")) {bail!("unexpected bpac latest-article URL: {url}");}
        if seen.insert(url.clone()){out.push((url,text_of(&a)));}
    }
    if out.is_empty(){bail!("bpac latest-articles carousel has no links");} Ok(out)
}

fn category(html:&str,base:&Url)->Result<Vec<(String,String)>> {
    let doc=Html::parse_document(html);
    let current=doc.select(&sel("#Gen2Content_CurrentCategory")).next().map(|e|text_of(&e)).context("bpac category title missing")?;
    if current!="Guidelines" {bail!("bpac source returned a different category: {current}");}
    let root=doc.select(&sel("#Gen2Content_ArticlesList_ListHolder")).next().context("bpac category article list missing")?;
    if doc.select(&sel("a[rel='next'], .pagination a[href], [id*='Pager'] a[href]")).next().is_some() {bail!("bpac category exposes pagination that requires reconciliation");}
    reject_gate(root)?;
    let mut seen=HashSet::new();let mut out=Vec::new();
    for a in root.select(&sel(".article-list-item h4 a[href]")) {
        let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};
        if Url::parse(&url)?.origin()!=base.origin(){bail!("bpac category links external content: {url}");}
        if seen.insert(url.clone()){out.push((url,text_of(&a)));}
    }
    if out.is_empty(){bail!("bpac guideline category contains no source items");}
    Ok(out)
}
fn terms_rights(html:&str,url:&str)->Result<Rights> {
    let doc=Html::parse_document(html);
    let heading=doc.select(&sel("h2")).find(|e|text_of(e).contains("Terms of Use")).context("bpac terms heading missing")?;
    let root=heading.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().classes().any(|c|c=="col-xs-12"))
        .context("bpac terms content root missing")?;
    reject_gate(root)?;
    let restrictions=root.select(&sel("p,li")).map(|e|text_of(&e)).filter(|t|{
        let l=t.to_lowercase();l.contains("retrieval system or service")||l.contains("research or private study")||l.contains("prior written permission")||l.contains("personal use")||l.contains("insubstantial portions")
    }).collect::<Vec<_>>();
    if !restrictions.iter().any(|t|t.to_lowercase().contains("retrieval system or service")){bail!("bpac terms restriction not found");}
    Ok(Rights{status:"restricted".into(),terms_url:Some(url.into()),restrictions,evidence_urls:vec![url.into()],
        note:Some("Website terms apply to bpac publications; linked third-party asset rights require independent review.".into()),..Default::default()})
}
fn article_hash(html:&str)->Result<String> {
    let doc=Html::parse_document(html);
    let body=doc.select(&sel(ARTICLE_BODY)).next().context("bpac full article body missing")?;
    let refs=doc.select(&sel(".row.refs")).next().map(|r|r.html()).unwrap_or_default();
    let title=doc.select(&sel("h1#Gen2Content_h1Title")).next().map(|e|text_of(&e)).context("bpac title missing")?;
    Ok(crate::common::sha256_hex(format!("{}\n{}\n{}",title,body.html(),refs).as_bytes()))
}

fn article_rights(html:&str,base:&Url)->Rights {
    let doc=Html::parse_document(html);
    let Some(refs)=doc.select(&sel(".row.refs")).next() else {return common::unknown_rights()};
    let notice=refs.select(&sel("p")).map(|p|text_of(&p)).find(|p|{
        let t=p.to_lowercase();t.contains("copyright") && t.contains("you may access it") && t.contains("you may not reproduce")
    });
    match notice {
        Some(text)=>Rights {status:"restricted".into(),rights_holders:vec!["bpacnz".into()],restrictions:vec![text],
            terms_url:refs.select(&sel("a[href]")).filter_map(|a|common::link(base,a.value().attr("href")?)).find(|u|u.ends_with("/terms-of-use.aspx")),
            evidence_urls:vec![base.to_string()],..Default::default()},
        None=>common::unknown_rights(),
    }
}

fn historical_pdfs(html:&str,base:&Url)->Result<(String,Vec<(String,String)>)>{
    let doc=Html::parse_document(html);
    let body=doc.select(&sel(".letter-content")).next().context("bpac historical landing root missing")?;
    reject_gate(body)?;
    let title=body.select(&sel("h2#Gen2Content_h1Title")).next().map(|e|text_of(&e)).context("bpac historical title missing")?;
    let mut seen=HashSet::new();let mut out=Vec::new();
    for note in body.select(&sel(".note")) {
        let Some(h)=note.select(&sel("h4")).next() else {continue};let label=text_of(&h);
        if !label.to_lowercase().starts_with("guideline") {continue;}
        for a in note.select(&sel("a[href]")) {
            let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};
            let parsed=Url::parse(&url)?;
            if parsed.origin()!=base.origin() || !parsed.path().to_lowercase().ends_with(".pdf") {continue;}
            if seen.insert(url.clone()){out.push((url,label.clone()));}
        }
    }
    if out.is_empty(){bail!("bpac historical landing has no full-guideline PDF links");}
    Ok((title,out))
}

/// bpacnz Guidelines (`/guidelines/N/`): NICE guidelines contextualised for New Zealand, one long HTML page with
/// numbered recommendations (`h4.section-number` under "1. Recommendations"). A separate template from articles.
struct Contextualised {
    title:String,
    dates:Vec<DateObservation>,
    blocks:Vec<Block>,
    warnings:Vec<String>,
    attachments:Vec<serde_json::Value>,
    /// The publisher's own status banner, verbatim (e.g. "no longer actively supported or updated").
    status_notice:Option<String>,
    /// Paragraphs under the page's own "Copyright" heading, verbatim.
    copyright:Vec<String>,
}
const GUIDELINE_ROOT:&str=".container-custom.content .col-md-8";
fn contextualised_hash(html:&str)->Result<String> {
    let doc=Html::parse_document(html);
    let title=doc.select(&sel(".guideline-header h1")).next().map(|e|text_of(&e)).context("bpac guideline title missing")?;
    let root=doc.select(&sel(GUIDELINE_ROOT)).next().context("bpac guideline body missing")?;
    Ok(crate::common::sha256_hex(format!("{}\n{}",title,root.html()).as_bytes()))
}
/// Replace the prose under each numbered recommendation heading (1.1.1, 1.2.3, ...) by one Recommendation block.
/// NICE-style statements carry no printed grade: strength/certainty stay None.
fn structure_numbered_recommendations(blocks:Vec<Block>,warnings:&mut Vec<String>)->Vec<Block> {
    let label=regex::Regex::new(r"^\d+(?:\.\d+){2,}\b").unwrap();
    let section=regex::Regex::new(r"(?i)^(?:\d+\.\s*)?recommendations$").unwrap();
    let mut out=Vec::with_capacity(blocks.len());let mut i=0;
    while i<blocks.len() {
        let b=&blocks[i];
        let numbered=matches!(b.kind,BlockKind::Heading{level:4}) && label.is_match(&b.text) && b.heading_path.iter().any(|h|section.is_match(h));
        if !numbered {out.push(b.clone());i+=1;continue;}
        out.push(b.clone());
        let mut j=i+1;let mut parts=Vec::new();let mut path=None;
        while j<blocks.len() && matches!(blocks[j].kind,BlockKind::Paragraph|BlockKind::ListItem) && blocks[j].heading_path.last()==Some(&b.text) {
            parts.push(blocks[j].text.clone());path.get_or_insert_with(||blocks[j].heading_path.clone());j+=1;
        }
        if parts.is_empty(){warnings.push(format!("Recommendation {} has no prose directly under its number; nothing structured.",b.text));i+=1;continue;}
        if j<blocks.len() && blocks[j].heading_path.last()==Some(&b.text) && !matches!(blocks[j].kind,BlockKind::Heading{..}) {
            warnings.push(format!("Recommendation {} continues after a table/figure; only the prose before it is in the recommendation block, the rest stays as section text.",b.text));
        }
        out.push(Block{block_id:format!("{}-recommendation",b.block_id),kind:BlockKind::Recommendation{label:Some(b.text.clone()),strength:None,certainty:None,population:None,grading_system:None,qualifier:None},
            text:parts.join("\n"),heading_path:path.unwrap_or_default(),page:None});
        i=j;
    }
    out
}
fn contextualised(html:&str,base:&Url)->Result<Contextualised> {
    let doc=Html::parse_document(html);
    let title=doc.select(&sel(".guideline-header h1")).next().map(|e|text_of(&e)).filter(|t|!t.is_empty()).context("bpac guideline title missing")?;
    let root=doc.select(&sel(GUIDELINE_ROOT)).next().context("bpac guideline body missing")?;
    reject_gate(root)?;
    if root.select(&sel("section#recommendations")).next().is_none(){bail!("bpac guideline recommendations section missing");}
    if text_of(&root).split_whitespace().count()<500 {bail!("bpac guideline page is too short to be the full guideline");}
    let mut dates=Vec::new();
    for d in root.select(&sel(".pubdate .date")) {let raw=text_of(&d);if !raw.is_empty(){dates.push(common::date("Published",&raw,base.as_str()));}}
    let status_notice=doc.select(&sel(".alert")).map(|e|text_of(&e)).find(|t|t.to_lowercase().contains("no longer actively supported"));
    let mut seen=HashSet::new();
    let attachments=root.select(&sel("a.guidelinepdf[href]")).filter_map(|a|{
        let url=common::link(base,a.value().attr("href")?)?;
        seen.insert(url.clone()).then(||json!({"url":url,"label":text_of(&a),"role":"publisher_pdf_rendition","rights":"unknown; PDF rendition not compared with the HTML"}))
    }).collect::<Vec<_>>();
    let mut copyright=Vec::new();
    if let Some(h)=root.select(&sel("h3")).find(|h|text_of(h)=="Copyright") {
        for sib in h.next_siblings().filter_map(ElementRef::wrap) {
            if sib.value().name()!="p" {break;}
            let t=text_of(&sib);if !t.is_empty(){copyright.push(t);}
        }
    }
    // The publish-date/download strip is page chrome; its date and PDF link are kept above as metadata.
    let mut body=root.html();
    for strip in root.select(&sel(".pubdate-container")) {body=body.replacen(&strip.html(),"",1);}
    let (blocks,mut warnings)=blocks_from_html(&format!("<div id='bpac-guideline'>{body}</div>"),Some(base),Some("#bpac-guideline"));
    let blocks=structure_numbered_recommendations(blocks,&mut warnings);
    let source=root.select(&sel("section#recommendations h4.section-number")).filter(|h|regex::Regex::new(r"^\d+(?:\.\d+){2,}\b").unwrap().is_match(&text_of(h))).count();
    let structured=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).count();
    if structured!=source {warnings.push(format!("Source shows {source} numbered recommendations; {structured} were structured. The rest stay as section text."));}
    warnings.push("Recommendation wording follows NICE conventions; no per-statement grade is printed, so strength/certainty are empty.".into());
    if attachments.is_empty(){warnings.push("No guideline PDF link found on the page.".into());}
    Ok(Contextualised{title,dates,blocks,warnings,attachments,status_notice,copyright})
}
/// Why a public page is neither an article, a contextualised guideline nor a PDF landing (never a login guess).
fn unrecognized_reason(html:&str)->String {
    let doc=Html::parse_document(html);
    let title=doc.select(&sel("title")).next().map(|e|text_of(&e)).unwrap_or_default();
    if title.trim().eq_ignore_ascii_case("error") {return "bpac served a site error page (HTTP 200, <title>Error</title>); broken source link, not gated".into();}
    if doc.select(&sel(ARTICLE_BODY)).next().is_some_and(|b|text_of(&b).split_whitespace().count()<200) {
        return "bpac article template with under 200 words of body (teaser or link hub), not a full clinical article".into();
    }
    "source page is gated, a teaser, or an unrecognized clinical/landing template".into()
}

#[async_trait::async_trait]
impl Adapter for NzBpac {
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  if limit==Some(0){return Ok(vec![]);}
  let home=public_get(f,src.catalog_url.as_deref().context("bpac homepage missing")?).await?;
  let base=Url::parse(&home.record.final_url)?;
  let home_html=home.text();
  let category_url={
   let doc=Html::parse_document(&home_html);
   doc.select(&sel("a[href]"))
    .filter_map(|a|common::link(&base,a.value().attr("href")?))
    .find(|u|Url::parse(u).ok().is_some_and(|u|u.origin()==base.origin() && u.path()=="/category.aspx" && u.query_pairs().any(|(k,v)|k=="CategoryId"&&v=="56")))
    .context("bpac source guidelines category link missing")?
  };
  let category_page=public_get(f,&category_url).await?;
  let mut candidates=category(&category_page.text(),&Url::parse(&category_page.record.final_url)?)?;
  candidates.extend(latest(&home_html,&base)?);
  let mut seen=HashSet::new();let mut rows=Vec::new();
  'items: for (url,catalog_title) in candidates {
   if !seen.insert(url.clone()){continue;}
   let source=match public_get(f,&url).await {
    Ok(v)=>v,
    Err(e)=>{
     let key=common::short_hash(&url);
     rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-unresolved"),url,title:Some(catalog_title),rendition:Some("html".into()),version:None,extra:json!({"discovery_error":format!("{e:#}")})});
     let blocked=rows.last().and_then(|r|r.extra["discovery_error"].as_str()).is_some_and(|e|["HTTP 401","HTTP 403","HTTP 429","robots"].iter().any(|x|e.contains(x)));
     if blocked || limit.is_some_and(|n|rows.len()>=n){return Ok(rows);} continue;
    }
   };
   let page_base=Url::parse(&source.record.final_url)?;let html=source.text();
   let article_result=article(&html,&page_base);
   if let Err(error)=&article_result {
    let message=format!("{error:#}").to_lowercase();
    if message.contains("authentication") || message.contains("challenge") || message.contains("paywall") {
     let key=common::short_hash(&url);
     rows.push(PlanRow {source_id:src.id.clone(),doc_key:format!("{key}-unresolved"),url,title:Some(catalog_title),rendition:Some("html".into()),version:None,extra:json!({"discovery_error":message})});
     break;
    }
   }
   if let Ok((title,_,_,_,_))=article_result {
    let hash=article_hash(&html)?;let slug=common::slug(page_base.path_segments().and_then(|s|s.last()).unwrap_or("article"));
    rows.push(PlanRow {source_id:src.id.clone(),doc_key:format!("{}-{}",slug,&hash[..12]),url,title:Some(title),rendition:Some("html".into()),version:None,
      extra:json!({"work_key":slug,"role":"article","clinical_sha256":hash})});
   } else if let Ok(g)=contextualised(&html,&page_base) {
    let hash=contextualised_hash(&html)?;let slug=common::slug(page_base.path());
    rows.push(PlanRow {source_id:src.id.clone(),doc_key:format!("{}-{}",slug,&hash[..12]),url,title:Some(g.title),rendition:Some("html".into()),version:None,
      extra:json!({"work_key":slug,"role":"contextualised_guideline","clinical_sha256":hash})});
   } else if let Ok((_,pdfs))=historical_pdfs(&html,&page_base) {
    for (pdf_url,title) in pdfs {
     if !seen.insert(pdf_url.clone()){continue;}
     let slug=common::slug(Url::parse(&pdf_url)?.path_segments().and_then(|s|s.last()).unwrap_or("guideline"));
     let mut row=PlanRow {source_id:src.id.clone(),doc_key:format!("{}-unresolved",slug),url:pdf_url,title:Some(title),rendition:Some("pdf".into()),version:None,
       extra:json!({"work_key":slug,"role":"full_guideline_pdf","parent_url":url})};
     match public_get(f,&row.url).await {
      Ok(pdf) if pdf.is_pdf()=>{row.doc_key=format!("{}-{}",slug,&pdf.record.sha256[..12]);row.extra["content_sha256"]=json!(pdf.record.sha256);},
      Ok(_)=>row.extra["discovery_error"]=json!("linked PDF did not return PDF bytes"),
      Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
     }
     let blocked=row.extra["discovery_error"].as_str().is_some_and(|e|["HTTP 401","HTTP 403","HTTP 429","robots"].iter().any(|x|e.contains(x)));
     rows.push(row);if blocked || limit.is_some_and(|n|rows.len()>=n){break 'items;}
    }
   } else {
    let key=common::short_hash(&url);
    rows.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-unresolved"),url,title:Some(catalog_title),rendition:Some("html".into()),version:None,
      extra:json!({"discovery_error":unrecognized_reason(&html)})});
   }
   if limit.is_some_and(|n|rows.len()>=n){break;}
  }
  Ok(rows)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
  if let Some(e)=row.extra["discovery_error"].as_str(){bail!("bpac discovery unresolved: {e}");}
  let policy_url="https://bpac.org.nz/terms-of-use.aspx";
  let policy=public_get(f,policy_url).await?;let mut rights=terms_rights(&policy.text(),policy_url)?;
  let page=public_get(f,&row.url).await?;let base=Url::parse(&page.record.final_url)?;
  let role=row.extra["role"].as_str().unwrap_or_default();
  let mut dates=Vec::new();let mut relations=Vec::new();let mut attachments=Vec::new();
  let mut fetches=vec![policy.record.clone()];
  let mut status:String="unknown".into();let mut search_metadata=serde_json::Value::Null;
  let (title,blocks,mut warnings)=if role=="article" {
   let html=page.text();
   if row.extra["clinical_sha256"].as_str().is_some_and(|h|article_hash(&html).map(|v|v!=h).unwrap_or(true)){bail!("bpac article changed since planning");}
   let (title,observations,blocks,warnings,assets)=article(&html,&base)?;dates=observations;attachments=assets;
   let specific=article_rights(&html,&base);
   if specific.status=="restricted" {rights.rights_holders=specific.rights_holders;rights.restrictions.extend(specific.restrictions);rights.evidence_urls.extend(specific.evidence_urls);}
   (title,blocks,warnings)
  } else if role=="contextualised_guideline" {
   let html=page.text();
   if row.extra["clinical_sha256"].as_str().is_some_and(|h|contextualised_hash(&html).map(|v|v!=h).unwrap_or(true)){bail!("bpac guideline changed since planning");}
   let g=contextualised(&html,&base)?;dates=g.dates;attachments=g.attachments;
   if let Some(notice)=&g.status_notice {status="archived".into();search_metadata=json!({"source_status_notice":{"text":notice,"source_url":row.url}});}
   let mut warnings=g.warnings;
   if !g.copyright.is_empty() {
    rights.restrictions.extend(g.copyright);rights.evidence_urls.push(row.url.clone());
    rights.note=Some("bpac website terms plus the page's own Copyright section (NICE copyright for the adapted source guideline). Linked asset rights need independent review.".into());
   } else {warnings.push("Guideline page has no Copyright section; only the bpac website terms are recorded.".into());}
   (g.title,g.blocks,warnings)
  } else if role=="full_guideline_pdf" {
   if !page.is_pdf(){bail!("bpac linked guideline did not return PDF bytes");}
   if row.extra["content_sha256"].as_str().is_some_and(|h|h!=page.record.sha256){bail!("bpac guideline PDF changed since planning");}
   let parent=row.extra["parent_url"].as_str().context("bpac PDF landing missing")?;
   let parent_page=public_get(f,parent).await?;let parent_base=Url::parse(&parent_page.record.final_url)?;
   let parent_html=parent_page.text();
   let (_,links)=historical_pdfs(&parent_html,&parent_base)?;
   if !links.iter().any(|(u,_)|u==&row.url){bail!("bpac PDF is no longer linked from the source landing");}
   {
    let source=Html::parse_document(&parent_html);
    if let Some(body)=source.select(&sel(".letter-content")).next() {
     for mut observation in article_dates(body,parent_base.as_str()) {observation.label=format!("Source landing {}",observation.label);dates.push(observation);}
    }
   }
   relations.push(Relation{rel:"related".into(),target:parent.into(),scope:Some("Source landing links this distinct full guideline PDF".into()),evidence_url:Some(parent.into())});
   fetches.push(parent_page.record);
   let (blocks,mut warnings,_)=super::static_list::blocks_for(&page,None,30).await?;
   if blocks.is_empty(){bail!("bpac PDF conversion yielded no content");}
   warnings.push("PDF layout, diagrams and asset-specific rights require independent fidelity review.".into());
   (row.title.clone().unwrap_or_default(),blocks,warnings)
  } else {bail!("bpac role is not a validated terminal clinical source");};
  warnings.push("Discovery covers the source Guidelines category and current Latest Articles carousel only; broader historical inventory is not established.".into());
  let published=dates.iter().find(|d|d.label=="Published").and_then(|d|d.normalized.clone());
  let updated=dates.iter().find(|d|d.label=="Updated").and_then(|d|d.normalized.clone());
  fetches.push(page.record);
  Ok(GuidelineDoc {source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),
   item_type:Some(if role=="article" {"educational_article"} else {"guideline"}.into()),title,url:row.url.clone(),status,version:None,
   published,updated,date_observations:dates,rights,relations,attachments,blocks,warnings,fetches,search_metadata,..Default::default()})
 }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
 use super::*;
 const BLEEDING_PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/bleeding-management.pdf"));
 const HOME:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/home.html"));
 const CATEGORY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/guidelines-category.html"));
 const TERMS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/terms.html"));
 const CKD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/ckd.html"));
 const BLEEDING:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/bleeding-landing.html"));
 #[test]
 fn real_article_dates_collapsed_tables_and_reference_tail(){
  let base=Url::parse("https://bpac.org.nz/2026/ckd.aspx").unwrap();
  let (title,dates,blocks,warnings,pdfs)=article(CKD,&base).unwrap();
  assert_eq!(title,"Chronic kidney disease: the canary in the coal mine");
  assert!(dates.iter().any(|d|d.label=="Published" && d.normalized.as_deref()==Some("2026-03-27")));
  assert!(dates.iter().any(|d|d.label=="Updated" && d.normalized.as_deref()==Some("2026-10-01")));
  assert!(dates.iter().any(|d|d.label=="What's changed?" && d.normalized.as_deref()==Some("2026-08-13")));
  assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),4);
  assert!(blocks.iter().any(|b|b.text.contains("Considerations when interpreting eGFR")));
  assert!(blocks.iter().any(|b|b.text.contains("Kidney Disease Improving Clinical Outcomes")));
  assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Figure{src_url:Some(u),..} if u.ends_with("/2026/img/ckd-figure1.png"))));
  assert!(pdfs.iter().any(|a|a["url"].as_str().is_some_and(|u|u.ends_with("/2026/docs/ckd.pdf"))));
  assert!(warnings.iter().any(|s|s.contains("image semantics")));
  let rights=article_rights(CKD,&base);assert_eq!(rights.status,"restricted");
  assert!(rights.restrictions.iter().any(|t|t.contains("may not reproduce")));
  let optional=CKD.replace("<body>","<body><form><input type='password'></form>");
  assert!(article(&optional,&base).is_ok());
  let blocked=CKD.replace("<h3 class=\"mt-0\">Key practice points</h3>","<input type='password'><h3 class=\"mt-0\">Key practice points</h3>");
  assert!(article(&blocked,&base).is_err());
 }
 #[test]
 fn historical_landing_identifies_two_separate_guidelines(){
  let base=Url::parse("https://bpac.org.nz/2018/bleeding-guidelines.aspx").unwrap();
  let (title,documents)=historical_pdfs(BLEEDING,&base).unwrap();
  assert!(title.contains("dabigatran and rivaroxaban"));assert_eq!(documents.len(),2);
  assert!(documents.iter().any(|(u,t)|u.ends_with("bleeding-management.pdf") && t.contains("management of bleeding")));
  assert!(documents.iter().any(|(u,t)|u.ends_with("testing-and-perioperative-management.pdf") && t.contains("testing and perioperative")));
  assert!(article(BLEEDING,&base).is_err());
 }
 #[test]
 fn saved_category_and_restricted_source_terms(){
  let base=Url::parse("https://bpac.org.nz/category.aspx?CategoryId=56").unwrap();
  let rows=category(CATEGORY,&base).unwrap();
  assert!(rows.iter().any(|(u,_)|u.ends_with("/2018/bleeding-guidelines.aspx")));
  let rights=terms_rights(TERMS,"https://bpac.org.nz/terms-of-use.aspx").unwrap();
  assert_eq!(rights.status,"restricted");
  assert!(rights.restrictions.iter().any(|s|s.contains("retrieval system or service")));
 }

 #[tokio::test]
 async fn source_index_pdf_separation_and_offline_article(){
  let home="https://bpac.org.nz/";let category_url="https://bpac.org.nz/category.aspx?CategoryId=56";
  let landing="https://bpac.org.nz/2018/bleeding-guidelines.aspx";let ckd="https://bpac.org.nz/2026/ckd.aspx";
  let terms="https://bpac.org.nz/terms-of-use.aspx";
  assert!(latest(HOME,&Url::parse(home).unwrap()).unwrap().iter().any(|(u,_)|u==ckd));
  let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,500,1<<20,"").unwrap();
  f.seed_cache_for_tests(home,HOME.as_bytes(),"text/html");f.seed_cache_for_tests(category_url,CATEGORY.as_bytes(),"text/html");
  f.seed_cache_for_tests(landing,BLEEDING.as_bytes(),"text/html");f.seed_cache_for_tests(ckd,CKD.as_bytes(),"text/html");
  f.seed_cache_for_tests(terms,TERMS.as_bytes(),"text/html");
  let pdf="https://bpac.org.nz/2018/docs/dabigatran-rivaroxaban-bleeding-management.pdf";
  f.seed_cache_for_tests(pdf,BLEEDING_PDF,"application/pdf");
  let src:SourceEntry=serde_json::from_value(json!({"id":"nz_bpac","access":"public","enabled":true,"catalog_url":home})).unwrap();
  let rows=NzBpac.discover(&src,&f,Some(2)).await.unwrap();
  assert_eq!(rows.len(),2);assert!(rows.iter().all(|r|r.rendition.as_deref()==Some("pdf")));
  assert_ne!(rows[0].url,rows[1].url);assert!(rows.iter().all(|r|r.extra["parent_url"]==landing));
  assert!(rows[0].extra["discovery_error"].is_null());assert!(rows[1].extra["discovery_error"].is_string());
  let guideline=NzBpac.extract(&src,&f,&rows[0]).await.unwrap();
  assert_eq!(guideline.item_type.as_deref(),Some("guideline"));assert_eq!(guideline.rights.status,"restricted");
  assert_eq!(guideline.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),3);
  assert!(guideline.blocks.iter().any(|b|b.text.contains("idarucizumab")||b.text.contains("Idarucizumab")));
  assert!(guideline.published.is_none());
  assert!(guideline.date_observations.iter().any(|d|d.label=="Source landing Published" && d.normalized.as_deref()==Some("2018-07-27")));
  assert!(guideline.relations.iter().any(|r|r.target==landing));
  let hash=article_hash(CKD).unwrap();
  let article_row=PlanRow{source_id:"nz_bpac".into(),doc_key:format!("ckd-{}",&hash[..12]),url:ckd.into(),title:None,rendition:Some("html".into()),version:None,
    extra:json!({"work_key":"ckd","role":"article","clinical_sha256":hash})};
  let doc=NzBpac.extract(&src,&f,&article_row).await.unwrap();
  assert_eq!(doc.published.as_deref(),Some("2026-03-27"));assert_eq!(doc.updated.as_deref(),Some("2026-10-01"));
  assert_eq!(doc.rights.status,"restricted");assert!(doc.rights.restrictions.iter().any(|s|s.contains("retrieval system or service")));
  assert_eq!(doc.fetches.len(),2);assert_eq!(doc.item_type.as_deref(),Some("educational_article"));
 }

}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod contextualised_and_variant_tests {
 use super::*;
 const SEPSIS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/guidelines-4-sepsis.html"));
 const AMS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/guidelines-3-antimicrobial.html"));
 const HEART:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/heart-failure.html"));
 const HUB:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/recovery-hub.html"));
 const ERROR_PAGE:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/recovery-cast-error.html"));
 const TERMS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nz_bpac/terms.html"));
 fn recs(b:&[Block])->Vec<&Block>{b.iter().filter(|x|matches!(x.kind,BlockKind::Recommendation{..})).collect()}
 #[test]
 fn sepsis_guideline_numbered_recommendations_dates_pdf_and_notice(){
  let base=Url::parse("https://bpac.org.nz/guidelines/4/").unwrap();
  assert!(article(SEPSIS,&base).is_err());
  let g=contextualised(SEPSIS,&base).unwrap();
  assert_eq!(g.title,"Sepsis: recognition, diagnosis and early management");
  let r=recs(&g.blocks);assert_eq!(r.len(),130,"{:?}",g.warnings);
  assert!(!g.warnings.iter().any(|w|w.contains("were structured")));
  let first=r[0];
  assert!(matches!(&first.kind,BlockKind::Recommendation{label:Some(l),strength:None,certainty:None,grading_system:None,..} if l=="1.1.1"));
  assert!(first.text.starts_with("Think ‘could this be sepsis?’"));
  assert!(first.heading_path.iter().any(|h|h=="1.1 Identifying people with suspected sepsis"));
  let five=r.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l=="1.1.5")).unwrap();
  assert!(five.text.contains("possible source of infection") && five.text.contains("factors that increase risk of sepsis"));
  // prose under a recommendation is moved into the block, not duplicated
  assert_eq!(g.blocks.iter().filter(|b|b.text.starts_with("Think ‘could this be sepsis?’")).count(),1);
  // research recommendations (section 3) are not formal recommendations
  assert!(!r.iter().any(|b|b.heading_path.iter().any(|h|h.contains("Recommendations for research"))));
  assert!(g.blocks.iter().any(|b|b.text.contains("Epidemiological study on presentation and management of sepsis")));
  assert!(g.dates.iter().any(|d|d.label=="Published" && d.raw=="June 2018" && d.normalized.as_deref()==Some("2018-06")));
  assert!(g.attachments.iter().any(|a|a["url"]=="https://bpac.org.nz/guidelines/4/docs/Sepsis.pdf"));
  assert!(g.status_notice.as_deref().is_some_and(|n|n.contains("no longer actively supported or updated")));
  assert!(g.copyright.iter().any(|c|c.contains("NICE Copyright material can be downloaded for private research and study")));
  assert!(!g.blocks.iter().any(|b|b.text.contains("Guideline (.pdf)")));
 }
 #[test]
 fn antimicrobial_guideline_ignores_appendix_numbers(){
  let base=Url::parse("https://bpac.org.nz/guidelines/3/").unwrap();
  let g=contextualised(AMS,&base).unwrap();
  let r=recs(&g.blocks);assert_eq!(r.len(),48,"{:?}",g.warnings);
  assert!(!r.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l.starts_with('A'))));
  assert_eq!(g.attachments.len(),2);
  assert!(g.copyright.is_empty());
 }
 #[test]
 fn article_column_variants_and_unrecognized_reasons(){
  let base=Url::parse("https://bpac.org.nz/2025/heart-failure.aspx").unwrap();
  let (title,dates,_,_,_)=article(HEART,&base).unwrap();
  assert_eq!(title,"Addressing heart failure in primary care");
  assert!(dates.iter().any(|d|d.label=="Published" && d.normalized.as_deref()==Some("2025-01-24")));
  assert!(article_hash(HEART).is_ok());
  let (_,_,_,w,_)=article(HEART,&base).unwrap();assert!(w.iter().any(|x|x.contains("no (uncommented) references")));
  // the overview hub has ~380 words of its own prose: kept as an article, flagged by the missing references
  let hub=Url::parse("https://bpac.org.nz/2024/recovery.aspx").unwrap();
  assert!(article(HUB,&hub).is_ok() && contextualised(HUB,&hub).is_err());
  let teaser="<html><body><h1 id=\"Gen2Content_h1Title\">T</h1><div class=\"container-fluid article-wrapper\"><div class=\"col-lg-7 col-lg-offset-1\"><p>Short teaser; read the full article.</p></div></div></body></html>";
  assert!(article(teaser,&hub).is_err());assert!(unrecognized_reason(teaser).contains("under 200 words"));
  assert!(unrecognized_reason(ERROR_PAGE).contains("site error page"));
 }
 #[tokio::test]
 async fn offline_contextualised_extract(){
  let url="https://bpac.org.nz/guidelines/4/";let terms="https://bpac.org.nz/terms-of-use.aspx";
  let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,500,1<<20,"").unwrap();
  f.seed_cache_for_tests(url,SEPSIS.as_bytes(),"text/html");f.seed_cache_for_tests(terms,TERMS.as_bytes(),"text/html");
  let src:SourceEntry=serde_json::from_value(json!({"id":"nz_bpac","access":"public","enabled":true,"catalog_url":"https://bpac.org.nz/"})).unwrap();
  let hash=contextualised_hash(SEPSIS).unwrap();
  let row=PlanRow{source_id:"nz_bpac".into(),doc_key:format!("guidelines-4-{}",&hash[..12]),url:url.into(),title:None,rendition:Some("html".into()),version:None,
    extra:json!({"work_key":"guidelines-4","role":"contextualised_guideline","clinical_sha256":hash})};
  let doc=NzBpac.extract(&src,&f,&row).await.unwrap();
  assert_eq!(doc.item_type.as_deref(),Some("guideline"));assert_eq!(doc.status,"archived");
  assert_eq!(doc.published.as_deref(),Some("2018-06"));
  assert_eq!(doc.rights.status,"restricted");
  assert!(doc.rights.restrictions.iter().any(|s|s.contains("retrieval system or service")));
  assert!(doc.rights.restrictions.iter().any(|s|s.contains("NICE Copyright material")));
  assert!(doc.search_metadata["source_status_notice"]["text"].as_str().is_some_and(|t|t.contains("no longer actively supported")));
  assert_eq!(recs(&doc.blocks).len(),130);
  let changed=SEPSIS.replace("Think ‘could this be sepsis?’","Think about sepsis");assert_ne!(changed,SEPSIS);
  f.seed_cache_for_tests(url,changed.as_bytes(),"text/html");assert!(NzBpac.extract(&src,&f,&row).await.is_err());
 }
}
