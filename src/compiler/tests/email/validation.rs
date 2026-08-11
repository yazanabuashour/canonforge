use super::*;

#[test]
fn malformed_selected_attachment_compiles_as_unavailable() {
    let (temp, source, assignments, checksums, manifest) = malformed_attachment_fixture();
    let package = temp.path().join("package");
    compile_with_email_attachments(
        &assignments,
        &source,
        &checksums,
        std::slice::from_ref(&manifest),
        &package,
    )
    .unwrap();
    compile_with_email_attachments(
        &assignments,
        &source,
        &checksums,
        std::slice::from_ref(&manifest),
        &package,
    )
    .unwrap();
    let units = load_package(&package).unwrap();
    let unit = &units[0];
    assert!(
        unit.spans
            .iter()
            .any(|span| span.text.contains("Searchable fictional parent text"))
    );
    assert_eq!(unit.sources.len(), 1);
    assert_eq!(unit.attachments.len(), 1);
    let attachment = &unit.attachments[0];
    assert_eq!(attachment.span_id, "s000001");
    assert_eq!(unit.spans[0].locator, "mail.mbox#message=1;thread=100");
    assert_eq!(attachment.locator, "mail.mbox#message=1;thread=100;part=2");
    assert!(attachment.filename.is_none());
    assert_eq!(attachment.media_type, "text/html");
    assert_eq!(attachment.disposition, AttachmentDisposition::Attachment);
    assert!(attachment.source.is_none());
    assert_eq!(attachment.error.as_deref(), Some(ATTACHMENT_DECODE_ERROR));
    let inspection = package_inspection(&units);
    assert_eq!(inspection.materialized_attachments, 0);
    assert_eq!(inspection.unavailable_attachments, 1);
}

#[test]
fn evidence_v3_rejects_invalid_attachment_availability() {
    let (temp, source, assignments, checksums, manifest) = malformed_attachment_fixture();
    let compile_package = |name: &str| {
        let package = temp.path().join(name);
        compile_with_email_attachments(
            &assignments,
            &source,
            &checksums,
            std::slice::from_ref(&manifest),
            &package,
        )
        .unwrap();
        package
    };

    let both = compile_package("both");
    rewrite_package_unit(&both, |unit| {
        unit.attachments[0].source = Some(unit.sources[0].clone());
    });
    assert!(validate(&both).is_err());

    let neither = compile_package("neither");
    rewrite_package_unit(&neither, |unit| {
        unit.attachments[0].error = None;
    });
    assert!(validate(&neither).is_err());

    let unknown = compile_package("unknown");
    rewrite_package_unit(&unknown, |unit| {
        unit.attachments[0].error = Some("fictional-error".into());
    });
    assert!(validate(&unknown).is_err());
}

#[test]
fn evidence_v3_rejects_invalid_attachment_relationships() {
    let (temp, source, assignments, checksums, manifest_path) = attachment_fixture();
    let compile_package = |name: &str| {
        let package = temp.path().join(name);
        compile_with_email_attachments(
            &assignments,
            &source,
            &checksums,
            std::slice::from_ref(&manifest_path),
            &package,
        )
        .unwrap();
        package
    };

    let duplicate = compile_package("duplicate-id");
    rewrite_package_unit(&duplicate, |unit| {
        let id = unit.attachments[0].id.clone();
        unit.attachments[1].id = id;
    });
    assert!(validate(&duplicate).is_err());

    let invalid_parent = compile_package("invalid-parent");
    rewrite_package_unit(&invalid_parent, |unit| {
        unit.attachments[0].span_id = "s999999".into();
    });
    assert!(validate(&invalid_parent).is_err());

    let missing_source = compile_package("missing-source");
    rewrite_package_unit(&missing_source, |unit| {
        unit.sources.pop();
    });
    assert!(validate(&missing_source).is_err());

    let mismatched_digest = compile_package("mismatched-digest");
    rewrite_package_unit(&mismatched_digest, |unit| {
        unit.attachments[0].source.as_mut().unwrap().sha256 = "0".repeat(64);
    });
    assert!(validate(&mismatched_digest).is_err());

    let unsafe_path = compile_package("unsafe-path");
    rewrite_package_unit(&unsafe_path, |unit| {
        unit.attachments[0].source.as_mut().unwrap().path = "../outside".into();
    });
    assert!(validate(&unsafe_path).is_err());

    let unknown = compile_package("unknown-field");
    let manifest: EvidencePackageManifest = read_json(&unknown.join("manifest.json")).unwrap();
    let unit_path = unknown.join(&manifest.units[0].path);
    let mut unit: Value = read_json(&unit_path).unwrap();
    unit["attachments"][0]["unexpected"] = true.into();
    write_private(&unit_path, &serde_json::to_vec(&unit).unwrap());
    assert!(validate(&unknown).is_err());
}
