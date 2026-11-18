//! CONITEC's source-rendered PCDT table -> full protocol bytes, with legal and annex events scoped separately.
use super::{common, static_list::blocks_for, Adapter, PlanRow};
use crate::guidelines::doc::{Block,BlockKind,DateObservation,FetchRecord,GuidelineDoc,Relation};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
pub struct BrazilConitecPcdt;
const MONTHS:[&str;12]=["janeiro","fevereiro","março","abril","maio","junho","julho","agosto","setembro","outubro","novembro","dezembro"];

fn pt_date(label:&str,raw:&str,url:&str)->DateObservation{
    let mut d=common::date(label,raw,url);
    let numeric=regex::Regex::new(r"^(\d{1,2})[º°]?/(\d{1,2})/(\d{4})$").unwrap();
    let words=regex::Regex::new(r"(?i)^(\d{1,2})[º°]?\s+de\s+([a-zç]+)\s+de\s+(\d{4})$").unwrap();
    let parsed=if let Some(c)=numeric.captures(&d.raw){Some((c[3].to_string(),c[2].parse::<u8>().ok(),c[1].parse::<u8>().ok()))}
        else if let Some(c)=words.captures(&d.raw){let m=MONTHS.iter().position(|m|*m==c[2].to_lowercase()).map(|n|n as u8+1);Some((c[3].to_string(),m,c[1].parse::<u8>().ok()))}else{None};
    if let Some((y,Some(m),Some(day)))=parsed{if (1..=12).contains(&m)&&(1..=31).contains(&day){d.normalized=Some(format!("{y}-{m:02}-{day:02}"));}}
    let month=regex::Regex::new(r"(?i)^([a-zç]+)\s+de\s*(\d{4})$").unwrap();
    if let Some(c)=month.captures(&d.raw){if let Some(m)=MONTHS.iter().position(|m|*m==c[1].to_lowercase()){d.normalized=Some(format!("{}-{:02}",&c[2],m+1));}}
    d
}
fn official_record(html:&str,base:&Url)->Result<Value>{
    let doc=Html::parse_document(html);let root=doc.select(&sel("#content")).next().context("Ministry record content missing")?;
    let title=root.select(&sel("h1.documentFirstHeading")).next().map(|e|text_of(&e)).context("Ministry record title missing")?;
    let description=root.select(&sel(".documentDescription")).next().map(|e|text_of(&e)).context("Ministry record description missing")?;
    let mut ds=vec![];let amendment=regex::Regex::new(r"(?i)(Anexo alterado em)\s+(\d{1,2}/\d{1,2}/\d{4})").unwrap();
    for c in amendment.captures_iter(&description){ds.push(pt_date(&c[1],&c[2],base.as_str()));}
    for (selector,label) in [(".documentPublished .value","Publicado em"),(".documentModified .value","Atualizado em")]{
        if let Some(e)=root.select(&sel(selector)).next(){let raw=text_of(&e);let mut d=pt_date(label,raw.split_whitespace().next().unwrap_or(""),base.as_str());d.raw=raw;ds.push(d);}
    }
    let downloads=root.select(&sel("#content-core a[href]")).filter_map(|a|{
        let u=common::link(base,a.value().attr("href")?)?;Url::parse(&u).ok()?.path().contains("@@download/file").then(||(u,text_of(&a)))
    }).collect::<Vec<_>>();
    if downloads.len()!=1{bail!("Ministry record has {} possible download links; no target is guessed",downloads.len());}
    Ok(json!({"url":base.as_str(),"title":title,"description":description,"dates":ds,"download_url":downloads[0].0,"download_label":downloads[0].1}))
}
fn catalog(src:&SourceEntry,html:&str,base:&Url)->Result<Vec<PlanRow>>{
    let doc=Html::parse_document(html);
    let tables=doc.select(&sel("#page-document [aria-label='Tabela de PCDT'] > table.slate-table-block")).collect::<Vec<_>>();
    if tables.len()!=1{bail!("CONITEC expected one source-rendered clinical protocol table, found {}",tables.len());}
    let numeric=regex::Regex::new(r"\b\d{1,2}[º°]?/\d{1,2}/\d{4}\b").unwrap();
    let published=regex::Regex::new(r"(?i)(Publicada(?: em)?)\s+(\d{1,2}[º°]?/\d{1,2}/\d{4})").unwrap();
    let amended=regex::Regex::new(r"(?i)(Anexo alterado em)\s+(\d{1,2}/\d{1,2}/\d{4})").unwrap();
    let mut out=vec![];let mut seen=HashSet::new();
    for (n,tr) in tables[0].select(&sel("tbody > tr")).enumerate(){
        let c=common::cells(tr);if c.len()!=4{bail!("CONITEC table row{} has {} cells; inventory requires review",n+1,c.len());}
        let primary=c[0].select(&sel("a[href]")).collect::<Vec<_>>();
        let cell_title=text_of(&c[0]);
        if primary.is_empty(){
            if cell_title.is_empty()||c[0].select(&sel("h2")).next().is_some(){continue;}
            let key=common::short_hash(&cell_title);
            out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("unresolved-catalog-record-{key}"),url:base.to_string(),title:Some(cell_title),rendition:None,version:None,
                extra:json!({"catalog_row":n+1,"discovery_error":"Named catalog row has no source-emitted primary protocol link; no URL is invented"})});continue;
        }
        let title=text_of(&primary[0]);if title.is_empty(){bail!("CONITEC protocol row{} has empty title",n+1);}
        let work=format!("{}-{}",common::slug(&title),common::short_hash(&title.to_lowercase()));
        let legal=text_of(&c[1]);let row_text=text_of(&tr);let mut ds=vec![];
        let legal_prefix=published.find(&legal).map(|m|&legal[..m.start()]).unwrap_or(&legal);
        if let Some(d)=numeric.find(legal_prefix){ds.push(pt_date("Portaria",d.as_str(),base.as_str()));}
        if let Some(p)=published.captures(&legal){ds.push(pt_date(&p[1],&p[2],base.as_str()));}
        for m in amended.captures_iter(&row_text){ds.push(pt_date(&m[1],&m[2],base.as_str()));}
        let mut attachments=vec![];
        for (cell,role) in [(&c[1],"legal_instrument"),(&c[2],"ministry_publication"),(&c[3],"protocol_summary")] {
            for link in cell.select(&sel("a[href]")){if let Some(u)=common::link(base,link.value().attr("href").unwrap_or_default()){attachments.push(json!({"url":u,"label":text_of(&link),"role":role,"rights":"unknown"}));}}
        }
        // Every link in the primary column is a separately resolved source asset. Do not assume
        // the first is the only clinical annex, or silently drop secondary source-emitted parts.
        for a in primary {
            let url=common::link(base,a.value().attr("href").unwrap_or_default()).context("CONITEC invalid primary link")?;
            if !seen.insert(url.clone()){
                if let Some(old)=out.iter_mut().find(|r|r.url==url){
                    if let Some(aliases)=old.extra.get_mut("catalog_aliases").and_then(Value::as_array_mut){aliases.push(json!({"title":title,"row":n+1,"row_text":row_text,"dates":ds,"attachments":attachments}));}
                }
                continue;
            }
            out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{work}-unresolved"),url,title:Some(title.clone()),rendition:Some("pdf".into()),version:None,
                extra:json!({"work_key":work,"catalog_url":base.as_str(),"catalog_row":n+1,"catalog_row_text":row_text,"catalog_primary_link_label":text_of(&a),"catalog_legal_instrument":legal,"catalog_dates":ds,"attachments":attachments,"catalog_aliases":[]})});
        }
    }
    if out.is_empty(){bail!("CONITEC clinical table yielded no primary protocol links");}
    Ok(out)
}
fn pdf_metadata(blocks:&[Block],url:&str)->(Vec<DateObservation>,Vec<Relation>,Value){
    let header=regex::Regex::new(r"(?i)^PORTARIA[^\n]*?\bDE\s+(\d{1,2}[º°]?\s+DE\s+[A-ZÇ]+\s+DE\s+\d{4})").unwrap();
    let mut ds=vec![];let mut legal=None;let mut validity=vec![];let mut revocations=vec![];let mut amendments=vec![];
    for b in blocks.iter().take(25){
        if let Some(c)=header.captures(&b.text){legal=Some(b.text.clone());ds.push(pt_date("Portaria",&c[1],url));}
        if b.text.to_lowercase().contains("entra em vigor"){validity.push(json!({"statement":b.text,"evidence_url":url,"block_id":b.block_id}));}
        if let Some((_,prior))=b.text.split_once("Fica revogada a "){
            let target=prior.split(", publicada").next().unwrap_or(prior).trim_end_matches('.').to_string();
            revocations.push(json!({"named_ordinance":target,"statement":b.text,"evidence_url":url,"block_id":b.block_id,"target_url":null}));
        }
    }
    let amendment=regex::Regex::new(r"(?i)^(ALTERAÇÃO PÓS PUBLICAÇÃO)\s*[–—-]\s*([a-zç]+\s+DE\s*\d{4})$").unwrap();
    let mut seen_amendments=HashSet::new();
    for b in blocks{
        for (statement,locator) in std::iter::once((b.text.as_str(),"block")).chain(b.heading_path.iter().map(|h|(h.as_str(),"heading_path"))){
            if let Some(c)=amendment.captures(statement){
                if !seen_amendments.insert(statement.to_string()){continue;}
                let date=pt_date(&c[1],&c[2],url);amendments.push(json!({"statement":statement,"block_id":b.block_id,"locator_kind":locator,"date":date,"evidence_url":url}));ds.push(date);
            }
        }
    }
    (ds,vec![],json!({"legal_instrument":legal,"validity_statements":validity,"revoked_instruments":revocations,"pdf_amendment_notices":amendments}))
}
fn explicit_recommendations(blocks:&mut [Block])->usize{
    let mut n=0;
    for b in blocks{
        if !matches!(b.kind,BlockKind::Paragraph){continue;}
        let label=if b.text.starts_with("Recomendação:"){Some("Recomendação".to_string())}else{None};
        if label.is_some()||b.text.starts_with("Recomenda-se "){
            b.kind=BlockKind::Recommendation{label,strength:None,certainty:None,population:None,grading_system:None,qualifier:None};n+=1;
        }
    }
    n
}
fn annex_heading(blocks:&mut [Block])->Option<String>{
    let i=blocks.iter().take(25).position(|b|b.text.starts_with("ANEXO PROTOCOLO CLÍNICO E DIRETRIZES TERAPÊUTICAS"))?;
    let title=blocks[i].text.clone();
    if matches!(blocks[i].kind,BlockKind::Paragraph){
        // The genuine epilepsy conversion leaves the printed annex title as a paragraph,
        // whereas asthma recognizes the same source role as H1. Recover that explicit boundary.
        blocks[i].kind=BlockKind::Heading{level:1};
        for b in blocks.iter_mut().skip(i+1){
            if matches!(b.kind,BlockKind::Heading{level:1}){break;}
            if !b.heading_path.iter().any(|h|h==&title){b.heading_path.insert(0,title.clone());}
        }
    }
    Some(title)
}
#[async_trait::async_trait]
impl Adapter for BrazilConitecPcdt{
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
        if limit==Some(0){return Ok(vec![]);}
        let page=f.get_ok(src.catalog_url.as_deref().context("CONITEC catalog URL missing")?).await?;
        let mut rows=catalog(src,&page.text(),&Url::parse(&page.record.final_url)?)?;
        rows.truncate(limit.unwrap_or(usize::MAX));
        for row in &mut rows{
            if row.extra["discovery_error"].is_string(){continue;}
            row.extra["catalog_fetch"]=serde_json::to_value(&page.record)?;
            match f.get_ok(&row.url).await{
                Ok(pdf) if pdf.is_pdf()=>{
                    let mut records=vec![];let mut fetches=vec![];
                    if let Some(urls)=row.title.as_ref().and_then(|t|src.config.get("comparison_records")?.get(t)).and_then(Value::as_array){
                        if urls.len()>10{bail!("CONITEC configured comparison records exceed bounded scope");}
                        for url in urls.iter().filter_map(Value::as_str){
                            match f.get_ok(url).await{
                                Ok(record_page)=>{
                                    fetches.push(record_page.record.clone());
                                    match official_record(&record_page.text(),&Url::parse(&record_page.record.final_url)?) {
                                        Ok(mut record)=>{
                                            let download=record["download_url"].as_str().unwrap().to_string();
                                            match f.get_ok(&download).await{
                                                Ok(other) if other.is_pdf()=>{record["download_sha256"]=json!(other.record.sha256);record["download_same_as_primary"]=json!(other.record.sha256==pdf.record.sha256);fetches.push(other.record);},
                                                Ok(_)=>record["download_error"]=json!("record download did not return PDF bytes"),
                                                Err(e)=>record["download_error"]=json!(format!("{e:#}")),
                                            }
                                            records.push(record);
                                        },
                                        Err(e)=>records.push(json!({"url":url,"error":format!("{e:#}")})),
                                    }
                                },
                                Err(e)=>records.push(json!({"url":url,"error":format!("{e:#}")})),
                            }
                        }
                    }
                    row.extra["comparison_records"]=json!(records);row.extra["comparison_fetches"]=serde_json::to_value(fetches)?;
                    // Fetch timestamps and raw portal markup are not clinical revision inputs.
                    let comparison_facts=row.extra["comparison_records"].as_array().map(|rs|rs.iter().map(|r|{let mut fact=r.clone();if let Some(m)=fact.as_object_mut(){m.remove("error");m.remove("download_error");}fact}).collect::<Vec<_>>()).unwrap_or_default();
                    let aliases=row.extra["catalog_aliases"].as_array().map(|rs|rs.iter().map(|r|json!({"title":r["title"],"row_text":r["row_text"],"dates":r["dates"],"attachments":r["attachments"]})).collect::<Vec<_>>()).unwrap_or_default();
                    let snapshot=crate::common::sha256_hex(serde_json::to_string(&(&pdf.record.sha256,&row.extra["catalog_row_text"],&row.extra["catalog_dates"],&row.extra["attachments"],aliases,comparison_facts))?.as_bytes());
                    row.doc_key=format!("{}-{}",row.extra["work_key"].as_str().unwrap(),&snapshot[..12]);row.extra["pdf_sha256"]=json!(pdf.record.sha256);
                },
                Ok(_)=>row.extra["discovery_error"]=json!("Source primary protocol link returned non-PDF wrapper; explicit terminal resolution is not validated"),
                Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
            }
        }
        Ok(rows)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("CONITEC discovery: {e}");}
        let pdf=f.get_ok(&row.url).await?;if !pdf.is_pdf(){bail!("CONITEC primary protocol is not actual PDF bytes");}
        if row.extra["pdf_sha256"].as_str().is_some_and(|s|s!=pdf.record.sha256){bail!("CONITEC PDF changed after planning; re-plan");}
        let (mut blocks,mut warnings,_)=blocks_for(&pdf,None,30).await?;
        if blocks.is_empty(){bail!("CONITEC PDF has no extracted clinical/legal content");}
        let annex_title=annex_heading(&mut blocks);
        if annex_title.is_none(){warnings.push("Clinical annex title was not recognized in this PDF. A legal-only rendition or incomplete excerpt does not establish full clinical protocol coverage; source role/body requires review.".into());}
        let mut ds:Vec<DateObservation>=serde_json::from_value(row.extra["catalog_dates"].clone()).context("CONITEC planned catalog date observations invalid")?;
        if let Some(aliases)=row.extra["catalog_aliases"].as_array(){for alias in aliases{let extra:Vec<DateObservation>=serde_json::from_value(alias["dates"].clone()).context("CONITEC alias date observations invalid")?;ds.extend(extra);}}
        let mut fetches=vec![];
        if !row.extra["catalog_fetch"].is_null(){fetches.push(serde_json::from_value::<FetchRecord>(row.extra["catalog_fetch"].clone()).context("CONITEC catalog fetch provenance invalid")?);}
        if !row.extra["comparison_fetches"].is_null(){let records:Vec<FetchRecord>=serde_json::from_value(row.extra["comparison_fetches"].clone()).context("CONITEC comparison fetch provenance invalid")?;fetches.extend(records);}
        if let Some(records)=row.extra["comparison_records"].as_array(){for record in records{
            if record["dates"].is_array(){let more:Vec<DateObservation>=serde_json::from_value(record["dates"].clone()).context("CONITEC official record date observations invalid")?;ds.extend(more);}
            if record["error"].is_string()||record["download_error"].is_string(){warnings.push(format!("Official comparison record or its source-emitted download remains unresolved: {}",record));}
            if record["download_same_as_primary"]==false{warnings.push("Official record download differs from catalog protocol PDF bytes; rendition/consolidation equivalence is unresolved. Both source URLs/checksums are retained without selecting a newer clinical edition.".into());}
        }}
        let publication_claims=ds.iter().filter(|d|d.label.to_lowercase().starts_with("publicada")).filter_map(|d|d.normalized.clone()).collect::<HashSet<_>>();
        let published=if publication_claims.len()==1{publication_claims.iter().next().cloned()}else{None};
        if publication_claims.len()>1{warnings.push("Catalog aliases state differing publication dates for the same asset; all claims retained, normalized published left unset.".into());}
        let (pdf_dates,relations,legal)=pdf_metadata(&blocks,&row.url);ds.extend(pdf_dates);
        let annex_claims=ds.iter().filter(|d|d.label.to_lowercase()=="anexo alterado em").filter_map(|d|d.normalized.clone()).collect::<HashSet<_>>();
        if annex_claims.len()>1{warnings.push("Conflicting Anexo alterado em dates are printed by the catalog and official record. Every scoped claim is retained; no single updated/current date is synthesized.".into());}
        if legal["revoked_instruments"].as_array().is_some_and(|a|!a.is_empty()){warnings.push("PDF names a revoked prior ordinance without an independently resolved URL/ID. Its explicit revocation and source locator are preserved in legal metadata, not emitted as an invalid typed relation target.".into());}
        let recs=explicit_recommendations(&mut blocks);
        let mut attachments=row.extra["attachments"].as_array().cloned().unwrap_or_default();
        let mut asset_keys=attachments.iter().map(|a|(a["url"].to_string(),a["role"].to_string())).collect::<HashSet<_>>();
        if let Some(aliases)=row.extra["catalog_aliases"].as_array(){for alias in aliases{if let Some(assets)=alias["attachments"].as_array(){for asset in assets{
            if asset_keys.insert((asset["url"].to_string(),asset["role"].to_string())){attachments.push(asset.clone());}
        }}}}
        if let Some(records)=row.extra["comparison_records"].as_array(){for record in records{
            if record["download_url"].is_string(){attachments.push(json!({"url":record["download_url"],"label":record["download_label"],"role":"official_record_download","rights":"unknown","sha256":record["download_sha256"]}));}
        }}
        let front=blocks.iter().take(3).map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ");
        let publisher=front.contains("MINISTÉRIO DA SAÚDE").then(||"MINISTÉRIO DA SAÚDE".into());
        let mut issuers=vec![];
        if let Some(header)=blocks.first().map(|b|b.text.as_str()).and_then(|s|s.strip_prefix("MINISTÉRIO DA SAÚDE ")){
            let mut parts=header.split(" SECRETARIA DE ");if let Some(first)=parts.next(){issuers.push(first.to_string());}for p in parts{issuers.push(format!("SECRETARIA DE {p}"));}
        }
        for b in &mut blocks{b.page=None;}
        warnings.push("Catalog Portaria, publication and annex-amendment dates are separate source claims. The original ordinance date does not establish that every annex is current or that later amendments are consolidated. No current status is inferred.".into());
        warnings.push("PDF conversion preserves text/tables but does not recover original graphical figure assets or establish merged-cell/algorithm fidelity. Physical page locators are unset until independently mapped. The portal footer/editorial-manual licences are not assigned to this individual protocol.".into());
        warnings.push("Catalog legal-instrument, Ministry-publication and summarized-protocol links retain separate attachment roles; they are not substituted for the primary full clinical protocol or automatically incorporated as extra clinical text.".into());
        if blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("Trata-se de uma técnica que estimula diretamente o nervo")){warnings.push("Converter table classification is unverified: the block on vagus-nerve stimulation contains continuous narrative arranged into cells. It is preserved as raw conversion output, not a verified publisher table.".into());}
        warnings.push(format!("Structured {} explicit Recomendação:/Recomenda-se paragraphs only; all other narrative clinical instructions remain ordered source blocks. Generic GRADE-methodology tables do not assign grades to individual statements.",recs));
        fetches.push(pdf.record);
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:row.title.clone().context("CONITEC catalog title missing")?,url:row.url.clone(),
            item_type:Some("protocolo_clinico_e_diretrizes_terapeuticas".into()),status:"unknown".into(),published,date_observations:ds,publisher,issuers,rights:common::unknown_rights(),relations,blocks,
            attachments,search_metadata:json!({"catalog_url":row.extra["catalog_url"],"catalog_row":row.extra["catalog_row"],"catalog_row_text":row.extra["catalog_row_text"],"catalog_legal_instrument":row.extra["catalog_legal_instrument"],"catalog_aliases":row.extra["catalog_aliases"],"official_record_claims":row.extra["comparison_records"],"pdf_annex_title":annex_title,"pdf_legal_evidence":legal}),warnings,fetches,..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests{
    use super::*;
    const HOME:&str="https://www.gov.br/conitec/pt-br/assuntos/avaliacao-de-tecnologias-em-saude/protocolos-clinicos-e-diretrizes-terapeuticas/pcdt";
    const EP:&str="https://www.gov.br/conitec/pt-br/midias/protocolos/pcdt_epilepsia-1.pdf/@@display-file/file";
    const ASTHMA:&str="https://www.gov.br/conitec/pt-br/midias/protocolos/2026/asma-portaria-conjunta-saes-sctie-no-43.pdf/@@display-file/file";
    const MINISTRY:&str="https://www.gov.br/saude/pt-br/assuntos/pcdt/e/epilepsia/view";
    const DOWNLOAD:&str="https://www.gov.br/saude/pt-br/assuntos/pcdt/e/epilepsia/@@download/file";
    const CATALOG:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/brazil_conitec_pcdt/catalog.html"));
    const RECORD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/brazil_conitec_pcdt/ministry-epilepsy.html"));
    const EP_BLOCKS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/brazil_conitec_pcdt/epilepsy-blocks.json"));
    const AS_BLOCKS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/brazil_conitec_pcdt/asthma-blocks.json"));
    const EP1:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/brazil_conitec_pcdt/epilepsy-page-1.pdf"));
    const EP29:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/brazil_conitec_pcdt/epilepsy-page-29.pdf"));
    const AS3:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/brazil_conitec_pcdt/asthma-page-3.pdf"));
    fn source()->SourceEntry{serde_json::from_value(json!({"id":"brazil_conitec_pcdt","access":"public","enabled":true,"catalog_url":HOME,
        "config":{"comparison_records":{"Epilepsia":[MINISTRY]}}})).unwrap()}
    #[test]
    fn portuguese_legal_and_catalog_dates_keep_their_own_precision_and_event_labels(){
        assert_eq!(pt_date("Portaria","21 DE JUNHO DE 2018","https://example.test/protocol").normalized.as_deref(),Some("2018-06-21"));
        assert_eq!(pt_date("Portaria","24 DE MARÇO DE 2026","https://example.test/protocol").normalized.as_deref(),Some("2026-03-24"));
        assert_eq!(pt_date("Publicada em","02/04/2026","https://example.test/catalog").normalized.as_deref(),Some("2026-04-02"));
        let d=pt_date("Anexo alterado em","04/09/2026","https://example.test/catalog");
        assert_eq!(d.normalized.as_deref(),Some("2026-09-04"));assert_eq!(d.label,"Anexo alterado em");assert_eq!(d.raw,"04/09/2026");
        assert!(pt_date("Vigência","na data de sua publicação","https://example.test/protocol").normalized.is_none());
        for raw in ["ABRIL DE 2026","ABRIL DE2026"]{let d=pt_date("ALTERAÇÃO PÓS PUBLICAÇÃO",raw,"https://example.test/protocol");assert_eq!(d.normalized.as_deref(),Some("2026-04"));assert_eq!(d.raw,raw);}
        assert_eq!(pt_date("Portaria","1º/07/2025","https://example.test/catalog").normalized.as_deref(),Some("2025-07-01"));
    }
    #[test]
    fn genuine_entire_catalog_has_scoped_dates_roles_and_last_row_ordinal_date(){
        let rows=catalog(&source(),CATALOG,&Url::parse(HOME).unwrap()).unwrap();
        assert!(rows.len()>100);
        let ep=rows.iter().find(|r|r.url==EP).unwrap();let ast=rows.iter().find(|r|r.url==ASTHMA).unwrap();
        for (row,legal,pubdate,amended) in [(ep,"2018-06-21","2018-06-27","2025-07-23"),(ast,"2026-03-24","2026-04-02","2026-09-04")]{
            let ds:Vec<DateObservation>=serde_json::from_value(row.extra["catalog_dates"].clone()).unwrap();
            assert!(ds.iter().any(|d|d.label=="Portaria"&&d.normalized.as_deref()==Some(legal)));
            assert!(ds.iter().any(|d|d.label=="Publicada em"&&d.normalized.as_deref()==Some(pubdate)));
            assert!(ds.iter().any(|d|d.label=="Anexo alterado em"&&d.normalized.as_deref()==Some(amended)));
            assert!(row.version.is_none(),"legal instrument/annex events are not fabricated publisher edition labels");
            assert!(row.extra["attachments"].as_array().unwrap().iter().any(|a|a["role"]=="legal_instrument"));
            assert!(row.extra["attachments"].as_array().unwrap().iter().any(|a|a["role"]=="protocol_summary"));
        }
        let last=rows.iter().find(|r|r.title.as_deref()==Some("Vasculite Associada aos Anticorpos Anti-citoplasma de Neutrófilos")).unwrap();
        let ds:Vec<DateObservation>=serde_json::from_value(last.extra["catalog_dates"].clone()).unwrap();
        assert!(ds.iter().any(|d|d.label=="Portaria"&&d.raw=="1º/07/2025"&&d.normalized.as_deref()==Some("2025-07-01")));
        assert!(ds.iter().any(|d|d.label=="Publicada em"&&d.normalized.as_deref()==Some("2025-07-14")),"ordinal first-day adoption must not be replaced with the publication date");
    }
    #[test]
    fn full_conversion_subsets_keep_all_table_blocks_legal_scope_and_explicit_directives(){
        for (input,total,legal_date,revoked) in [(EP_BLOCKS,17,"2018-06-21","1.319/SAS/MS"),(AS_BLOCKS,26,"2026-03-24","32, de 20 de dezembro de 2023")]{
            let mut blocks:Vec<Block>=serde_json::from_str(input).unwrap();
            assert!(annex_heading(&mut blocks).is_some());
            assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Heading{level:1})&&b.text.starts_with("ANEXO PROTOCOLO CLÍNICO")));
            assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),total,"pin complete converter table-block collection, not a claim that every classification is visually verified");
            let (ds,relations,meta)=pdf_metadata(&blocks,if total==17{EP}else{ASTHMA});
            assert!(ds.iter().any(|d|d.label=="Portaria"&&d.normalized.as_deref()==Some(legal_date)));
            assert!(relations.is_empty(),"named prior ordinances without URL/ID are facts, not valid typed targets");
            assert!(meta["revoked_instruments"].as_array().unwrap().iter().any(|r|r["named_ordinance"].as_str().unwrap().contains(revoked)&&r["target_url"].is_null()));
            assert!(meta["validity_statements"].as_array().unwrap().iter().any(|v|v["statement"].as_str().unwrap().contains("entra em vigor na data de sua publicação")));
            let recs=explicit_recommendations(&mut blocks);
            assert_eq!(recs,if total==17{0}else{3});
            assert!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).all(|b|matches!(b.kind,BlockKind::Recommendation{strength:None,certainty:None,grading_system:None,..})),"GRADE methodology tables are not individual clinical grades");
            if total==17{
                assert!(ds.iter().any(|d|d.label=="ALTERAÇÃO PÓS PUBLICAÇÃO"&&d.raw=="ABRIL DE 2026"&&d.normalized.as_deref()==Some("2026-04")));
                assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("gap diagnóstico de 87%")),"retain the final national-evidence table tail");
            }else{
                assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("Asma Grave")&&b.text.contains("comorbidades não tratadas")));
                assert!(blocks.iter().any(|b|b.text.contains("Fonte: Adaptado de Gina, 2025")));
                assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("Primeira versão do Protocolo Clínico e Diretrizes Terapêuticas da Asma.")),"retain the final legal-history table");
            }
        }
    }
    #[test]
    fn actual_ministry_record_and_independent_fetch_checksums_are_not_date_equivalence(){
        let record=official_record(RECORD,&Url::parse(MINISTRY).unwrap()).unwrap();
        assert_eq!(record["download_url"],DOWNLOAD);
        let ds:Vec<DateObservation>=serde_json::from_value(record["dates"].clone()).unwrap();
        assert!(ds.iter().any(|d|d.label=="Anexo alterado em"&&d.normalized.as_deref()==Some("2026-05-05")));
        assert!(ds.iter().any(|d|d.label=="Publicado em"&&d.raw=="23/07/2025 11h38"&&d.normalized.as_deref()==Some("2025-07-23")));
        assert!(ds.iter().any(|d|d.label=="Atualizado em"&&d.raw=="05/05/2026 11h17"));
        let a:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/brazil_conitec_pcdt/conitec-epilepsy-response.json"))).unwrap();
        let b:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/brazil_conitec_pcdt/ministry-epilepsy-response.json"))).unwrap();
        assert_ne!(a["url"],b["url"]);assert_eq!(a["bytes"],708645);assert_eq!(a["bytes"],b["bytes"]);assert_eq!(a["sha256"],b["sha256"]);
        assert_eq!(a["sha256"],"26ddc5868cf23457187db45c0300dd645b2667e9ddd6b9d8f05aef5ef75cd466");
    }
    #[tokio::test]
    async fn genuine_pdf_excerpts_are_honest_about_clinical_coverage_and_conflicting_amendments(){
        for (url,bytes,kind) in [(EP,EP1,1),(EP,EP29,29),(ASTHMA,AS3,3)]{
            assert!(bytes.len()<200_000);
            let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<24,"").unwrap();
            f.seed_cache_for_tests(HOME,CATALOG.as_bytes(),"text/html");f.seed_cache_for_tests(url,bytes,"application/pdf");
            f.seed_cache_for_tests(MINISTRY,RECORD.as_bytes(),"text/html");
            // Original official PDFs were independently verified byte-identical (metadata test
            // above). This reuses the same genuine small excerpt for their two cache routes.
            if url==EP{f.seed_cache_for_tests(DOWNLOAD,bytes,"application/pdf");}
            let rows=BrazilConitecPcdt.discover(&source(),&f,None).await.unwrap();
            assert!(rows.iter().any(|r|r.extra["discovery_error"].is_string()),"unseeded catalog bodies remain audits rather than dropped records");
            let row=rows.iter().find(|r|r.url==url).unwrap();
            let d=BrazilConitecPcdt.extract(&source(),&f,row).await.unwrap();
            assert_eq!(d.status,"unknown");assert!(d.updated.is_none()&&d.version.is_none());
            assert_eq!(d.rights.status,"unknown");assert!(d.rights.license.is_none());
            assert!(d.blocks.iter().all(|b|b.page.is_none()&&!matches!(b.kind,BlockKind::Figure{..})));
            assert!(d.warnings.iter().any(|w|w.contains("Physical page locators are unset")));
            assert!(d.warnings.iter().any(|w|w.contains("Clinical annex title was not recognized")),"these one-page excerpts are not complete clinical protocols");
            if url==EP{
                assert_eq!(d.published.as_deref(),Some("2018-06-27"));
                for date in ["2025-07-23","2026-05-05"]{assert!(d.date_observations.iter().any(|v|v.label=="Anexo alterado em"&&v.normalized.as_deref()==Some(date)));}
                assert!(d.warnings.iter().any(|w|w.contains("Conflicting Anexo alterado em")));
                assert_eq!(d.search_metadata["official_record_claims"][0]["download_same_as_primary"],true);
                assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),0);
                if kind==1{
                    assert!(d.date_observations.iter().any(|v|v.source_url.as_deref()==Some(EP)&&v.normalized.as_deref()==Some("2018-06-21")));
                    assert!(d.blocks.iter().any(|b|b.text.contains("Fica revogada a Portaria nº 1.319/SAS/MS")));
                }else{
                    let all_text=d.blocks.iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ");
                    let tail=all_text.split("Fenobarbital:").nth(1).expect("retain the genuine narrative-page final drug label");
                    assert!(tail.contains("hiperatividade em crianças"),"retain the adjacent continuation of the final drug-list item");
                }
            }else{
                assert_eq!(d.published.as_deref(),Some("2026-04-02"));
                assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),1);
                assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("Asma Grave")&&b.text.contains("comorbidades não tratadas")));
                assert!(d.blocks.iter().any(|b|b.text.contains("Fonte: Adaptado de Gina, 2025")));
            }
            let mut stale=row.clone();stale.extra["pdf_sha256"]=json!("stale");
            assert!(BrazilConitecPcdt.extract(&source(),&f,&stale).await.unwrap_err().to_string().contains("changed after planning"));
        }
    }
}
