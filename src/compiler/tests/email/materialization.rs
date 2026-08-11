use super::*;

#[test]
fn nested_multipart_attachment_materializes_only_leaf_parts() {
    let (temp, source, _assignments, checksums) = frontend_fixture(
        "email:300",
        "conversation-email",
        &json!({"file":"mail.mbox","thread_id":"300"}),
    );
    let mailbox = concat!(
        "From lin@example.test Tue Jan 04 00:00:00 2022\n",
        "X-GM-THRID: 300\n",
        "From: Lin <lin@example.test>\n",
        "Subject: Fictional nested multipart attachment\n",
        "Date: Tue, 4 Jan 2022 00:00:00 +0000\n",
        "MIME-Version: 1.0\n",
        "Content-Type: multipart/mixed; boundary=outer-fictional\n\n",
        "--outer-fictional\n",
        "Content-Type: text/plain; charset=utf-8\n\n",
        "Message body.\n",
        "--outer-fictional\n",
        "Content-Type: multipart/related; boundary=nested-fictional\n",
        "Content-Disposition: attachment; filename=bundle.mime\n\n",
        "--nested-fictional\n",
        "Content-Type: text/plain\n",
        "Content-Transfer-Encoding: quoted-printable\n\n",
        "Inherited=20attachment\n",
        "--nested-fictional\n",
        "Content-Type: image/jpeg\n",
        "Content-Disposition: inline; filename=preview.jpg\n",
        "Content-ID: <fictional-preview>\n",
        "Content-Transfer-Encoding: base64\n\n",
        "/9j/ZmljdGlvbmFs\n",
        "--nested-fictional--\n",
        "--outer-fictional--\n"
    );
    write_source(&source, &checksums, "mail.mbox", mailbox.as_bytes());
    let manifest_path = temp.path().join("nested-attachments.json");
    email_attachments::materialize(
        &source,
        Path::new("mail.mbox"),
        Path::new("_artifacts/sha256"),
        &manifest_path,
    )
    .unwrap();
    let manifest: Value = read_json(&manifest_path).unwrap();
    let parts = manifest["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(
        parts[0]["locator"],
        "mail.mbox#message=1;thread=300;part=2.1"
    );
    assert_eq!(parts[0]["disposition"], "attachment");
    assert_eq!(parts[0]["media_type"], "text/plain");
    assert!(parts[0]["filename"].is_null());
    assert_eq!(
        parts[1]["locator"],
        "mail.mbox#message=1;thread=300;part=2.2"
    );
    assert_eq!(parts[1]["disposition"], "inline");
    assert_eq!(parts[1]["filename"], "preview.jpg");
    assert_eq!(parts[1]["content_id"], "fictional-preview");

    let boundary = b"nested-fictional";
    for part in parts {
        let artifact = source.join(part["source"]["path"].as_str().unwrap());
        let bytes = fs::read(artifact).unwrap();
        assert!(
            !bytes
                .windows(boundary.len())
                .any(|window| window == boundary)
        );
    }
}

#[test]
fn leafless_multipart_attachment_fails_closed() {
    let (temp, source, _assignments, checksums) = frontend_fixture(
        "email:301",
        "conversation-email",
        &json!({"file":"mail.mbox","thread_id":"301"}),
    );
    let mailbox = concat!(
        "From noa@example.test Wed Jan 05 00:00:00 2022\n",
        "X-GM-THRID: 301\n",
        "From: Noa <noa@example.test>\n",
        "Subject: Fictional empty multipart attachment\n",
        "Date: Wed, 5 Jan 2022 00:00:00 +0000\n",
        "MIME-Version: 1.0\n",
        "Content-Type: multipart/mixed; boundary=outer-empty\n\n",
        "--outer-empty\n",
        "Content-Type: text/plain\n\n",
        "Message body.\n",
        "--outer-empty\n",
        "Content-Type: multipart/mixed; boundary=empty-fictional\n",
        "Content-Disposition: attachment; filename=empty-bundle.mime\n\n",
        "--empty-fictional--\n",
        "--outer-empty--\n"
    );
    write_source(&source, &checksums, "mail.mbox", mailbox.as_bytes());
    let error = email_attachments::materialize(
        &source,
        Path::new("mail.mbox"),
        Path::new("_artifacts/sha256"),
        &temp.path().join("leafless.json"),
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("classified multipart contains no leaf parts"));
    assert!(error.contains("mime_path=\"2\""));
    assert!(error.contains("disposition=attachment"));
    assert!(error.contains("filename=Some(\"empty-bundle.mime\")"));
}

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
        .collect::<HashSet<_>>();
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

fn assert_interrupted_materialization_cleanup(
    source: &Path,
    manifest_path: &Path,
    manifest: &Value,
) {
    let digest_path = manifest["parts"][0]["source"]["path"].as_str().unwrap();
    let staging_path = PathBuf::from(format!("{}.staging", source.join(digest_path).display()));
    write_private(&staging_path, b"interrupted staging");
    email_attachments::materialize(
        source,
        Path::new("mail.mbox"),
        Path::new("_artifacts/sha256"),
        manifest_path,
    )
    .unwrap();
    assert!(!staging_path.exists());
}

fn assert_changed_artifact_rejected(
    root: &Path,
    source: &Path,
    assignments: &Path,
    checksums: &Path,
    manifest_path: &Path,
    manifest: &Value,
) {
    let digest_path = manifest["parts"][0]["source"]["path"].as_str().unwrap();
    let changed = b"changed planned artifact";
    write_private(&source.join(digest_path), changed);
    let changed_digest = digest(changed);
    let updated_checksums = fs::read_to_string(checksums)
        .unwrap()
        .lines()
        .map(|line| {
            if line.ends_with(&format!("./{digest_path}")) {
                format!("{changed_digest}  ./{digest_path}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    write_private(checksums, format!("{updated_checksums}\n").as_bytes());
    assert!(
        compile_with_email_attachments(
            assignments,
            source,
            checksums,
            std::slice::from_ref(&manifest_path.to_path_buf()),
            &root.join("contradictory-receipts"),
        )
        .is_err()
    );
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
    assert_interrupted_materialization_cleanup(&source, &manifest_path, &manifest);
    assert_changed_artifact_rejected(
        temp.path(),
        &source,
        &assignments,
        &checksums,
        &manifest_path,
        &manifest,
    );
}

#[test]
fn email_attachment_artifact_failures_are_visible_and_leave_no_staging() {
    let (temp, source, assignments, checksums, manifest_path) = attachment_fixture();
    let manifest: Value = read_json(&manifest_path).unwrap();
    let artifact = source.join(manifest["parts"][0]["source"]["path"].as_str().unwrap());
    let saved = fs::read(&artifact).unwrap();
    fs::remove_file(&artifact).unwrap();
    assert!(
        compile_with_email_attachments(
            &assignments,
            &source,
            &checksums,
            std::slice::from_ref(&manifest_path),
            &temp.path().join("missing-package"),
        )
        .is_err()
    );
    write_private(&artifact, &saved);
    write_private(&artifact, b"tampered artifact bytes");
    let staging = PathBuf::from(format!("{}.staging", artifact.display()));
    write_private(&staging, b"interrupted staging");
    let error = email_attachments::materialize(
        &source,
        Path::new("mail.mbox"),
        Path::new("_artifacts/sha256"),
        &manifest_path,
    )
    .unwrap_err();
    let error = format!("{error:#}");
    for detail in [
        "source_path=\"mail.mbox\"",
        "message_ordinal=1",
        "thread_id=\"100\"",
        "mime_path=\"2\"",
        "media_type=\"application/pdf\"",
        "disposition=attachment",
        "filename=Some(\"first.pdf\")",
    ] {
        assert!(
            error.contains(detail),
            "missing failure detail {detail}: {error}"
        );
    }
    assert!(!staging.exists());
    assert!(
        compile_with_email_attachments(
            &assignments,
            &source,
            &checksums,
            std::slice::from_ref(&manifest_path),
            &temp.path().join("tampered-package"),
        )
        .is_err()
    );
}
