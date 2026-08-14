use super::*;

mod mime;
mod package;

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
fn email_attachment_manifest_rejects_reserved_output_names() {
    use std::os::fd::AsRawFd;

    let (temp, source, _assignments, checksums) = frontend_fixture(
        "email:100",
        "conversation-email",
        &json!({"file":"mail.mbox","thread_id":"100"}),
    );
    let mailbox = attachment_mailbox();
    write_source(&source, &checksums, "mail.mbox", &mailbox);
    let reserved_parent = private_dir(temp.path(), "reports.staging");
    let reserved_parent_handle = fs::File::open(&reserved_parent).unwrap();
    let manifest_paths = [
        temp.path().join("attachments.lock"),
        temp.path().join("attachments.LOCK"),
        temp.path().join("attachments.ſtaging"),
        temp.path().join("attachments.staging.owner.json"),
        reserved_parent.join("attachments.json"),
        PathBuf::from(format!(
            "/proc/self/fd/{}/attachments.json",
            reserved_parent_handle.as_raw_fd()
        )),
    ];
    let entry_names = |path: &Path| {
        fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<HashSet<_>>()
    };

    for output_manifest in manifest_paths {
        let manifest_parent = output_manifest.parent().unwrap();
        let manifest_parent_entries = entry_names(manifest_parent);
        let source_entries = entry_names(&source);

        let error = email_attachments::materialize(
            &source,
            Path::new("mail.mbox"),
            Path::new("_artifacts/sha256"),
            &output_manifest,
        )
        .unwrap_err();
        let error = format!("{error:#}");

        assert!(
            error.contains("output path uses a reserved lock or staging component"),
            "unexpected error for {}: {error}",
            output_manifest.display()
        );
        assert_eq!(
            entry_names(manifest_parent),
            manifest_parent_entries,
            "manifest temporary state remains for {}",
            output_manifest.display()
        );
        assert_eq!(
            entry_names(&source),
            source_entries,
            "artifact or blob state remains for {}",
            output_manifest.display()
        );
    }
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
