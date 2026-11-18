//! CDC 2021 STI guidance: each source chapter is a separate clinical document.
use super::{common,Adapter,PlanRow};
use crate::guidelines::doc::{Block,BlockKind,GuidelineDoc,Relation,Rights};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html,sel,text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail,Context,Result};
use scraper::Html;
use serde_json::json;
use std::collections::HashSet;
use url::Url;

pub struct CdcSti;
const ROOT:&str="main[role='main'] div.col.content";

fn clinical(html:&str,base:&Url)->Result<(String,Vec<Block>,Vec<String>)> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel(ROOT)).next().context("CDC STI clinical root missing")?;
    let title=root.select(&sel("h1.page-title")).next().map(|e|text_of(&e)).context("CDC STI chapter title missing")?;
    let (mut blocks,mut warnings)=blocks_from_html(html,Some(base),Some(ROOT));
    let mut used=HashSet::new();
    let mut recs=Vec::new();
    for card in root.select(&sel("div.card")) {
        let Some(header)=card.select(&sel(".card-header")).next() else {continue};
        let label=text_of(&header); let lower=label.to_lowercase();
        if !(lower.starts_with("recommended regimen")||lower.starts_with("alternative regimen")) {continue;}
        let body=card.select(&sel(".card-body")).next().context("CDC regimen card missing body")?;
        if body.select(&sel("table")).next().is_some() {warnings.push(format!("Regimen card {label} contains a table; structured recommendation requires review.")); continue;}
        let text=text_of(&body); if text.is_empty(){bail!("CDC regimen card {label} has no body");}
        let position=blocks.iter().enumerate().find(|(i,b)|!used.contains(i) && b.text==label).map(|(i,_)|i);
        if let Some(i)=position { used.insert(i); recs.push((i,label,text)); }
        else {warnings.push(format!("Regimen card label not located in clinical block stream: {label}"));}
    }
    for (n,(i,label,text)) in recs.into_iter().enumerate().rev() {
        let path=blocks[i].heading_path.clone();
        blocks.insert(i+1,Block {block_id:format!("cdc-regimen-{}",n+1),kind:BlockKind::Recommendation {label:Some(label),strength:None,certainty:None,population:None,grading_system:None,qualifier:None},text,heading_path:path,page:None});
    }
    warnings.push("Regimen cards retain dose, alternatives and qualifying conditions together; other narrative clinical recommendations remain prose.".into());
    Ok((title,blocks,warnings))
}

fn clinical_hash(html:&str)->Result<String> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel(ROOT)).next().context("CDC STI clinical root missing")?;
    Ok(crate::common::sha256_hex(root.html().as_bytes()))
}

/// Read the publisher's table of contents and retain all local chapter links.
fn toc_links(html:&str,base:&Url)->Result<Vec<(String,String)>> {
    let doc=Html::parse_document(html);
    let root=doc.select(&sel(ROOT)).next().context("CDC STI table of contents root missing")?;
    let mut seen=HashSet::new(); let mut out=Vec::new();
    for a in root.select(&sel("ul#nav-primary a[href]")) {
        let Some(url)=common::link(base,a.value().attr("href").unwrap_or_default()) else {continue};
        let parsed=Url::parse(&url)?; let path=parsed.path();
        if parsed.origin()!=base.origin() || !path.starts_with("/std/treatment-guidelines/") || !path.ends_with(".htm") || path.ends_with("/toc.htm") || path.ends_with("/default.htm") {continue;}
        if seen.insert(url.clone()) {out.push((url,text_of(&a)));}
    }
    if out.is_empty(){bail!("CDC STI table of contents contains no chapter links");}
    Ok(out)
}

#[async_trait::async_trait]
impl Adapter for CdcSti {
    async fn discover(&self,src:&SourceEntry,f:&Fetcher,limit:Option<usize>)->Result<Vec<PlanRow>> {
        if limit==Some(0) {return Ok(vec![]);}
        let index=f.get_ok(src.catalog_url.as_deref().context("CDC STI catalog missing")?).await?;
        let toc={
            let doc=Html::parse_document(&index.text()); let base=Url::parse(&index.record.final_url)?;
            doc.select(&sel("main a[href]")).filter_map(|a|common::link(&base,a.value().attr("href")?))
                .find(|url|Url::parse(url).ok().is_some_and(|u|u.origin()==base.origin() && u.path().ends_with("/std/treatment-guidelines/toc.htm")))
                .context("CDC STI source TOC link missing")?
        };
        let toc_page=f.get_ok(&toc).await?;
        let chapters=toc_links(&toc_page.text(),&Url::parse(&toc_page.record.final_url)?)?;
        let mut rows=Vec::new();
        for (url,title) in chapters {
            let slug=Url::parse(&url)?.path_segments().and_then(|s|s.last()).unwrap_or("chapter").trim_end_matches(".htm").to_string();
            let mut row=PlanRow {source_id:src.id.clone(),doc_key:format!("{slug}-unresolved"),url,title:Some(title),rendition:Some("html".into()),version:None,
                extra:json!({"work_key":slug,"toc_url":toc})};
            match f.get_ok(&row.url).await {
                Ok(page)=>match clinical_hash(&page.text()) {
                    Ok(hash)=>{row.doc_key=format!("{}-{}",slug,&hash[..12]);row.extra["clinical_sha256"]=json!(hash);},
                    Err(error)=>row.extra["discovery_error"]=json!(format!("{error:#}")),
                },
                Err(error)=>row.extra["discovery_error"]=json!(format!("{error:#}")),
            }
            rows.push(row);
            if limit.is_some_and(|n|rows.len()>=n){break;}
        }
        Ok(rows)
    }
    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc> {
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("CDC STI discovery: {e}");}
        let policy_url="https://www.cdc.gov/other/agencymaterials.html";
        let policy=f.get_ok(policy_url).await?;
        let page=f.get_ok(&row.url).await?; let base=Url::parse(&page.record.final_url)?; let html=page.text();
        if row.extra["clinical_sha256"].as_str().is_some_and(|s|clinical_hash(&html).map(|h|h!=s).unwrap_or(true)){bail!("CDC STI chapter changed since planning");}
        let (title,blocks,mut warnings)=clinical(&html,&base)?;
        let doc=Html::parse_document(&html); let mut dates=Vec::new();
        if let Some(raw)=doc.select(&sel("meta[property='cdc:first_published']")).next().and_then(|e|e.value().attr("content")) {
            dates.push(common::date("cdc:first_published",raw,base.as_str()));
        }
        if let Some(raw)=doc.select(&sel("#last-reviewed-date")).next().map(|e|text_of(&e)) {dates.push(common::date("Last Reviewed",&raw,base.as_str()));}
        let published=dates.iter().find(|d|d.label=="cdc:first_published").and_then(|d|d.normalized.clone());
        let terms=Html::parse_document(&policy.text());
        let paragraphs=terms.select(&sel("main .syndicate p")).map(|e|text_of(&e)).collect::<Vec<_>>();
        let restrictions=paragraphs.iter().filter(|p|p.starts_with("There are, however, a few exceptions.") || p.starts_with("2) You must") || p.starts_with("3) You may not") || p.starts_with("4) You must")).cloned().collect();
        let rights=Rights {status:"unknown".into(),terms_url:Some(policy_url.into()),
            note:Some("CDC's agency-wide terms are conditional; applicability to this item and its assets remains unverified.".into()),
            attribution:paragraphs.iter().find(|p|p.starts_with("1) Attribution")).cloned(),restrictions,evidence_urls:vec![policy_url.into()],..Default::default()};
        warnings.push("CDC agency-material terms have third-party exceptions; item and asset reuse rights have not been established.".into());
        let toc=row.extra["toc_url"].as_str().unwrap_or_default();
        let relations=if toc.is_empty(){vec![]}else{vec![Relation{rel:"part_of".into(),target:toc.into(),scope:None,evidence_url:Some(toc.into())}]};
        Ok(GuidelineDoc {source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),
            item_type:Some("guideline_chapter".into()),title,url:row.url.clone(),status:"unknown".into(),published,date_observations:dates,relations,
            rights,
            language:common::page_language(&doc),blocks,warnings,fetches:vec![page.record,policy.record],..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    const INDEX:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_sti/index.html"));
    const TOC:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_sti/toc.html"));
    const TERMS:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_sti/terms.html"));
    const ADULT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_sti/gonorrhea-adults.html"));
    const NEONATE:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/cdc_sti/gonorrhea-neonates.html"));
    #[test]
    fn real_regimen_cards_preserve_population_and_conditions() {
        let (title,adult,_) = clinical(ADULT,&Url::parse("https://www.cdc.gov/std/treatment-guidelines/gonorrhea-adults.htm").unwrap()).unwrap();
        assert_eq!(title,"Gonococcal Infections Among Adolescents and Adults");
        let rec=adult.iter().find(|b|matches!(&b.kind,BlockKind::Recommendation{label:Some(s),..} if s.contains("Cervix, Urethra, or Rectum"))).unwrap();
        assert!(rec.text.contains("500 mg") && rec.text.contains("150 kg") && rec.text.contains("1 g") && rec.text.contains("chlamydial infection"));
        let (_,neonate,_)=clinical(NEONATE,&Url::parse("https://www.cdc.gov/std/treatment-guidelines/gonorrhea-neonates.htm").unwrap()).unwrap();
        assert!(neonate.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..}) && b.text.to_lowercase().contains("ceftriaxone") && b.text.contains("mg/kg")));
        assert!(adult.iter().all(|b|if let BlockKind::Recommendation{strength,certainty,..}=&b.kind {strength.is_none()&&certainty.is_none()} else {true}));
    }
    #[tokio::test]
    async fn saved_toc_and_regimen_discovery_offline() {
        let base=Url::parse("https://www.cdc.gov/std/treatment-guidelines/toc.htm").unwrap();
        let links=toc_links(TOC,&base).unwrap();
        for suffix in ["gonorrhea-adults.htm","gonorrhea-neonates.htm","gonorrhea-children.htm"] {
            assert!(links.iter().any(|(u,_)|u.ends_with(suffix)));
        }
        let index="https://www.cdc.gov/std/treatment-guidelines/default.htm";
        let adult="https://www.cdc.gov/std/treatment-guidelines/gonorrhea-adults.htm";
        let needle="<li><a href=\"/std/treatment-guidelines/gonorrhea-adults.htm\">Gonococcal Infections Among Adolescents and Adults</a></li>";
        assert!(TOC.contains(needle));
        let bounded=format!("<main role='main'><div class='col content'><h1 class='page-title'>Table of Contents</h1><ul id='nav-primary'>{needle}</ul></div></main>");
        let d=tempfile::tempdir().unwrap();let f=Fetcher::new(d.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(index,INDEX.as_bytes(),"text/html");f.seed_cache_for_tests(base.as_str(),bounded.as_bytes(),"text/html");
        f.seed_cache_for_tests(adult,ADULT.as_bytes(),"text/html");f.seed_cache_for_tests("https://www.cdc.gov/other/agencymaterials.html",TERMS.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"cdc_sti","access":"public","enabled":true,"catalog_url":index})).unwrap();
        let rows=CdcSti.discover(&src,&f,None).await.unwrap();assert_eq!(rows.len(),1);
        let doc=CdcSti.extract(&src,&f,&rows[0]).await.unwrap();
        assert_eq!(doc.title,"Gonococcal Infections Among Adolescents and Adults");
        assert_eq!(doc.published.as_deref(),Some("2021-07-22"));
        assert!(doc.date_observations.iter().any(|d|d.label=="Last Reviewed" && d.normalized.as_deref()==Some("2022-09-21")));
        assert_eq!(doc.rights.status,"unknown"); assert!(!doc.rights.restrictions.is_empty());assert!(doc.rights.attribution.is_some());
        assert_eq!(doc.fetches.len(),2);
    }

}
