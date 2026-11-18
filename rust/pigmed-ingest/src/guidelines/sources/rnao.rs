//! RNAO catalog -> source detail -> complete English guideline PDF.
//! HTML panels are independently inventoried; the detailed PDF supplies statement text and grades.
use super::{common, static_list::blocks_for, Adapter, PlanRow};
use crate::guidelines::doc::{Block, BlockKind, DateObservation, GuidelineDoc, Relation, Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;

pub struct Rnao;
const ROOT:&str="main#main article.node--type-best-practice-guideline";
fn identity(url:&Url)->String {format!("{}{}{}",url.host_str().unwrap_or_default().trim_start_matches("www."),url.path().trim_end_matches('/'),url.query().map(|q|format!("?{q}")).unwrap_or_default())}
fn same_source(a:&Url,b:&Url)->bool {a.host_str().map(|s|s.trim_start_matches("www."))==b.host_str().map(|s|s.trim_start_matches("www."))}
fn catalog(html:&str,base:&Url)->Result<(Vec<(String,String)>,Option<String>)>{
    let doc=Html::parse_document(html);
    let root=doc.select(&sel("main#main")).next().context("RNAO catalog main missing")?;
    let mut seen=HashSet::new();let mut out=vec![];
    for row in root.select(&sel(".views-row")) {
        for a in row.select(&sel("a[href]")) {
            let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};
            let u=Url::parse(&url)?;let title=text_of(&a);
            if !same_source(&u,base)||!u.path().starts_with("/bpg/guidelines/")||title.is_empty(){continue;}
            if seen.insert(identity(&u)){out.push((url,title));}
        }
    }
    if out.is_empty(){bail!("RNAO catalog contains no guideline cards");}
    let next=root.select(&sel(".pager__item--next a[href][rel='next']")).next().and_then(|a|common::link(base,a.value().attr("href")?));
    if let Some(ref next)=next {let u=Url::parse(next)?;if !same_source(&u,base)||u.path()!=base.path(){bail!("RNAO next page leaves catalog path");}}
    Ok((out,next))
}
struct Detail {title:String,pdf:String,version:Option<String>,published:Option<String>,dates:Vec<DateObservation>,attachments:Vec<Value>,summary:Vec<Value>,revision:String,node_id:Option<String>,language:Option<String>,status:String,relations:Vec<Relation>,predecessors:Vec<Value>}
fn detail(html:&str,base:&Url)->Result<Detail>{
    let doc=Html::parse_document(html);
    let root=doc.select(&sel(ROOT)).next().context("RNAO modern detail root missing; legacy template requires separate validation")?;
    let title=root.select(&sel("h1")).next().map(|e|text_of(&e)).context("RNAO title missing")?;
    let mut pdfs=vec![];let mut seen=HashSet::new();
    for a in root.select(&sel(".field--name-field-bpg-translations a[href]")) {
        if let Some(u)=common::link(base,a.value().attr("href").unwrap_or_default()){if seen.insert(u.clone()){pdfs.push(u);}}
    }
    // Actual multilingual detail template: English is the primary split-button anchor;
    // French/Spanish are separately labelled menuitems and must not become competing PDFs.
    for a in root.select(&sel(".rnc-split-button__btn a[href]")){
        if text_of(&a)!="Download (en)"{continue;}
        if let Some(u)=common::link(base,a.value().attr("href").unwrap_or_default()){if seen.insert(u.clone()){pdfs.push(u);}}
    }
    if pdfs.len()!=1{bail!("RNAO expected one unambiguous source-labelled guideline download, found {}",pdfs.len());}
    let pdf=pdfs.remove(0);
    let mut version=None;let mut dates=vec![];let mut published=None;
    let details=root.select(&sel("table.guideline-details")).next().context("RNAO guideline-details metadata table missing")?;
    for row in details.select(&sel("tr")) {
        let c=common::cells(row);if c.len()!=2{continue;}
        let label=text_of(&c[0]);let value=text_of(&c[1]);
        match label.trim_end_matches(':') {
            "Current edition"=>version=Some(value),
            "Publish date"=>{let d=common::date(label.trim_end_matches(':'),&value,base.as_str());published=d.normalized.clone();dates.push(d);},
            "Anticipated revision publication"=>dates.push(common::date(label.trim_end_matches(':'),&value,base.as_str())),
            _=>{}
        }
    }
    let mut status="unknown".to_string();let mut relations=vec![];let mut predecessors=vec![];
    if let Some(revisions)=root.select(&sel("#tab-content-status .revisions")).next(){
        let text=text_of(&revisions);let current=regex::Regex::new(r"Current edition published (\d{4})\.").unwrap();
        let mut evidence=base.clone();evidence.set_fragment(Some("tab-content-status"));
        if let Some(c)=current.captures(&text){status="current".into();dates.push(common::date("Current edition published",&c[1],evidence.as_str()));}
        if let Some(replacement)=revisions.select(&sel("p")).find(|p|text_of(p).contains("replaces the previous edition BPG")){
            let statement=text_of(&replacement);
            // The revision container also holds a shared recruitment/contact block.
            // Only the immediate publisher-labelled predecessor list describes replacements.
            let list=replacement.next_siblings().filter_map(ElementRef::wrap).next().filter(|e|e.value().name()=="ul")
                .context("RNAO predecessor statement has no adjacent predecessor list")?;
            for item in list.children().filter_map(ElementRef::wrap).filter(|e|e.value().name()=="li"){
                let target=item.select(&sel("a[href]")).next().and_then(|a|common::link(base,a.value().attr("href")?)).filter(|u|u!=base.as_str());
                predecessors.push(json!({"predecessor_title":text_of(&item),"statement":statement,"target_url":target,"evidence_url":evidence.as_str()}));
                if let Some(target)=target{relations.push(Relation{rel:"supersedes".into(),target,scope:Some("previous edition BPG".into()),evidence_url:Some(evidence.to_string())});}
            }
        }
    }
    let panel=root.select(&sel("#tab-content-recommendations")).next().context("RNAO preloaded recommendation panel missing")?;
    let label=regex::Regex::new(r"(?i)^(Good practice statement|Recommendation)\s+\d+(?:\.\d+)+[a-z]?:?$").unwrap();
    let mut summary=vec![];
    for item in panel.select(&sel(".rnc-accordion__item")) {
        let heading=item.select(&sel(".rnc-accordion__titletext")).next().map(|e|text_of(&e));
        for p in item.select(&sel(".rnc-accordion__panel p")) {
            let Some(strong)=p.select(&sel("strong")).next()else{continue};let l=text_of(&strong);
            if !label.is_match(&l){continue;}
            let text=text_of(&p);let mut statement=text.strip_prefix(&l).unwrap_or(&text).trim_start_matches(|c:char|c==':'||c.is_whitespace()).to_string();
            // Source continuations include numbered assessment questions, multi-level
            // clinical lists and marked footnotes; keep them with their labelled statement.
            for next in p.next_siblings().filter_map(ElementRef::wrap){
                if next.value().name().starts_with('h'){break;}
                if next.select(&sel("strong")).any(|s|label.is_match(&text_of(&s))){break;}
                let tail=text_of(&next);if !tail.is_empty(){statement.push('\n');statement.push_str(&tail);}
            }
            let mut anchor=base.clone();anchor.set_fragment(p.ancestors().filter_map(scraper::ElementRef::wrap).find(|e|e.value().classes().any(|c|c=="rnc-accordion__panel")).and_then(|e|e.value().id()));
            summary.push(json!({"label":l,"statement":statement,"heading":heading,"source_url":anchor.as_str()}));
        }
    }
    if summary.is_empty(){bail!("RNAO recommendation panel contains no recognized source-labelled statements");}
    let mut attachments=vec![json!({"url":pdf,"role":"full_guideline","label":"Download"})];
    seen.clear();seen.insert(pdf.clone());
    for a in root.select(&sel(".rnc-split-button__dropdown-menu a[href]")){
        if let Some(u)=common::link(base,a.value().attr("href").unwrap_or_default()){
            if seen.insert(u.clone()){attachments.push(json!({"url":u,"label":text_of(&a),"role":"linked_translation","rights":"unknown"}));}
        }
    }
    for (selector,role) in [("#tab-content-methodology a[href]","methodology_document"),("#tab-content-implementation a[href]","implementation_document")] {
        for a in root.select(&sel(selector)) {
            let Some(u)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};
            let parsed=Url::parse(&u)?;
            if parsed.path().starts_with("/media/")&&seen.insert(u.clone()){attachments.push(json!({"url":u,"label":text_of(&a),"role":role}));}
        }
    }
    for video in root.select(&sel(".field--name-field-getting-started-video iframe[src]")) {
        if let Some(url)=common::link(base,video.value().attr("src").unwrap_or_default()){
            if seen.insert(url.clone()){attachments.push(json!({"url":url,"role":"guideline_overview_video","label":video.value().attr("title")}));}
        }
    }
    let revision=crate::common::sha256_hex(serde_json::to_string(&(&title,&pdf,&version,&dates,&summary,&attachments,&status,&relations,&predecessors))?.as_bytes());
    let language=text_of(&root).contains("Available in English").then(||"en".into());
    Ok(Detail{title,pdf,version,published,dates,attachments,summary,revision,node_id:root.value().attr("data-history-node-id").map(String::from),language,status,relations,predecessors})
}
fn unformat(s:&str)->String {s.replace("<u>","").replace("</u>","").replace("**","")}
fn pdf_recommendations(blocks:&mut Vec<Block>)->Vec<Value>{
    // Actual editions use standalone headings, bold inline labels, or grouped summary rows.
    let label=regex::Regex::new(r"(?i)^(Good practice statement|Recommendation)\s+(\d+(?:\.\d+)+[a-z]?):\s*(.*)$").unwrap();
    let grade=regex::Regex::new(r"(?i)^(Strength of the recommendation|Certainty of the evidence of effects|Confidence in evidence):\s*(.+)$").unwrap();
    let level=regex::Regex::new(r"(?i)^Level of Evidence\s*=\s*(.+)$").unwrap();
    let has_grade=blocks.iter().any(|b|b.text.contains("GRADE"));
    let has_cerqual=blocks.iter().any(|b|b.text.contains("CERQual"));
    let clean=|s:&str|unformat(s).trim().trim_start_matches(|c:char|matches!(c,'▪'|'■'|'•')).trim().to_string();
    let mut records=vec![];let mut changes=vec![];let mut mapped=HashSet::new();
    for (i,b) in blocks.iter().enumerate(){
        let raw=clean(&b.text);let Some(c)=label.captures(&raw)else{continue};
        if !matches!(b.kind,BlockKind::Heading{..}|BlockKind::Paragraph){continue;}
        let name=format!("{} {}",&c[1],&c[2]);let gps=c[1].eq_ignore_ascii_case("Good practice statement");
        let (start,mut text)=if c[3].is_empty(){
            let Some(next)=blocks.get(i+1).filter(|n|matches!(n.kind,BlockKind::Paragraph))else{continue};(i+1,clean(&next.text))
        }else{(i,c[3].to_string())};
        let mut strength=None;let mut certainty=None;let mut grading_system=None;let mut observations=vec![];let mut ids=vec![blocks[start].block_id.clone()];let mut reached_grade=false;
        for next in blocks.iter().skip(start+1){
            let raw=clean(&next.text);let lower=raw.to_lowercase();
            if label.is_match(&raw)||lower.starts_with("discussion of evidence")||lower.starts_with("good practice statement criteria"){break;}
            if let Some(c)=grade.captures(&raw){
                reached_grade=true;observations.push(json!({"label":&c[1],"value":&c[2],"block_id":next.block_id}));
                if !gps{match c[1].to_lowercase().as_str(){"strength of the recommendation"=>strength=Some(c[2].to_string()),"confidence in evidence"=>{certainty=Some(c[2].to_string());grading_system=has_cerqual.then(||"GRADE-CERQual".into());},_=>{certainty=Some(c[2].to_string());grading_system=has_grade.then(||"GRADE".into());}}}continue;
            }
            if let Some(c)=level.captures(&raw){reached_grade=true;observations.push(json!({"label":"Level of Evidence","value":&c[1],"block_id":next.block_id}));if !gps{certainty=Some(c[1].to_string());}continue;}
            if matches!(next.kind,BlockKind::Heading{..}){break;}
            if !reached_grade&&!raw.is_empty(){text.push('\n');text.push_str(&next.text);ids.push(next.block_id.clone());}
        }
        if text.is_empty(){continue;}
        mapped.insert(name.to_lowercase());
        records.push(json!({"label":name,"block_id":blocks[start].block_id,"statement_source_blocks":ids,"grade_observations":observations,"strength":strength,"certainty":certainty}));
        changes.push((start,text,BlockKind::Recommendation{label:Some(name),strength,certainty,population:None,grading_system,qualifier:gps.then(||"Good practice statement".into())}));
    }
    for (i,text,kind) in changes{blocks[i].text=text;blocks[i].kind=kind;}
    // End-of-life deliberately assigns one evidence range to a grouped row. Preserve the
    // combined source labels and complete statement; never spread that grade to sublabels.
    let grouped=regex::Regex::new(r"^\d+(?:\.\d+)+(?:\s+\d+(?:\.\d+)+)*$").unwrap();
    let roman=regex::Regex::new(r"^(?:I[ab]?|II[ab]?|III|IV|V)(?:\s*[–−-]\s*(?:I[ab]?|II[ab]?|III|IV|V))?\*?(?:,\s*(?:I[ab]?|II[ab]?|III|IV|V)(?:\s*[–−-]\s*(?:I[ab]?|II[ab]?|III|IV|V))?\*?)*$").unwrap();
    let mut insertions=vec![];
    for (i,b) in blocks.iter().enumerate(){
        if !matches!(b.kind,BlockKind::Table{..}){continue;}
        let lines=b.text.lines().collect::<Vec<_>>();let Some(header)=lines.first()else{continue};
        let modern=header.contains("Recommendations and good practice statements")&&header.contains("Strength of the recommendation");
        let older=(header.contains("RECOMMENDATION")&&header.contains("Level of Evidence"))||
            (b.heading_path.iter().any(|h|h=="Summary of Recommendations")&&{
                let first=header.trim().trim_start_matches('|').trim_end_matches('|').split('|').map(str::trim).collect::<Vec<_>>();
                first.len()==3&&grouped.is_match(first[0])&&roman.is_match(first[2])
            });
        if !modern&&!older{continue;}
        let mut statements=vec![];
        // A continuation table may start with a clinical row which the PDF converter
        // represented as the Markdown header. Validate every line, including that row.
        for (n,line) in lines.iter().enumerate(){
            let cells=line.trim().trim_start_matches('|').trim_end_matches('|').split('|').map(str::trim).collect::<Vec<_>>();if cells.len()<2{continue;}
            let (name,text,strength,certainty,system,qualifier)=if modern{
                let raw=clean(cells[0]);let Some(c)=label.captures(&raw)else{continue};let name=format!("{} {}",&c[1],&c[2]);if mapped.contains(&name.to_lowercase()){continue;}
                let gps=c[1].eq_ignore_ascii_case("Good practice statement");let printed=cells[1];
                if !gps&&!matches!(printed,"Strong"|"Conditional"){continue;}
                (name,c[3].to_string(),(!gps).then(||printed.to_string()),None,(!gps&&has_grade).then(||"GRADE".to_string()),Some(if gps{format!("Good practice statement; source strength: {printed}")}else{"Summary of recommendations".into()}))
            }else{
                if cells.len()!=3||!grouped.is_match(cells[0])||!roman.is_match(cells[2]){continue;}
                (cells[0].to_string(),cells[1].to_string(),None,Some(cells[2].to_string()),None,Some("Grouped source recommendation row; evidence range applies to the entire row".into()))
            };
            let id=format!("{}-summary-{}",b.block_id,n+1);let source_text=format!("{name}: {text}");
            records.push(json!({"label":name,"block_id":id,"source_table":b.block_id,"source_row":n+1,"strength":strength,"certainty":certainty,"printed_cells":cells}));
            statements.push(Block{block_id:id,kind:BlockKind::Recommendation{label:Some(name),strength,certainty,population:None,grading_system:system,qualifier},text:source_text,heading_path:b.heading_path.clone(),page:b.page});
        }
        if !statements.is_empty(){insertions.push((i+1,statements));}
    }
    for (i,rows) in insertions.into_iter().rev(){blocks.splice(i..i,rows);}
    records
}
fn pdf_rights(blocks:&[Block],url:&str)->Rights{
    let mut r=common::unknown_rights();
    if let Some((i,b))=blocks.iter().enumerate().find(|(_,b)|b.heading_path.last().is_some_and(|h|h=="Copyright")&&b.text.starts_with("With the exception of those portions")) {
        r.status="restricted".into();r.note=Some(b.text.clone());r.restrictions.push(b.text.clone());r.evidence_urls.push(url.into());r.terms_url=Some(url.into());
        if let Some(cite)=blocks.get(i+1).filter(|n|n.heading_path.last().is_some_and(|h|h=="Copyright")){r.attribution=Some(cite.text.clone());}
        if r.attribution.as_ref().is_some_and(|s|s.starts_with("Registered Nurses’ Association of Ontario")){r.rights_holders.push("Registered Nurses’ Association of Ontario (RNAO)".into());}
    }
    r
}
fn retired_record(html:&str,base:&Url)->Result<Option<GuidelineDoc>>{
    let doc=Html::parse_document(html);
    let Some(root)=doc.select(&sel("#main-content .node-type-bpg")).next()else{return Ok(None)};
    let body=root.select(&sel(".field-body")).next().context("RNAO legacy body missing")?;
    let text=text_of(&body);
    let retired=regex::Regex::new(r"(?i)retired as of ([A-Za-z]+\s+\d{4})").unwrap();
    let Some(c)=retired.captures(&text)else{bail!("RNAO legacy record is not the validated retired template");};
    let title=common::first_text(&doc,"h1#page-title").context("RNAO retired title missing")?;
    let mut ds=vec![];
    if let Some(p)=root.select(&sel(".field-bpg-date .date-display-single")).next(){ds.push(common::date("Publication date",&text_of(&p),base.as_str()));}
    ds.push(common::date("Retired as of",&c[1],base.as_str()));
    let (blocks,mut warnings)=blocks_from_html(&body.html(),Some(base),Some(".field-body"));
    warnings.push("Retired guideline record only: this source page contains its retirement notice and related resources, not the historical full clinical guideline. No historical recommendations are inferred.".into());
    let relations=body.select(&sel("a[href]")).filter_map(|a|Some(Relation{rel:"related".into(),target:common::link(base,a.value().attr("href")?)?,scope:Some("RNAO encourages users to review this guideline".into()),evidence_url:Some(base.to_string())})).collect();
    let attachments=root.select(&sel(".field-translations a[href], .field-implementation-resources a[href]")).filter_map(|a|Some(json!({"url":common::link(base,a.value().attr("href")?)?,"label":text_of(&a),"role":"retired_record_related_resource"}))).collect();
    Ok(Some(GuidelineDoc{title,url:base.to_string(),item_type:Some("guideline_record".into()),status:"archived".into(),published:ds.iter().find(|d|d.label=="Publication date").and_then(|d|d.normalized.clone()),date_observations:ds,blocks,warnings,relations,attachments,rights:common::unknown_rights(),..Default::default()}))
}
fn retired_revision(d:&GuidelineDoc)->Result<String>{Ok(crate::common::sha256_hex(serde_json::to_string(&(&d.title,&d.date_observations,&d.blocks,&d.relations,&d.attachments))?.as_bytes()))}
enum Resolved{Modern(Detail,String),Retired(GuidelineDoc,String)}
#[async_trait::async_trait]
impl Adapter for Rnao {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
        if limit==Some(0){return Ok(vec![]);}
        let mut next=Some(src.catalog_url.clone().context("RNAO catalog URL missing")?);let mut pages=HashSet::new();let mut hashes=HashSet::new();let mut seen=HashSet::new();let mut out=vec![];
        while let Some(url)=next {
            if !pages.insert(url.clone())||pages.len()>100{bail!("RNAO catalog pagination cycle or >100pages; inventory incomplete");}
            let page=f.get_ok(&url).await?;if !hashes.insert(page.record.sha256.clone()){bail!("RNAO catalog repeats response body across pages; refusing false completion");}
            let (entries,n)=catalog(&page.text(),&Url::parse(&page.record.final_url)?)?;next=n;
            for (url,hint) in entries {
                let u=Url::parse(&url)?;if !seen.insert(identity(&u)){continue;}
                let work=common::slug(u.path().trim_end_matches('/').rsplit('/').next().unwrap_or(""));
                let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url,title:Some(hint),rendition:Some("pdf".into()),version:None,extra:json!({"work_key":work})};
                let result=async {
                    let landing=f.get_ok(&row.url).await?;let base=Url::parse(&landing.record.final_url)?;
                    if let Some(d)=retired_record(&landing.text(),&base)?{let sha=retired_revision(&d)?;return Ok::<_,anyhow::Error>(Resolved::Retired(d,sha));}
                    let d=detail(&landing.text(),&base)?;
                    let pdf=f.get_ok(&d.pdf).await?;if !pdf.is_pdf(){bail!("RNAO guideline download is not actual PDF bytes");}
                    Ok::<_,anyhow::Error>(Resolved::Modern(d,pdf.record.sha256))
                }.await;
                match result {
                    Ok(Resolved::Modern(d,sha))=>{let snapshot=common::short_hash(&format!("{}:{sha}",d.revision));row.doc_key=format!("{work}-{snapshot}");row.title=Some(d.title);row.version=d.version;row.extra["pdf_url"]=json!(d.pdf);row.extra["pdf_sha256"]=json!(sha);row.extra["detail_revision"]=json!(d.revision);},
                    Ok(Resolved::Retired(d,sha))=>{row.doc_key=format!("{work}-{}",&sha[..12]);row.title=Some(d.title);row.rendition=Some("html".into());row.extra["retired_revision"]=json!(sha);},
                    Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
                }
                out.push(row);if limit.is_some_and(|n|out.len()>=n){return Ok(out);}
            }
        }
        Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("RNAO discovery: {e}");}
        let landing=f.get_ok(&row.url).await?;let base=Url::parse(&landing.record.final_url)?;
        if let Some(expected)=row.extra["retired_revision"].as_str(){
            let mut d=retired_record(&landing.text(),&base)?.context("RNAO retired template changed after planning")?;
            if retired_revision(&d)?!=expected{bail!("RNAO retired record changed after planning; re-plan");}
            d.source_id=src.id.clone();d.doc_key=row.doc_key.clone();d.work_key=row.extra["work_key"].as_str().map(String::from);d.fetches.push(landing.record);return Ok(d);
        }
        let d=detail(&landing.text(),&base)?;
        if row.extra["detail_revision"].as_str().is_some_and(|s|s!=d.revision){bail!("RNAO detail changed after planning; re-plan");}
        let pdf=f.get_ok(&d.pdf).await?;if !pdf.is_pdf(){bail!("RNAO full guideline download returned a wrapper rather than PDF bytes");}
        if row.extra["pdf_sha256"].as_str().is_some_and(|s|s!=pdf.record.sha256){bail!("RNAO full PDF changed after planning; re-plan");}
        let (mut blocks,mut warnings,_)=blocks_for(&pdf,None,30).await?;
        if blocks.is_empty(){bail!("RNAO full guideline has no extracted clinical content");}
        // Converter page hints can be printed footer numbers (the genuine last-page fixture says
        // page 9). They are not a validated physical mapping for this PDF, so leave locators unset.
        for b in &mut blocks{b.page=None;}
        warnings.push("Physical PDF page locators are unset: converter footer/page hints have not been independently mapped to physical pages.".into());
        let records=pdf_recommendations(&mut blocks);
        let source_labels=d.summary.iter().filter_map(|s|s["label"].as_str()).collect::<HashSet<_>>();
        let pdf_labels=records.iter().filter_map(|s|s["label"].as_str()).collect::<HashSet<_>>();
        if source_labels!=pdf_labels{warnings.push(format!("Recommendation coverage mismatch: HTML panel declares {} distinct labels; detailed PDF extraction found {}. Missing/unmatched statements require review; no strength is borrowed from another statement.",source_labels.len(),pdf_labels.len()));}
        let rights=pdf_rights(&blocks,&d.pdf);
        if rights.evidence_urls.is_empty(){warnings.push("Individual PDF copyright/reproduction notice was not recognized; rights remain unknown.".into());}
        warnings.push("Primary clinical blocks come from the full guideline PDF, not the HTML overview. PDF conversion does not recover original figure images/diagram branches or guarantee merged-cell fidelity; no figures are invented. Methodology/evidence-profile, implementation and overview-video assets are separately linked, not acquired as full guideline text.".into());
        let publisher=(!rights.rights_holders.is_empty()).then(||"Registered Nurses’ Association of Ontario (RNAO)".into());
        if d.predecessors.iter().any(|p|p["target_url"].is_null()){warnings.push("Publisher states that this edition replaces a prior BPG, but its revision panel gives an unlinked predecessor title. The stated supersession is preserved in metadata; no unresolved title is emitted as a typed relation target.".into());}
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),item_type:Some("best_practice_guideline".into()),
            title:d.title,url:row.url.clone(),publisher,version:d.version,status:d.status,published:d.published,date_observations:d.dates,language:d.language,rights,relations:d.relations,blocks,attachments:d.attachments,
            search_metadata:json!({"source_node_id":d.node_id,"html_recommendation_summary":d.summary,"predecessor_claims":d.predecessors,"structured_recommendation_provenance":{"source_url":d.pdf,"records":records}}),fetches:vec![landing.record,pdf.record],warnings,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const CATALOG:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/catalog.html"));
    const DETAIL:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/suicidal-ideation.html"));
    const LAST:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/catalog-last.html"));
    const LEGACY:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/legacy-insulin.html"));
    const SOURCE_BLOCKS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/source-blocks.json"));
    const HOME:&str="https://rnao.ca/bpg/guidelines";
    const TOPIC:&str="https://rnao.ca/bpg/guidelines/suicidal-ideation-and-behaviour";
    const PDF:&str="https://rnao.ca/media/11024/download";
    const OLD:&str="https://www.rnao.ca/bpg/guidelines/bpg-subcutaneous-administration-insulin-adults-type-2-diabetes";
    const COPYRIGHT:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/copyright.pdf"));
    const SUMMARY18:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/summary-18.pdf"));
    const SUMMARY19:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/summary-19.pdf"));
    fn source()->SourceEntry {serde_json::from_value(json!({"id":"rnao","access":"public","enabled":true,"catalog_url":HOME})).unwrap()}
    #[test]
    fn genuine_catalog_and_hidden_recommendation_panel_inventory() {
        let (items,next)=catalog(CATALOG,&Url::parse(HOME).unwrap()).unwrap();
        assert_eq!(items.len(),10);
        assert_eq!(next.as_deref(),Some("https://rnao.ca/bpg/guidelines?page=1"));
        assert!(items.iter().any(|(u,_)|u=="https://www.rnao.ca/bpg/guidelines/bpg-subcutaneous-administration-insulin-adults-type-2-diabetes"));
        let d=detail(DETAIL,&Url::parse(TOPIC).unwrap()).unwrap();
        assert_eq!(d.pdf,PDF);
        assert_eq!(d.published.as_deref(),Some("2026-08"));
        assert_eq!(d.version.as_deref(),Some("Second"));
        assert_eq!(d.node_id.as_deref(),Some("16625"));
        assert_eq!(d.summary.len(),8,"all hidden accordion panels, not just first visible groups");
        assert_eq!(d.summary.iter().filter(|s|s["label"].as_str().unwrap().starts_with("Good practice statement")).count(),4);
        assert!(d.summary.iter().any(|s|s["label"]=="Recommendation 4.0"&&s["statement"].as_str().unwrap().contains("remote contact intervention")));
        assert!(d.summary.iter().any(|s|s["label"]=="Good practice statement 4.0"&&s["statement"].as_str().unwrap().contains("postvention")));
        assert_eq!(d.dates.len(),3);
        assert!(d.dates.iter().any(|d|d.label=="Anticipated revision publication"&&d.normalized.as_deref()==Some("2030")));
        assert!(d.attachments.iter().any(|a|a["role"]=="methodology_document"&&a["url"].as_str().unwrap().contains("/media/11016/download")));
        assert!(d.summary.iter().all(|s|s["source_url"].as_str().unwrap().contains("#sect-")));
        assert_eq!(d.status,"current","revision panel explicitly states Current edition published 2026");
        assert!(d.relations.iter().all(|r|r.rel!="supersedes"));
        assert_eq!(d.predecessors.len(),1,"recruitment and contact lists are not predecessor guidelines");
        assert!(d.predecessors.iter().any(|p|p["predecessor_title"].as_str().is_some_and(|s|s.contains("(2009)"))&&p["target_url"].is_null()&&p["evidence_url"].as_str().is_some_and(|s|s.ends_with("#tab-content-status"))));
        let (last,next)=catalog(LAST,&Url::parse("https://rnao.ca/bpg/guidelines?page=5").unwrap()).unwrap();
        assert_eq!(last.len(),3);assert!(next.is_none(),"follow the real final-page terminal, not a guessed total");
    }
    #[test]
    fn genuine_full_pdf_subset_has_eight_distinct_labels_and_exact_own_grades(){
        let mut blocks:Vec<Block>=serde_json::from_str(SOURCE_BLOCKS).unwrap();
        let records=pdf_recommendations(&mut blocks);
        assert_eq!(records.len(),8);
        assert_eq!(records.iter().filter(|r|r["label"].as_str().unwrap().starts_with("Good practice statement")).count(),4);
        for (n,certainty) in [(1,"Very Low"),(2,"Very low"),(3,"Very Low"),(4,"Moderate")] {
            let expected=format!("Recommendation {n}.0");
            let b=blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l==&expected)).unwrap();
            assert!(matches!(&b.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),..}if s=="Conditional"&&c==certainty));
            let gps=format!("Good practice statement {n}.0");
            let b=blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..}if l==&gps)).unwrap();
            assert!(matches!(b.kind,BlockKind::Recommendation{strength:None,certainty:None,..}),"publisher explicitly gives good-practice statements no grade");
        }
        assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("Not applicable*")),"preserve summary tables as well as detailed structured statements");
        assert!(blocks.iter().any(|b|b.text.starts_with("All RNAO BPGs are available for download")));
        let rights=pdf_rights(&blocks,PDF);
        assert_eq!(rights.status,"restricted");
        assert!(rights.note.as_ref().unwrap().contains("educational or non- commercial purposes"));
        assert!(rights.note.as_ref().unwrap().contains("written permission"));
        assert!(rights.attribution.as_ref().unwrap().contains("2nd ed. Toronto (ON): RNAO; 2026."));
    }
    #[tokio::test]
    async fn actual_legacy_retirement_record_is_not_mistaken_for_full_clinical_content(){
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<24,"").unwrap();
        f.seed_cache_for_tests(HOME,CATALOG.as_bytes(),"text/html");f.seed_cache_for_tests(OLD,LEGACY.as_bytes(),"text/html");
        let rows=Rnao.discover(&source(),&f,Some(10)).await.unwrap();
        let row=rows.iter().find(|r|r.url==OLD).unwrap();
        let d=Rnao.extract(&source(),&f,row).await.unwrap();
        assert_eq!(d.status,"archived");assert_eq!(d.item_type.as_deref(),Some("guideline_record"));
        assert_eq!(d.published.as_deref(),Some("2004"));
        assert!(d.date_observations.iter().any(|d|d.label=="Retired as of"&&d.normalized.as_deref()==Some("2020-03")));
        assert!(d.blocks.iter().any(|b|b.text.contains("is retired as of March 2020")));
        assert!(d.relations.iter().any(|r|r.rel=="related"&&r.target=="https://guidelines.diabetes.ca/cpg"));
        assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..}|BlockKind::Figure{..})));
        assert!(d.warnings.iter().any(|w|w.contains("not the historical full clinical guideline")));
    }
    #[tokio::test]
    async fn saved_catalog_discovery_retains_unavailable_terminal_as_error() {
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<24,"").unwrap();
        f.seed_cache_for_tests(HOME,CATALOG.as_bytes(),"text/html");
        let rows=Rnao.discover(&source(),&f,Some(1)).await.unwrap();
        assert_eq!(rows.len(),1);
        assert!(rows[0].extra["discovery_error"].is_string(),"unseeded genuine first catalog entry is not silently dropped");
        assert!(Rnao.extract(&source(),&f,&rows[0]).await.is_err());
    }
    #[tokio::test]
    async fn genuine_small_pdf_excerpts_do_not_pretend_to_cover_the_full_clinical_guideline(){
        for (physical,bytes,expected_tables) in [(2,COPYRIGHT,0),(18,SUMMARY18,1),(19,SUMMARY19,1)] {
            assert!(bytes.len()<200_000);
            let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<24,"").unwrap();
            f.seed_cache_for_tests(HOME,CATALOG.as_bytes(),"text/html");
            f.seed_cache_for_tests(TOPIC,DETAIL.as_bytes(),"text/html");
            // These genuine one-page PDF derivatives are intentionally incomplete. The unmodified
            // catalog's fourth card is our observed topic; first three unseeded leaves are audits.
            f.seed_cache_for_tests(PDF,bytes,"application/pdf");
            let rows=Rnao.discover(&source(),&f,Some(4)).await.unwrap();
            assert_eq!(rows.len(),4);
            let row=rows.iter().find(|r|r.url==TOPIC).unwrap();
            assert!(row.extra["discovery_error"].is_null());
            let d=Rnao.extract(&source(),&f,row).await.unwrap();
            assert_eq!(d.fetches.len(),2);
            assert_eq!(d.published.as_deref(),Some("2026-08"));
            assert_eq!(d.version.as_deref(),Some("Second"));assert_eq!(d.status,"current");
            assert!(d.updated.is_none());
            assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),expected_tables);
            assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).count(),0,"excerpt contains summaries/copyright, not the eight detailed labelled-statement sections");
            assert!(d.warnings.iter().any(|w|w.contains("HTML panel declares 8")&&w.contains("PDF extraction found 0")));
            assert!(d.warnings.iter().any(|w|w.contains("original figure images/diagram branches")));
            assert!(d.blocks.iter().all(|b|b.page.is_none()&&!matches!(b.kind,BlockKind::Figure{..})));
            assert_eq!(d.search_metadata["html_recommendation_summary"].as_array().unwrap().len(),8);
            if physical==2 {
                assert_eq!(d.rights.status,"restricted");
                assert!(d.rights.attribution.as_ref().unwrap().contains("RNAO; 2026."));
            }else{
                assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("Conditional")));
                let tail=if physical==18{"Good practice statement 3.0"}else{"Good practice statement 4.0"};
                assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains(tail)));
                if physical==19{assert!(d.blocks.iter().any(|b|b.text.contains("do not have an associated strength")));}
            }
            let mut stale=row.clone();stale.extra["pdf_sha256"]=json!("stale");
            assert!(Rnao.extract(&source(),&f,&stale).await.unwrap_err().to_string().contains("changed after planning"));
        }
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod live_template_tests {
    use super::*;
    const A:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/adult-asthma.html"));
    const B:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/anti-black-racism.html"));
    const E:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/end-of-life.html"));
    #[test]
    fn genuine_multilingual_download_and_labeled_clinical_continuations(){
        let a=detail(A,&Url::parse("https://rnao.ca/bpg/guidelines/adult-asthma-care").unwrap()).unwrap();
        assert_eq!(a.pdf,"https://rnao.ca/media/3791/download");assert_eq!(a.summary.len(),23);
        assert!(a.attachments.iter().any(|v|v["url"]=="https://rnao.ca/media/3790/download"&&v["role"]=="linked_translation"));
        assert!(a.attachments.iter().any(|v|v["url"]=="https://rnao.ca/media/6621/download"&&v["role"]=="linked_translation"));
        let questions=a.summary.iter().find(|v|v["label"]=="Recommendation 1.1:").unwrap()["statement"].as_str().unwrap();
        assert!(questions.contains("Have you ever been told")&&questions.contains("Have you ever used a puffer/inhaler"));
        let assessment=a.summary.iter().find(|v|v["label"]=="Recommendation 1.2a:").unwrap()["statement"].as_str().unwrap();
        for text in ["< 4 doses/week", "Sputum eosinophils < 2–3%", "not be available", "scope of practice"]{assert!(assessment.contains(text),"missing {text}");}
        assert!(!assessment.contains("severity and need for urgent medical attention"));
        assert!(a.summary.iter().any(|v|v["label"]=="Recommendation 6.2:"));
        let b=detail(B,&Url::parse("https://rnao.ca/bpg/guidelines/addressing-anti-Black-racism-in-nursing").unwrap()).unwrap();
        assert_eq!(b.pdf,"https://rnao.ca/media/10394/download");assert_eq!(b.summary.len(),10);
        assert_eq!(b.summary[0]["label"],"Good Practice Statement 1.0:");
        let education=b.summary.iter().find(|v|v["label"]=="Recommendation 1.1:").unwrap()["statement"].as_str().unwrap();
        assert!(education.contains("small and large group discussions")&&education.contains("activities that challenge personal bias"));
        let e=detail(E,&Url::parse("https://rnao.ca/bpg/guidelines/endoflife-care-during-last-days-and-hours").unwrap()).unwrap();
        assert_eq!(e.pdf,"https://rnao.ca/media/6023/download");assert_eq!(e.summary.len(),24);
        assert!(e.summary.iter().any(|v|v["label"]=="Recommendation 1.1.1:"));
        let signs=e.summary.iter().find(|v|v["label"]=="Recommendation 1.2.1:").unwrap()["statement"].as_str().unwrap();
        assert!(signs.contains("progressive weakness")&&signs.contains("decreased level of consciousness"));
        assert!(!signs.contains("Canadian Hospice Palliative Care Association Domains"));
        assert!(e.summary.iter().any(|v|v["label"]=="Recommendation 5.4:"));
        // Root's source-scoped predecessor-list correction is retained, not replaced by
        // extracting unrelated recruitment lists from the revisions container.
        assert!(b.predecessors.iter().all(|v|!v["predecessor_title"].as_str().unwrap_or_default().contains("recruitment")));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod additional_pdf_template_tests{
    use super::*;
    fn fixture(raw:&str)->(Value,Vec<Block>){let v:Value=serde_json::from_str(raw).unwrap();let b=serde_json::from_value(v["blocks"].clone()).unwrap();(v,b)}
    const A:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/adult-asthma-blocks.json"));
    const B:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/anti-black-racism-blocks.json"));
    const E:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/rnao/end-of-life-blocks.json"));
    fn recommendation<'a>(blocks:&'a [Block],label:&str)->&'a Block{blocks.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l.eq_ignore_ascii_case(label))).unwrap()}
    #[test]
    fn genuine_asthma_uppercase_labels_and_complete_evidence_level_statements(){
        let (source,mut blocks)=fixture(A);assert_eq!(source["source_fetches"][0]["sha256"],"6f3bbc0a6dc07dec8dd37d8c5aba4fb4e4a06cfc2bad5c926cb53e95a77df6d4");
        let original_tables=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).map(|b|b.text.clone()).collect::<Vec<_>>();
        let records=pdf_recommendations(&mut blocks);assert_eq!(records.len(),23);
        let first=recommendation(&blocks,"RECOMMENDATION 1.1");
        assert!(first.text.contains("Have you ever been told")&&first.text.contains("Have you ever used a puffer/inhaler"));
        assert!(matches!(&first.kind,BlockKind::Recommendation{strength:None,certainty:Some(c),grading_system:None,..} if c=="V"));
        let control=recommendation(&blocks,"RECOMMENDATION 1.2a");
        assert!(control.text.contains("4 doses/week")&&control.text.contains("2–3%")&&control.text.contains("scope of practice"));
        let planning=recommendation(&blocks,"RECOMMENDATION 2.1");
        assert!(planning.text.contains("Learning needs")&&planning.text.contains("Empowerment"));
        assert!(matches!(&planning.kind,BlockKind::Recommendation{strength:None,certainty:Some(c),..} if c=="Ib, IV"));
        assert!(recommendation(&blocks,"RECOMMENDATION 6.2").text.contains("resources and professional training"));
        assert_eq!(original_tables,blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).map(|b|b.text.clone()).collect::<Vec<_>>());
        let rights=pdf_rights(&blocks,source["source_url"].as_str().unwrap());assert_eq!(rights.status,"restricted");assert_eq!(rights.rights_holders.len(),1);assert!(rights.attribution.unwrap().contains("(2017)"));
    }
    #[test]
    fn genuine_anti_black_inline_labels_cerqual_and_ungraded_gps(){
        let (source,mut blocks)=fixture(B);assert_eq!(source["source_fetches"][0]["sha256"],"7a38f50763d7730f13729828f011ad71f5df155b49178a0776ef5a26f785ee49");
        let records=pdf_recommendations(&mut blocks);assert_eq!(records.len(),10);
        let education=recommendation(&blocks,"Recommendation 1.1");
        // This PDF says singular "discussion" and prints confidence on every clause;
        // the independent HTML summary says plural "discussions" without those annotations.
        for clause in ["small and large group discussion (confidence: moderate)","critical self-reflection and self-sharing activities (confidence: moderate)","activities that challenge personal bias (confidence: moderate)"]{assert!(education.text.contains(clause),"missing exact PDF clause: {clause}");}
        let record=records.iter().find(|r|r["label"]=="Recommendation 1.1").unwrap();
        assert_eq!(record["statement_source_blocks"],json!(["m00364","m00365","m00366","m00367"]));
        assert!(record["grade_observations"].as_array().unwrap().iter().any(|v|v["block_id"]=="m00369"&&v["label"]=="Confidence in evidence"&&v["value"]=="Moderate"));
        assert!(matches!(&education.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),grading_system:Some(g),..} if s=="Conditional"&&c=="Moderate"&&g=="GRADE-CERQual"));
        let intervention=recommendation(&blocks,"Recommendation 1.0");
        assert!(matches!(&intervention.kind,BlockKind::Recommendation{strength:Some(s),certainty:Some(c),grading_system:Some(g),..} if s=="Conditional"&&c=="Very low"&&g=="GRADE"));
        for n in 1..=5{let gps=recommendation(&blocks,&format!("Good practice statement {n}.0"));assert!(matches!(&gps.kind,BlockKind::Recommendation{strength:None,certainty:None,grading_system:None,..}));}
        assert!(recommendation(&blocks,"Good practice statement 5.0").text.contains("committees that include Black nurse leaders"));
        assert!(records.iter().any(|r|r["source_table"].is_string()&&r["label"]=="Good practice statement 3.0"));
    }
    #[test]
    fn genuine_end_of_life_groups_keep_one_evidence_range_and_continuation_first_rows(){
        let (source,mut blocks)=fixture(E);assert_eq!(source["source_fetches"][0]["sha256"],"48e0dfc55912ccf107e8b0c10e875b2b13122954522979259447cf1a3cb7114a");
        let records=pdf_recommendations(&mut blocks);assert_eq!(records.len(),16);
        let grouped=recommendation(&blocks,"1.1 1.1.1 1.1.2");assert!(grouped.text.contains("disease specific indicators")&&grouped.text.contains("end-of-life trajectories"));
        assert!(matches!(&grouped.kind,BlockKind::Recommendation{strength:None,certainty:Some(c),grading_system:None,..} if c=="IIa–IV"));
        assert!(blocks.iter().all(|b|!matches!(&b.kind,BlockKind::Recommendation{label:Some(l),..} if l=="1.1.1")));
        assert!(matches!(&recommendation(&blocks,"2.1").kind,BlockKind::Recommendation{certainty:Some(c),..} if c=="Ib, IV, IV*"));
        assert!(matches!(&recommendation(&blocks,"2.2").kind,BlockKind::Recommendation{certainty:Some(c),..} if c=="Ia – IV*"));
        assert!(recommendation(&blocks,"4.1").text.contains("knowledge of relevant legislation"));
        assert!(recommendation(&blocks,"5.1").text.contains("individual and family relationship"));
        assert!(recommendation(&blocks,"5.4").text.contains("End-of-life Care During the Last Days and Hours"));
        assert!(pdf_rights(&blocks,source["source_url"].as_str().unwrap()).attribution.unwrap().contains("(2011)"));
    }
}
