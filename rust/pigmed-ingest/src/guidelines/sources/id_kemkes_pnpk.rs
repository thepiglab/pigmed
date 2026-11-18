//! Source-emitted PNPK collections, year pages, Unduh records and official legal/clinical PDFs.
use crate::guidelines::doc::{Block,DateObservation,FetchRecord,GuidelineDoc};
use super::{common,static_list::blocks_for,Adapter,PlanRow};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::registry::SourceEntry;
pub struct IdKemkesPnpk;
use crate::guidelines::html::{sel,text_of};
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::{json,Value};
use std::collections::HashSet;
use url::Url;
const FOLDER:&str="/id/media/subfolder/pedoman/pedoman-nasional-pelayanan-kedokteran-pnpk";
const YEAR:&str="/id/media/list/pedoman/pedoman-nasional-pelayanan-kedokteran-pnpk/";
fn folder(raw:&str,base:&Url)->Result<Vec<Value>>{
 let doc=Html::parse_document(raw);let roots=doc.select(&sel(".content-wrapper > .container")).collect::<Vec<_>>();if roots.len()!=1{bail!("PNPK expected one collection body, found {}",roots.len());}let root=roots[0];
 if root.select(&sel("h2.title")).next().map(|e|text_of(&e)).as_deref()!=Some("PEDOMAN NASIONAL PELAYANAN KEDOKTERAN (PNPK)"){bail!("PNPK collection title missing");}
 let mut out=vec![];let mut seen=HashSet::new();
 for a in root.select(&sel(".card > a.link[href]")){
  let url=base.join(a.value().attr("href").unwrap())?;if url.origin()!=base.origin()||!url.username().is_empty()||url.password().is_some()||!url.path().starts_with(YEAR)||url.query().is_some(){bail!("PNPK collection card has unvalidated target {url}");}
  let label=a.select(&sel("h4")).next().map(|e|text_of(&e)).context("PNPK collection label missing")?;
  if seen.insert(url.to_string()){out.push(json!({"url":url.as_str(),"role":"source_collection","label":label,"description":a.select(&sel("p")).next().map(|e|text_of(&e)),"scope":"folder collection label; not a clinical edition/publication date"}));}
 }
 if out.is_empty(){bail!("PNPK collection cards empty");}
 for a in root.select(&sel(".pagination a[href]")){
  let href=a.value().attr("href").unwrap();if href.starts_with('#'){continue;}let mut url=base.join(href)?;url.set_fragment(None);
  let suffix=url.path().strip_prefix(FOLDER).unwrap_or("invalid");let numbered=suffix.strip_prefix('/').is_some_and(|s|!s.is_empty()&&s.chars().all(|c|c.is_ascii_digit()));
  if url.origin()!=base.origin()||!url.username().is_empty()||url.password().is_some()||url.query().is_some()||!numbered{continue;}
  let requested=base.path().strip_prefix(FOLDER).and_then(|s|s.strip_prefix('/')).and_then(|s|s.parse::<usize>().ok()).unwrap_or(1);let target=url.path().rsplit('/').next().unwrap().parse::<usize>()?;if target<=requested{continue;}
  if seen.insert(url.to_string()){out.push(json!({"url":url.as_str(),"role":"folder_page","label":text_of(&a),"page_number":target}));}
 }
 pager_gap_warning(&mut out,"folder_page",base);
 Ok(out)
}
/// The portal's numbered pager shows a window of pages (e.g. 1 [2] [3] Next on page 1), so every forward page it
/// emits is queued; the visited set dedupes. If the immediately following page is not among the emitted links
/// while later ones are, the gap is kept as a warning on those page nodes (and so on the rows planned from them),
/// not a fatal error and not a guessed URL.
fn pager_gap_warning(out:&mut [Value],role:&str,base:&Url){
 let requested=base.path().trim_end_matches('/').rsplit('/').next().and_then(|s|s.parse::<usize>().ok()).unwrap_or(1);
 let pages=out.iter().filter(|n|n["role"]==role).filter_map(|n|n["page_number"].as_u64()).collect::<Vec<_>>();
 if pages.is_empty()||pages.contains(&(requested as u64+1)){return}
 for n in out.iter_mut().filter(|n|n["role"]==role){n["pager_warning"]=json!(format!("source pager on {base} (page {requested}) emits no link to page {} but links later pages; pages are followed as emitted and the gap may leave records unlisted",requested+1));}
}
fn year(raw:&str,base:&Url)->Result<Vec<Value>>{
 if !base.path().starts_with(YEAR){bail!("PNPK year catalog route mismatch");}let doc=Html::parse_document(raw);let roots=doc.select(&sel(".content-wrapper > .container")).collect::<Vec<_>>();if roots.len()!=1{bail!("PNPK year catalog body missing or ambiguous");}let root=roots[0];let title=root.select(&sel("h2.title")).next().map(|e|text_of(&e)).context("PNPK year title missing")?;
 let mut out:Vec<Value>=vec![];let mut seen=std::collections::HashMap::<String,usize>::new();
 for a in root.select(&sel(".row > div > a.link[href]")){
  let url=base.join(a.value().attr("href").unwrap())?;if url.origin()!=base.origin()||!url.username().is_empty()||url.password().is_some()||!url.path().starts_with("/id/")||url.path().starts_with("/id/media/")||url.query().is_some(){bail!("PNPK source card unvalidated detail target {url}");}
  let label=a.select(&sel(".card h4")).next().map(|e|text_of(&e)).context("PNPK detail card title missing")?;
  let time=a.select(&sel("time")).next().map(|e|json!({"label":"Catalog card date","raw":text_of(&e),"datetime":e.value().attr("datetime"),"source_url":base.as_str(),"scope":"catalog card timestamp, not legal enactment or clinical publication/current status"}));
  let mention=json!({"label":label,"collection_title":title,"source_collection":base.as_str(),"date_observation":time,"source_card":a.html()});
  if let Some(i)=seen.get(url.as_str()){out[*i]["catalog_mentions"].as_array_mut().unwrap().push(mention);}else{seen.insert(url.to_string(),out.len());out.push(json!({"url":url.as_str(),"role":"detail_record","label":label,"catalog_mentions":[mention]}));}
 }
 if out.is_empty(){bail!("PNPK year catalog contains no detail records");}
 let path=base.path().trim_end_matches('/');let series=if path.rsplit('/').next().is_some_and(|p|p.chars().all(|c|c.is_ascii_digit())){path.rsplit_once('/').unwrap().0}else{path};
 for a in root.select(&sel(".pagination a[href]")){let href=a.value().attr("href").unwrap();if href.starts_with('#'){continue;}let url=base.join(href)?;
  let numbered=url.path().strip_prefix(&format!("{series}/")).is_some_and(|s|!s.is_empty()&&s.chars().all(|c|c.is_ascii_digit()));if url.origin()!=base.origin()||!url.username().is_empty()||url.password().is_some()||url.query().is_some()||!numbered{continue;}
  let requested=if path!=series{path.rsplit('/').next().unwrap().parse::<usize>()?}else{1};let target=url.path().rsplit('/').next().unwrap().parse::<usize>()?;if target<=requested{continue;}
  if seen.insert(url.to_string(),out.len()).is_none(){out.push(json!({"url":url.as_str(),"role":"year_page","label":text_of(&a),"page_number":target}));}
 }pager_gap_warning(&mut out,"year_page",base);Ok(out)
}
fn terminal(raw:&str,base:&Url)->Result<Value>{
 let doc=Html::parse_document(raw);let bodies=doc.select(&sel(".content-wrapper > .container .col-md-8.mb-4 > .card > .card-body")).collect::<Vec<_>>();if bodies.len()!=1{bail!("PNPK expected one source terminal card body, found {}",bodies.len());}let body=bodies[0];
 let title=body.select(&sel("h1.main-title")).next().map(|e|text_of(&e)).context("PNPK terminal clinical title missing")?;
 let download=body.select(&sel("a[href]")).filter(|a|text_of(a)=="Unduh").collect::<Vec<_>>();if download.is_empty(){bail!("PNPK portal record has no Unduh (download) link: the portal has not attached a guideline PDF to this record, so there is nothing to extract yet (re-plan later)");}if download.len()!=1{bail!("PNPK terminal expected one source Unduh link, found {}",download.len());}let url=base.join(download[0].value().attr("href").unwrap())?;
 if url.origin()!=base.origin()||!url.username().is_empty()||url.password().is_some()||!url.path().starts_with("/app_asset/file_content_download/")||!url.path().ends_with(".pdf")||url.query().is_some(){bail!("PNPK emitted download route not validated: {url}");}
 let time=body.select(&sel(".artikel-meta time")).next().map(|t|json!({"label":"Portal record date","raw":text_of(&t),"datetime":t.value().attr("datetime"),"source_url":base.as_str(),"scope":"portal posting observation; independent of PDF legal adoption/commencement/currentness"}));
 let overview=body.select(&sel(".isi-konten")).next().map(|e|json!({"html":e.html(),"text":text_of(&e),"role":"source record overview; full clinical content is linked PDF"})).context("PNPK terminal overview missing")?;
 Ok(json!({"title":title,"pdf_url":url.as_str(),"portal_date":time,"overview":overview,"source_url":base.as_str(),"download_label":"Unduh"}))
}
fn adoption_dates(blocks:&[Block],url:&str)->(Vec<DateObservation>,Vec<Value>){
 let date=regex::Regex::new(r"Ditetapkan\s+di\s+Jakarta\s+pada\s+tanggal\s+(\d{1,2})\s+([A-Za-z]+)\s+((?:19|20)\d{2})\b").unwrap();let months=["januari","februari","maret","april","mei","juni","juli","agustus","september","oktober","november","desember"];let mut dates=vec![];let mut spans=vec![];let legal_end=blocks.iter().position(|b|b.text.contains("LAMPIRAN")&&b.text.contains("KEPUTUSAN MENTERI KESEHATAN")).unwrap_or(blocks.len());
 for i in 0..legal_end{if !blocks[i].text.contains("Ditetapkan"){continue;}for end in i+1..=(i+4).min(legal_end){let printed=blocks[i..end].iter().map(|b|b.text.as_str()).collect::<Vec<_>>().join(" ");let clean=printed.replace(['*','_'],"").replace("<u>","").replace("</u>","");let Some(c)=date.captures(&clean)else{continue};let raw=format!("{} {} {}",&c[1],&c[2],&c[3]);let mut observed=common::date("Ditetapkan di Jakarta pada tanggal",&raw,url);if let Some(month)=months.iter().position(|m|*m==c[2].to_lowercase()){if let Ok(day)=c[1].parse::<u8>(){if (1..=31).contains(&day){observed.normalized=Some(format!("{}-{:02}-{day:02}",&c[3],month+1));}}}spans.push(json!({"source_block_ids":blocks[i..end].iter().map(|b|b.block_id.as_str()).collect::<Vec<_>>(),"source_url":url,"source_text":printed,"printed_date":raw,"scope":"literal adoption label/date in source legal front, allowing adjacent layout blocks; no filename/identifier year inference"}));dates.push(observed);break;}}
 (dates,spans)
}
fn legal_metadata(blocks:&[Block],url:&str)->Result<(Vec<DateObservation>,Value)>{
 let first=blocks.first().context("PNPK PDF has no content")?;if !first.text.contains("KEPUTUSAN MENTERI KESEHATAN REPUBLIK INDONESIA"){bail!("PNPK PDF legal decree heading missing");}
 let id=regex::Regex::new(r"HK\.\d{2}\.\d{2}/MENKES/\d+/(?:19|20)\d{2}").unwrap().find(&first.text).map(|m|m.as_str().to_string()).context("PNPK explicit legal decree identifier missing")?;
 let(dates,date_spans)=adoption_dates(blocks,url);let mut annexes=vec![];let mut revocation=vec![];let mut effective=vec![];let mut labels=vec![];let grades=regex::Regex::new(r"(?i)Rekomendasi\s+([A-D])\b|Kelas\s+(I{1,3}[ab]?),\s*Peringkat\s+bukti\s+([A-C])\b").unwrap();
 for (position,b) in blocks.iter().enumerate(){
  if b.text.contains("LAMPIRAN")&&b.text.contains("KEPUTUSAN MENTERI KESEHATAN")&&b.text.contains(&id)&&b.text.contains("TENTANG"){
   annexes.push(json!({"source_block_id":b.block_id,"ordered_block_position":position+1,"printed_heading":b.text,"physical_page":null,"scope":"clinical annex heading; leading printed number is not a validated physical page locator"}));
  }
  if annexes.is_empty()&&b.text.contains("dicabut dan dinyatakan tidak berlaku"){revocation.push(json!({"source_block_id":b.block_id,"text":b.text,"target_url":null,"scope":"explicit named-instrument revocation; no typed relation until an actual source target URL/ID is resolved"}));}
  if annexes.is_empty()&&b.text.contains("mulai berlaku pada tanggal ditetapkan"){effective.push(json!({"source_block_id":b.block_id,"text":b.text,"scope":"legal commencement claim; not present-day clinical currency"}));}
  for c in grades.captures_iter(&b.text){labels.push(json!({"printed":c.get(0).unwrap().as_str(),"printed_grade":c.get(1).map(|m|m.as_str()),"printed_class":c.get(2).map(|m|m.as_str()),"printed_evidence_rank":c.get(3).map(|m|m.as_str()),"source_block_id":b.block_id,"source_url":url,"start_byte_in_source_block":c.get(0).unwrap().start(),"source_block_text":b.text,"role":if b.text.contains("bila berdasar")||b.text.contains("Derajat Rekomendasi"){"grading_legend"}else{"source_grade_mention"},"association":"exact source block/occurrence retained; no whole-table/paragraph grade projected"}));}
 }
 let conflicts=dates.iter().filter(|d|d.normalized.as_deref().is_some_and(|n|n.get(..4)!=id.rsplit('/').next())).map(|d|json!({"printed_decree_identifier":id,"adoption_date":d,"scope":"source identifier year differs from printed adoption date year; both retained without correction"})).collect::<Vec<_>>();
 Ok((dates,json!({"decree_identifier":id,"adoption_date_source_spans":date_spans,"identifier_adoption_year_conflicts":conflicts,"source_legal_title":first.text,"clinical_annexes":annexes,"revocation_claims":revocation,"commencement_claims":effective,"printed_recommendation_grade_mentions":labels,"grading_legend_contexts":blocks.iter().enumerate().filter(|(_,b)|b.text.starts_with("|Kelas|")||b.text.starts_with("|Peringkat bukti|")).map(|(i,b)|json!({"source_table_block_id":b.block_id,"source_blocks":blocks[i..(i+4).min(blocks.len())].iter().map(|b|json!({"source_block_id":b.block_id,"text":b.text})).collect::<Vec<_>>(),"scope":"converted source legend with subsequent source context; row geometry and split continuations not invented"})).collect::<Vec<_>>(),"status_scope":"legal current status requires its actual legal record; decree/date/folder year alone do not establish current clinical status"})))
}
#[async_trait::async_trait]
impl Adapter for IdKemkesPnpk{
 async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>>{
  if limit==Some(0){return Ok(vec![]);}let start=src.catalog_url.clone().context("PNPK catalog URL missing")?;let mut pending=std::collections::VecDeque::from([(start,"folder_page".to_string(),Value::Null)]);let mut visited=HashSet::new();let mut records=HashSet::new();let mut out:Vec<PlanRow>=vec![];
  while let Some((url,role,context))=pending.pop_front(){if !visited.insert(url.clone()){continue;}if visited.len()>300{bail!("PNPK emitted graph exceeds300 catalog pages");}
   let page=f.get_ok(&url).await?;let base=Url::parse(&page.record.final_url)?;let nodes=if role=="folder_page"{folder(&page.text(),&base)?}else{year(&page.text(),&base)?};
   for node in nodes{let url=node["url"].as_str().unwrap().to_string();let role=node["role"].as_str().unwrap();if role!="detail_record"{pending.push_back((url,if role=="folder_page"{"folder_page"}else{"year_page"}.into(),node));continue;}
    if !records.insert(url.clone()){if let Some(existing)=out.iter_mut().find(|r|r.url==url){existing.extra["additional_catalog_records"].as_array_mut().unwrap().push(json!({"catalog_record":node,"catalog_fetch":page.record}));}continue;}
    let key=common::slug(Url::parse(&url)?.path().trim_matches('/'));let mut row=PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-unresolved"),url,title:node["label"].as_str().map(String::from),rendition:Some("pdf".into()),version:None,extra:json!({"work_key":key,"source_role":"official_legal_clinical_guideline","source_catalog_parent":context,"catalog_record":node,"additional_catalog_records":[],"catalog_fetch":page.record,"inventory_scope":"source-emitted folder/year graph and explicit Unduh links; source branch failures remain errors, not completion"})};
    let result=async{let detail=f.get_ok(&row.url).await?;let base=Url::parse(&detail.record.final_url)?;let record=terminal(&detail.text(),&base)?;let pdf=f.get_ok(record["pdf_url"].as_str().unwrap()).await?;if !pdf.is_pdf(){bail!("PNPK source Unduh target is not actual PDF bytes");}Ok::<_,anyhow::Error>((record,detail.record,pdf.record))}.await;
    match result{Ok((record,detail,pdf))=>{row.doc_key=format!("{}-{}",row.extra["work_key"].as_str().unwrap(),&pdf.sha256[..12]);row.title=record["title"].as_str().map(String::from);row.extra["terminal_record"]=record;row.extra["terminal_fetch"]=json!(detail);row.extra["pdf_sha256"]=json!(pdf.sha256);row.extra["pdf_fetch"]=json!(pdf);},Err(error)=>row.extra["discovery_error"]=json!(format!("{error:#}"))}
    out.push(row);if limit.is_some_and(|n|out.len()>=n){return Ok(out);}
   }
  }Ok(out)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
  if let Some(error)=row.extra["discovery_error"].as_str(){bail!("PNPK discovery: {error}");}let response=f.get_ok(&row.url).await?;let base=Url::parse(&response.record.final_url)?;let record=terminal(&response.text(),&base)?;let url=record["pdf_url"].as_str().unwrap();if row.extra["terminal_record"]["pdf_url"].as_str().is_some_and(|old|old!=url){bail!("PNPK source download changed; re-plan");}let pdf=f.get_ok(url).await?;if !pdf.is_pdf(){bail!("PNPK source download not PDF bytes");}if row.extra["pdf_sha256"].as_str().is_some_and(|sha|sha!=pdf.record.sha256){bail!("PNPK source PDF changed; re-plan");}
  let(mut blocks,mut warnings,_)=blocks_for(&pdf,None,30).await?;for b in &mut blocks{b.page=None;}let(mut dates,legal)=legal_metadata(&blocks,url)?;if let Some(raw)=record["portal_date"]["raw"].as_str(){let mut observed=common::date("Portal record date",raw,base.as_str());if let Some(machine)=record["portal_date"]["datetime"].as_str(){if regex::Regex::new(r"^(?:19|20)\d{2}-\d{2}-\d{2}$").unwrap().is_match(machine){observed.normalized=Some(machine.to_string());}}dates.push(observed);}let mut rights=common::unknown_rights();rights.evidence_urls.push(url.to_string());let notices=blocks.iter().filter(|b|b.text.contains("Hak Cipta")||b.text.contains("hak cipta")||b.text.contains("Copyright")||b.text.contains("©")).map(|b|json!({"source_block_id":b.block_id,"text":b.text})).collect::<Vec<_>>();
  let mut fetches=vec![];if let Ok(fetch)=serde_json::from_value::<FetchRecord>(row.extra["catalog_fetch"].clone()){fetches.push(fetch);}for additional in row.extra["additional_catalog_records"].as_array().into_iter().flatten(){if let Ok(fetch)=serde_json::from_value::<FetchRecord>(additional["catalog_fetch"].clone()){fetches.push(fetch);}}fetches.push(response.record);fetches.push(pdf.record.clone());
  if !legal["identifier_adoption_year_conflicts"].as_array().unwrap().is_empty(){warnings.push("Source decree identifier year conflicts with the printed adoption-date year; both exact observations are retained without repair or inferred currentness.".into());}
  warnings.push("Portal posting date, source collection label/year, legal adoption and commencement are separate observations. No current clinical or legal status, publication date, geographic applicability or reuse licence inferred from government identity. Named repealed instruments remain verbatim claims without invented target relations.".into());
  warnings.push("Official source-emitted PDF retains legal front matter, clinical annexes and converted tables in source order. Exact grade mentions are preserved with their source blocks, not projected over whole paragraphs/tables. Physical pages, merged cells, diagrams and original figures remain unverified. Portal PDF and separate JDIH PDF are not presumed byte-equivalent.".into());
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title:record["title"].as_str().unwrap().to_string(),url:row.url.clone(),item_type:Some("official_legal_clinical_guideline".into()),status:"unknown".into(),version:legal["decree_identifier"].as_str().map(String::from),date_observations:dates,rights,blocks,attachments:vec![json!({"url":url,"role":"source_official_pdf","sha256":pdf.record.sha256})],search_metadata:json!({"source_catalog":row.extra,"source_terminal_record":record,"source_pdf_legal_metadata":legal,"source_rights_notices":notices,"coverage":{"converted_blocks_available":true,"complete_visual_semantics":false,"physical_pages":"unmapped","original_figures":"not extracted"}}),fetches,warnings,..Default::default()})
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests{
 use super::*;
 const HTML:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/catalog.html"));
 #[tokio::test]
 async fn genuine_portal_annex_pdf_extracts_and_dated_page_preserves_adoption_scope(){
  const PDF5:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/portal-anesthesia-source-page-5.pdf"));const PDF4:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/portal-anesthesia-source-page-4.pdf"));const RECORD:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/terminal-2022.html"));let sha="398ce4bbc5279c182910492503949d1c9be7160b25aa84438176f714d67de39b";assert_eq!(PDF5.len(),170850);assert_eq!(crate::common::sha256_hex(PDF5),sha);assert_eq!(PDF4.len(),225413);assert_eq!(crate::common::sha256_hex(PDF4),"e9efac8aeb70bb9131fbb7be7880a2c979cfe4dfc5b5b5a43db8ae1561aa0f9d");
  let source:SourceEntry=serde_json::from_value(json!({"id":"id_kemkes_pnpk","enabled":true,"access":"public","catalog_url":format!("https://kemkes.go.id{FOLDER}")})).unwrap();let url=Url::parse("https://kemkes.go.id/id/pnpk-2022---tata-laksana-anestesiologi-dan-terapi-intensif").unwrap();let native=terminal(RECORD,&url).unwrap();let pdf_url=native["pdf_url"].as_str().unwrap().to_string();let mut row=PlanRow{source_id:source.id.clone(),doc_key:format!("anesthesia-{}",&sha[..12]),url:url.to_string(),title:Some("PNPK source anesthesia".into()),rendition:Some("pdf".into()),version:None,extra:json!({"work_key":"source-anesthesia","terminal_record":native,"pdf_sha256":sha,"fixture_scope":{"physical_source_page":5,"source_pages":504,"source_sha256":"056e7aa01838059eec88e22b622c558831935fc678a8b5059aeb1e956cc4d6fe","scope":"one genuine clinical annex introduction page, not complete504page source; full golden166tables tested separately"}})};
  let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests(url.as_str(),RECORD.as_bytes(),"text/html");f.seed_cache_for_tests(&pdf_url,PDF5,"application/pdf");let d=IdKemkesPnpk.extract(&source,&f,&row).await.unwrap();assert_eq!(d.doc_key,row.doc_key);assert_eq!(d.version.as_deref(),Some("HK.01.07/MENKES/1541/2022"));assert_eq!(d.search_metadata["source_pdf_legal_metadata"]["clinical_annexes"].as_array().unwrap().len(),1);assert!(d.blocks.iter().any(|b|b.text.contains("BAB I")&&b.text.contains("PENDAHULUAN")));assert!(d.blocks.iter().any(|b|b.text.contains("mortalitas 24 jam pascaanestesi umum saat operasi sebesar 46%")));assert!(d.date_observations.iter().any(|v|v.label=="Portal record date"&&v.normalized.as_deref()==Some("2022-10-07")));assert!(!d.date_observations.iter().any(|v|v.label=="Ditetapkan di Jakarta pada tanggal"));assert!(d.published.is_none()&&d.updated.is_none()&&d.country.is_none());assert_eq!(d.rights.status,"unknown");assert!(d.blocks.iter().all(|b|b.page.is_none()));row.extra["pdf_sha256"]=json!("changed");assert!(IdKemkesPnpk.extract(&source,&f,&row).await.is_err());
  let temp4=tempfile::tempdir().unwrap();let f4=Fetcher::new(temp4.path(),true,1,2<<20,"").unwrap();f4.seed_cache_for_tests(&pdf_url,PDF4,"application/pdf");let fetched=f4.get_ok(&pdf_url).await.unwrap();let(mut page4,_,_)=blocks_for(&fetched,None,30).await.unwrap();for b in &mut page4{b.block_id=format!("physical4-{}",b.block_id);}let gold:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/anesthesia-blocks-1.json"))).unwrap();let mut original_header:Block=serde_json::from_value(gold["blocks"][0].clone()).unwrap();original_header.block_id="genuine-full-source-physical1-header".into();let mut context=vec![original_header];context.extend(page4);let(dates,metadata)=legal_metadata(&context,&pdf_url).unwrap();if !dates.iter().any(|v|v.normalized.as_deref()==Some("2022-09-29")){for b in &context{eprintln!("INDONESIA_GENUINE_PHYSICAL4_BLOCK {} {:?}: {}",b.block_id,b.kind,b.text);}}assert!(dates.iter().any(|v|v.normalized.as_deref()==Some("2022-09-29")));assert_eq!(metadata["adoption_date_source_spans"].as_array().unwrap().len(),1);assert_eq!(metadata["commencement_claims"].as_array().unwrap().len(),1);assert!(metadata["identifier_adoption_year_conflicts"].as_array().unwrap().is_empty());
 }
 #[test]
 fn genuine_stroke_keeps_conflicting_printed_years_and_each_native_class_evidence_pair(){
  let gold:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/stroke-blocks-1.json"))).unwrap();let blocks:Vec<Block>=serde_json::from_value(gold["blocks"].clone()).unwrap();assert_eq!(blocks.iter().filter(|b|matches!(b.kind,crate::guidelines::doc::BlockKind::Table{..})).count(),18);let(dates,metadata)=legal_metadata(&blocks,gold["source_pdf_url"].as_str().unwrap()).unwrap();assert_eq!(metadata["decree_identifier"],"HK.01.07/MENKES/304/2026");assert!(dates.iter().any(|v|v.raw=="17 April 2025"&&v.normalized.as_deref()==Some("2025-04-17")));assert_eq!(metadata["identifier_adoption_year_conflicts"].as_array().unwrap().len(),1);let mentions=metadata["printed_recommendation_grade_mentions"].as_array().unwrap();assert!(mentions.iter().any(|g|g["printed_class"]=="III"&&g["printed_evidence_rank"]=="B"));assert!(mentions.iter().any(|g|g["printed_class"]=="I"&&g["printed_evidence_rank"]=="A"));assert!(metadata["grading_legend_contexts"].as_array().unwrap().iter().any(|g|g["source_blocks"].as_array().unwrap().iter().any(|b|b["text"].as_str().unwrap().contains("tidak III menguntungkan atau berbahaya"))));assert_eq!(metadata["clinical_annexes"].as_array().unwrap().len(),1);
 }
 #[tokio::test]
 async fn genuine_discovery_retains_access_failures_without_guessing_downloads(){
  const Y:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/year-2026.html"));const S:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/terminal-stroke-2026.html"));
  let catalog=format!("https://kemkes.go.id{FOLDER}");let source:SourceEntry=serde_json::from_value(json!({"id":"id_kemkes_pnpk","enabled":true,"access":"public","catalog_url":catalog})).unwrap();let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests(&catalog,HTML.as_bytes(),"text/html");f.seed_cache_for_tests(&format!("https://kemkes.go.id{YEAR}kumpulan-pnpk-tahun-2026"),Y.as_bytes(),"text/html");f.seed_cache_for_tests("https://kemkes.go.id/id/pnpk-tata-laksana-stroke-2026",S.as_bytes(),"text/html");
  let rows=IdKemkesPnpk.discover(&source,&f,Some(2)).await.unwrap();assert_eq!(rows.len(),2);assert!(rows[0].url.ends_with("/pnpk-tata-laksana-obesitas-dewasa-2026"));assert!(rows[1].url.ends_with("/pnpk-tata-laksana-stroke-2026"));assert!(rows.iter().all(|r|r.extra["discovery_error"].as_str().unwrap().contains("offline and not cached")));assert!(rows[1].extra["discovery_error"].as_str().unwrap().contains("17871331996a857d0f35d526.86783996.pdf"));assert!(IdKemkesPnpk.discover(&source,&f,Some(0)).await.unwrap().is_empty());
 }
 #[test]
 fn genuine_first_folder_has_ten_source_collections_and_one_next_page(){
  let base=Url::parse(&format!("https://kemkes.go.id{FOLDER}")).unwrap();let nodes=folder(HTML,&base).unwrap();assert_eq!(nodes.len(),11);assert_eq!(nodes.iter().filter(|n|n["role"]=="source_collection").count(),10);assert_eq!(nodes.iter().filter(|n|n["role"]=="folder_page").count(),1);
  assert!(nodes.iter().any(|n|n["label"]=="Koleksi Pedoman"));assert_eq!(nodes.last().unwrap()["url"],format!("https://kemkes.go.id{FOLDER}/2"));
  assert_eq!(nodes[0]["label"],"PNPK 2026");assert_eq!(nodes[1]["label"],"PNPK 2024");assert_eq!(nodes[2]["label"],"PNPK 2025");
  assert!(folder(&HTML.replace("content-wrapper","missing-collection-root"),&base).is_err());
 }

 #[test]
 fn genuine_last_folder_and_year_roles_pagination_and_aliases(){
  const LAST:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/catalog-last.html"));
  const Y22:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/year-2022.html"));
  const Y26:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/year-2026.html"));
  let nodes=folder(LAST,&Url::parse(&format!("https://kemkes.go.id{FOLDER}/2")).unwrap()).unwrap();assert_eq!(nodes.len(),2);assert!(nodes.iter().all(|n|n["role"]=="source_collection"));assert!(nodes.iter().any(|n|n["label"]=="PNPK 2017"));assert!(nodes.iter().any(|n|n["label"]=="PNPK 2015"));
  let nodes=year(Y22,&Url::parse(&format!("https://kemkes.go.id{YEAR}pnpk-2022")).unwrap()).unwrap();assert_eq!(nodes.len(),9);assert!(nodes.iter().all(|n|n["role"]=="detail_record"));assert!(nodes.iter().any(|n|n["url"]=="https://kemkes.go.id/id/pnpk-2022---tata-laksana-anestesiologi-dan-terapi-intensif"));
  let nodes=year(Y26,&Url::parse(&format!("https://kemkes.go.id{YEAR}kumpulan-pnpk-tahun-2026")).unwrap()).unwrap();assert_eq!(nodes.iter().filter(|n|n["role"]=="detail_record").count(),9);assert_eq!(nodes.iter().filter(|n|n["role"]=="year_page").count(),1);let stroke=nodes.iter().find(|n|n["url"]=="https://kemkes.go.id/id/pnpk-tata-laksana-stroke-2026").unwrap();assert_eq!(stroke["catalog_mentions"].as_array().unwrap().len(),2);assert!(nodes.iter().any(|n|n["url"]=="https://kemkes.go.id/id/pedoman-klinis-lla-dewasa-kmk-2026"));
 }

 #[test]
 fn genuine_terminal_downloads_and_closed_second_year_page_are_exact(){
  const A:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/terminal-2022.html"));const S:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/terminal-stroke-2026.html"));const P:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/year-2026-page2.html"));
  let a=terminal(A,&Url::parse("https://kemkes.go.id/id/pnpk-2022---tata-laksana-anestesiologi-dan-terapi-intensif").unwrap()).unwrap();assert_eq!(a["pdf_url"],"https://kemkes.go.id/app_asset/file_content_download/17000960906555685a551199.77640603.pdf");assert_eq!(a["portal_date"]["datetime"],"2022-10-07");
  let s=terminal(S,&Url::parse("https://kemkes.go.id/id/pnpk-tata-laksana-stroke-2026").unwrap()).unwrap();assert_eq!(s["pdf_url"],"https://kemkes.go.id/app_asset/file_content_download/17871331996a857d0f35d526.86783996.pdf");assert_eq!(s["portal_date"]["datetime"],"2026-08-19");assert!(s["overview"]["text"].as_str().unwrap().contains("HK.01.07/MENKES/304/2026"));
  let nodes=year(P,&Url::parse(&format!("https://kemkes.go.id{YEAR}kumpulan-pnpk-tahun-2026/2")).unwrap()).unwrap();assert_eq!(nodes.len(),7);assert!(nodes.iter().all(|n|n["role"]=="detail_record"));assert!(nodes.iter().any(|n|n["label"]=="PEDOMAN NASIONAL PELAYANAN KLINIS TATALAKSANA KATARAK PADA DEWASA"));
 }
 #[test]
 fn genuine_windowed_year_pager_follows_every_forward_page_without_failing(){
  // pnpk-2019 page 1 emits "1 [2] [3] Next(2)": the first-pass plan aborted here with "year pager skips adjacent page".
  const Y19:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/year-2019-page-1.html"));
  let base=Url::parse(&format!("https://kemkes.go.id{YEAR}pnpk-2019")).unwrap();let nodes=year(Y19,&base).unwrap();
  let pages=nodes.iter().filter(|n|n["role"]=="year_page").map(|n|n["url"].as_str().unwrap()).collect::<Vec<_>>();
  assert_eq!(pages,vec![format!("https://kemkes.go.id{YEAR}pnpk-2019/2"),format!("https://kemkes.go.id{YEAR}pnpk-2019/3")]);
  assert!(nodes.iter().all(|n|n["pager_warning"].is_null()));assert_eq!(nodes.iter().filter(|n|n["role"]=="detail_record").count(),10);
  // a pager that skips the adjacent page keeps a warning on the emitted pages instead of aborting the plan
  let gap=Y19.replace(&format!("href=\"https://kemkes.go.id{YEAR}pnpk-2019/2\""),"href=\"#\"");let nodes=year(&gap,&base).unwrap();
  let later=nodes.iter().filter(|n|n["role"]=="year_page").collect::<Vec<_>>();assert_eq!(later.len(),1);assert!(later[0]["pager_warning"].as_str().unwrap().contains("no link to page 2"));
 }
 #[tokio::test]
 async fn discovery_walks_windowed_year_pages_in_source_order(){
  const Y19:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/year-2019-page-1.html"));
  let first=format!("https://kemkes.go.id{YEAR}pnpk-2019");let source:SourceEntry=serde_json::from_value(json!({"id":"id_kemkes_pnpk","enabled":true,"access":"public","catalog_url":format!("https://kemkes.go.id{FOLDER}")})).unwrap();
  let temp=tempfile::tempdir().unwrap();let f=Fetcher::new(temp.path(),true,1,2<<20,"").unwrap();f.seed_cache_for_tests(&source.catalog_url.clone().unwrap(),format!("<div class='content-wrapper'><div class='container'><h2 class='title'>PEDOMAN NASIONAL PELAYANAN KEDOKTERAN (PNPK)</h2><div class='card'><a class='link' href='{first}'><h4>PNPK 2019</h4></a></div></div></div>").as_bytes(),"text/html");f.seed_cache_for_tests(&first,Y19.as_bytes(),"text/html");
  // pages 2 and 3 are not cached offline: the walk reaches both after page 1's ten records, then stops on the first unavailable page
  let err=IdKemkesPnpk.discover(&source,&f,None).await.unwrap_err();assert!(format!("{err:#}").contains("pnpk-2019/2"),"{err:#}");
  let rows=IdKemkesPnpk.discover(&source,&f,Some(10)).await.unwrap();assert_eq!(rows.len(),10);assert!(rows.iter().all(|r|r.extra["source_catalog_parent"]["role"]=="source_collection"));
 }
 #[test]
 fn genuine_legal_dates_annexes_and_source_grade_mentions_do_not_infer_current_status(){
  let p:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/pregnancy-blocks-1.json"))).unwrap();let blocks:Vec<Block>=serde_json::from_value(p["blocks"].clone()).unwrap();let(dates,meta)=legal_metadata(&blocks,p["source_pdf_url"].as_str().unwrap()).unwrap();assert_eq!(meta["decree_identifier"],"HK.01.07/MENKES/91/2017");assert_eq!(dates[0].normalized.as_deref(),Some("2017-02-17"));assert_eq!(meta["clinical_annexes"].as_array().unwrap().len(),4);let labels=meta["printed_recommendation_grade_mentions"].as_array().unwrap();assert!(labels.iter().any(|g|g["role"]=="grading_legend"&&g["printed_grade"]=="A"));assert!(labels.iter().any(|g|g["role"]=="source_grade_mention"&&g["printed_grade"]=="B"));assert!(meta["revocation_claims"].as_array().unwrap().is_empty());
 }

 #[test]
 fn genuine_2022_legal_revocation_has_no_guessed_target_or_clinical_status(){
  let source:Value=serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/id_kemkes_pnpk/anesthesia-blocks-1.json"))).unwrap();let blocks:Vec<Block>=serde_json::from_value(source["blocks"].clone()).unwrap();let(dates,metadata)=legal_metadata(&blocks,source["source_pdf_url"].as_str().unwrap()).unwrap();assert_eq!(metadata["decree_identifier"],"HK.01.07/MENKES/1541/2022");assert_eq!(dates[0].raw,"29 September 2022");assert_eq!(dates[0].normalized.as_deref(),Some("2022-09-29"));assert_eq!(metadata["clinical_annexes"].as_array().unwrap().len(),1);let claims=metadata["revocation_claims"].as_array().unwrap();assert_eq!(claims.len(),1);assert!(claims[0]["text"].as_str().unwrap().contains("HK.02.02/MENKES/251/2015"));assert!(claims[0]["target_url"].is_null());assert_eq!(metadata["commencement_claims"].as_array().unwrap().len(),1);assert_eq!(blocks.iter().filter(|b|matches!(b.kind,crate::guidelines::doc::BlockKind::Table{..})).count(),166);
 }
}
#[cfg(all(test, feature = "guideline-fixtures"))]
mod record_without_download {
    use super::*;
    #[test]
    fn real_2026_record_without_unduh_link_is_reported_as_no_attachment() {
        let html = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/id_kemkes_pnpk/terminal-obesitas-2026-no-unduh.html"));
        let base = Url::parse("https://kemkes.go.id/id/pnpk-tata-laksana-obesitas-dewasa-2026").unwrap();
        let e = format!("{:#}", terminal(html, &base).unwrap_err());
        assert!(e.contains("has no Unduh (download) link"), "{e}");
        let stroke = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/id_kemkes_pnpk/terminal-stroke-2026.html"));
        assert!(terminal(stroke, &Url::parse("https://kemkes.go.id/id/pnpk-tata-laksana-stroke-2026").unwrap()).is_ok());
    }
}
