//! Clinicalinfo family menus -> full-view chapters, retaining independent chapter/table dates.
//! Appendix preview tables explicitly link their full versions; those are separate graph nodes.
use super::{common, Adapter, PlanRow};
use crate::guidelines::doc::{Block, BlockKind, DateObservation, GuidelineDoc, Relation};
use crate::guidelines::fetch::Fetcher;
use crate::guidelines::html::{blocks_from_html, sel, text_of};
use crate::guidelines::registry::SourceEntry;
use anyhow::{bail, Context, Result};
use scraper::Html;
use serde_json::json;
use std::collections::{HashSet, VecDeque};
use url::Url;

pub struct NihHiv;
const NODE: &str = ".node--type-bookpart, .node--type-sub-bookpart";
const BODY: &str = ".node--type-bookpart .field--name-field-section, .node--type-sub-bookpart .field--name-field-section";

fn family(url: &Url) -> Option<String> {
    let parts = url.path_segments()?.collect::<Vec<_>>();
    let i = parts.iter().position(|p| *p == "guidelines")?;
    let name = parts.get(i + 1)?;
    (!name.is_empty() && *name != "archived-guidelines").then(|| name.to_string())
}

fn families(doc: &Html, base: &Url) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    doc.select(&sel(".node--type-guideline .guideline-title a[href]"))
        .filter_map(|a| {
            let u = common::link(base,a.value().attr("href")?)?;
            let parsed = Url::parse(&u).ok()?;
            if parsed.host_str() != base.host_str() || family(&parsed).is_none() || !seen.insert(u.clone()) { return None; }
            Some((u,text_of(&a)))
        }).collect()
}

fn chapters(doc: &Html, base: &Url) -> Vec<(String, String, bool)> {
    let fam = family(base);
    let mut seen = HashSet::new();
    doc.select(&sel("#block-customblocktableofcontent a[href], .field--name-field-section a.link-over-table[href], .node--type-sub-bookpart .field--name-field-section a[href]"))
        .filter_map(|a| {
            let u = common::link(base,a.value().attr("href")?)?;
            let parsed = Url::parse(&u).ok()?;
            let full_table = a.value().classes().any(|c|c == "link-over-table") || parsed.path().ends_with("-full");
            if parsed.host_str() != base.host_str() || family(&parsed) != fam || !seen.insert(u.clone()) { return None; }
            if !full_table && !parsed.query_pairs().any(|(k,v)| k == "view" && v == "full") { return None; }
            Some((u,text_of(&a),full_table))
        }).collect()
}

fn dates(doc: &Html, base: &Url) -> Result<Vec<DateObservation>> {
    let mut out = vec![];
    // Sub-bookpart repeats metadata inside the brief field. Read only its full-text field.
    let chapter = doc.select(&sel(".node--type-bookpart .node-updated-info, .node--type-sub-bookpart .field--name-field-section .node-updated-info")).next().context("NIH chapter Updated/Reviewed metadata missing")?;
    // The publisher emits malformed div,span tags; match their explicit label/time pairing,
    // rather than relying on positions or propagating one date to the other label.
    let labelled = regex::Regex::new(r"(?is)\b(Updated|Reviewed)\s*</[^>]+>\s*(?:<div\b[^>]*>\s*)?<time[^>]*>([^<]+)</time>").unwrap();
    for c in labelled.captures_iter(&chapter.html()) { out.push(common::date(&c[1],c[2].trim(),base.as_str())); }
    if out.is_empty() { bail!("NIH chapter dates could not be paired with publisher labels"); }
    let inline = regex::Regex::new(r"(?is)<strong>\s*(Updated|Reviewed):?\s*</strong>\s*([^<]+)").unwrap();
    let body = doc.select(&sel(BODY)).next().context("NIH full section missing")?;
    let mut scope = base.clone();
    for e in body.select(&sel("h2, h3, h4, h5, p")) {
        if e.value().name().starts_with('h') {
            scope.set_fragment(e.value().id());
        } else {
            for c in inline.captures_iter(&e.html()) { out.push(common::date(&c[1],c[2].trim(),scope.as_str())); }
        }
    }
    Ok(out)
}

fn content(doc: &Html, base: &Url) -> Result<(String,String,Vec<DateObservation>)> {
    // The published standalone templates differ: Appendix A uses .table-newtab,
    // while Appendix B places three tables and reference prose directly in the body field.
    let has_newtab = doc.select(&sel("article.tables .table-newtab")).next().is_some();
    let standalone = doc.select(&sel(if has_newtab {"article.tables .table-newtab"}else{"article.tables .field--name-body > .field--item"})).collect::<Vec<_>>();
    let (title,mut selected,ds) = if !standalone.is_empty() {
        if standalone.len()!=1 || standalone[0].select(&sel("table")).next().is_none() {bail!("NIH standalone table root changed; expected one body containing complete tables");}
        // Standalone source pages have their own strict template and no printed revision dates.
        // Parent preview dates must never be inherited by this independently fetched table.
        let title = common::first_text(doc,".region-content h1.page-header").context("NIH standalone table title missing")?;
        let mut html=standalone[0].html();
        for p in standalone[0].select(&sel("p")) {
            if matches!(text_of(&p).as_str(),"Back to Appendix A"|"Back to Appendix B") {html=html.replacen(&p.html(),"",1);}
        }
        (title,html,vec![])
    } else {
        if doc.select(&sel(NODE)).next().is_none() { bail!("NIH supported chapter/table content root missing"); }
        let title = common::first_text(doc,".node--type-bookpart .field--name-node-title")
            .or_else(||common::first_text(doc,".node--type-sub-bookpart .field--name-field-section h3"))
            .context("NIH chapter title missing")?;
        let roots = doc.select(&sel(BODY)).collect::<Vec<_>>();
        if roots.len() != 1 { bail!("NIH expected one full-text section, found {}",roots.len()); }
        (title,roots[0].html(),dates(doc,base)?)
    };
    // Full and brief are distinct fields. Inactive/commented historical tables are not clinical
    // content, and must not affect identity or accidentally become extra rendered tables.
    let comments = regex::Regex::new(r"(?s)<!--.*?-->").unwrap();
    selected = comments.replace_all(&selected,"").into_owned();
    let junk = regex::Regex::new(r"(?is)<(?:script|style)\b[^>]*>.*?</(?:script|style)>").unwrap();
    selected = junk.replace_all(&selected,"").into_owned();
    let editor = regex::Regex::new(r#"\s(?:data-list-item-id|data-pdffiller-[\w-]+)="[^"]*""#).unwrap();
    selected = editor.replace_all(&selected,"").into_owned();
    Ok((title,format!("<div id='nih-clinical'>{selected}</div>"),ds))
}

fn revision(title: &str, html: &str, dates: &[DateObservation]) -> Result<String> {
    // Date labels, raw dates and scoped source URLs are all meaningful revision evidence.
    Ok(crate::common::sha256_hex(serde_json::to_string(&(title,html,dates))?.as_bytes()))
}

fn recommendations(html: &str, blocks: &mut Vec<Block>) -> Result<Vec<String>> {
    let doc = Html::parse_fragment(html);
    let table_ids = blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).map(|b|b.block_id.clone()).collect::<Vec<_>>();
    let mut warnings = vec![];
    let grade = regex::Regex::new(r"\(([ABC])(III|II|I)\)").unwrap();
    for (i,table) in doc.select(&sel(".field--name-field-section table")).enumerate() {
        let Some(header) = table.select(&sel("thead th")).next().map(|h|text_of(&h)) else {continue};
        if header != "Panel's Recommendations" {continue;}
        let table_id = table_ids.get(i).context("NIH recommendation table missing from extracted blocks")?;
        let legend = text_of(&table);
        let has_legend = legend.contains("Rating of Recommendations:") && legend.contains("Rating of Evidence:");
        let mut statements = vec![];
        for (n,item) in table.select(&sel("tbody li")).enumerate() {
            let text = text_of(&item);
            let pairs = grade.captures_iter(&text).map(|c|(c[1].to_string(),c[2].to_string())).collect::<HashSet<_>>();
            // Preserve the complete bullet, including its grade tokens. Repeated identical grades
            // (two AI clauses in the observed first bullet) are consistent, not duplicate statements.
            let pair = if has_legend && pairs.len()==1 {pairs.iter().next().cloned()} else {None};
            if !has_legend || pairs.len()>1 {
                warnings.push(format!("Recommendation bullet {} in table {} has no supported rating legend or multiple different grades; its complete source text is retained without collapsing grades.",n+1,i+1));
            }
            statements.push(Block{block_id:format!("{table_id}-recommendation-{}",n+1),kind:BlockKind::Recommendation{
                label:None,strength:pair.as_ref().map(|p|p.0.clone()),certainty:pair.as_ref().map(|p|p.1.clone()),population:None,
                grading_system:has_legend.then(||"Rating of Recommendations / Rating of Evidence".into()),qualifier:Some(header.clone())},
                text,heading_path:vec![],page:None});
        }
        if statements.is_empty(){bail!("NIH Panel's Recommendations table has no statement bullets; refusing silent omission");}
        common::insert_table_recommendations(blocks,table_id,statements)?;
    }
    Ok(warnings)
}

fn table_footnotes(html:&str,blocks:&mut Vec<Block>)->Result<()> {
    let doc=Html::parse_fragment(html);
    let ids=blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).map(|b|b.block_id.clone()).collect::<Vec<_>>();
    for (i,table) in doc.select(&sel("#nih-clinical table")).enumerate() {
        let cells=table.select(&sel("td")).filter(|td|td.select(&sel(".Table_Footnote")).next().is_some()).collect::<Vec<_>>();
        if cells.is_empty(){continue;}
        let id=ids.get(i).context("NIH footnote table missing from extracted blocks")?;
        let pos=blocks.iter().position(|b|&b.block_id==id).context("NIH footnote table block vanished")?;
        let path=blocks[pos].heading_path.clone();
        let rows=cells.iter().enumerate().map(|(n,cell)|Block{block_id:format!("{id}-footnote-{}",n+1),kind:BlockKind::Footnote,text:text_of(cell),heading_path:path.clone(),page:None}).collect::<Vec<_>>();
        // Keep the complete footer cell together: reference links can split a footnote across
        // several spans. A separate block also keeps long-table footnotes retrievable.
        blocks.splice(pos+1..pos+1,rows);
    }
    Ok(())
}

fn laboratory_schedule(html:&str,blocks:&mut [Block])->Result<()> {
    let doc=Html::parse_fragment(html);
    let ids=blocks.iter().enumerate().filter(|(_,b)|matches!(b.kind,BlockKind::Table{..})).map(|(i,_)|i).collect::<Vec<_>>();
    for (i,table) in doc.select(&sel("#nih-clinical table")).enumerate(){
        if table.value().attr("summary")!=Some("Laboratory Testing Schedule Before and After Antiretroviral Therapy"){continue;}
        let span=|cell:scraper::ElementRef<'_>,attr:&str|->Result<usize>{let n=cell.value().attr(attr).map(str::parse::<usize>).transpose()?.unwrap_or(1);if n==0||n>100{bail!("NIH laboratory schedule span outside validated bound");}Ok(n)};
        let mut carry:Vec<Option<(String,usize)>>=vec![None;9];let mut grid=vec![];
        for row in table.select(&sel("tr")){
            let mut values:Vec<Option<String>>=vec![None;9];
            for j in 0..9{if let Some((t,n))=carry[j].clone(){values[j]=Some(t.clone());carry[j]=if n>1{Some((t,n-1))}else{None};}}
            let mut col=0;
            for cell in common::cells(row){
                while col<9&&values[col].is_some(){col+=1;}let cs=span(cell,"colspan")?;let rs=span(cell,"rowspan")?;
                if col+cs>9{bail!("NIH full laboratory schedule exceeds nine validated columns");}
                let t=text_of(&cell).replace('|',"\\|");
                for j in col..col+cs{if values[j].is_some(){bail!("NIH laboratory schedule spans overlap");}values[j]=Some(t.clone());if rs>1{carry[j]=Some((t.clone(),rs-1));}}col+=cs;
            }
            if values.iter().any(Option::is_none){bail!("NIH laboratory schedule missing a scoped cell");}
            grid.push(values.into_iter().map(Option::unwrap).collect::<Vec<_>>());
        }
        if carry.iter().any(Option::is_some)||grid.len()<3||grid[0][0]!="Laboratory Test"||grid[1][0]!="Laboratory Test"{bail!("NIH laboratory schedule header/rowspan template changed");}
        let headers=(0..9).map(|c|if grid[0][c]==grid[1][c]{grid[0][c].clone()}else{format!("{}: {}",grid[0][c],grid[1][c])}).collect::<Vec<_>>();
        let mut lines=vec![format!("| {} |",headers.join(" | ")),format!("| {} |",vec!["---";9].join(" | "))];
        lines.extend(grid.into_iter().skip(2).map(|r|format!("| {} |",r.join(" | "))));
        let pos=*ids.get(i).context("NIH full laboratory table lost during extraction")?;blocks[pos].text=lines.join("\n");
        // Actual a–s notes are source-labelled superscript paragraphs outside the table.
        for p in doc.select(&sel("#nih-clinical .field--item p")){
            if p.ancestors().filter_map(scraper::ElementRef::wrap).any(|e|e.value().name()=="table"){continue;}
            let Some(first)=p.children().filter_map(scraper::ElementRef::wrap).next()else{continue};
            if first.value().name()!="sup"{continue;}let marker=text_of(&first);
            if marker.len()!=1||!matches!(marker.as_bytes()[0],b'a'..=b's'){continue;}
            let text=text_of(&p);let b=blocks.iter_mut().find(|b|b.text==text).context("NIH labelled laboratory footnote lost during extraction")?;b.kind=BlockKind::Footnote;
        }
    }Ok(())
}

#[derive(Clone)]
struct Pending { url:String, hint:String, family_url:Option<String>, parent:Option<String>, terminal:bool }

#[async_trait::async_trait]
impl Adapter for NihHiv {
    async fn discover(&self, src:&SourceEntry, f:&Fetcher, limit:Option<usize>) -> Result<Vec<PlanRow>> {
        if limit == Some(0) { return Ok(vec![]); }
        let catalog = src.catalog_url.as_deref().context("NIH catalog_url missing")?;
        let mut queue = VecDeque::from([Pending {url:catalog.into(),hint:String::new(),family_url:None,parent:None,terminal:false}]);
        let mut seen = HashSet::new();
        let mut emitted = HashSet::new();
        let mut out = vec![];
        while let Some(p) = queue.pop_front() {
            if !seen.insert(p.url.clone()) { continue; }
            if seen.len()>1500 { bail!("NIH graph exceeded 1500 source-emitted nodes; inventory is incomplete"); }
            let fetched = match f.get_ok(&p.url).await {
                Ok(v)=>v,
                Err(e)=>{
                    if !p.terminal && p.family_url.is_none() { return Err(e).context(format!("NIH catalog discovery failed: {}",p.url)); }
                    let key = common::slug(Url::parse(&p.url)?.path());
                    out.push(PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-unresolved"),url:p.url,title:Some(p.hint),rendition:Some("html".into()),version:None,
                        extra:json!({"work_key":key,"discovery_error":format!("{e:#}")})});
                    if limit.is_some_and(|n|out.len()>=n) { return Ok(out); }
                    continue;
                }
            };
            let base = Url::parse(&fetched.record.final_url)?;
            let doc = Html::parse_document(&fetched.text());
            let fs = families(&doc,&base);
            for (url,hint) in &fs { queue.push_back(Pending{url:url.clone(),hint:hint.clone(),family_url:Some(url.clone()),parent:None,terminal:false}); }
            let links = chapters(&doc,&base);
            for (url,hint,is_table) in &links { queue.push_back(Pending{url:url.clone(),hint:hint.clone(),family_url:p.family_url.clone(),parent:is_table.then(||base.to_string()),terminal:true}); }
            // A family landing can redirect to a chapter without ?view=full. Its own menu emits
            // the full rendition; follow it rather than silently treating the landing as full text.
            let emit = p.terminal || base.query_pairs().any(|(k,v)|k=="view"&&v=="full") || (links.is_empty() && doc.select(&sel(BODY)).next().is_some());
            if !emit {
                if fs.is_empty() && links.is_empty() { bail!("NIH discovery page has neither families nor full-view chapter links: {}",p.url); }
                continue;
            }
            if !emitted.insert(base.to_string()) { continue; }
            let key = common::slug(base.path());
            let parsed = content(&doc,&base);
            let mut row = PlanRow{source_id:src.id.clone(),doc_key:format!("{key}-unresolved"),url:base.to_string(),title:Some(p.hint),rendition:Some("html".into()),version:None,
                extra:json!({"work_key":key,"family_url":p.family_url,"parent_url":p.parent})};
            match parsed {
                Ok((title,html,ds))=>{
                    let sha = revision(&title,&html,&ds)?;
                    row.doc_key=format!("{key}-{}",&sha[..12]);
                    row.title=Some(title);
                    row.extra["clinical_sha256"]=json!(sha);
                }
                Err(e)=>row.extra["discovery_error"]=json!(format!("{e:#}")),
            }
            out.push(row);
            if limit.is_some_and(|n|out.len()>=n) {return Ok(out);}
        }
        if out.is_empty() {bail!("NIH catalog yielded no full-view chapters");}
        Ok(out)
    }

    async fn extract(&self,src:&SourceEntry,f:&Fetcher,row:&PlanRow)->Result<GuidelineDoc>{
        if let Some(e)=row.extra["discovery_error"].as_str(){bail!("NIH discovery: {e}");}
        let fetched=f.get_ok(&row.url).await?;
        let base=Url::parse(&fetched.record.final_url)?;
        let page=Html::parse_document(&fetched.text());
        let (title,html,ds)=content(&page,&base)?;
        let sha=revision(&title,&html,&ds)?;
        if row.extra["clinical_sha256"].as_str().is_some_and(|s|s!=sha){bail!("NIH clinical revision changed after planning; re-plan");}
        let (mut blocks,mut warnings)=blocks_from_html(&html,Some(&base),Some("#nih-clinical"));
        laboratory_schedule(&html,&mut blocks)?;
        warnings.extend(recommendations(&html,&mut blocks)?);
        table_footnotes(&html,&mut blocks)?;
        let updated=ds.iter().find(|d|d.label=="Updated"&&d.source_url.as_deref()==Some(base.as_str())).and_then(|d|d.normalized.clone());
        let mut rights=common::unknown_rights();
        rights.terms_url=page.select(&sel("a[href]")).find(|a|text_of(a)=="Disclaimers").and_then(|a|common::link(&base,a.value().attr("href")?));
        if rights.terms_url.is_some(){rights.evidence_urls.push(base.to_string());}
        let mut seen=HashSet::new();
        let attachments=page.select(&sel(".block-download-guidelines a[href], .field--name-field-section a.link-over-table[href], article.tables .field--name-body a[href][data-guideline_id]"))
            .filter_map(|a|{
                let url=common::link(&base,a.value().attr("href")?)?;
                if !seen.insert(url.clone()){return None;}
                let label=text_of(&a);let references=a.value().attr("data-guideline_id").is_some()&&label=="References";
                if a.value().attr("data-guideline_id").is_some()&&!references{return None;}
                Some(json!({"url":url,"label":label,"role":if references{"references"}else if a.value().classes().any(|c|c=="link-over-table"){"full_table"}else{"linked_rendition"},"rights":"unknown"}))
            }).collect();
        let mut relations=vec![];
        if let Some(parent)=row.extra["parent_url"].as_str().or_else(||row.extra["family_url"].as_str()){
            relations.push(Relation{rel:"part_of".into(),target:parent.into(),scope:None,evidence_url:Some(row.url.clone())});
        }
        if chapters(&page,&base).iter().any(|(_,_,full)|*full){
            warnings.push("This chapter contains preview tables with separately linked full versions. Previews are retained; complete table coverage requires the linked full-table documents and their footnotes.".into());
        }
        let standalone=page.select(&sel("article.tables")).next().is_some();
        if standalone&&ds.is_empty(){warnings.push("Standalone full-table page prints no independent Updated/Reviewed dates; parent chapter/table-preview dates are not inherited.".into());}
        if page.select(&sel("article.tables .field--name-body a[data-guideline_id]")).any(|a|text_of(&a)=="References"){
            warnings.push("This standalone full table supplies lettered notes and a Key but delegates its bibliography to the separately linked References route. That source-emitted route is retained as a references attachment; the standalone body does not itself contain the reference list.".into());
        }
        warnings.push("Discovery covers the source-emitted current family/chapter menus; archived-edition catalogs and historical PDF collections are not yet enumerated. PDF rendition parity and image-only branch interpretation remain unverified.".into());
        Ok(GuidelineDoc{source_id:src.id.clone(),doc_key:row.doc_key.clone(),work_key:row.extra["work_key"].as_str().map(String::from),title,url:row.url.clone(),
            item_type:Some(if standalone{"guideline_table"}else{"guideline_chapter"}.into()),status:"unknown".into(),updated,date_observations:ds,rights,relations,blocks,attachments,fetches:vec![fetched.record],warnings,
            language:common::page_language(&page),search_metadata:json!({"family_title":common::first_text(&page,".views-field-field-subtitle h1")}),..Default::default()})
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    use crate::guidelines::doc::BlockKind;
    const ROOT: &str="https://clinicalinfo.hiv.gov/en/guidelines";
    const GOALS: &str="https://clinicalinfo.hiv.gov/en/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/treatment-goals?view=full";
    const TABLES: &str="https://clinicalinfo.hiv.gov/en/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/drug-characteristics-tables?view=full";
    const FAMILY: &str="https://clinicalinfo.hiv.gov/en/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv";
    const INITIATION: &str="https://clinicalinfo.hiv.gov/en/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/initiation-antiretroviral-therapy?view=full";
    const RENAL: &str="https://clinicalinfo.hiv.gov/en/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/arv-dosing-renal-hepatic-insufficiency?view=full";
    const CATALOG: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/catalog.html"));
    const G: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/treatment-goals.html"));
    const F: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/family.html"));
    const I: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/initiation.html"));
    const R: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/renal-hepatic.html"));
    const NRTI: &str="https://clinicalinfo.hiv.gov/en/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/appendix-b-characteristics-nrti-full";
    const PI: &str="https://clinicalinfo.hiv.gov/en/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/appendix-b-characteristics-pi-full";
    const NNRTI: &str="https://clinicalinfo.hiv.gov/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/appendix-b-characteristics-nnrti-full";
    const N: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/nrti-full.html"));
    const P: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/pi-full.html"));
    const NN: &str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/nnrti-full.html"));
    const RFULL:&str="https://clinicalinfo.hiv.gov/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/appendix-b-renal-hepatic-insufficiency-full";
    const RF:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/renal-full.html"));
    fn tables()->String {
        // Consecutive, unmodified newline-boundary slices of the genuine 423355-byte response.
        // Every fixture stays below 200 KB; concatenation reproduces the entire saved resource.
        [include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/tables.html.part-aa")),
         include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/tables.html.part-ab")),
         include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/tables.html.part-ac")),
         include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/tables.html.part-ad")),
         include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/tables.html.part-ae")),
         include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/tables.html.part-af"))].concat()
    }
    fn source(url:&str)->SourceEntry {
        serde_json::from_value(json!({"id":"nih_hiv","access":"public","enabled":true,"catalog_url":url})).unwrap()
    }
    #[test]
    fn genuine_catalog_and_chapter_graph_do_not_use_search_facet_counts() {
        let f=families(&Html::parse_document(CATALOG),&Url::parse(ROOT).unwrap());
        assert_eq!(f.len(),6);
        assert!(f.iter().any(|(u,_)|u.ends_with("/pediatric-arv")));
        assert!(!f.iter().any(|(u,_)|u.contains("archived-guidelines")));
        let links=chapters(&Html::parse_document(G),&Url::parse(GOALS).unwrap());
        assert!(links.len()>40);
        assert!(links.iter().any(|(u,_,_)|u==TABLES));
        assert!(links.iter().all(|(u,_,_)|Url::parse(u).unwrap().query_pairs().any(|(k,v)|k=="view"&&v=="full")));
        let t=tables();
        assert_eq!(t.len(),423355);
        let links=chapters(&Html::parse_document(&t),&Url::parse(TABLES).unwrap());
        let full=links.iter().filter(|(_,_,full)|*full).collect::<Vec<_>>();
        assert_eq!(full.len(),3);
        assert!(full.iter().any(|(u,_,_)|u=="https://clinicalinfo.hiv.gov/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/appendix-b-characteristics-nnrti-full"));
    }
    #[tokio::test]
    async fn genuine_full_sections_dates_table_tails_and_offline_terminal_failures() {
        let tmp=tempfile::tempdir().unwrap();
        let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        f.seed_cache_for_tests(ROOT,CATALOG.as_bytes(),"text/html");
        f.seed_cache_for_tests(FAMILY,F.as_bytes(),"text/html");
        f.seed_cache_for_tests(GOALS,G.as_bytes(),"text/html");
        f.seed_cache_for_tests(INITIATION,I.as_bytes(),"text/html");
        f.seed_cache_for_tests(RENAL,R.as_bytes(),"text/html");
        for (url,body) in [(NRTI,N),(PI,P),(NNRTI,NN),(RFULL,RF)] {f.seed_cache_for_tests(url,body.as_bytes(),"text/html");}
        let t=tables();
        f.seed_cache_for_tests(TABLES,t.as_bytes(),"text/html");
        let src=source(ROOT);
        // The source menu is unmodified. Unseeded chapter bodies deliberately produce auditable
        // unresolved rows, rather than silently dropping menu entries or pretending coverage.
        let rows=NihHiv.discover(&src,&f,None).await.unwrap();
        assert!(rows.iter().filter(|r|r.extra["discovery_error"].is_string()).count()>40);
        let goal=rows.iter().find(|r|r.url==GOALS).unwrap();
        let table=rows.iter().find(|r|r.url==TABLES).unwrap();
        let gd=NihHiv.extract(&src,&f,goal).await.unwrap();
        assert_eq!(gd.title,"Treatment Goals");
        assert_eq!(gd.updated.as_deref(),Some("2026-09-24"));
        assert_eq!(gd.date_observations.len(),2);
        assert_eq!(gd.date_observations[0].label,"Updated");
        assert_eq!(gd.date_observations[1].label,"Reviewed");
        assert_eq!(gd.date_observations[1].normalized.as_deref(),Some("2026-09-24"));
        assert!(gd.blocks.iter().any(|b|b.text=="Maximally and durably suppress plasma HIV RNA (viral load)"));
        assert!(gd.blocks.iter().any(|b|b.text.contains("Rizzardini G")&&b.text.contains("33136751")));
        assert_eq!(gd.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),0);
        assert!(gd.blocks.iter().all(|b|!matches!(b.kind,BlockKind::Recommendation{..})));
        assert!(!gd.blocks.iter().any(|b|b.text=="Table Of Content"||b.text=="Download Guidelines"||b.text=="Clinicalinfo Drug Database"));
        assert_eq!(gd.attachments.len(),4);
        let initiation=rows.iter().find(|r|r.url==INITIATION).unwrap();
        let id=NihHiv.extract(&src,&f,initiation).await.unwrap();
        assert_eq!(id.updated.as_deref(),Some("2025-09-25"));
        assert_eq!(id.date_observations.len(),2);
        assert!(id.date_observations.iter().all(|d|d.normalized.as_deref()==Some("2025-09-25")));
        let recs=id.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Recommendation{..})).collect::<Vec<_>>();
        assert_eq!(recs.len(),3);
        for (b,certainty) in recs.iter().zip(["I","II","III"]) {
            assert!(matches!(&b.kind,BlockKind::Recommendation{strength:Some(a),certainty:Some(c),label:None,..} if a=="A"&&c==certainty));
        }
        assert_eq!(recs[0].text.matches("(AI)").count(),2,"retain the two printed grades attached to the two specific clauses");
        assert!(recs[0].text.contains("reduce morbidity and mortality (AI) and to prevent transmission of HIV to others (AI)."));
        assert!(recs[1].text.ends_with("(AII)."));
        assert!(recs[2].text.ends_with("(AIII)."));
        assert_eq!(id.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),1);
        assert!(id.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("A = Strong; B = Moderate; C = Weak")&&b.text.contains("III = Expert opinion")));
        assert!(id.blocks.iter().any(|b|b.text.contains("Rudy BJ")&&b.text.contains("19866536")),"retain full-statement final references, not just the recommendation box");
        let renal=rows.iter().find(|r|r.url==RENAL).unwrap();
        let rd=NihHiv.extract(&src,&f,renal).await.unwrap();
        assert_eq!(rd.updated.as_deref(),Some("2026-10-27"));
        assert_eq!(rd.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),1);
        assert!(rd.blocks.iter().any(|b|b.text.contains("Orkin C")&&b.text.contains("41763229")));
        assert!(rd.attachments.iter().any(|a|a["url"]=="https://clinicalinfo.hiv.gov/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/appendix-b-renal-hepatic-insufficiency-full"));
        assert!(rd.warnings.iter().any(|w|w.contains("preview tables")));
        let td=NihHiv.extract(&src,&f,table).await.unwrap();
        assert_eq!(td.title,"Appendix A. Drug Characteristics Tables");
        assert_eq!(td.updated.as_deref(),Some("2026-10-27"));
        let tables=td.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).collect::<Vec<_>>();
        assert_eq!(tables.len(),10,"full section only; exclude duplicate brief and obsolete commented table");
        assert!(tables[0].text.contains("Bixlenvo"));
        assert!(td.blocks.iter().any(|b|b.text.contains("Biktarvy-specific formulation effect")||b.text.contains("-specific formulation effect")));
        assert!(tables[9].text.contains("MVC 600 mg PO twice daily"));
        assert!(tables[9].text.contains("TPV/r = tipranavir/ritonavir"));
        assert!(tables[9].text.contains("hepatic insufficiency"));
        assert_eq!(td.date_observations.len(),22,"chapter Updated/Reviewed plus both independently printed dates for each of ten tables");
        assert!(td.date_observations.iter().any(|d|d.label=="Updated"&&d.normalized.as_deref()==Some("2025-09-25")&&d.source_url.as_deref().is_some_and(|u|u.ends_with("#table-nrtis-fixeddose-combination-tablets"))));
        assert!(td.date_observations.iter().any(|d|d.label=="Reviewed"&&d.normalized.as_deref()==Some("2026-09-24")&&d.source_url.as_deref().is_some_and(|u|u.ends_with("#table-nrtis-fixeddose-combination-tablets"))));
        // Table 10 prints `<strong>Reviewed: </strong>September 24, 2026` with a
        // space inside the strong tag; it is a real date, not an inherited value.
        assert!(td.date_observations.iter().any(|d|d.label=="Reviewed"&&d.normalized.as_deref()==Some("2026-09-24")&&d.source_url.as_deref().is_some_and(|u|u.ends_with("#table-characteristics-ccr5-antagonist"))));
        assert!(td.warnings.iter().any(|w|w.contains("preview tables")));
        assert_eq!(td.attachments.len(),7);
        assert!(td.published.is_none()&&td.version.is_none());
        assert_eq!(td.status,"unknown");
        for (url,kind,tail) in [(NRTI,"Nucleoside Reverse Transcriptase Inhibitors","STR = single-tablet regimen"),(PI,"Protease Inhibitors","UGT1 = uridine diphosphate glucuronyl transferase 1 family"),(NNRTI,"Non-nucleoside Reverse Transcriptase Inhibitors","Late-onset neurotoxicities")] {
            let row=rows.iter().find(|r|r.url==url).expect("source-linked full table discovered");
            let d=NihHiv.extract(&src,&f,row).await.unwrap();
            assert!(d.title.contains(kind));
            assert_eq!(d.item_type.as_deref(),Some("guideline_table"));
            assert_eq!(d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),1);
            assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains(tail)));
            assert!(d.blocks.iter().any(|b|matches!(b.kind,BlockKind::Footnote)&&b.text.contains(tail)),"long-table footer must remain independently retrievable");
            assert!(!d.blocks.iter().any(|b|b.text.contains("Back to Appendix A")));
            assert!(d.relations.iter().any(|r|r.rel=="part_of"&&r.target==TABLES));
            assert!(d.date_observations.is_empty()&&d.updated.is_none()&&d.published.is_none());
            assert!(d.warnings.iter().any(|w|w.contains("no independent Updated/Reviewed dates")));
            assert!(!d.warnings.iter().any(|w|w.contains("preview tables")));
        }
        let mut stale=table.clone();stale.extra["clinical_sha256"]=json!("stale");
        let rfrow=rows.iter().find(|r|r.url==RFULL).unwrap();
        let rfd=NihHiv.extract(&src,&f,rfrow).await.unwrap();
        assert_eq!(rfd.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),3);
        assert!(rfd.blocks.iter().any(|b|b.text=="Child-Pugh Classifications"));
        assert!(rfd.blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("Class C (Severe)")&&b.text.contains(">9 points")));
        assert!(rfd.blocks.iter().any(|b|matches!(b.kind,BlockKind::Footnote)&&b.text.contains("Biktarvy")&&b.text.contains("-specific formulation effect")&&b.text.contains("SQ = subcutaneous")));
        assert!(rfd.blocks.iter().any(|b|b.text.contains("Orkin C")&&b.text.contains("41763229")));
        assert!(!rfd.blocks.iter().any(|b|b.text.contains("Back to Appendix B")));
        assert!(rfd.date_observations.is_empty()&&rfd.updated.is_none());
        assert!(rfd.relations.iter().any(|r|r.rel=="part_of"&&r.target==RENAL));
        assert!(NihHiv.extract(&src,&f,&stale).await.unwrap_err().to_string().contains("changed after planning"));
    }

    #[test]
    fn different_clause_grades_are_never_collapsed_to_one_rating() {
        let page=Html::parse_document(I);
        let (_,html,_)=content(&page,&Url::parse(INITIATION).unwrap()).unwrap();
        let changed=html.replacen("(AI)","(BI)",1);
        assert_ne!(changed,html);
        let (mut blocks,_)=blocks_from_html(&changed,None,Some(".field--name-field-section"));
        let warnings=recommendations(&changed,&mut blocks).unwrap();
        let first=blocks.iter().find(|b|matches!(b.kind,BlockKind::Recommendation{..})).unwrap();
        assert!(first.text.contains("mortality (BI)")&&first.text.contains("others (AI)."));
        assert!(matches!(first.kind,BlockKind::Recommendation{strength:None,certainty:None,..}));
        assert!(warnings.iter().any(|w|w.contains("multiple different grades")));
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod live_sub_bookpart_tests {
    use super::*;
    const LAB:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/laboratory-testing.html"));
    const DRUG:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/drug-resistance.html"));
    const START:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/what-to-start.html"));
    #[tokio::test]
    async fn genuine_sub_bookpart_full_fields_dates_and_table_tails(){
        let src:SourceEntry=serde_json::from_value(json!({"id":"nih_hiv","enabled":true,"access":"public"})).unwrap();
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();
        for (path,raw,title) in [
            ("tests-initial-assessment-follow-up",LAB,"Laboratory Testing for Initial Assessment and Monitoring of People With HIV"),
            ("drug-resistance-testing",DRUG,"Drug-Resistance Testing"),
            ("what-start-initial-combination-regimens",START,"Initial Combination Antiretroviral Regimens for People With HIV")]{
            let url=Url::parse(&format!("https://clinicalinfo.hiv.gov/en/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/{path}?view=full")).unwrap();
            let (t,html,dates)=content(&Html::parse_document(raw),&url).unwrap();assert_eq!(t,title);
            assert_eq!(dates.len(),2);assert_eq!(dates[0].label,"Updated");assert_eq!(dates[1].label,"Reviewed");
            if path=="tests-initial-assessment-follow-up"{assert_eq!(dates[0].normalized.as_deref(),Some("2026-10-27"));assert_eq!(dates[1].normalized.as_deref(),Some("2026-10-27"));}
            let (mut blocks,_)=blocks_from_html(&html,Some(&url),Some("#nih-clinical"));
            assert_eq!(blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).count(),2);
            assert!(blocks.iter().all(|b|!b.text.contains("Download Guidelines")&&!b.text.contains("CONNECT WITH US")));
            recommendations(&html,&mut blocks).unwrap();table_footnotes(&html,&mut blocks).unwrap();
            if path=="tests-initial-assessment-follow-up"{
                assert!(blocks.iter().any(|b|b.text.contains("TDF = tenofovir disoproxil fumarate")&&matches!(b.kind,BlockKind::Footnote)));
                assert!(blocks.iter().any(|b|b.text.contains("41358893")));
                let links=chapters(&Html::parse_document(raw),&url);assert!(links.iter().any(|(u,_,is_table)|*is_table&&u.ends_with("tests-initial-assessment-and-follow-up-full")));
            }else if path=="drug-resistance-testing"{
                assert_eq!(dates[0].normalized.as_deref(),Some("2024-09-12"));
                assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Recommendation{..})&&b.text.contains("(AI)")));
            }else{
                assert!(blocks.iter().any(|b|matches!(b.kind,BlockKind::Table{..})&&b.text.contains("HLA-B*5701-negative")&&b.text.contains("contraindicated")));
                assert!(blocks.iter().any(|b|b.text.contains("Rating of Evidence:")&&matches!(b.kind,BlockKind::Footnote)));
            }
            f.seed_cache_for_tests(url.as_str(),raw.as_bytes(),"text/html");
            let row=PlanRow{source_id:"nih_hiv".into(),doc_key:"live-fixture".into(),url:url.to_string(),title:None,rendition:Some("html".into()),version:None,extra:json!({})};
            NihHiv.extract(&src,&f,&row).await.unwrap();
        }
    }
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod full_lab_schedule_tests{
    use super::*;
    const FULL:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/laboratory-full.html"));
    const REDIRECT:&str=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/guidelines/nih_hiv/laboratory-full-redirect.html"));
    const URL:&str="https://clinicalinfo.hiv.gov/en/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/tests-initial-assessment-and-follow-up-full";
    #[tokio::test]
    async fn genuine_full_schedule_timepoint_columns_and_all_lettered_notes(){
        let base=Url::parse(URL).unwrap();
        assert_eq!(crate::common::sha256_hex(FULL.as_bytes()),"f0fe0d724084f686d81bc927c10fe7fa7e7f9d46bfdbf86cdf2d61d41488beda");
        assert_eq!(crate::common::sha256_hex(REDIRECT.as_bytes()),"628c106a3a1565b7367b0c57c715c703275865a10e424d3960a562480fed62d3");
        let (t,h,dates)=content(&Html::parse_document(FULL),&base).unwrap();let (rt,rh,rd)=content(&Html::parse_document(REDIRECT),&base).unwrap();
        assert_eq!(revision(&t,&h,&dates).unwrap(),revision(&rt,&rh,&rd).unwrap());assert!(dates.is_empty());
        let tmp=tempfile::tempdir().unwrap();let f=Fetcher::new(tmp.path(),true,500,1<<20,"").unwrap();f.seed_cache_for_tests(URL,FULL.as_bytes(),"text/html");
        let src:SourceEntry=serde_json::from_value(json!({"id":"nih_hiv","access":"public","enabled":true})).unwrap();
        let row=PlanRow{source_id:"nih_hiv".into(),doc_key:"laboratory-full".into(),url:URL.into(),title:None,rendition:Some("html".into()),version:None,extra:json!({})};
        let d=NihHiv.extract(&src,&f,&row).await.unwrap();assert!(d.updated.is_none()&&d.published.is_none()&&d.date_observations.is_empty());
        let tables=d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Table{..})).collect::<Vec<_>>();assert_eq!(tables.len(),1);
        let cells=|line:&str|line.trim().trim_start_matches('|').trim_end_matches('|').split('|').map(|v|v.trim().to_string()).collect::<Vec<_>>();
        let header=cells(tables[0].text.lines().next().unwrap());assert_eq!(header.len(),9);assert_eq!(header[0],"Laboratory Test");
        for (i,timepoint) in [(1,"Entry Into Care"),(2,"ART Initiation"),(3,"After ART Initiation or Modification"),(4,"Every 3–4 Months"),(7,"Treatment Failure"),(8,"Clinically Indicated")]{assert!(header[i].contains(timepoint),"header {i} lost {timepoint}");}
        let viral=cells(tables[0].text.lines().find(|l|l.starts_with("| HIV Viral Load |")).unwrap());assert_eq!(viral.len(),9);assert!(viral[3].contains("At 4-8 weeks"));assert!(viral[4].contains('d')&&viral[5].contains('d'));assert!(viral[6].is_empty());
        let cd4=cells(tables[0].text.lines().find(|l|l.starts_with("| CD4 Count |")).unwrap());assert!(cd4[4].contains("<300"));assert!(cd4[5].contains("Optional unless clinically indicated"));
        let notes=d.blocks.iter().filter(|b|matches!(b.kind,BlockKind::Footnote)).collect::<Vec<_>>();assert_eq!(notes.len(),19);
        assert!(notes.iter().any(|b|b.text.starts_with("q ")&&b.text.contains("HbA1C is no longer recommended for diagnosis of diabetes in people with HIV on ART")));
        assert!(notes.iter().any(|b|b.text.starts_with("r ")&&b.text.contains("before initiating TAF- or TDF-containing regimens")&&b.text.contains("monitored during treatment")));
        assert!(notes.iter().any(|b|b.text=="s For women of reproductive potential."));
        assert!(d.blocks.iter().any(|b|b.text.starts_with("Key:")&&b.text.contains("TDF = tenofovir disoproxil fumarate")));
        assert!(d.attachments.iter().any(|a|a["role"]=="references"&&a["url"]=="https://clinicalinfo.hiv.gov/en/guidelines/hiv-clinical-guidelines-adult-and-adolescent-arv/tests-initial-assessment-and-follow"));
        assert!(d.warnings.iter().any(|w|w.contains("delegates its bibliography")));
    }
}
