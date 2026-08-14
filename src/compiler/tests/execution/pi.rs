use super::*;

mod fixtures;

use fixtures::{
    assert_pi_message_spans, assert_pi_metadata_spans, assert_pi_private_fields_excluded,
    pi_supported_history,
};

#[test]
fn pi_execution_history_projects_supported_records_without_media_or_reasoning() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "execution:pi",
        "execution-history",
        &json!({"files":["pi.jsonl"]}),
    );
    let history = pi_supported_history();
    write_source(&source, &checksums, "pi.jsonl", &history);
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let units = load_package(&package).unwrap();
    assert_pi_message_spans(&units[0].spans);
    assert_pi_metadata_spans(&units[0].spans);
    assert_pi_private_fields_excluded(&units);
}

#[test]
fn pi_execution_history_fails_closed_on_unknown_content_and_role_pairings() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "execution:pi-invalid",
        "execution-history",
        &json!({"files":["pi.jsonl"]}),
    );
    let header = json!({"type":"session","version":3,"id":"pi-invalid"});
    for (record, output) in [
        (
            json!({
                "type":"message","message":{"role":"user","content":[{
                    "type":"audio","data":"must not disappear"
                }]}
            }),
            "unknown-content",
        ),
        (
            json!({
                "type":"message","message":{"role":"user","content":[{
                    "type":"toolCall","id":"call","name":"tool","arguments":{}
                }]}
            }),
            "role-mismatch",
        ),
        (
            json!({"type":"custom","customType":"unknown-extension","data":{}}),
            "unknown-custom",
        ),
        (json!({"type":"unknown-record"}), "unknown-record"),
    ] {
        let history = jsonl(&[header.clone(), record]);
        write_source(&source, &checksums, "pi.jsonl", &history);
        assert!(compile(&assignments, &source, &checksums, &temp.path().join(output)).is_err());
    }
}

#[test]
fn pi_execution_history_preserves_assistant_transport_errors() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "execution:pi-error",
        "execution-history",
        &json!({"files":["pi.jsonl"]}),
    );
    let history = jsonl(&[
        json!({"type":"session","version":3,"id":"pi-error"}),
        json!({
            "type":"message",
            "timestamp":"fictional-time",
            "message":{
                "role":"assistant",
                "content":[],
                "stopReason":"error",
                "errorMessage":"Synthetic provider failure"
            }
        }),
    ]);
    write_source(&source, &checksums, "pi.jsonl", &history);
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let units = load_package(&package).unwrap();
    assert_eq!(units[0].spans[1].locator, "pi.jsonl#line=2;error");
    assert_eq!(units[0].spans[1].role.as_deref(), Some("assistant"));
    assert_eq!(units[0].spans[1].text, "Synthetic provider failure");
}

#[test]
fn execution_history_requires_a_consistent_recognized_header() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "execution:headers",
        "execution-history",
        &json!({"files":["first.jsonl","second.jsonl"]}),
    );
    let write_histories = |first: &[u8], second: &[u8]| {
        write_private(&source.join("first.jsonl"), first);
        write_private(&source.join("second.jsonl"), second);
        write_private(
            &checksums,
            format!(
                "{}  ./first.jsonl\n{}  ./second.jsonl\n",
                digest(first),
                digest(second)
            )
            .as_bytes(),
        );
    };
    let missing_header = jsonl(&[json!({
        "type":"event_msg","payload":{"type":"user_message","message":"text"}
    })]);
    write_histories(&missing_header, &missing_header);
    let error = compile(
        &assignments,
        &source,
        &checksums,
        &temp.path().join("missing-header"),
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("first.jsonl"));
    assert!(message.contains("event_msg"));

    let codex = jsonl(&[json!({
        "type":"session_meta","payload":{"id":"codex-session","session_id":"group-one"}
    })]);
    let pi = jsonl(&[json!({"type":"session","version":3,"id":"pi-session"})]);
    write_histories(&codex, &pi);
    assert!(
        compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("mixed-formats")
        )
        .is_err()
    );

    let inconsistent = jsonl(&[json!({
        "type":"session_meta",
        "payload":{"id":"delegate-session","session_id":"group-two"}
    })]);
    write_histories(&inconsistent, &codex);
    let error = compile(
        &assignments,
        &source,
        &checksums,
        &temp.path().join("inconsistent-identities"),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("inconsistent session identity"));
}
