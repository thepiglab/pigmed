use super::*;

const XML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guidelines/easl_pmc_cloud/patient-synthetic.xml"
));
const NATIVE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guidelines/easl_pmc_cloud/metadata-synthetic.json"
));
const JPEG: &[u8] = b"\xff\xd8\xffsynthetic-signature-only";
const PDF: &[u8] = b"%PDF-1.4\nSynthetic unparsed fixture only\n%%EOF";

fn digest(raw: &[u8]) -> String {
    format!("{:x}", Md5::digest(raw))
}
fn native_value(xml: &str) -> Value {
    let mut value: Value = serde_json::from_str(NATIVE).unwrap();
    value["xml_url"] = json!(format!(
        "s3://pmc-oa-opendata/PMC8514420.1/PMC8514420.1.xml?md5={}",
        digest(xml.as_bytes())
    ));
    value
}
fn meta(xml: &str) -> Metadata {
    metadata(&serde_json::to_vec(&native_value(xml)).unwrap()).unwrap()
}
fn parsed(xml: &str) -> GuidelineDoc {
    content(xml.as_bytes(), &meta(xml)).unwrap()
}
fn source() -> SourceEntry {
    serde_json::from_value(json!({"id":"runtime_patient_source","access":"public","enabled":true,"adapter":"easl_pmc_cloud",
        "catalog_url":META,"max_bytes":FILE_CAP,"config":{"pmcid":PMCID,"processing_version":1}})).unwrap()
}
fn fetched(item: &Object, body: &[u8], mime: &str) -> Fetched {
    Fetched {
        record: FetchRecord {
            url: item.url.clone(),
            final_url: item.url.clone(),
            redirects: vec![],
            status: 200,
            content_type: Some(mime.into()),
            bytes: body.len() as u64,
            sha256: sha(body),
            fetched_at: "Sat, 14 Nov 2026 23:00:00 GMT".into(),
            from_cache: true,
        },
        body: body.to_vec(),
    }
}
fn seed(f: &Fetcher, xml: &str) {
    let value = native_value(xml);
    let m = meta(xml);
    f.seed_cache_for_tests(
        META,
        &serde_json::to_vec(&value).unwrap(),
        "binary/octet-stream",
    );
    f.seed_cache_for_tests(&m.xml.url, xml.as_bytes(), "binary/octet-stream");
    for item in &m.media {
        f.seed_cache_for_tests(
            &item.url,
            if item.role == "figure_asset" {
                JPEG
            } else {
                PDF
            },
            "binary/octet-stream",
        );
    }
}

#[test]
fn native_mixed_order_boxed_ownership_and_list_boundaries() {
    let doc = parsed(XML);
    let ids: Vec<_> = doc.blocks.iter().map(|b| b.block_id.as_str()).collect();
    let position = |id: &str| ids.iter().position(|s| *s == id).unwrap();
    assert!(position("p1") < position("fig1") && position("fig1") < position("p1~part2"));
    assert!(
        position("p2") < position("box1~heading")
            && position("box1~heading") < position("undfig1")
            && position("undfig1") < position("p2~part2")
    );
    assert!(
        position("lip1") < position("nested-p") && position("nested-p") < position("lip1~part2")
    );
    assert!(matches!(
        doc.blocks[position("lip1")].kind,
        BlockKind::ListItem
    ));
    assert_eq!(
        doc.blocks[position("lip1")].text,
        "a. First synthetic item."
    );
    assert!(doc.blocks[position("undfig1")]
        .heading_path
        .iter()
        .any(|s| s == "Box A Synthetic box heading"));
    assert!(
        doc.search_metadata["native_figures"][1]["ancestors_nearest_first"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["native_id"] == "box1")
    );
    assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len());
}

#[test]
fn owned_rights_are_restricted_and_reference_license_is_not_a_grant() {
    let doc = parsed(XML);
    assert_eq!(doc.rights.status, "restricted");
    assert_eq!(doc.rights.terms_url.as_deref(), Some(CC));
    assert_eq!(doc.rights.evidence_urls, vec![meta(XML).xml.url]);
    assert!(doc.rights.note.as_deref().unwrap().contains("CC BY-NC-ND"));
    let start = XML.find("<permissions>").unwrap();
    let end = XML.find("</permissions>").unwrap() + "</permissions>".len();
    let permission = &XML[start..end];
    let without = format!("{}{}", &XML[..start], &XML[end..]);
    let moved = without.replace(
        "<ref id=\"bib1\">",
        &format!("<ref id=\"bib1\">{permission}"),
    );
    assert!(content(moved.as_bytes(), &meta(&moved)).is_err());
    let wrong = XML.replace("CC BY-NC-ND license", "different license");
    assert!(content(wrong.as_bytes(), &meta(&wrong)).is_err());
}

#[test]
fn own_epub_collection_history_and_processing_roles_stay_separate() {
    let doc = parsed(XML);
    assert_eq!(doc.published.as_deref(), Some("2021-09-17"));
    assert!(doc.updated.is_none() && doc.version.is_none());
    assert_eq!(doc.status, "unknown");
    assert!(doc
        .date_observations
        .iter()
        .all(|d| d.source_url.as_deref() == Some(meta(XML).xml.url.as_str())));
    assert_eq!(doc.search_metadata["dataset_processing_version"], 1);
    assert!(doc
        .date_observations
        .iter()
        .any(|d| d.label == "pub-date:collection" && d.normalized.as_deref() == Some("2021-10")));
    assert!(doc
        .date_observations
        .iter()
        .any(|d| d.label == "history:received" && d.normalized.as_deref() == Some("2021-05-27")));
    assert!(doc
        .date_observations
        .iter()
        .any(|d| d.label.starts_with("pub-history:")
            && d.normalized.as_deref() == Some("2026-03-02")));
    assert!(!doc
        .blocks
        .iter()
        .any(|b| matches!(b.kind, BlockKind::Recommendation { .. })));
}

#[test]
fn native_metadata_requires_actual_integer_boolean_identity_types() {
    for (key, value) in [
        ("pmid", json!("34693236")),
        ("version", json!("1")),
        ("is_retracted", json!(true)),
        ("is_manuscript", json!(null)),
        ("pmcid", json!("PMC999")),
        ("title", json!("Different patient work")),
        ("doi", json!("10.0000/different")),
    ] {
        let mut native = native_value(XML);
        native[key] = value;
        assert!(
            metadata(&serde_json::to_vec(&native).unwrap()).is_err(),
            "{key}"
        );
    }
    assert!(
        metadata(&serde_json::to_vec(&json!({"records":[native_value(XML)]})).unwrap()).is_err()
    );
    assert!(meta(XML).raw["mid"].is_null());
}

#[test]
fn inventory_allows_only_exact_official_namespace_and_one_md5() {
    let valid = format!(
        "s3://pmc-oa-opendata/PMC8514420.1/gr1.jpg?md5={}",
        digest(JPEG)
    );
    let good = object(&valid, "figure_asset", ".jpg").unwrap();
    assert_eq!(good.url, valid.replacen("s3://pmc-oa-opendata", ORIGIN, 1));
    for bad in [
        valid.replace("pmc-oa-opendata/", "other-bucket/"),
        valid.replace("PMC8514420.1/", "PMC8514420.2/"),
        valid.replace("gr1.jpg", "../gr1.jpg"),
        valid.replace("gr1.jpg", "%2e%2e/gr1.jpg"),
        valid.replace("gr1.jpg", "gr1.jpg/extra"),
        format!("{valid}&md5={}", digest(JPEG)),
        valid.replace("?md5=", "#md5="),
        valid.replace("?md5=", "?other="),
    ] {
        assert!(object(&bad, "figure_asset", ".jpg").is_err());
    }
    let mut native = native_value(XML);
    let first = native["media_urls"][0].clone();
    native["media_urls"].as_array_mut().unwrap().push(first);
    assert!(metadata(&serde_json::to_vec(&native).unwrap()).is_err());
}

#[test]
fn graphic_resolution_never_guesses_extensions_or_foreign_targets() {
    for target in [
        "gr1",
        "https://other.invalid/gr1.jpg",
        "http://synthetic@example.invalid/gr1.jpg",
        "missing.jpg",
    ] {
        let xml = XML.replace(
            "xlink:href=\"gr1.jpg\"",
            &format!("xlink:href=\"{target}\""),
        );
        assert!(content(xml.as_bytes(), &meta(&xml)).is_err());
    }
}

#[test]
fn unsafe_native_contact_href_is_hash_only_and_never_a_request_target() {
    let doc = parsed(XML);
    let encoded = serde_json::to_string(&doc).unwrap();
    assert!(!encoded.contains("http://synthetic@example.invalid"));
    assert!(encoded.contains(&sha(b"http://synthetic@example.invalid")));
    assert!(encoded.contains("unresolved_unsafe_native_href"));
    let unsafe_uri = uri_descriptor("http://synthetic@example.invalid");
    assert!(unsafe_uri["native_href"].is_null() && unsafe_uri["normalized_uri"].is_null());
    assert_eq!(unsafe_uri["target_fetched"], false);
    let whitespace = uri_descriptor("http://example.invalid/with space");
    assert!(whitespace["native_href"].is_null());
}

#[test]
fn object_verification_distinguishes_provider_md5_and_local_sha256() {
    let m = meta(XML);
    let item = m.media.iter().find(|o| o.basename == "gr1.jpg").unwrap();
    let good = fetched(item, JPEG, "binary/octet-stream");
    let verified = verify(&good, item).unwrap();
    assert_eq!(verified["provider_md5"], digest(JPEG));
    assert_eq!(verified["sha256"], sha(JPEG));
    assert_eq!(verified["provider_md5_verified"], true);
    assert_eq!(verified["OCR_or_visual_content_verified"], false);
    let bad = fetched(item, b"\xff\xd8\xffchanged bytes", "binary/octet-stream");
    assert!(verify(&bad, item).is_err());
}

#[test]
fn redirected_changed_target_html_and_signature_receipts_are_rejected() {
    let m = meta(XML);
    let item = m.media.iter().find(|o| o.basename == "gr1.jpg").unwrap();
    let mut got = fetched(item, JPEG, "binary/octet-stream");
    got.record.redirects.push("https://other.invalid/".into());
    assert!(verify(&got, item).is_err());
    let mut got = fetched(item, JPEG, "binary/octet-stream");
    got.record.final_url = "https://other.invalid/gr1.jpg".into();
    assert!(verify(&got, item).is_err());
    let got = fetched(item, JPEG, "text/html");
    assert!(verify(&got, item).is_err());
    let fake = object(
        &format!(
            "s3://pmc-oa-opendata/PMC8514420.1/gr1.jpg?md5={}",
            digest(b"not an image")
        ),
        "figure_asset",
        ".jpg",
    )
    .unwrap();
    assert!(verify(
        &fetched(&fake, b"not an image", "binary/octet-stream"),
        &fake
    )
    .is_err());
}

#[test]
fn xml_preorder_not_lexicographic_inventory_orders_figures() {
    let m = meta(XML);
    assert_eq!(m.media[0].basename, "fx1.jpg");
    let doc = content(XML.as_bytes(), &m).unwrap();
    let names: Vec<_> = doc.search_metadata["native_figures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["native_href"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["gr1.jpg", "fx1.jpg"]);
    assert!(
        !doc.search_metadata["native_figures"][1]["caption_is_full_graphic_content"]
            .as_bool()
            .unwrap()
    );
}

#[test]
fn table_geometry_caption_row_groups_and_owned_footnote_survive() {
    let doc = parsed(XML);
    let table = &doc.search_metadata["native_tables"][0];
    assert_eq!(table["caption"], "Synthetic table caption.");
    assert_eq!(table["rows"][0]["row_group"], "thead");
    assert_eq!(table["rows"][0]["cells"][0]["colspan"], "2");
    assert_eq!(table["rows"][1]["cells"][0]["rowspan"], "2");
    assert_eq!(table["rows"][2]["cells"][0]["origin_column"], 2);
    assert_eq!(table["footnotes"][0]["owner_table"], "tbl1");
    let t = doc
        .blocks
        .iter()
        .position(|b| b.block_id == "tbl1")
        .unwrap();
    let f = doc.blocks.iter().position(|b| b.block_id == "tf1").unwrap();
    assert!(t < f && doc.blocks[t].text.contains("\n| ---"));
    assert!(matches!(doc.blocks[f].kind, BlockKind::Footnote));
    for value in ["0", "9", "bad"] {
        let changed = XML.replace("rowspan=\"2\"", &format!("rowspan=\"{value}\""));
        assert!(content(changed.as_bytes(), &meta(&changed)).is_err());
    }
}

#[test]
fn duplicate_ids_fail_and_unknown_clinical_tags_are_retained_with_warning() {
    let duplicate = XML.replace("id=\"p2\"", "id=\"p1\"");
    assert!(content(duplicate.as_bytes(), &meta(&duplicate)).is_err());
    let unknown=XML.replace("</body>","<synthetic-unknown><p id=\"unknown-p\">Unreviewed synthetic content.</p></synthetic-unknown></body>");
    let doc = parsed(&unknown);
    assert!(doc.warnings.iter().any(|w| w.contains("synthetic-unknown")));
    assert!(doc
        .blocks
        .iter()
        .any(|b| b.text == "Unreviewed synthetic content."));
    assert!(serde_json::to_string(&doc.search_metadata["native_scopes"])
        .unwrap()
        .contains("synthetic-unknown"));
}

#[test]
fn native_identity_empty_body_entity_and_borrowed_publication_are_rejected() {
    for changed in [
        XML.replace(">PMC8514420<", ">PMC999<"),
        XML.replace(">PMC8514420.1<", ">PMC8514420.2<"),
        XML.replace(">34693236<", ">999<"),
        XML.replace(
            "Non-alcoholic fatty liver disease: A patient guideline",
            "Different title",
        ),
        XML.replace("pub-type=\"epub\"", "pub-type=\"accepted\""),
        XML.replace("<body>", "<not-body>"),
        XML.replace("<front>", "<!ENTITY synthetic 'forbidden'><front>"),
    ] {
        assert!(content(changed.as_bytes(), &meta(&changed)).is_err());
    }
    let start = XML.find("<body>").unwrap();
    let end = XML.find("</body>").unwrap() + 7;
    let empty = format!("{}<body/>{}", &XML[..start], &XML[end..]);
    assert!(content(empty.as_bytes(), &meta(&empty)).is_err());
}

#[test]
fn clinical_snapshot_covers_text_heading_geometry_caption_legal_and_href_hashes() {
    let base = snapshot(&parsed(XML)).unwrap();
    for (from, to) in [
        ("Synthetic before", "Changed synthetic before"),
        ("Synthetic heading</title>", "Changed heading</title>"),
        ("colspan=\"2\"", "colspan=\"3\""),
        ("Synthetic caption A.", "Changed caption A."),
        ("© 2021 The Authors", "© 2021 The Authors; synthetic notice"),
        (
            "http://synthetic@example.invalid",
            "http://different@example.invalid",
        ),
    ] {
        let changed = XML.replace(from, to);
        assert_ne!(snapshot(&parsed(&changed)).unwrap(), base, "{from}");
    }
}

#[tokio::test]
async fn offline_headerless_discover_extract_keeps_runtime_source_and_verified_media() {
    let temp = tempfile::tempdir().unwrap();
    let f = Fetcher::new(temp.path(), true, 500, FILE_CAP as u64, "").unwrap();
    seed(&f, XML);
    let src = source();
    let rows = EaslPmcCloud.discover(&src, &f, Some(1)).await.unwrap();
    assert_eq!(rows.len(), 1);
    let doc = EaslPmcCloud.extract(&src, &f, &rows[0]).await.unwrap();
    assert_eq!(doc.source_id, src.id);
    assert!(doc.version.is_none());
    assert_eq!(doc.fetches.len(), 5);
    assert!(doc.fetches.iter().all(|r| r.from_cache));
    assert_eq!(doc.search_metadata["media_inventory_objects_verified"], 3);
    assert!(doc
        .attachments
        .iter()
        .any(|a| a["role"] == "supplementary_pdf"
            && a["bytes_acquired"] == true
            && a["parsed"] == false));
    assert!(doc
        .attachments
        .iter()
        .any(|a| a["role"] == "article_pdf" && a["bytes_acquired"] == false));
    assert!(
        doc.search_metadata["scope_complete"] == false
            && doc.search_metadata["asset_OCR_or_vision_verified"] == false
    );
}

#[tokio::test]
async fn offline_same_processing_version_metadata_or_xml_drift_requires_rediscovery() {
    let temp = tempfile::tempdir().unwrap();
    let f = Fetcher::new(temp.path(), true, 500, FILE_CAP as u64, "").unwrap();
    seed(&f, XML);
    let src = source();
    let rows = EaslPmcCloud.discover(&src, &f, None).await.unwrap();
    let changed = XML.replace("Synthetic before", "Changed synthetic before");
    seed(&f, &changed);
    let error = EaslPmcCloud
        .extract(&src, &f, &rows[0])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("rediscover"));
    seed(&f, XML);
    let mut native = native_value(XML);
    native["mid"] = json!("new metadata fact");
    f.seed_cache_for_tests(
        META,
        &serde_json::to_vec(&native).unwrap(),
        "binary/octet-stream",
    );
    assert!(EaslPmcCloud
        .extract(&src, &f, &rows[0])
        .await
        .unwrap_err()
        .to_string()
        .contains("rediscover"));
}

#[tokio::test]
async fn offline_same_url_asset_corruption_fails_provider_md5() {
    let temp = tempfile::tempdir().unwrap();
    let f = Fetcher::new(temp.path(), true, 500, FILE_CAP as u64, "").unwrap();
    seed(&f, XML);
    let src = source();
    let rows = EaslPmcCloud.discover(&src, &f, None).await.unwrap();
    let m = meta(XML);
    f.seed_cache_for_tests(
        &m.media[0].url,
        b"\xff\xd8\xffcorrupted synthetic bytes",
        "binary/octet-stream",
    );
    assert!(EaslPmcCloud
        .extract(&src, &f, &rows[0])
        .await
        .unwrap_err()
        .to_string()
        .contains("MD5"));
}

#[tokio::test]
async fn zero_limit_and_invalid_processing_or_plan_identity_are_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let f = Fetcher::new(temp.path(), true, 500, FILE_CAP as u64, "").unwrap();
    let mut src = source();
    assert!(EaslPmcCloud
        .discover(&src, &f, Some(0))
        .await
        .unwrap()
        .is_empty());
    src.config["processing_version"] = json!(2);
    assert!(EaslPmcCloud.discover(&src, &f, Some(0)).await.is_err());
    src = source();
    seed(&f, XML);
    let mut row = EaslPmcCloud
        .discover(&src, &f, None)
        .await
        .unwrap()
        .remove(0);
    row.version = Some("1".into());
    assert!(EaslPmcCloud.extract(&src, &f, &row).await.is_err());
}
