use std::collections::BTreeSet;

use super::super::*;

fn assert_attachment_manifest(source: &Path, manifest_path: &Path) -> Value {
    let before = fs::read(manifest_path).unwrap();
    email_attachments::materialize(
        source,
        Path::new("mail.mbox"),
        Path::new("_artifacts/sha256"),
        manifest_path,
    )
    .unwrap();
    assert_eq!(fs::read(manifest_path).unwrap(), before);
    let manifest: Value = read_json(manifest_path).unwrap();
    assert_eq!(manifest["summary"]["parsed_messages"], 3);
    assert_eq!(manifest["summary"]["attachment_occurrences"], 8);
    assert_eq!(manifest["summary"]["unique_blobs"], 6);
    assert_eq!(manifest["summary"]["by_disposition"]["attachment"], 7);
    assert_eq!(manifest["summary"]["by_disposition"]["inline"], 1);
    assert_eq!(
        manifest["parts"][1]["locator"],
        "mail.mbox#message=1;thread=100;part=3.2"
    );
    assert_eq!(manifest["parts"][1]["content_id"], "fictional-image");
    assert!(manifest["parts"][2]["filename"].is_null());
    assert_eq!(manifest["parts"][3]["media_type"], "text/plain");
    assert_ne!(
        manifest["parts"][0]["source"],
        manifest["parts"][5]["source"]
    );
    assert!(
        manifest["parts"][6]["filename"]
            .as_str()
            .unwrap()
            .contains("../unsafe/path-")
    );
    assert!(matches_schema(
        "email-attachment-manifest.schema.json",
        &manifest
    ));
    assert!(matches_schema(
        "email-attachment-receipt.schema.json",
        &manifest["summary"]
    ));
    assert_eq!(
        fs::metadata(manifest_path).unwrap().permissions().mode() & 0o077,
        0
    );
    let artifact_paths = manifest["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part["source"]["path"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    for relative in artifact_paths {
        let path = source.join(relative);
        assert!(path.is_file());
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
    assert!(!source.join("unsafe").exists());
    manifest
}

fn add_docling_attachment_source(
    source: &Path,
    assignments: &Path,
    checksums: &Path,
    manifest: &Value,
) {
    let original_file = manifest["parts"][0]["source"]["path"].as_str().unwrap();
    let document = serde_json::to_vec(&json!({
        "body":{"children":[{"$ref":"#/texts/0"}]},
        "furniture":{"children":[]},
        "texts":[{
            "self_ref":"#/texts/0","children":[],"label":"paragraph",
            "text":"Externally extracted fictional document"
        }]
    }))
    .unwrap();
    write_private(&source.join("document.json"), &document);
    let mut assignment: Value = read_json(assignments).unwrap();
    assignment["units"].as_array_mut().unwrap().push(json!({
        "unit_id":"docling:first-pdf",
        "source_type":"docling-json",
        "locator":{"file":"document.json","original_file":original_file},
        "metadata":{}
    }));
    write_private(assignments, &serde_json::to_vec(&assignment).unwrap());
    let checksums_text = fs::read_to_string(checksums).unwrap();
    write_private(
        checksums,
        format!(
            "{checksums_text}{}  ./document.json\n{}  ./{original_file}\n",
            digest(&document),
            manifest["parts"][0]["source"]["sha256"].as_str().unwrap()
        )
        .as_bytes(),
    );
}

fn compile_and_assert_attachment_package(
    root: &Path,
    source: &Path,
    assignments: &Path,
    checksums: &Path,
    manifest_path: &Path,
) {
    let package = root.join("package");
    assert!(
        compile(
            assignments,
            source,
            checksums,
            &root.join("missing-manifest")
        )
        .is_err()
    );
    compile_with_email_attachments(
        assignments,
        source,
        checksums,
        std::slice::from_ref(&manifest_path.to_path_buf()),
        &package,
    )
    .unwrap();
    let units = load_package(&package).unwrap();
    assert_eq!(units[0].attachments.len(), 5);
    assert_eq!(units[0].attachments[0].span_id, "s000001");
    assert_eq!(
        units[0].attachments[1].locator,
        "mail.mbox#message=1;thread=100;part=3.2"
    );
    assert_eq!(
        units[0].attachments[1].disposition,
        AttachmentDisposition::Inline
    );
    assert_eq!(
        units[0].attachments[1].content_id.as_deref(),
        Some("fictional-image")
    );
    assert_eq!(units[0].attachments[4].span_id, "s000002");
    assert_eq!(
        units[0].attachments[4].locator,
        "mail.mbox#message=3;thread=100;part=2"
    );
    assert_eq!(units[0].sources.len(), 5);
    assert_eq!(units[1].attachments.len(), 3);
    assert_eq!(
        units[0].attachments[0].source,
        units[1].attachments[0].source
    );
    assert_ne!(
        units[0].attachments[0].filename,
        units[1].attachments[0].filename
    );
    assert_eq!(
        units[0].attachments[0].source.as_ref(),
        Some(&units[2].sources[1])
    );
    validate(&package).unwrap();
}

#[test]
fn email_attachments_materialize_deduplicate_and_compile_exact_occurrences() {
    let (temp, source, assignments, checksums, manifest_path) = attachment_fixture();
    let manifest = assert_attachment_manifest(&source, &manifest_path);
    add_docling_attachment_source(&source, &assignments, &checksums, &manifest);
    compile_and_assert_attachment_package(
        temp.path(),
        &source,
        &assignments,
        &checksums,
        &manifest_path,
    );
    super::assert_interrupted_materialization_cleanup(&source, &manifest_path, &manifest);
    super::assert_changed_artifact_rejected(
        temp.path(),
        &source,
        &assignments,
        &checksums,
        &manifest_path,
        &manifest,
    );
}
