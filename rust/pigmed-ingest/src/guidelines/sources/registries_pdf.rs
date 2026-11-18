//! Group D (national registries with PDF/office terminals) helpers.
//!
//! * `pdfium_resave`: some registry PDFs are valid for pdfium but rejected by pdf-inspector (e.g. Chile MINSAL's
//!   2014 Mac OS X Quartz PDFs: CR-only line ends, incremental `/Prev` xref and `/Encrypt null` in the trailer give
//!   "PDF parsing error: document has no readable pages"). Importing the pages into a fresh pdfium document and saving
//!   it yields a plain PDF with the same page content, which then goes through the normal converter (Markdown
//!   layout, OCR of image-only pages). The original bytes stay the fetched, checksummed rendition.
//! * `blocks_for_pdf`: `static_list::blocks_for` with that fallback, plus a warning that records it.
//! * `convert_bytes`: the same conversion for bytes that were not fetched directly (ZIP members).
use super::static_list::blocks_for;
use crate::guidelines::doc::Block;
use crate::guidelines::fetch::Fetched;
use crate::guidelines::html::blocks_from_markdown;
use anyhow::{anyhow, bail, Result};

/// Import every page of `bytes` into a new pdfium document and save it (no security handler, rebuilt xref).
pub fn pdfium_resave(bytes: &[u8]) -> Result<Vec<u8>> {
    let lib = crate::media::pdfium()?;
    let _gate = crate::media::PDFIUM_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let src = lib.load_pdf_from_byte_slice(bytes, None).map_err(|e| anyhow!("pdfium cannot open the PDF: {e:?}"))?;
    if src.pages().len() == 0 {
        bail!("pdfium opened the PDF but it has no pages");
    }
    let mut out = lib.create_new_pdf().map_err(|e| anyhow!("pdfium create: {e:?}"))?;
    out.pages_mut().append(&src).map_err(|e| anyhow!("pdfium page import: {e:?}"))?;
    out.save_to_bytes().map_err(|e| anyhow!("pdfium save: {e:?}"))
}

fn repair_note(first: &anyhow::Error) -> String {
    format!(
        "The source PDF was rejected by the primary converter ({first:#}); its pages were re-serialised by pdfium \
         (same page content, new file structure) and converted from that copy. The fetched bytes and checksum are \
         the original."
    )
}

/// `blocks_for` for a fetched PDF; on a converter failure of real PDF bytes, retry on a pdfium re-serialised copy.
pub async fn blocks_for_pdf(p: &Fetched, ocr_pages: usize) -> Result<(Vec<Block>, Vec<String>, Option<String>)> {
    match blocks_for(p, None, ocr_pages).await {
        Ok(x) => Ok(x),
        Err(first) if p.is_pdf() && format!("{first:#}").contains("convert failed") => {
            let note = repair_note(&first);
            let body = p.body.clone();
            let fixed = tokio::task::spawn_blocking(move || pdfium_resave(&body)).await?.map_err(|e| first.context(e))?;
            let copy = Fetched { record: p.record.clone(), body: fixed };
            let (blocks, mut warnings, title) = blocks_for(&copy, None, ocr_pages).await?;
            warnings.push(note);
            Ok((blocks, warnings, title))
        }
        Err(e) => Err(e),
    }
}

/// Convert bytes that were not fetched as their own response (e.g. a ZIP member) by extension: PDF (with the pdfium
/// fallback), DOCX and the other formats `crate::convert` sniffs.
pub async fn convert_bytes(bytes: Vec<u8>, ext: &str, ocr_pages: usize) -> Result<(Vec<Block>, Vec<String>)> {
    async fn run(bytes: Vec<u8>, ext: String, ocr_pages: usize) -> Result<crate::convert::Converted> {
        tokio::task::spawn_blocking(move || {
            let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
            let p = dir.path().join(format!("member{ext}"));
            std::fs::write(&p, &bytes).map_err(|e| e.to_string())?;
            crate::convert::convert(&p, &ext, crate::convert::Opts { ocr_pages })
        })
        .await?
        .map_err(|e| anyhow!("convert failed: {e}"))
    }
    let is_pdf = bytes.starts_with(b"%PDF-");
    let mut warnings = vec![];
    let conv = match run(bytes.clone(), ext.to_string(), ocr_pages).await {
        Ok(c) => c,
        Err(first) if is_pdf => {
            warnings.push(repair_note(&first));
            let fixed = tokio::task::spawn_blocking(move || pdfium_resave(&bytes)).await?.map_err(|e| first.context(e))?;
            run(fixed, ".pdf".into(), ocr_pages).await?
        }
        Err(e) => return Err(e),
    };
    if conv.pages_without_text > 0 {
        warnings.push(format!("{} image-only pages without text (OCR cap {ocr_pages})", conv.pages_without_text));
    }
    Ok((blocks_from_markdown(&conv.md), warnings))
}

#[cfg(all(test, feature = "guideline-fixtures"))]
mod tests {
    use super::*;
    use crate::guidelines::doc::FetchRecord;
    /// Hand-made one-page PDF with the traits of the failing MINSAL file: CR-only line ends, `/Encrypt null` in the
    /// trailer and a Quartz producer string.
    const CR_PDF: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/guidelines/cl_minsal_ges/quartz-cr-encrypt-null.pdf"));
    #[tokio::test]
    async fn pdfium_resave_recovers_text_when_primary_converter_rejects_the_file() {
        let f = Fetched {
            record: FetchRecord {
                url: "https://example.invalid/x.pdf".into(),
                final_url: "https://example.invalid/x.pdf".into(),
                status: 200,
                content_type: Some("application/pdf".into()),
                ..Default::default()
            },
            body: CR_PDF.to_vec(),
        };
        let direct = blocks_for(&f, None, 0).await;
        let (blocks, warnings, _) = blocks_for_pdf(&f, 0).await.unwrap();
        let text = blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join(" ");
        assert!(text.contains("Insuficiencia renal"), "blocks: {blocks:?}");
        // The fallback is recorded exactly when the primary converter failed.
        assert_eq!(direct.is_err(), warnings.iter().any(|w| w.contains("re-serialised by pdfium")), "direct: {direct:?}");
        let (b2, _) = convert_bytes(CR_PDF.to_vec(), ".pdf", 0).await.unwrap();
        assert!(b2.iter().any(|b| b.text.contains("Insuficiencia renal")));
    }
}
