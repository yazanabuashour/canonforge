use super::super::*;

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
