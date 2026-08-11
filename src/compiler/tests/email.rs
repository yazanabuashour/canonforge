use super::*;

mod materialization;
mod validation;

#[test]
fn email_frontend_requires_complete_mbox_framing() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "email:42",
        "conversation-email",
        &json!({"file":"mail.mbox","thread_id":"42"}),
    );
    let mailbox = b"From ada@example.test Sat Jan 01 00:00:00 2022\nX-GM-THRID: 42\nFrom: Ada <ada@example.test>\nSubject: Fictional note\nDate: Sat, 1 Jan 2022 00:00:00 +0000\nContent-Type: text/plain; charset=utf-8\n\nR&D Complete&#847;&hairsp;&#8202;fictional 1 < 2<br>evidence.\nItem      Qty\n  coat     1\nFrom ada@example.test Sun Jan 02 00:00:00 2022\nX-GM-THRID: 42\nFrom: Ada <ada@example.test>\nSubject: Fictional HTML note\nDate: Sun, 2 Jan 2022 00:00:00 +0000\nContent-Type: text/html; charset=utf-8\n\n<p>Show &amp;lt;br&amp;gt;</p>\n";
    write_private(&source.join("mail.mbox"), mailbox);
    write_private(
        &checksums,
        format!("{}  ./mail.mbox\n", digest(mailbox)).as_bytes(),
    );
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let units = load_package(&package).unwrap();
    assert!(
        units[0].spans[0]
            .text
            .contains("R&D Complete fictional 1 < 2\nevidence.\nItem      Qty\n  coat     1")
    );
    assert!(units[0].spans[1].text.contains("Show &lt;br&gt;"));

    let epoch_mailbox = b"From ada@example.test Thu Jan 01 00:00:00 1970\nX-GM-THRID: 42\nFrom: Ada <ada@example.test>\nSubject: Fictional note\nDate: Thu, 1 Jan 1970 00:00:00 +0000\nContent-Type: text/plain; charset=utf-8\n\nEpoch evidence.\n";
    write_private(&source.join("mail.mbox"), epoch_mailbox);
    write_private(
        &checksums,
        format!("{}  ./mail.mbox\n", digest(epoch_mailbox)).as_bytes(),
    );
    compile(
        &assignments,
        &source,
        &checksums,
        &temp.path().join("epoch-package"),
    )
    .unwrap();

    let malformed = [b"unconsumed prefix\n".as_slice(), mailbox].concat();
    write_private(&source.join("mail.mbox"), &malformed);
    write_private(
        &checksums,
        format!("{}  ./mail.mbox\n", digest(&malformed)).as_bytes(),
    );
    assert!(
        compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("malformed-package")
        )
        .is_err()
    );

    let malformed_envelope = b"From ada@example.test not-a-valid-date\nX-GM-THRID: 42\nFrom: Ada <ada@example.test>\nSubject: Fictional note\nDate: Sat, 1 Jan 2022 00:00:00 +0000\nContent-Type: text/plain; charset=utf-8\n\nComplete fictional evidence.\n";
    write_private(&source.join("mail.mbox"), malformed_envelope);
    write_private(
        &checksums,
        format!("{}  ./mail.mbox\n", digest(malformed_envelope)).as_bytes(),
    );
    assert!(
        compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("malformed-envelope-package")
        )
        .is_err()
    );
}

fn attachment_mailbox() -> Vec<u8> {
    concat!(
        "From ada@example.test Sat Jan 01 00:00:00 2022\n",
        "X-GM-THRID: 100\n",
        "From: Ada <ada@example.test>\n",
        "Subject: Fictional multipart one\n",
        "Date: Sat, 1 Jan 2022 00:00:00 +0000\n",
        "MIME-Version: 1.0\n",
        "Content-Type: multipart/mixed; boundary=outer-one\n\n",
        "--outer-one\n",
        "Content-Type: text/plain; charset=utf-8\n\n",
        "First fictional message.\n",
        "--outer-one\n",
        "Content-Type: application/pdf\n",
        "Content-Disposition: attachment; filename=first.pdf\n",
        "Content-Transfer-Encoding: base64\n\n",
        "JVBERi1maWN0aW9uYWw=\n",
        "--outer-one\n",
        "Content-Type: multipart/related; boundary=inner-one\n\n",
        "--inner-one\n",
        "Content-Type: text/html; charset=utf-8\n\n",
        "<p>Inline fictional message.</p>\n",
        "--inner-one\n",
        "Content-Type: image/jpeg\n",
        "Content-Disposition: inline\n",
        "Content-ID: <fictional-image>\n",
        "Content-Transfer-Encoding: quoted-printable\n\n",
        "=FF=D8=FFfictional\n",
        "--inner-one\n",
        "Content-Type: application/octet-stream\n",
        "Content-Disposition: attachment\n",
        "Content-Transfer-Encoding: base64\n\n",
        "\n",
        "--inner-one\n",
        "Content-Type: text/plain\n",
        "Content-Disposition: attachment; filename=quoted.txt\n",
        "Content-Transfer-Encoding: quoted-printable\n\n",
        "same=20bytes\n",
        "--inner-one--\n",
        "--outer-one--\n",
        "From grace@example.test Sun Jan 02 00:00:00 2022\n",
        "X-GM-THRID: 200\n",
        "From: Grace <grace@example.test>\n",
        "Subject: Fictional multipart two\n",
        "Date: Sun, 2 Jan 2022 00:00:00 +0000\n",
        "MIME-Version: 1.0\n",
        "Content-Type: multipart/mixed; boundary=outer-two\n\n",
        "--outer-two\n",
        "Content-Type: text/plain; charset=utf-8\n\n",
        "Second fictional message.\n",
        "--outer-two\n",
        "Content-Type: application/pdf\n",
        "Content-Disposition: attachment; filename=renamed.pdf\n",
        "Content-Transfer-Encoding: base64\n\n",
        "JVBERi1maWN0aW9uYWw=\n",
        "--outer-two\n",
        "Content-Type: application/pdf\n",
        "Content-Disposition: attachment; filename=first.pdf\n",
        "Content-Transfer-Encoding: base64\n\n",
        "JVBERi1kaWZmZXJlbnQ=\n",
        "--outer-two\n",
        "Content-Type: application/octet-stream\n",
        "Content-Disposition: attachment; filename=\"../unsafe/path-\u{1}-雪.bin\"\n",
        "Content-Transfer-Encoding: base64\n\n",
        "c2FmZS1ieXRlcw==\n",
        "--outer-two--\n",
        "From ada@example.test Mon Jan 03 00:00:00 2022\n",
        "X-GM-THRID: 100\n",
        "From: Ada <ada@example.test>\n",
        "Subject: Fictional follow-up\n",
        "Date: Mon, 3 Jan 2022 00:00:00 +0000\n",
        "MIME-Version: 1.0\n",
        "Content-Type: multipart/mixed; boundary=outer-three\n\n",
        "--outer-three\n",
        "Content-Type: text/plain; charset=utf-8\n\n",
        "Follow-up in the first fictional thread.\n",
        "--outer-three\n",
        "Content-Type: application/pdf\n",
        "Content-Disposition: attachment; filename=third.pdf\n",
        "Content-Transfer-Encoding: base64\n\n",
        "JVBERi1maWN0aW9uYWw=\n",
        "--outer-three--\n"
    )
    .as_bytes()
    .to_vec()
}

pub(super) fn attachment_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "email:100",
        "conversation-email",
        &json!({"file":"mail.mbox","thread_id":"100"}),
    );
    let mailbox = attachment_mailbox();
    write_source(&source, &checksums, "mail.mbox", &mailbox);
    let mut assignment: Value = read_json(&assignments).unwrap();
    assignment["units"].as_array_mut().unwrap().push(json!({
        "unit_id":"email:200",
        "source_type":"conversation-email",
        "locator":{"file":"mail.mbox","thread_id":"200"},
        "metadata":{}
    }));
    write_private(&assignments, &serde_json::to_vec(&assignment).unwrap());
    let manifest = temp.path().join("attachments.json");
    email_attachments::materialize(
        &source,
        Path::new("mail.mbox"),
        Path::new("_artifacts/sha256"),
        &manifest,
    )
    .unwrap();
    (temp, source, assignments, checksums, manifest)
}

fn malformed_attachment_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "email:100",
        "conversation-email",
        &json!({"file":"mail.mbox","thread_id":"100"}),
    );
    let mailbox = b"From ada@example.test Sat Jan 01 00:00:00 2022\nX-GM-THRID: 100\nFrom: Ada <ada@example.test>\nSubject: Fictional malformed attachment\nDate: Sat, 1 Jan 2022 00:00:00 +0000\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=bad\n\n--bad\nContent-Type: text/plain\n\nSearchable fictional parent text.\n--bad\nContent-Type: text/html\nContent-Disposition: attachment\nContent-Transfer-Encoding: base64\n\n%%%not-base64%%%\n--bad--\n";
    write_source(&source, &checksums, "mail.mbox", mailbox);
    let manifest = temp.path().join("malformed.json");
    email_attachments::materialize(
        &source,
        Path::new("mail.mbox"),
        Path::new("_artifacts/sha256"),
        &manifest,
    )
    .unwrap();
    (temp, source, assignments, checksums, manifest)
}
