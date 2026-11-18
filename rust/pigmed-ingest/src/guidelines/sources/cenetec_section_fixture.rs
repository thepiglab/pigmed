//! Acquisition-only CENETEC AJAX fixture adapter. Not a full guideline adapter.
use super::{Adapter,PlanRow};
use crate::guidelines::doc::{GuidelineDoc,Rights};
use crate::guidelines::fetch::{Fetcher,Req,looks_like_login_wall};
use crate::guidelines::html::blocks_from_html;
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use serde_json::Value;
use url::Url;
pub struct CenetecSectionFixture;
#[async_trait::async_trait]
impl Adapter for CenetecSectionFixture {
 async fn discover(&self,src:&SourceEntry,_:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
  let sections=src.config.get("fixture_sections").and_then(Value::as_array).context("fixture_sections missing")?;
  let mut out=Vec::new();
  for v in sections {
   let url=v["url"].as_str().context("fixture url missing")?;
   let u=Url::parse(url)?;
   if u.origin()!=Url::parse("https://gpc.salud.gob.mx")?.origin() || u.path()!="/DDIMBE/DDIMBE/ObtenerContenidoSeccion" {bail!("invalid fixture URL");}
   let doc_key=v["doc_key"].as_str().context("fixture doc_key missing")?;
   out.push(PlanRow{source_id:src.id.clone(),doc_key:doc_key.into(),url:url.into(),title:v["title"].as_str().map(String::from),rendition:Some("html".into()),version:None,extra:Value::Null});
   if limit.is_some_and(|n|out.len()>=n){break;}
  }
  Ok(out)
 }
 async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
  let req=Req {headers:vec![("x-requested-with".into(),"XMLHttpRequest".into())],..Req::get(&row.url)};
  let page=f.request(&req).await?;
  if page.record.status!=200 {bail!("fixture HTTP {}",page.record.status);}
  let html=page.text();
  if looks_like_login_wall(&html) {bail!("fixture login wall");}
  let lower=html.to_lowercase();
  if lower.contains("captcha") || lower.contains("checking your browser") {bail!("fixture challenge");}
  if !page.is_html(){bail!("fixture is not HTML");}
  let base=Url::parse(&page.record.final_url)?;
  let clean=html.replace("src=\"~/", "src=\"/DDIMBE/").replace("src='~/", "src='/DDIMBE/")
   .replace("src=\"~", "src=\"/DDIMBE/").replace("src='~", "src='/DDIMBE/");
  let (blocks,mut warnings)=blocks_from_html(&clean,Some(&base),None);
  if blocks.is_empty(){bail!("empty fixture fragment");}
  warnings.push("INCOMPLETE ACQUISITION FIXTURE: one AJAX section, not a full guideline; linked images and rights are unvalidated.".into());
  Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),title:row.title.clone().unwrap_or_default(),url:row.url.clone(),
   item_type:Some("source_section_fixture".into()),status:"unknown".into(),rights:Rights{status:"unknown".into(),..Default::default()},
   blocks,warnings,fetches:vec![page.record],..Default::default()})
 }
}
