//! ASHM's server-rendered clinical topic graph and preloaded accordion bodies.
//! A regimen is an intact population row, retaining combination/alternative columns and notes.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,GuidelineDoc};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::{ElementRef,Html};
use serde_json::{json,Value};
use std::collections::{HashSet,VecDeque};
use url::Url;
pub struct AuSti;
const ROOT:&str="main#main-content .wp-content";
fn escaped(s:&str)->String{s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;").replace('"',"&quot;")}
fn clinical_path(path:&str)->bool{
    ["/sexually-transmissible-infections/","/syndromes/","/populations-and-situations/","/contact-tracing/"].iter().any(|p|path.starts_with(p))
        ||matches!(path,"/standard-asymptomatic-checkup/"|"/specimen-collection/")
}
fn index_path(path:&str)->bool{matches!(path,"/sexually-transmissible-infections/"|"/syndromes/"|"/populations-and-situations/")}
fn links(root:ElementRef<'_>,base:&Url)->Vec<(String,String,bool)>{
    let mut seen=HashSet::new();root.select(&sel("a[href]")).filter_map(|a|{
        let url=common::link(base,a.value().attr("href")?)?;let u=Url::parse(&url).ok()?;
        if u.host_str()!=base.host_str()||u.query().is_some()||!clinical_path(u.path())||!seen.insert(url.clone()){return None;}
        let draft=a.ancestors().filter_map(ElementRef::wrap).any(|e|e.value().classes().any(|c|c=="consultation-notice"));
        Some((url,text_of(&a),draft))
    }).collect()
}
struct Page{title:String,html:String,notices:Vec<Value>,dates:Vec<DateObservation>,assets:Vec<Value>,is_draft:bool,language:Option<String>}
fn page(html:&str,base:&Url,draft_route:bool)->Result<Page>{
    let doc=Html::parse_document(html);let roots=doc.select(&sel(ROOT)).collect::<Vec<_>>();
    if roots.len()!=1{bail!("ASHM expected one clinical wp-content root, found {}",roots.len());}
    let root=roots[0];let title=common::first_text(&doc,"main#main-content h1").context("ASHM clinical title missing")?;
    let mut selected=root.html();let mut notices=vec![];let mut dates=vec![];
    for time in root.select(&sel(".consultation-updated time[datetime]")){
        let mut d=common::date("Last updated",time.value().attr("datetime").unwrap(),base.as_str());d.raw=text_of(&time);dates.push(d);
    }
    // A consultation deadline is not a clinical publication or revision date.
    for time in root.select(&sel("time[datetime]")){
        if !title.starts_with("Draft:"){continue;}
        if time.ancestors().filter_map(ElementRef::wrap).any(|e|e.value().classes().any(|c|c=="consultation-notice"||c=="consultation-updated")){continue;}
        let mut d=common::date("Feedback due",time.value().attr("datetime").unwrap(),base.as_str());d.raw=text_of(&time);dates.push(d);
    }
    for notice in root.select(&sel(".consultation-notice")){
        let heading=notice.select(&sel("h3")).next().map(|e|text_of(&e)).context("ASHM consultation notice title missing")?;
        let mut source=base.clone();
        if let Some(item)=notice.ancestors().filter_map(ElementRef::wrap).find(|e|e.value().classes().any(|c|c=="wp-block-accordion-item")){
            source.set_fragment(item.select(&sel("h2[id]")).next().and_then(|h|h.value().id()));
        }
        for time in notice.select(&sel(".consultation-deadline time[datetime]")){
            let raw=text_of(&time);let mut d=common::date("Feedback due",time.value().attr("datetime").unwrap(),source.as_str());d.raw=raw;dates.push(d);
        }
        let targets=notice.select(&sel("a[href]")).filter_map(|a|Some(json!({"url":common::link(base,a.value().attr("href")?)?,"label":text_of(&a)}))).collect::<Vec<_>>();
        notices.push(json!({"title":heading,"text":text_of(&notice),"status":"in_review","source_url":source.as_str(),"links":targets}));
        selected=selected.replacen(&notice.html(),"",1);
    }
    // The shared walker skips buttons. Replace controls with their exact printed section title.
    for heading in root.select(&sel("h2.wp-block-accordion-heading")){
        let label=heading.select(&sel(".wp-block-accordion-heading__toggle-title")).next().context("ASHM accordion title missing")?;
        let id=heading.value().id().map(|s|format!(" id=\"{}\"",escaped(s))).unwrap_or_default();
        selected=selected.replacen(&heading.html(),&format!("<h2{id}>{}</h2>",label.inner_html()),1);
    }
    for p in root.select(&sel("p.wp-block-paragraph")){
        if p.ancestors().filter_map(ElementRef::wrap).any(|e|e.value().name()=="table"||e.value().classes().any(|c|c=="consultation-notice")){continue;}
        let Some(strong)=p.select(&sel("strong")).next()else{continue};let t=text_of(&p);
        if t!=text_of(&strong)||t.len()>80||t.ends_with('.')||t.is_empty(){continue;}
        let level=if t=="Overview"{2}else{3};
        let id=p.value().id().map(|s|format!(" id=\"{}\"",escaped(s))).unwrap_or_default();
        selected=selected.replacen(&p.html(),&format!("<h{level}{id}>{}</h{level}>",escaped(&t)),1);
    }
    // Styles, scripts, comments and collapsed-control attributes are not clinical revision data.
    let comments=regex::Regex::new(r"(?s)<!--.*?-->").unwrap();selected=comments.replace_all(&selected,"").into_owned();
    let scripts=regex::Regex::new(r"(?is)<(?:script|style)\b[^>]*>.*?</(?:script|style)>").unwrap();selected=scripts.replace_all(&selected,"").into_owned();
    let mut assets=vec![];let mut seen=HashSet::new();
    for a in doc.select(&sel("main#main-content a[href]")){
        let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default())else{continue};let u=Url::parse(&url)?;
        let label=text_of(&a);let path=u.path().to_lowercase();
        if [".pdf",".rtf",".docx",".doc"].iter().any(|s|path.ends_with(s))&&seen.insert(url.clone()){
            assets.push(json!({"url":url,"label":label,"role":if u.host_str()==base.host_str()&&path.starts_with("/pdfs/"){"print_rendition"}else{"linked_resource"},"rights":"unknown"}));
        }
    }
    let is_draft=title.starts_with("Draft:")&&(draft_route||text_of(&root).contains("public consultation"));
    let selected=format!("<div id='ashm-clinical'><h1>{}</h1>{selected}</div>",escaped(&title));
    Ok(Page{title,html:selected,notices,dates,assets,is_draft,language:common::page_language(&doc)})
}
fn span(cell:ElementRef<'_>,attr:&str)->Result<usize>{let n=cell.value().attr(attr).map(str::parse::<usize>).transpose().context("ASHM invalid table span")?.unwrap_or(1);if n==0||n>200{bail!("ASHM table span outside validated bound");}Ok(n)}
fn adjacent_notes(table:ElementRef<'_>)->Vec<String>{
    let container=table.parent().and_then(ElementRef::wrap).filter(|e|e.value().name()=="figure").unwrap_or(table);
    let abbreviation=regex::Regex::new(r"(?i)^[a-z]{1,8}\.?:").unwrap();let mut notes=vec![];
    for next in container.next_siblings().filter_map(ElementRef::wrap){
        if next.value().name()!="p"{break;}
        let t=text_of(&next);if t.starts_with('*')||abbreviation.is_match(&t){notes.push(t);}else{break;}
    }
    notes
}
struct Regimen{fields:Vec<String>,group:Option<String>,source_id:Option<String>}
fn table_rows(table:ElementRef<'_>)->Result<Option<(Vec<String>,Vec<Regimen>,Vec<String>)>>{
    let rows=table.select(&sel("tr")).collect::<Vec<_>>();
    // Actual anorectal template: one column, one cell containing a three-drug PLUS list.
    // Treat the entire cell as one regimen, never individual list items as alternatives.
    if rows.len()==2 {
        let h=common::cells(rows[0]);let d=common::cells(rows[1]);
        if h.len()==1&&d.len()==1&&text_of(&h[0])=="Empirical treatment should be initiated without waiting for results" {
            return Ok(Some((vec![text_of(&h[0])],vec![Regimen{fields:vec![text_of(&d[0])],group:Some(text_of(&h[0])),source_id:rows[1].value().id().map(String::from)}],adjacent_notes(table))));
        }
    }
    let Some((header_i,headers))=rows.iter().enumerate().find_map(|(i,r)|{
        let cells=common::cells(*r);let h=cells.iter().map(text_of).collect::<Vec<_>>();
        (h.len()>=2&&cells.iter().all(|c|c.value().name()=="th")&&h.iter().any(|s|s=="Recommended"||s=="Recommended treatment")).then_some((i,h))
    })else{return Ok(None)};
    let width=headers.len();if width>12{bail!("ASHM regimen table has too many columns for validated row mapping");}
    let mut group=rows[..header_i].iter().rev().find_map(|r|{let c=common::cells(*r);(c.len()==1).then(||text_of(&c[0]))});
    let mut carry:Vec<Option<(String,usize)>>=vec![None;width];let mut out=vec![];let mut notes=adjacent_notes(table);
    for row in rows.iter().skip(header_i+1){
        let cells=common::cells(*row);if cells.is_empty(){continue;}
        if cells.len()==1&&span(cells[0],"colspan")?==width{
            let t=text_of(&cells[0]);
            if t=="OR"||t=="PLUS"{bail!("ASHM regimen uses a connector across physical rows; table retained without unsafe row splitting");}
            if cells[0].value().name()=="th"{group=Some(t);carry.fill(None);continue;}
            if t.starts_with('*')||t.starts_with("Note:")||t.starts_with("NB:"){notes.push(t);continue;}
            bail!("ASHM full-width clinical row requires table-specific grouping; no row recommendations inferred");
        }
        if cells.iter().map(text_of).collect::<Vec<_>>()==headers{continue;}
        let mut values:Vec<Option<String>>=vec![None;width];
        for j in 0..width{if let Some((text,remaining))=carry[j].clone(){values[j]=Some(text.clone());carry[j]=if remaining>1{Some((text,remaining-1))}else{None};}}
        let mut col=0;
        for c in cells{
            while col<width&&values[col].is_some(){col+=1;}
            let cs=span(c,"colspan")?;let rs=span(c,"rowspan")?;
            if col+cs>width{bail!("ASHM regimen row span exceeds declared columns");}
            let text=text_of(&c);
            for j in col..col+cs{if values[j].is_some(){bail!("ASHM regimen spans overlap");}values[j]=Some(text.clone());if rs>1{carry[j]=Some((text.clone(),rs-1));}}
            col+=cs;
        }
        if values.iter().any(Option::is_none){bail!("ASHM regimen row has unscoped/missing cells");}
        out.push(Regimen{fields:values.into_iter().map(Option::unwrap).collect(),group:group.clone(),source_id:row.value().id().map(String::from)});
    }
    if carry.iter().any(Option::is_some){bail!("ASHM regimen rowspan extends beyond observed table");}
    Ok(Some((headers,out,notes)))
}
fn regimens(html:&str,base:&Url,blocks:&mut Vec<Block>,draft:bool)->Result<(Vec<Value>,Vec<String>)>{
    let doc=Html::parse_fragment(html);let table_ids=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).map(|b|b.block_id.clone()).collect::<Vec<_>>();
    let tables=doc.select(&sel("#ashm-clinical table")).collect::<Vec<_>>();if tables.len()!=table_ids.len(){bail!("ASHM source/extracted table counts disagree; refusing silent omission");}
    let mut evidence=vec![];let mut warnings=vec![];
    for (i,table) in tables.iter().enumerate(){
        let parsed=match table_rows(*table){Ok(v)=>v,Err(e)=>{warnings.push(format!("Unstructured regimen table {}: {e:#}; complete source table retained.",i+1));continue;}};
        let Some((headers,rows,notes))=parsed else{continue};let mut statements=vec![];
        for (n,row) in rows.into_iter().enumerate(){
            let mut pieces=vec![];if let Some(ref group)=row.group{pieces.push(group.clone());}
            for (h,t) in headers.iter().zip(&row.fields){pieces.push(format!("{h}: {t}"));}
            let has_marker=row.fields.iter().any(|v|v.contains('*'));
            for note in &notes{if !note.starts_with('*')||has_marker{pieces.push(note.clone());}}
            let population=matches!(headers[0].as_str(),"Situation"|"Infection"|"Infection or Situation"|"Population"|"Syndrome"|"Condition"|"Clinical situation"|"Patient group").then(||row.fields[0].clone());
            let id=format!("{}-regimen-{}",table_ids[i],n+1);let mut source=base.clone();source.set_fragment(row.source_id.as_deref());
            evidence.push(json!({"block_id":id,"source_url":source.as_str(),"headers":headers,"values":row.fields,"notes":notes,"status":if draft{"in_review"}else{"unknown"}}));
            statements.push(Block{block_id:id,kind:BlockKind::Recommendation{label:row.group,strength:None,certainty:None,population,grading_system:None,qualifier:draft.then(||"Draft".into())},text:pieces.join("\n"),heading_path:vec![],page:None});
        }
        common::insert_table_recommendations(blocks,&table_ids[i],statements)?;
    }
    Ok((evidence,warnings))
}
fn snapshot(p:&Page)->Result<String>{Ok(crate::common::sha256_hex(serde_json::to_string(&(&p.title,&p.html,&p.notices,&p.dates,&p.assets,p.is_draft))?.as_bytes()))}
struct Pending{url:String,title:String,draft:bool,index:bool}
#[async_trait::async_trait]
impl Adapter for AuSti{
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
        if limit==Some(0){return Ok(vec![]);}
        let start=src.catalog_url.clone().context("ASHM catalog URL missing")?;
        let mut queue=VecDeque::from([Pending{url:start,title:String::new(),draft:false,index:true}]);let mut seen=HashSet::new();let mut out=vec![];
        while let Some(p)=queue.pop_front(){
            if !seen.insert(p.url.clone()){continue;}if seen.len()>400{bail!("ASHM clinical graph exceeds400source-emitted nodes; coverage incomplete");}
            let fetched=match f.get_ok(&p.url).await{Ok(v)=>v,Err(e)=>{
                if p.index{return Err(e).context("ASHM index acquisition failed");}
                let work=common::slug(Url::parse(&p.url)?.path());out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:p.url,title:Some(p.title),rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"discovery_error":format!("{e:#}")})});
                if limit.is_some_and(|n|out.len()>=n){return Ok(out);}continue;
            }};
            let base=Url::parse(&fetched.record.final_url)?;
            let emitted={let doc=Html::parse_document(&fetched.text());doc.select(&sel(ROOT)).next().map(|root|links(root,&base))};
            if p.index&&emitted.is_none(){bail!("ASHM index clinical root missing");}
            for (url,title,draft) in emitted.unwrap_or_default(){if url!=base.as_str(){let index=index_path(Url::parse(&url)?.path());queue.push_back(Pending{url,title,draft,index});}}
            if p.index{continue;}
            let work=common::slug(base.path());let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url:base.to_string(),title:Some(p.title),rendition:Some("html".into()),version:None,extra:json!({"work_key":work,"draft_route":p.draft})};
            match page(&fetched.text(),&base,p.draft){Ok(body)=>{let hash=snapshot(&body)?;row.doc_key=format!("{work}-{}",&hash[..12]);row.title=Some(body.title);row.extra["clinical_sha256"]=json!(hash);},Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}"))};
            out.push(row);if limit.is_some_and(|n|out.len()>=n){return Ok(out);}
        }
        if out.is_empty(){bail!("ASHM clinical index yielded no document graph");}Ok(out)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("ASHM discovery: {e}");}
        let fetched=f.get_ok(&row.url).await?;let base=Url::parse(&fetched.record.final_url)?;let raw=fetched.text();let p=page(&raw,&base,row.extra["draft_route"].as_bool().unwrap_or(false))?;
        if row.extra["clinical_sha256"].as_str().is_some_and(|s|snapshot(&p).map(|v|v!=s).unwrap_or(true)){bail!("ASHM clinical revision changed after planning; re-plan");}
        let (mut blocks,mut warnings)=blocks_from_html(&p.html,Some(&base),Some("#ashm-clinical"));
        let (evidence,more)=regimens(&p.html,&base,&mut blocks,p.is_draft)?;warnings.extend(more);
        for (i,notice) in p.notices.iter().enumerate(){blocks.push(Block{block_id:format!("ashm-consultation-note-{}",i+1),kind:BlockKind::Footnote,text:notice["text"].as_str().unwrap_or_default().into(),heading_path:vec!["Contact Tracing".into(),notice["title"].as_str().unwrap_or("Draft consultation").into()],page:None});}
        if !p.notices.is_empty(){warnings.push("Displayed contact-tracing draft consultation is retained as a separately labelled footnote/metadata and linked graph node; its deadline and supersession claims are not current chapter recommendations or publication/update dates.".into());}
        warnings.push("Recommended/Alternative are source treatment-option roles, not evidence-strength grades. Row populations, PLUS/OR text, conditional advice and marked footnotes are kept together; relative references are not expanded by borrowing another row's dose. Print/PDF and external resource parity/rights remain unverified.".into());
        let doc=Html::parse_document(&raw);let mut rights=common::unknown_rights();let mut issuers=vec![];
        for para in doc.select(&sel("footer p")){let t=text_of(&para);if t.contains("Copyright © ASHM"){
            rights.note=Some(t.clone());rights.rights_holders.push("ASHM".into());rights.evidence_urls.push(base.to_string());
            if t.contains("Developed by: ASHM Health"){issuers.push("ASHM Health".into());}
        }}
        rights.terms_url=doc.select(&sel("footer a[href]")).filter_map(|a|common::link(&base,a.value().attr("href")?)).find(|u|u.contains("copyright-disclaimer"));
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:p.title,url:row.url.clone(),item_type:Some(if p.is_draft{"consultation_notice"}else{"guideline_chapter"}.into()),
            status:if p.is_draft{"in_review"}else{"unknown"}.into(),updated:if p.is_draft{p.dates.iter().find(|d|d.label=="Last updated").and_then(|d|d.normalized.clone())}else{None},date_observations:p.dates,language:p.language,rights,issuers,blocks,attachments:p.assets,
            search_metadata:json!({"regimen_source_rows":evidence,"draft_consultation_notices":p.notices}),fetches:vec![fetched.record],warnings,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const HOME:&str="https://sti.guidelines.org.au/sexually-transmissible-infections/";
    const C:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_sti/chlamydia.html"));
    const G:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_sti/gonorrhoea.html"));
    const A:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_sti/anorectal.html"));
    const S:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_sti/specimen-collection.html"));
    const D:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_sti/contact-consultation.html"));
    const CAT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/au_sti/catalog.html"));
    fn source()->SourceEntry{serde_json::from_value(json!({"id":"au_sti","access":"public","enabled":true,"catalog_url":HOME})).unwrap()}
    async fn extract_fixture(html:&str,path:&str,draft:bool)->GuidelineDoc{
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        let url=Url::parse("https://sti.guidelines.org.au/").unwrap().join(path).unwrap();
        f.seed_cache_for_tests(url.as_str(),html.as_bytes(),"text/html");
        let row=PlanRow{source_id:"au_sti".into(),doc_key:"fixture".into(),url:url.to_string(),title:None,rendition:Some("html".into()),version:None,extra:json!({"work_key":path,"draft_route":draft})};
        AuSti.extract(&source(),&f,&row).await.unwrap()
    }
    fn recs(d:&GuidelineDoc)->Vec<&Block>{d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect()}
    fn has_all(b:&Block,parts:&[&str])->bool{parts.iter().all(|p|b.text.contains(p))}
    #[tokio::test]
    async fn actual_chlamydia_complete_rows_and_collapsed_followup(){
        let d=extract_fixture(C,"sexually-transmissible-infections/chlamydia/",false).await;
        assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),4);
        let r=recs(&d);assert_eq!(r.len(),5);
        assert!(r.iter().any(|b|has_all(b,&["Infection: Anorectal infection","Recommended: Doxycycline 100 mg PO, BD for 7 days if asymptomatic, but 21 days if symptomatic","Alternative: Azithromycin 1 g PO, stat. and repeat in 12-24 hours"])));
        assert!(r.iter().any(|b|has_all(b,&["Rectal co-infection","ceftriaxone","PLUS","doxycycline","21 days if symptomatic"])));
        assert!(r.iter().all(|b|matches!(&b.kind,BlockKind::Recommendation{strength:None,certainty:None,grading_system:None,..})));
        assert!(d.blocks.iter().any(|b|b.text.contains("no earlier than 4 weeks")));
        assert!(d.blocks.iter().any(|b|b.text.contains("3 months")));
        assert!(d.blocks.iter().any(|b|b.text.contains("New Best Practice Guidance")&&b.heading_path.iter().any(|h|h=="Resources")));
        assert!(d.blocks.iter().any(|b|b.text=="Management"));
        assert!(d.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Figure{..})&&!b.text.contains("Become an ASHM")));
        assert_eq!(d.status,"unknown");assert!(d.updated.is_none()&&d.published.is_none()&&d.country.is_none());
        assert!(d.rights.note.as_deref().unwrap().contains("Copyright © ASHM"));assert!(d.rights.license.is_none());
        assert_eq!(d.search_metadata["draft_consultation_notices"].as_array().unwrap().len(),1);
        assert!(d.date_observations.iter().any(|x|x.label=="Feedback due"&&x.normalized.as_deref()==Some("2026-12-02")));
    }
    #[tokio::test]
    async fn actual_gonorrhoea_combinations_asterisk_and_regional_scope(){
        let d=extract_fixture(G,"sexually-transmissible-infections/gonorrhoea/",false).await;
        assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),4);
        let r=recs(&d);assert_eq!(r.len(),7);
        assert!(r.iter().any(|b|has_all(b,&["Uncomplicated pharyngeal infection*","Ceftriaxone 500 mg IMI","PLUS","Azithromycin 2 g PO","1 g 6 hours later","do not need to be re-treated with the higher dose","test of cure is recommended"])));
        let regional=r.iter().find(|b|b.text.contains("Amoxicillin 3 g")).unwrap();
        assert!(has_all(regional,&["penicillin susceptible","local public health unit","Probenecid 1 g","Azithromycin 1 g","when chlamydia not excluded","acquired beyond local or other remote locations, use principal treatment option"]));
        assert_eq!(regional.text.matches("PLUS").count(),2);
        let pregnancy=r.iter().find(|b|b.text.contains("Situation: Pregnancy")).unwrap();
        assert!(pregnancy.text.contains("Same as principal treatment option"));assert!(!pregnancy.text.contains("Ceftriaxone"));
        assert!(d.blocks.iter().any(|b|b.text.contains("recalled for repeat testing after 3 months")&&b.heading_path.iter().any(|h|h=="Auditable Outcomes")));
    }
    #[tokio::test]
    async fn actual_anorectal_empirical_cell_and_specimen_appendix(){
        let d=extract_fixture(A,"syndromes/anorectal-syndromes/",false).await;
        assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),3);
        let r=recs(&d);assert_eq!(r.len(),1);
        assert!(has_all(r[0],&["without waiting for results","doxycycline 100 mg PO bd for 21 days","PLUS ceftriaxone 500mg in 2mL of 1% lignocaine","PLUS Valaciclovir 500mg PO, bd for 5 – 10 days"]));
        assert!(d.blocks.iter().any(|b|has_all(b,&["Doxycycline can be stopped after 7 days","LGV is confirmed negative"])));
        assert!(d.blocks.iter().any(|b|b.text.contains("PCR-positive primary syphilis")&&b.heading_path.iter().any(|h|h=="Further reading")));
        let s=extract_fixture(S,"specimen-collection/",false).await;
        assert!(s.blocks.iter().any(|b|has_all(b,&["Rectal swab:","2-4cms","transport tube for NAAT testing"])));
        assert!(s.blocks.iter().any(|b|has_all(b,&["midstream urine (MSU)","not required for NAAT"])));
        assert!(s.blocks.iter().any(|b|b.text.contains("tonsils and oropharynx")));
        assert!(recs(&s).is_empty());
    }
    #[tokio::test]
    async fn genuine_draft_template_separates_update_and_deadline(){
        let d=extract_fixture(D,"contact-tracing/public-consultation/",true).await;
        assert_eq!(d.status,"in_review");assert_eq!(d.updated.as_deref(),Some("2026-10"));assert!(d.published.is_none());
        assert!(d.date_observations.iter().any(|x|x.label=="Last updated"&&x.raw=="October 2026"));
        assert!(d.date_observations.iter().any(|x|x.label=="Feedback due"&&x.normalized.as_deref()==Some("2026-12-02")));
        assert!(d.attachments.iter().any(|v|v["url"].as_str().is_some_and(|u|u.ends_with("/ASHM-Contact-Tracing-Guidelines.pdf"))));
        assert!(d.blocks.iter().any(|b|has_all(b,&["Following the public consultation period","will be integrated","subsequently be archived"])));
        assert!(d.relations.is_empty());assert!(recs(&d).is_empty());
    }
    #[tokio::test]
    async fn genuine_catalog_graph_is_finite_deduped_and_body_scoped(){
        let doc=Html::parse_document(CAT);let base=Url::parse(HOME).unwrap();let entries=links(doc.select(&sel(ROOT)).next().unwrap(),&base);
        assert_eq!(entries.len(),17);assert!(entries.iter().all(|(u,_,_)|u.starts_with(HOME)));
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(HOME,CAT.as_bytes(),"text/html");
        for (path,html) in [("sexually-transmissible-infections/chlamydia/",C),("sexually-transmissible-infections/gonorrhoea/",G),("syndromes/anorectal-syndromes/",A),("specimen-collection/",S),("contact-tracing/public-consultation/",D)]{
            f.seed_cache_for_tests(&format!("https://sti.guidelines.org.au/{path}"),html.as_bytes(),"text/html");
        }
        let rows=AuSti.discover(&source(),&f,None).await.unwrap();
        let unique=rows.iter().map(|r|&r.url).collect::<HashSet<_>>();assert_eq!(unique.len(),rows.len());
        assert!(rows.iter().any(|r|r.url.ends_with("/specimen-collection/")&&r.extra["clinical_sha256"].is_string()));
        let draft=rows.iter().find(|r|r.url.ends_with("/public-consultation/")).unwrap();assert_eq!(draft.extra["draft_route"],true);
        assert!(rows.iter().all(|r|!r.url.contains("/profile/")&&!r.url.contains("/mira/")));
        assert!(rows.iter().any(|r|r.extra["discovery_error"].is_string())); // unseeded nodes remain explicit failures
        let c=rows.iter().find(|r|r.url.ends_with("/chlamydia/")).unwrap();
        AuSti.extract(&source(),&f,c).await.unwrap();
        let changed=C.replace("BD 7 days","BD 8 days");f.seed_cache_for_tests(&c.url,changed.as_bytes(),"text/html");
        assert!(AuSti.extract(&source(),&f,c).await.is_err());
        let p=page(C,&Url::parse(&c.url).unwrap(),false).unwrap();
        assert_eq!(snapshot(&p).unwrap(),snapshot(&page(&C.replace("/images/ashm-logo.png","/images/different-brand.png"),&Url::parse(&c.url).unwrap(),false).unwrap()).unwrap());
    }
}
