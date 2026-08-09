#![expect(
    clippy::unwrap_used,
    reason = "process-level regression tests fail immediately on fixture and UTF-8 errors"
)]

use std::process::Command;

fn canonforge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_canonforge"))
}

#[test]
fn help_version_and_failures_have_stable_process_contracts() {
    let version = canonforge().arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("canonforge {}\n", env!("CARGO_PKG_VERSION"))
    );

    let parse_error = canonforge().arg("not-a-command").output().unwrap();
    assert_eq!(parse_error.status.code(), Some(2));

    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("package");
    let domain_error = canonforge()
        .args(["compile", "--assignments", "missing.json", "--source-root"])
        .arg(directory.path())
        .args(["--checksums", "missing-sums", "--output"])
        .arg(&output)
        .output()
        .unwrap();
    assert_eq!(domain_error.status.code(), Some(1));
    assert!(!output.exists());
}

#[test]
fn help_exposes_only_compiler_commands() {
    let output = canonforge().arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in [
        "compile",
        "validate",
        "inspect",
        "inventory-conversation-tables",
        "materialize-email-attachments",
    ] {
        assert!(help.contains(command));
    }
    for removed in ["sqlite-build", "sqlite-query", "reranker", "authorization"] {
        assert!(!help.contains(removed));
    }
}
