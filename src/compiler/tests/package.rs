use super::email::attachment_fixture;
use super::*;

#[test]
fn compile_validate_and_detect_tampering() {
    let (temp, source, assignments, checksums) = markdown_fixture();
    set_mode(temp.path(), 0o755);
    let mut assignment: Value = read_json(&assignments).unwrap();
    assignment["units"][0]["locator"]["line"] = json!(1.0);
    assignment["units"][0]["metadata"]["integral"] = json!(2.0);
    write_private(
        &assignments,
        serde_json::to_vec(&assignment).unwrap().as_slice(),
    );
    let package = temp.path().join("package");
    let stale = private_dir(temp.path(), "package.staging");
    write_private(&stale.join("partial"), b"incomplete");
    assert!(compile(&assignments, &source, &checksums, &package).is_err());
    assert!(stale.join("partial").exists());
    fs::remove_dir_all(&stale).unwrap();
    let unrelated_marker_sibling = temp.path().join("package.staging.owner.json.tmp");
    write_private(&unrelated_marker_sibling, b"{");
    compile(&assignments, &source, &checksums, &package).unwrap();
    assert!(unrelated_marker_sibling.exists());
    validate(&package).unwrap();
    assert!(PrivateDirectory::new(&package.join("nested")).is_err());
    compile(&assignments, &source, &checksums, &package).unwrap();

    let manifest: EvidencePackageManifest = read_json(&package.join("manifest.json")).unwrap();
    let unit_path = package.join(&manifest.units[0].path);
    let mut unit: Value = read_json(&unit_path).unwrap();
    assert_eq!(unit["source_locator"]["line"], json!(1));
    assert_eq!(unit["metadata"]["integral"], json!(2));
    unit["spans"][0]["text"] = "tampered".into();
    write_private(&unit_path, serde_json::to_vec(&unit).unwrap().as_slice());
    assert!(validate(&package).is_err());
}

#[test]
fn validation_rejects_unmanifested_and_schema_invalid_units() {
    let (temp, source, assignments, checksums) = markdown_fixture();
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    write_private(&package.join("units/extra.json"), b"{}");
    assert!(validate(&package).is_err());
    fs::remove_file(package.join("units/extra.json")).unwrap();

    let mut manifest: EvidencePackageManifest = read_json(&package.join("manifest.json")).unwrap();
    let unit_path = package.join(&manifest.units[0].path);
    let mut unit: EvidenceUnit = read_json(&unit_path).unwrap();
    unit.sources.clear();
    unit.unit_sha256 = digest(&unit.canonical_bytes().unwrap());
    manifest.units[0].unit_sha256.clone_from(&unit.unit_sha256);
    write_private(&unit_path, serde_json::to_vec(&unit).unwrap().as_slice());
    write_private(
        &package.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap().as_slice(),
    );
    assert!(validate(&package).is_err());

    unit.sources = vec![SourceFile {
        path: "unrelated.md".into(),
        sha256: "0".repeat(64),
        bytes: 0,
    }];
    unit.unit_sha256 = digest(&unit.canonical_bytes().unwrap());
    manifest.units[0].unit_sha256.clone_from(&unit.unit_sha256);
    write_private(&unit_path, serde_json::to_vec(&unit).unwrap().as_slice());
    write_private(
        &package.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap().as_slice(),
    );
    assert!(validate(&package).is_err());
}

#[test]
fn validation_and_loading_accept_evidence_v1_and_v2() {
    let (temp, source, assignments, checksums) = markdown_fixture();
    let package_v1 = temp.path().join("package-v1");
    compile(&assignments, &source, &checksums, &package_v1).unwrap();
    rewrite_package_schema_version(&package_v1, 1);
    let units = load_package(&package_v1).unwrap();
    assert_eq!(units[0].schema_version, 1);
    assert!(units[0].attachments.is_empty());

    let (temp, source, assignments, checksums, manifest) = attachment_fixture();
    let package_v2 = temp.path().join("package-v2");
    compile_with_email_attachments(
        &assignments,
        &source,
        &checksums,
        std::slice::from_ref(&manifest),
        &package_v2,
    )
    .unwrap();
    rewrite_package_schema_version(&package_v2, 2);
    let units = load_package(&package_v2).unwrap();
    assert!(units.iter().all(|unit| unit.schema_version == 2));
    assert!(
        units
            .iter()
            .flat_map(|unit| &unit.attachments)
            .all(|attachment| attachment.source.is_some() && attachment.error.is_none())
    );
}

#[test]
fn compiled_package_matches_public_schemas() {
    let (_temp, _source, assignments, _checksums) = markdown_fixture();
    let assignment: Value = read_json(&assignments).unwrap();
    let mut incomplete_docling = assignment;
    incomplete_docling["units"][0]["source_type"] = "docling-json".into();
    assert!(!matches_schema(
        "source-assignment.schema.json",
        &incomplete_docling
    ));
    let converted = json!({
        "schema_version": 1,
        "units": [{
            "unit_id": "markdown:converted",
            "source_type": "canonical-markdown",
            "locator": {"file": "notes.md", "line": 1},
            "metadata": {"heading": "Former locator value"}
        }]
    });
    assert!(matches_schema("source-assignment.schema.json", &converted));
    let mut unconverted = converted;
    unconverted["units"][0]["locator"]["heading"] = "Legacy field".into();
    assert!(!matches_schema(
        "source-assignment.schema.json",
        &unconverted
    ));
    let mut unit = json!({
        "schema_version": 2,
        "unit_id": "markdown:one",
        "source_type": "canonical-markdown",
        "source_locator": {"file": "notes.md", "line": 1},
        "metadata": {},
        "sources": [{"path": "notes.md", "sha256": "0".repeat(64), "bytes": 1}],
        "spans": [{
            "id": "s000001",
            "locator": "notes.md#line=1",
            "role": null,
            "timestamp": null,
            "text_sha256": "1".repeat(64),
            "text": "evidence"
        }],
        "attachments": [],
        "unit_sha256": "2".repeat(64)
    });
    assert!(matches_schema("evidence-unit.schema.json", &unit));
    let mut invalid_v2 = unit.clone();
    invalid_v2["source_type"] = "conversation-email".into();
    invalid_v2["source_locator"] = json!({"file":"mail.mbox","thread_id":"100"});
    invalid_v2["attachments"] = json!([{
        "id":"a000001",
        "span_id":"s000001",
        "locator":"mail.mbox#message=1;thread=100;part=2",
        "filename":null,
        "media_type":"text/html",
        "disposition":"attachment",
        "content_id":null,
        "source":{"path":"artifact","sha256":"0".repeat(64),"bytes":1},
        "error":ATTACHMENT_DECODE_ERROR
    }]);
    assert!(!matches_schema("evidence-unit.schema.json", &invalid_v2));
    unit["unexpected"] = true.into();
    assert!(!matches_schema("evidence-unit.schema.json", &unit));
    assert!(matches_schema(
        "package-inspection.schema.json",
        &serde_json::to_value(PackageInspection {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            units: 1,
            source_types: BTreeMap::from([("canonical-markdown".into(), 1)]),
            source_files: 1,
            spans: 3,
            attachments: 0,
            materialized_attachments: 0,
            unavailable_attachments: 0,
        })
        .unwrap()
    ));
}

#[test]
fn inspection_counts_shared_source_once() {
    let (temp, source, assignments, checksums) = markdown_fixture();
    let mut assignment: Value = read_json(&assignments).unwrap();
    assignment["units"].as_array_mut().unwrap().push(json!({
        "unit_id": "markdown:beta",
        "source_type": "canonical-markdown",
        "locator": {"file": "notes.md", "line": 9},
        "metadata": {"collection": "fictional"}
    }));
    write_private(
        &assignments,
        serde_json::to_vec(&assignment).unwrap().as_slice(),
    );
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let inspection = package_inspection(&load_package(&package).unwrap());
    assert_eq!(inspection.units, 2);
    assert_eq!(inspection.source_files, 1);
}

#[test]
fn canonical_evidence_unit_digest_vector_is_stable() {
    let source_locator = json!({"file": "notes/é.md", "line": 7});
    let metadata = BTreeMap::from([
        ("count".into(), json!(2)),
        (
            "nested".into(),
            json!({"enabled": true, "labels": ["α", "line\nbreak"]}),
        ),
    ]);
    let sources = [SourceFile {
        path: "notes/é.md".into(),
        sha256: "0".repeat(64),
        bytes: 12,
    }];
    let spans = [Span {
        id: "unit:é#span=1".into(),
        locator: "notes/é.md#line=7".into(),
        role: Some("heading".into()),
        timestamp: None,
        text_sha256: "1".repeat(64),
        text: "Café\n\u{1}".into(),
    }];
    let attachments = [];
    assert_eq!(
        digest(
            &serde_json::to_vec(&EvidenceUnitCore {
                schema_version: EVIDENCE_SCHEMA_VERSION,
                unit_id: "unit:é",
                source_type: "canonical-markdown",
                source_locator: &source_locator,
                metadata: &metadata,
                sources: &sources,
                spans: &spans,
                attachments: &attachments,
            })
            .unwrap()
        ),
        "a3cefec463d9c9e84e22149cff7daac493f4819e1c2019c1a0948663e997097c"
    );
}

#[test]
fn compilation_preserves_markdown_fences_and_large_spans() {
    let (temp, source, assignments, checksums) = markdown_fixture();
    let text = format!(
        "# Alpha\n\n```text\n# not a heading\n\nblank line preserved\n```\n\nAfter fence\n\n{}\n\n# Beta\n\nExcluded.\n",
        "x".repeat(70_000)
    );
    write_private(&source.join("notes.md"), text.as_bytes());
    write_private(
        &checksums,
        format!("{}  ./notes.md\n", digest(text.as_bytes())).as_bytes(),
    );
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let units = load_package(&package).unwrap();
    assert!(
        units[0]
            .spans
            .iter()
            .any(|span| span.text.len() > 64 * 1024)
    );
    assert!(units[0].spans.iter().any(|span| {
        span.text
            .contains("```text\n# not a heading\n\nblank line preserved\n```")
    }));
    assert!(
        units[0]
            .spans
            .iter()
            .all(|span| !span.text.contains("Excluded."))
    );
}

#[test]
fn compilation_rejects_generated_units_outside_the_contract() {
    let (temp, source, assignments, checksums) = markdown_fixture();
    let text = b"# Alpha\n\ninvalid\0evidence\n";
    write_private(&source.join("notes.md"), text);
    write_private(
        &checksums,
        format!("{}  ./notes.md\n", digest(text)).as_bytes(),
    );
    let package = temp.path().join("package");
    assert!(compile(&assignments, &source, &checksums, &package).is_err());
    assert!(!package.exists());
    assert!(!temp.path().join("package.staging").exists());
}
