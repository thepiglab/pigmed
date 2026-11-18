//! SIGN catalog and publisher-labelled full guideline PDF assets.
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct Sign;
const ROOT:&str="https://www.sign.ac.uk/guidelines/";
struct Entry {number:String,title:String,url:Option<String>,status:String,published:String,topic:String,detail:String}
fn catalog(html:&str,base:&Url)->Result<Vec<Entry>> {
    let doc=Html::parse_document(html);let root=doc.select(&sel("#myTabContent")).next().context("SIGN catalog tab content missing")?;
    let mut out=vec![];
    for status in ["current","in-scoping","in-development","proposed","archived"] {
        let selector=format!("#{status} table tbody tr");
        for row in root.select(&sel(&selector)) {
            let cells:Vec<_>=row.select(&sel("td")).collect();
            // Current and archived rows have a guideline number and published
            // year; the three pipeline tabs use different unnumbered schemas.
            let (number,title_index,topic_index,published,detail)=match status {
                "current"|"archived" if cells.len()>=4 => (text_of(&cells[0]),1,2,text_of(&cells[3]),cells.iter().skip(4).map(text_of).collect::<Vec<_>>().join("; ")),
                "in-scoping" if cells.len()>=3 => (String::new(),0,1,String::new(),format!("Status: {}",text_of(&cells[2]))),
                "in-development" if cells.len()>=5 => (String::new(),0,1,String::new(),format!("Status: {}; Remit: {}; Estimated publication: {}",text_of(&cells[2]),text_of(&cells[3]),text_of(&cells[4]))),
                "proposed" if cells.len()>=6 => {
                    let proposal=cells[5].select(&sel("a[href]")).next().and_then(|a|a.value().attr("href")).and_then(|h|common::link(base,h)).unwrap_or_default();
                    (String::new(),0,1,String::new(),format!("Request type: {}; Proposer: {}; Stage: {}; Proposal: {}; Proposal URL: {}",text_of(&cells[2]),text_of(&cells[3]),text_of(&cells[4]),text_of(&cells[5]),proposal))
                },
                _ => continue,
            };
            let title=text_of(&cells[title_index]);if title.is_empty(){continue;}
            // A proposal document is a supporting asset, not a full guideline.
            let url=if status=="current" || status=="archived" {cells[title_index].select(&sel("a[href]")).next().and_then(|a|a.value().attr("href")).and_then(|h|common::link(base,h))} else {None};
            out.push(Entry{number,title,url,status:status.into(),topic:text_of(&cells[topic_index]),published,detail});
        }
    }
    if !out.iter().any(|e|e.status=="current") {bail!("SIGN current guideline rows absent");}Ok(out)
}
/// Labels SIGN uses for the main publication link in `#guidelinesLinks` (2026-11 survey of 48 landing pages).
fn main_publication_label(lower:&str,own_sign:bool)->bool {
    own_sign || lower.contains("full guideline") || lower.contains("download guideline") || lower.contains("position statement")
}
fn landing(html:&str,base:&Url,number:&str)->Result<(String,String,Vec<Value>)> {
    let doc=Html::parse_document(html);let main=doc.select(&sel("#main")).next().context("SIGN detail root missing")?;
    let info=main.select(&sel("#documentInformation")).next().map(|x|text_of(&x)).unwrap_or_default();
    // Guideline pages use #guidelinesLinks; position-statement pages (SIGN 165) use #positionStatementLinks.
    let section=["#guidelinesLinks","#positionStatementLinks"].into_iter().find(|s|main.select(&sel(s)).next().is_some()).context("SIGN guideline asset section missing")?;
    let source=main.select(&sel(section)).next().context("SIGN guideline asset section missing")?;
    let mut full=vec![];let mut assets=vec![];let mut seen=HashSet::new();let mut external_hosts=vec![];
    for a in main.select(&sel(&format!("{section} a[href],#supportingLinks a[href]"))) {
        let label=text_of(&a);let Some(url)=a.value().attr("href").and_then(|h|common::link(base,h)) else {continue};
        if !seen.insert(url.clone()){continue;}
        let lower=label.to_lowercase();let u=Url::parse(&url)?;
        let pdf=u.path().to_lowercase().ends_with(".pdf");
        let excluded=["decision aid","plain language","patient","summary","update report","copyright","handbook","scoping","quick reference"];
        let own_sign=!number.is_empty() && regex::Regex::new(&format!(r"\bsign ?{}\b",regex::escape(number))).is_ok_and(|re|re.is_match(&lower));
        let in_main=source.select(&sel("a[href]")).any(|x|x.value().attr("href")==a.value().attr("href"));
        if in_main && u.origin()!=base.origin() {if let Some(h)=u.host_str(){if !external_hosts.contains(&h.to_string()){external_hosts.push(h.to_string());}}}
        let eligible=in_main && pdf && !excluded.iter().any(|s|lower.contains(s)) && main_publication_label(&lower,own_sign) && u.origin()==base.origin();
        if eligible {full.push((url.clone(),own_sign));}
        assets.push(json!({"url":url,"label":label,"role":if eligible {"candidate_full_guideline"} else if lower.contains("update report") {"update_report"} else if lower.contains("plain language") || lower.contains("patient") {"patient_information"} else {"supporting_or_external"},"rights":"unknown; asset not independently reviewed"}));
    }
    // A page can list another SIGN guideline's full PDF next to its own (SIGN 116 / SIGN 154 pages link each
    // other); the label naming this page's own SIGN number decides.
    if full.len()>1 && full.iter().filter(|(_,own)|*own).count()==1 {
        let own=full.iter().find(|(_,own)|*own).unwrap().0.clone();
        for a in assets.iter_mut() {if a["role"]=="candidate_full_guideline" && a["url"]!=json!(own) {a["role"]=json!("other_sign_guideline");}}
        full.retain(|(u,_)|*u==own);
    }
    if full.is_empty() && !external_hosts.is_empty() {bail!("SIGN landing links its guideline only on external hosts ({}); not a SIGN-hosted full text",external_hosts.join(", "));}
    if full.len()!=1 {bail!("SIGN expected one publisher-labelled full guideline PDF, found {}",full.len());}
    Ok((info,full.remove(0).0,assets))
}
/// A landing page whose own text states the guideline was withdrawn (SIGN 162). Returns the stated date and the
/// notice HTML; only applies when the page offers no guideline asset at all.
fn withdrawal_notice(html:&str)->Option<(String,String)> {
    let doc=Html::parse_document(html);let main=doc.select(&sel("#main")).next()?;
    if main.select(&sel("#guidelinesLinks a[href],#positionStatementLinks a[href]")).next().is_some() {return None;}
    let body=main.select(&sel(".col-md-8")).next()?;let text=text_of(&body);
    let re=regex::Regex::new(r"(?i)\bwas\s+withdrawn\s+on\s+(\d{1,2}\s+(?:January|February|March|April|May|June|July|August|September|October|November|December)\s+(?:19|20)\d{2})").unwrap();
    let date=re.captures(&text)?[1].to_string();
    let html=body.children().filter_map(scraper::ElementRef::wrap).filter(|e|e.value().attr("id")!=Some("documentInformation")).map(|e|e.html()).collect::<Vec<_>>().join("\n");
    Some((date,html))
}
fn pdf_rights(blocks:&[Block],url:&str)->Rights {
    let text=blocks.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ");
    if text.contains("Creative Commons Attribution-Noncommercial-NoDerivatives") && text.contains("4.0 International Licence") {
        Rights{status:"restricted".into(),license:Some("CC BY-NC-ND 4.0".into()),terms_url:Some("https://creativecommons.org/licenses/by-nc-nd/4.0/".into()),evidence_urls:vec![url.into()],attribution:Some("SIGN must be fully acknowledged and given credit".into()),restrictions:vec!["Noncommercial; material must not be remixed, transformed or built upon.".into()],note:Some("This licence is evidenced by this PDF only; linked third-party assets require separate review.".into()),..Default::default()}
    } else {Rights{status:"unknown".into(),note:Some("PDF-specific licence not established from extracted text.".into()),..Default::default()}}
}
fn mark_practice_points(blocks:&mut Vec<Block>)->Vec<String> {
    let source_key=blocks.iter().any(|b|b.text.replace("**","").contains("Recommendations based on the evidence review are marked E, while those based on the consensus process are marked C"));
    let bold_pair=regex::Regex::new(r"^\*\*R (.+?)\*\*\s+\*\*(C) (.+?)\*\*(.*)$").unwrap();
    let mut add=vec![];
    for (i,b) in blocks.iter().enumerate() {
        let t=b.text.trim();
        if b.heading_path.iter().any(|h|{let h=h.to_lowercase();h.contains("good-practice points") || h.contains("key to evidence")}) {continue;}
        let marked=if source_key {
            if let Some(c)=bold_pair.captures(t) {
                let statement=format!("{} {}{}",&c[1],&c[3],&c[4]).replace("**","");
                if statement.trim().ends_with('.') {Some(("R",statement,Some(if &c[2]=="C" {"C: consensus process"} else {"E: evidence review"}.to_string())))} else {None}
            } else if let Some(first)=t.strip_prefix("R ") {
                if !first.ends_with('.') {
                    blocks.get(i+1).and_then(|next|{
                        if next.heading_path!=b.heading_path || !matches!(b.kind,BlockKind::Heading{level:5}) || !matches!(next.kind,BlockKind::Heading{level:5}) {return None;}
                        let nt=next.text.trim();let (basis,tail)=if let Some(x)=nt.strip_prefix("C "){("C: consensus process",x)} else {return None};
                        if !tail.ends_with('.') {return None;}Some(("R",format!("{first} {tail}"),Some(basis.to_string())))
                    })
                } else {None}
            } else {None}
        } else {None};
        if let Some((marker,statement,qualifier))=marked {
            add.push((i,Block{block_id:format!("{}-marked-recommendation",b.block_id),kind:BlockKind::Recommendation{label:Some(marker.into()),strength:None,certainty:None,population:None,grading_system:None,qualifier},text:statement,heading_path:b.heading_path.clone(),page:b.page}));continue;
        }
        if t.starts_with("✓ ") && t.ends_with('.') && t.len()>=50 {
            add.push((i,Block{block_id:format!("{}-practice-point",b.block_id),kind:BlockKind::Recommendation{label:Some("✓".into()),strength:None,certainty:None,population:None,grading_system:None,qualifier:Some("Good-practice point".into())},text:t.into(),heading_path:b.heading_path.clone(),page:b.page}));
        }
    }
    for (i,b) in add.into_iter().rev(){blocks.insert(i+1,b);}
    vec!["Only visually confirmed standalone or R/C margin-marker patterns are structured; other markers, source reading order, tables, figures and recommendation coverage require visual comparison.".into()]
}
/// Pre-2015 SIGN guidelines key their recommendations with grades A–D ("GRADES OF RECOMMENDATION" in the key);
/// PDF conversion keeps each boxed recommendation as one bold paragraph that starts with its grade letter.
fn mark_graded_recommendations(blocks:&mut Vec<Block>)->Vec<String> {
    if !blocks.iter().any(|b|b.text.contains("GRADES OF RECOMMENDATION")) {return vec![];}
    let re=regex::Regex::new(r"^\*\*([ABCD]) (.+)$").unwrap();let mut add=vec![];
    for (i,b) in blocks.iter().enumerate() {
        if !matches!(b.kind,BlockKind::Paragraph) {continue;}
        let t=b.text.trim();let Some(c)=re.captures(t) else {continue};
        let statement=c[2].replace("**","").split_whitespace().collect::<Vec<_>>().join(" ");
        if statement.len()<40 || !statement.ends_with('.') {continue;}
        add.push((i,Block{block_id:format!("{}-graded-recommendation",b.block_id),kind:BlockKind::Recommendation{label:Some(c[1].to_string()),strength:None,certainty:None,population:None,grading_system:Some("SIGN grades of recommendation A–D (strength of supporting evidence, per the guideline key)".into()),qualifier:None},text:statement,heading_path:b.heading_path.clone(),page:b.page}));
    }
    if add.is_empty() {return vec![];}
    for (i,b) in add.into_iter().rev(){blocks.insert(i+1,b);}
    vec!["Grade A–D recommendations are recognised only as bold paragraphs led by the grade letter; boxed recommendations split across pages or columns may be missed.".into()]
}
/// SIGN guidelines of 2015–2019 (SIGN 50, 2015 edition; e.g. 145, 148) key their recommendations as
/// "KEY TO EVIDENCE STATEMENTS AND RECOMMENDATIONS": strength is carried by the wording ('should' / 'should be
/// considered'), each recommendation box has an "R" in the margin, and good-practice points a ✓. PDF conversion
/// keeps the R as a leading "R\t" / "R " (or a lone "R" line) and splits a box over a heading-like first line and
/// bold continuation lines; the ✓ glyph often comes out as U+FFFD. Recognised here (I2-recs-pacing):
/// - an R box: a block starting with the R marker (or after a lone R), continued by unnumbered heading lines until
///   the sentence ends and by bold paragraph lines; label "R", strength not inferred from the wording;
/// - a good-practice point: a paragraph led by "�" when the key prints "✓ … Recommended best practice"; label ✓.
/// Not used when the guideline carries the pre-2015 grade key or the 2020+ E/C key (handled above).
fn mark_r_recommendations(blocks:&mut Vec<Block>)->Vec<String> {
    let has=|s:&str|blocks.iter().any(|b|b.text.replace("**","").contains(s));
    if !has("KEY TO EVIDENCE STATEMENTS AND RECOMMENDATIONS") || has("GRADES OF RECOMMENDATION") || has("are marked E, while those based on the consensus process are marked C") {return vec![];}
    let tick_key=has("GOOD-PRACTICE POINTS") && blocks.iter().any(|b|{let t=b.text.replace("**","");t.contains("✓") && t.contains("Recommended best practice")});
    let start=regex::Regex::new(r"^(?:\*\*)?R(?:\*\*)?[\t ]+(?:\*\*)?\s*([A-Z(0-9].*)$").unwrap();
    let numbered=regex::Regex::new(r"^\d+(?:\.\d+)*\.?\s").unwrap();
    let page_no=regex::Regex::new(r"^\**\s*(?:\d+\s*\|\s*|\|\s*\d+)\s*\**$").unwrap();
    let plain=|t:&str|crate::common::collapse_ws(&t.replace("**","").replace('\t'," "));
    let is_heading=|b:&Block|matches!(b.kind,BlockKind::Heading{..});
    let lone_r=|b:&Block|plain(&b.text)=="R";
    let caps=|t:&str|t.chars().any(|c|c.is_alphabetic()) && !t.chars().any(|c|c.is_lowercase());
    let mut add:Vec<(usize,Block)>=vec![];let mut gpp=0;let mut i=0;
    while i<blocks.len() {
        let b=&blocks[i];let t=b.text.trim();
        if !matches!(b.kind,BlockKind::Heading{..}|BlockKind::Paragraph) {i+=1;continue;}
        if tick_key && matches!(b.kind,BlockKind::Paragraph) && t.starts_with('\u{FFFD}') {
            let statement=plain(t.trim_start_matches('\u{FFFD}'));
            if statement.len()>=30 {gpp+=1;add.push((i,Block{block_id:format!("{}-practice-point",b.block_id),kind:BlockKind::Recommendation{label:Some("✓".into()),strength:None,certainty:None,population:None,grading_system:None,qualifier:Some("Good-practice point".into())},text:statement,heading_path:b.heading_path.clone(),page:b.page}));}
            i+=1;continue;
        }
        // statement start: "R\t..." in this block, or a lone "R" block followed by the statement
        let (first,mut j)=if let Some(c)=start.captures(t) {(plain(&c[1]),i+1)} else if lone_r(b) && i+1<blocks.len() && !start.is_match(blocks[i+1].text.trim()) {(plain(&blocks[i+1].text),i+2)} else {i+=1;continue};
        let (id,path,page)=(b.block_id.clone(),b.heading_path.clone(),b.page);
        let mut statement=first;let mut last=j-1;
        while j<blocks.len() {
            let n=&blocks[j];let nt=n.text.trim();let p=plain(nt);
            if page_no.is_match(nt) {j+=1;continue;}
            let ended=statement.ends_with('.');
            let stop=start.is_match(nt) || lone_r(n) || nt.starts_with('\u{FFFD}') || nt.starts_with('✓') || p.is_empty();
            let cont=if stop {false} else if is_heading(n) {!ended && !numbered.is_match(&p) && !caps(&p)} else if matches!(n.kind,BlockKind::Paragraph) {nt.contains("**") && (!ended || nt.starts_with("**"))} else {false};
            if !cont {break;}
            statement=format!("{statement} {p}");last=j;j+=1;
        }
        if statement.len()>=30 {add.push((last,Block{block_id:format!("{id}-r-recommendation"),kind:BlockKind::Recommendation{label:Some("R".into()),strength:None,certainty:None,population:None,grading_system:Some("SIGN 50 (2015) key: R marks a recommendation; its strength is expressed by the wording ('should' / 'should be considered'), not a grade".into()),qualifier:None},text:statement,heading_path:path,page}));}
        i=last+1;
    }
    if add.is_empty() {return vec![];}
    let r=add.len()-gpp;
    add.sort_by_key(|(k,_)|*k);for (k,b) in add.into_iter().rev(){blocks.insert(k+1,b);}
    let mut w=vec![format!("{r} 'R' recommendations recognised from the R margin marker of the SIGN 2015 key (statement = R line plus its bold continuation lines); strength is left to the wording, not inferred. Boxes whose R marker the PDF text lost are missed.")];
    if gpp>0 {w.push(format!("{gpp} good-practice points recognised: the ✓ glyph is extracted as U+FFFD and mapped to ✓ per the guideline's key."));}
    w
}
async fn withdrawal_doc(src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
    let p=f.get_ok(&row.url).await?;let base=Url::parse(&p.record.final_url)?;
    if base.origin()!=Url::parse(ROOT)?.origin(){bail!("SIGN landing redirected outside source");}
    let (date,notice)=withdrawal_notice(&p.text()).context("SIGN withdrawal notice no longer present; re-plan")?;
    if row.extra["snapshot"].as_str()!=Some(crate::common::sha256_hex(notice.as_bytes()).as_str()){bail!("SIGN withdrawal notice changed; re-plan");}
    let (blocks,mut warnings)=crate::guidelines::html::blocks_from_html(&notice,Some(&base),None);
    if blocks.is_empty(){bail!("SIGN withdrawal notice empty");}
    warnings.push("Withdrawal notice only: the source page states the guideline was withdrawn and offers no guideline document. The catalog tab status is kept in search_metadata, not as the document status.".into());
    let doc=Html::parse_document(&notice);
    let attachments=doc.select(&sel("a[href]")).filter_map(|a|a.value().attr("href").and_then(|h|common::link(&base,h)).map(|u|json!({"url":u,"label":text_of(&a),"role":"source_referenced_alternative","rights":"unknown; external asset not acquired"}))).collect();
    let mut fetches=vec![];if let Ok(x)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(x);}fetches.push(p.record);
    Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some("withdrawal_notice".into()),version:row.extra["number"].as_str().filter(|s|!s.is_empty() && s.chars().all(|c|c.is_ascii_digit())).map(|s|format!("SIGN {s}")),status:"withdrawn".into(),rights:common::unknown_rights(),date_observations:vec![common::date("Withdrawn",&date,&row.url)],blocks,attachments,fetches,warnings,search_metadata:json!({"catalog_status":row.extra["catalog_status"],"catalog_status_detail":row.extra["status_detail"],"catalog_publication":row.extra["catalog_published"]}),..Default::default()})
}
#[async_trait::async_trait]
impl Adapter for Sign {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0){return Ok(vec![])}let cat=f.get_ok(src.catalog_url.as_deref().unwrap_or(ROOT)).await?;
        let entries=catalog(&cat.text(),&Url::parse(&cat.record.final_url)?)?;let mut out=vec![];
        for e in entries {
            let work=if !e.number.is_empty() && e.number.chars().all(|c|c.is_ascii_digit()) {format!("sign-{}",e.number)} else {format!("sign-title-{}",common::slug(&e.title))};
            let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-{}-unresolved",e.status),url:e.url.clone().unwrap_or_else(||cat.record.final_url.clone()),title:Some(e.title),rendition:None,version:None,extra:json!({"work_key":work,"number":e.number,"catalog_status":e.status,"catalog_published":e.published,"topic":e.topic,"status_detail":e.detail,"catalog_fetch":&cat.record})};
            if let Some(url)=e.url {
                if Url::parse(&url)?.origin()!=Url::parse(ROOT)?.origin() {row.extra["discovery_error"]=json!("SIGN catalog item is externally hosted; full content and rights require independent review");}
                else {match f.get_ok(&url).await {
                    Ok(p)=>match landing(&p.text(),&Url::parse(&p.record.final_url)?,&e.number) {
                        Ok((info,pdf,assets))=>{row.url=pdf.clone();row.rendition=Some("pdf".into());row.extra["landing_url"]=json!(url);row.extra["landing_fetch"]=json!(&p.record);row.extra["document_information"]=json!(info);
                            let label=assets.iter().find(|a|a["url"]==json!(pdf)).and_then(|a|a["label"].as_str()).unwrap_or_default().to_lowercase();
                            row.extra["item_type"]=json!(if label.contains("position statement") {"position_statement"} else {"guideline"});row.extra["assets"]=json!(assets);
                            match f.get_ok(&pdf).await {Ok(p) if p.is_pdf()=>{row.doc_key=format!("{work}-{}",&p.record.sha256[..12]);row.extra["pdf_sha256"]=json!(p.record.sha256);},Ok(_)=>row.extra["discovery_error"]=json!("SIGN full guideline asset is not PDF"),Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}
                        },Err(err)=>match withdrawal_notice(&p.text()) {
                            Some((date,notice))=>{let hash=crate::common::sha256_hex(notice.as_bytes());row.doc_key=format!("{work}-withdrawn-{}",&hash[..12]);row.rendition=Some("html".into());row.extra["role"]=json!("withdrawal_notice");row.extra["withdrawn_date"]=json!(date);row.extra["snapshot"]=json!(hash);row.extra["landing_url"]=json!(url);row.extra["landing_fetch"]=json!(&p.record);},
                            None=>row.extra["discovery_error"]=json!(format!("{err:#}")),
                        }},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))}}
            } else {row.extra["discovery_error"]=json!("Catalog entry has no source-emitted terminal link; inventory only");}
            out.push(row);if limit.is_some_and(|n|out.len()>=n){break;}
        }Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("SIGN discovery: {e}");}
        if row.extra["role"]=="withdrawal_notice" {return withdrawal_doc(src,f,row).await;}
        let p=f.get_ok(&row.url).await?;
        if !p.is_pdf(){bail!("SIGN guideline response is not PDF");}if row.extra["pdf_sha256"].as_str().is_some_and(|s|s!=p.record.sha256){bail!("SIGN PDF changed since discovery; re-plan");}
        let (mut blocks,mut warnings,_)=blocks_for(&p,None,30).await?;if blocks.is_empty(){bail!("SIGN PDF extracted no text");}
        let rights=pdf_rights(&blocks,&row.url);warnings.extend(mark_practice_points(&mut blocks));warnings.extend(mark_graded_recommendations(&mut blocks));warnings.extend(mark_r_recommendations(&mut blocks));warnings.push("Publisher-linked summaries, update reports, app and patient materials have not been acquired; asset rights are separate.".into());warnings.push("Catalog status reflects the saved source tab; source-wide enumeration and status reconciliation remain incomplete.".into());
        let mut dates=vec![];let date=regex::Regex::new(r"(?i)First published\s+((?:January|February|March|April|May|June|July|August|September|October|November|December)\s+\d{4})").unwrap();
        if let Some(m)=blocks.iter().find_map(|b|date.captures(&b.text).map(|x|x[1].to_string())) {dates.push(common::date("First published",&m,&row.url));}
        let mut fetches=vec![];for k in ["catalog_fetch","landing_fetch"]{if let Ok(x)=serde_json::from_value::<crate::guidelines::doc::FetchRecord>(row.extra[k].clone()){fetches.push(x);}}fetches.push(p.record);
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),item_type:Some(row.extra["item_type"].as_str().unwrap_or("guideline").into()),version:row.extra["number"].as_str().filter(|s|!s.is_empty() && s.chars().all(|c|c.is_ascii_digit())).map(|s|format!("SIGN {s}")),status:row.extra["catalog_status"].as_str().unwrap_or("unknown").into(),rights,date_observations:dates,blocks,attachments:row.extra["assets"].as_array().cloned().unwrap_or_default(),fetches,warnings,search_metadata:json!({"landing_url":row.extra["landing_url"],"document_information":row.extra["document_information"],"catalog_status_detail":row.extra["status_detail"],"catalog_publication":row.extra["catalog_published"]}),..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/catalog.html"));
    const ASTHMA:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/asthma.html"));
    const LANDING167:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/landing167.html"));
    const LANDING172:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/landing172.html"));
    const PDF:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/sign-167-page-10.pdf"));
    const MARKERS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/sign167-marker-blocks.json"));
    #[test]
    fn publisher_catalog_is_not_just_the_first_page() {
        let rows=catalog(CAT,&Url::parse(ROOT).unwrap()).unwrap();
        assert_eq!(rows.iter().filter(|r|r.status=="current").count(),48);
        assert!(rows.iter().any(|r|r.number=="167" && r.status=="current" && r.url.as_deref().is_some_and(|u|u.ends_with("/care-of-deteriorating-patients/"))));
        assert!(rows.iter().any(|r|r.number=="139" && r.status=="archived" && r.detail.contains("Withdrawn June 2023")));
        assert!(rows.iter().any(|r|r.status=="in-scoping" && r.title=="Difficult and severe asthma" && r.topic=="Respiratory" && r.published.is_empty() && r.url.is_none() && r.detail.contains("In Scoping")));
        assert!(rows.iter().any(|r|r.status=="in-development" && r.title.contains("Palliative Care") && r.published.is_empty() && r.detail.contains("Estimated publication: Spring 2026") && r.url.is_none()));
        assert!(rows.iter().any(|r|r.status=="proposed" && r.title=="Management of metastatic breast cancer" && r.published.is_empty() && r.detail.contains("Rejected") && r.detail.contains("metastatic-breast-cancer-proposal") && r.url.is_none()));
    }
    #[test]
    fn genuine_167_172_asset_roles_are_separate() {
        let u167=Url::parse("https://www.sign.ac.uk/guidelines/care-of-deteriorating-patients/").unwrap();
        let (_,pdf167,a167)=landing(LANDING167,&u167,"167").unwrap();assert_eq!(pdf167,"https://www.sign.ac.uk/media/2091/sign-167-care-of-deteriorating-patients.pdf");
        assert!(a167.iter().any(|a|a["label"].as_str().is_some_and(|l|l.contains("Quick reference guide")) && a["role"]!="candidate_full_guideline"));
        let u172=Url::parse("https://www.sign.ac.uk/guidelines/prevention-and-remission-of-type-2-diabetes/").unwrap();
        let (_,pdf172,a172)=landing(LANDING172,&u172,"172").unwrap();assert!(pdf172.ends_with("/20251103-type-2-diabetes-prevention-gl-publication-v4.pdf"));
        assert!(a172.iter().any(|a|a["role"]=="update_report"));assert!(a172.iter().any(|a|a["role"]=="patient_information"));
        assert!(a172.iter().any(|a|a["label"].as_str().is_some_and(|l|l.contains("(app)")) && a["role"]!="candidate_full_guideline"));
    }
    #[test]
    fn source_asset_roles_and_partial_marker_handling() {
        let url=Url::parse("https://www.sign.ac.uk/guidelines/british-guideline-on-the-management-of-asthma/").unwrap();
        let (info,pdf,assets)=landing(ASTHMA,&url,"158").unwrap();
        assert!(info.contains("158") && info.contains("November 2024"));
        assert!(pdf.ends_with("/sign-158-2024-british-guideline-on-the-management-of-asthma-may-2025.pdf"));
        assert!(assets.iter().any(|a|a["label"].as_str().is_some_and(|s|s.contains("Decision aid")) && a["role"]!="candidate_full_guideline"));
        let mut blocks:Vec<Block>=serde_json::from_str(MARKERS).unwrap();let warnings=mark_practice_points(&mut blocks);
        assert!(blocks.iter().any(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),qualifier:Some(q),..} if l=="✓" && q=="Good-practice point") && b.text.contains("AoMRC")));
        let rs:Vec<_>=blocks.iter().filter(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l=="R")).collect();
        assert_eq!(rs.len(),3);
        assert!(rs[0].text.contains("completed with input from the patient and their family") && rs[0].text.contains("accessible to all care providers"));
        assert!(rs[1].text.ends_with("in the event of cardiac or respiratory arrest."));
        assert!(rs[2].text.ends_with("sole focus of the anticipatory care plan."));
        assert!(rs.iter().all(|b|matches!(&b.kind,BlockKind::Recommendation{strength:None,certainty:None,qualifier:Some(q),..} if q=="C: consensus process")));
        assert!(warnings.iter().any(|w|w.contains("visual comparison")));
    }
    #[tokio::test]
    async fn offline_genuine_167_page_and_revision() {
        let row={let doc=Html::parse_document(CAT);let selected=doc.select(&sel("#current tbody tr")).find(|r|r.select(&sel("a[href]")).any(|a|a.value().attr("href")==Some("/guidelines/care-of-deteriorating-patients/"))).unwrap().html();selected};
        let bounded=format!("<div id='myTabContent'><div id='current'><table><tbody>{row}</tbody></table></div></div>");
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        let page="https://www.sign.ac.uk/guidelines/care-of-deteriorating-patients/";let pdf="https://www.sign.ac.uk/media/2091/sign-167-care-of-deteriorating-patients.pdf";
        f.seed_cache_for_tests(ROOT,bounded.as_bytes(),"text/html");f.seed_cache_for_tests(page,LANDING167.as_bytes(),"text/html");f.seed_cache_for_tests(pdf,PDF,"application/pdf");
        let src:SourceEntry=serde_json::from_value(json!({"id":"sign","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let plan=Sign.discover(&src,&f,None).await.unwrap();assert_eq!(plan.len(),1);assert_eq!(plan[0].url,pdf);
        let d=Sign.extract(&src,&f,&plan[0]).await.unwrap();assert_eq!(d.version.as_deref(),Some("SIGN 167"));
        assert!(d.blocks.iter().any(|b|b.text.to_lowercase().contains("anticipatory care plan")));
        assert_eq!(d.rights.status,"unknown","single clinical page does not include licence evidence");
        assert!(d.warnings.iter().any(|w|w.contains("visual comparison")));
        let mut changed=PDF.to_vec();changed.extend_from_slice(b"\nchanged snapshot");f.seed_cache_for_tests(pdf,&changed,"application/pdf");assert!(Sign.extract(&src,&f,&plan[0]).await.is_err());
    }

}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod landing_variant_tests {
    //! Landing variants from the 2026-11-14 cache: withdrawn (162), own-number choice (116 vs 154), NICE adoption
    //! labelled "Download guideline (PDF)" (NG197), position statement (165), and the pre-2015 A–D grade key (129).
    use super::*;
    const W162:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/landing162-withdrawn.html"));
    const L116:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/landing116-two-full-pdfs.html"));
    const NG197:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/landing-ng197-download-guideline.html"));
    const L165:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/landing165-position-statement.html"));
    const GRADED:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/sign129-graded-blocks.json"));
    const W162_URL:&str="https://www.sign.ac.uk/guidelines/assessment-of-covid-19-in-primary-care/";
    fn u(s:&str)->Url{Url::parse(s).unwrap()}
    #[test]
    fn own_number_label_and_other_publication_labels() {
        let (_,pdf,assets)=landing(L116,&u("https://www.sign.ac.uk/guidelines/management-of-diabetes/"),"116").unwrap();
        assert_eq!(pdf,"https://www.sign.ac.uk/media/1054/sign116.pdf");
        assert!(assets.iter().any(|a|a["role"]=="other_sign_guideline" && a["url"]=="https://www.sign.ac.uk/media/1090/sign154.pdf"));
        assert!(assets.iter().any(|a|a["label"].as_str().is_some_and(|l|l.contains("quick reference")) && a["role"]!="candidate_full_guideline"));
        let (_,pdf,_)=landing(NG197,&u("https://www.sign.ac.uk/guidelines/shared-decision-making-nice-guideline-ng197/"),"").unwrap();
        assert!(pdf.ends_with("/media/2008/20220927-nice-shared-decision-making.pdf"));
        let (_,pdf,assets)=landing(L165,&u("https://www.sign.ac.uk/guidelines/long-acting-buprenorphine/"),"165").unwrap();
        assert!(pdf.ends_with("/media/1947/buprenorphine-position-statement-sign-165.pdf"));
        assert_eq!(assets.iter().filter(|a|a["role"]=="candidate_full_guideline").count(),1,"consultation and consensus reports are supporting assets");
    }
    #[tokio::test]
    async fn withdrawn_landing_becomes_a_withdrawal_notice() {
        assert!(landing(W162,&u(W162_URL),"162").is_err());
        let (date,notice)=withdrawal_notice(W162).unwrap();assert_eq!(date,"1 September 2023");assert!(notice.contains("gov.uk"));
        assert!(withdrawal_notice(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/landing167.html"))).is_none());
        let bounded=format!("<div id='myTabContent'><div id='current'><table><tbody><tr><td>162</td><td><a href='/guidelines/assessment-of-covid-19-in-primary-care/'>Assessment of COVID-19 in primary care</a></td><td>Infectious diseases</td><td>2020</td></tr></tbody></table></div></div>");
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(ROOT,bounded.as_bytes(),"text/html");f.seed_cache_for_tests(W162_URL,W162.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"sign","access":"public","enabled":true,"catalog_url":ROOT})).unwrap();
        let plan=Sign.discover(&src,&f,None).await.unwrap();assert_eq!(plan.len(),1);assert!(plan[0].extra["discovery_error"].is_null());
        let d=Sign.extract(&src,&f,&plan[0]).await.unwrap();
        assert_eq!(d.item_type.as_deref(),Some("withdrawal_notice"));assert_eq!(d.status,"withdrawn");assert_eq!(d.version.as_deref(),Some("SIGN 162"));
        assert!(d.date_observations.iter().any(|x|x.label=="Withdrawn" && x.raw=="1 September 2023"));
        assert!(d.blocks.iter().any(|b|b.text.contains("was withdrawn")));assert!(d.attachments.iter().any(|a|a["url"].as_str().unwrap().starts_with("https://www.gov.uk/")));
        assert_eq!(d.search_metadata["catalog_status"],"current");
    }
    const SIGN148:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/sign/sign148-p2-12-13.pdf"));
    fn dump(b:&[Block])->String{b.iter().take(120).map(|x|format!("{} {:?} {:?}\n",x.block_id,x.kind,x.text.chars().take(140).collect::<String>())).collect()}
    /// SIGN 148 (2016) pages 2, 12, 13 of the real PDF: the SIGN 50 (2015) key and "R" boxes (I2-recs-pacing).
    #[tokio::test]
    async fn sign_2015_key_r_boxes_become_recommendations() {
        let f=crate::guidelines::fetch::Fetched{body:SIGN148.to_vec(),record:crate::guidelines::doc::FetchRecord{url:"https://www.sign.ac.uk/media/1084/sign148.pdf".into(),final_url:"https://www.sign.ac.uk/media/1084/sign148.pdf".into(),status:200,content_type:Some("application/pdf".into()),..Default::default()}};
        let (mut blocks,_,_)=blocks_for(&f,None,0).await.unwrap();
        assert!(mark_practice_points(&mut blocks).len()==1 && mark_graded_recommendations(&mut blocks).is_empty());
        let before=blocks.iter().filter(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l=="R")).count();
        let w=mark_r_recommendations(&mut blocks);
        let rs:Vec<&Block>=blocks.iter().filter(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l=="R")).collect();
        assert_eq!(before,0,"{}",dump(&blocks));
        assert!(rs.len()>=6,"{} R recs\n{}",rs.len(),dump(&blocks));
        assert!(w[0].contains("'R' recommendations"));
        assert!(rs.iter().any(|b|b.text.starts_with("In patients with suspected acute coronary syndrome, measurement of cardiac troponin") && b.text.ends_with("to rule out myocardial infarction.")),"{:#?}",rs.iter().map(|b|&b.text).collect::<Vec<_>>());
        assert!(rs.iter().any(|b|b.text.starts_with("When primary percutaneous coronary intervention cannot be provided within 120 minutes of ECG diagnosis") && b.text.ends_with("thrombolytic therapy.")),"{:#?}",rs.iter().map(|b|&b.text).collect::<Vec<_>>());
        assert!(rs.iter().any(|b|b.text.starts_with("Patients with acute coronary syndrome should receive dual antiplatelet therapy for six months.")));
        assert!(rs.iter().all(|b|matches!(&b.kind,BlockKind::Recommendation{strength:None,certainty:None,grading_system:Some(g),..} if g.contains("wording"))));
        assert!(rs.iter().all(|b|!b.text.starts_with("R ") && !b.text.contains("**")));
        // other keys are left to their own markers
        let mut graded:Vec<Block>=serde_json::from_str(GRADED).unwrap();assert!(mark_r_recommendations(&mut graded).is_empty());
    }
    #[test]
    fn sign_2015_split_r_box_and_unreadable_tick() {
        let mk=|id:&str,kind:BlockKind,t:&str|Block{block_id:id.into(),kind,text:t.into(),heading_path:vec!["Initial management".into()],page:None};
        let p=||BlockKind::Paragraph;let h=||BlockKind::Heading{level:5};
        // verbatim shapes from the SIGN 148 probe dump (m00004, m00019-20, m00164-172, m00182-183, m00276-279)
        let mut b=vec![mk("k",h(),"KEY TO EVIDENCE STATEMENTS AND RECOMMENDATIONS"),mk("g",h(),"GOOD-PRACTICE POINTS"),mk("t",p(),"✓Recommended best practice based on the clinical experience of the guideline development group."),
            mk("a",h(),"R\tIn patients with suspected acute coronary syndrome, serum troponin concentration should be"),mk("a2",h(),"measured at presentation to guide appropriate management and treatment."),
            mk("c",p(),"**R\tSex-specific thresholds of cardiac troponin should be used for the diagnosis of myocardial infarction in men and women.**"),
            mk("d",p(),"\u{FFFD} Further troponin measurements may be necessary in patients who present within three hours of the onset of chest pain."),
            mk("e",h(),"R\tPatients with acute coronary syndrome should be managed within a specialist cardiology service."),mk("e2",h(),"4.2 CARDIAC MONITORING"),
            mk("f",h(),"R\tPatients with confirmed acute coronary syndrome and diabetes mellitus or marked hyperglycaemia"),mk("f2",p(),"(>11.0 mmol/L) **should have immediate blood glucose control aiming for a target glucose** **concentration of 7.0 to 10.9 mmol/L**"),mk("f3",p(),"***16 |***"),mk("f4",p(),"Acute coronary syndrome 5 • Reperfusion therapy"),
            mk("x",p(),"Rescue PCI is undertaken within 12 hours of thrombolysis when there is an apparent failure to reperfuse.")];
        let w=mark_r_recommendations(&mut b);assert_eq!(w.len(),2);
        let recs:Vec<(String,String)>=b.iter().filter_map(|x|match &x.kind{BlockKind::Recommendation{label:Some(l),..}=>Some((l.clone(),x.text.clone())),_=>None}).collect();
        assert_eq!(recs,vec![
            ("R".into(),"In patients with suspected acute coronary syndrome, serum troponin concentration should be measured at presentation to guide appropriate management and treatment.".into()),
            ("R".into(),"Sex-specific thresholds of cardiac troponin should be used for the diagnosis of myocardial infarction in men and women.".into()),
            ("✓".into(),"Further troponin measurements may be necessary in patients who present within three hours of the onset of chest pain.".into()),
            ("R".into(),"Patients with acute coronary syndrome should be managed within a specialist cardiology service.".into()),
            ("R".into(),"Patients with confirmed acute coronary syndrome and diabetes mellitus or marked hyperglycaemia (>11.0 mmol/L) should have immediate blood glucose control aiming for a target glucose concentration of 7.0 to 10.9 mmol/L".into()),
        ]);
        // the R recommendation follows its last source line; source blocks stay
        let pos=|id:&str|b.iter().position(|x|x.block_id==id).unwrap();
        assert_eq!(pos("a-r-recommendation"),pos("a2")+1);assert!(b.iter().any(|x|x.block_id=="x" && matches!(x.kind,BlockKind::Paragraph)));
        // no key, no marking
        let mut nokey:Vec<Block>=b.iter().filter(|x|x.block_id!="k" && !matches!(x.kind,BlockKind::Recommendation{..})).cloned().collect();assert!(mark_r_recommendations(&mut nokey).is_empty());
    }
    #[test]
    fn pre_2015_grade_letters_become_recommendations() {
        let mut blocks:Vec<Block>=serde_json::from_str(GRADED).unwrap();
        blocks.push(Block{block_id:"neg".into(),kind:BlockKind::Paragraph,text:"**A national clinical guideline**".into(),heading_path:vec![],page:None});
        let w=mark_graded_recommendations(&mut blocks);assert!(!w.is_empty());
        let recs:Vec<_>=blocks.iter().filter_map(|b|match &b.kind{BlockKind::Recommendation{label,grading_system,..}=>Some((label.clone(),grading_system.clone(),b.text.clone())),_=>None}).collect();
        assert_eq!(recs.len(),5);
        assert_eq!(recs[0].0.as_deref(),Some("A"));assert_eq!(recs[0].2,"To minimise the risk of bleeding, the lowest recommended dose of aspirin should be used for the clinical indication.");
        assert_eq!(recs[1].0.as_deref(),Some("B"));assert!(recs[1].2.contains("(age<65 and lone AF) do not require antithrombotic therapy."));
        assert!(recs.iter().all(|r|r.1.as_deref().is_some_and(|g|g.contains("A–D"))));
        let mut no_key:Vec<Block>=serde_json::from_str::<Vec<Block>>(GRADED).unwrap().into_iter().skip(1).collect();
        assert!(mark_graded_recommendations(&mut no_key).is_empty());assert!(!no_key.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})));
    }
}
