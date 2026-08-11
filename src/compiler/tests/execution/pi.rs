use super::*;

fn pi_message_records() -> Vec<Value> {
    vec![
        json!({
            "type":"message", "id":"entry-user", "parentId":null, "timestamp":"time-2",
            "message":{"role":"user","content":[
                {"type":"text","text":"Pi user text"},
                {"type":"image","mimeType":"image/png","data":"NEVER_COPY_PI_IMAGE"}
            ]}
        }),
        json!({
            "type":"message", "id":"entry-assistant", "parentId":"entry-user",
            "timestamp":"time-3", "message":{"role":"assistant","content":[
                {"type":"text","text":"Pi assistant text"},
                {"type":"thinking","thinking":"NEVER_COPY_PI_THINKING"},
                {"type":"toolCall","id":"call-pi","name":"fictional_tool",
                 "arguments":{"query":"fictional"},"thoughtSignature":"NEVER_COPY_TOOL_REASONING"}
            ]}
        }),
        json!({
            "type":"message", "id":"entry-result", "parentId":"entry-assistant",
            "timestamp":"time-4", "message":{"role":"toolResult","toolCallId":"call-pi",
                "toolName":"fictional_tool","content":[{"type":"text","text":"Pi tool result"}],
                "isError":false}
        }),
    ]
}

fn pi_lifecycle_records() -> Vec<Value> {
    vec![
        json!({"type":"model_change","id":"model","parentId":"entry-result",
            "timestamp":"time-5","provider":"fictional","modelId":"model-one"}),
        json!({"type":"thinking_level_change","id":"level","parentId":"model",
            "timestamp":"time-6","thinkingLevel":"high"}),
        json!({"type":"session_info","id":"info","parentId":"level",
            "timestamp":"time-7","name":"Fictional session"}),
        json!({"type":"compaction","id":"compact","parentId":"info",
            "timestamp":"time-8","summary":"NEVER_COPY_COMPACTION_SUMMARY",
            "firstKeptEntryId":"entry-result","tokensBefore":1200}),
    ]
}

fn pi_custom_records() -> Vec<Value> {
    vec![
        json!({
            "type":"custom","id":"search","parentId":"compact","timestamp":"time-9",
            "customType":"web-search-results",
            "data":{"query":"fictional query","results":[{"title":"Synthetic result"}]}
        }),
        json!({
            "type":"custom_message","id":"recap","parentId":"search","timestamp":"time-10",
            "customType":"summary-recap","content":"NEVER_COPY_RECAP_BODY",
            "details":{"reasoning":"NEVER_COPY_RECAP_REASONING"}
        }),
        json!({
            "type":"custom","id":"btw","parentId":"recap","timestamp":"time-11",
            "customType":"btw-result", "data":{"status":"completed","title":"Aside",
                "answer":"Stable answer","errorText":"Synthetic aside error",
                "transient":"NEVER_COPY_UNSTABLE_BTW_FIELD"}
        }),
        json!({
            "type":"custom_message","id":"terminal","parentId":"btw","timestamp":"time-12",
            "customType":"background-terminal-result","content":"Terminal completed",
            "details":{"id":"terminal-job","status":"failed","title":"Terminal","exitCode":7,
                "signal":"TERM","transient":"NEVER_COPY_TERMINAL_TRANSIENT"}
        }),
        json!({
            "type":"custom_message","id":"subagent","parentId":"terminal","timestamp":"time-13",
            "customType":"subagent-result","content":"Delegate result",
            "details":{"id":"delegate-job","status":"completed","title":"Delegate"}
        }),
        json!({
            "type":"custom","id":"ready","parentId":"subagent","timestamp":"time-14",
            "customType":"web-search-content-ready","data":{"body":"NEVER_COPY_READY_BODY"}
        }),
    ]
}

fn pi_supported_history() -> Vec<u8> {
    let mut records = vec![json!({
        "type":"session", "version":3, "id":"pi-session", "timestamp":"time-1",
        "cwd":"/fictional/project"
    })];
    records.extend(pi_message_records());
    records.extend(pi_lifecycle_records());
    records.extend(pi_custom_records());
    jsonl(&records)
}

fn assert_pi_message_spans(spans: &[Span]) {
    let at = |locator: &str| spans.iter().find(|span| span.locator == locator).unwrap();
    assert_eq!(at("pi.jsonl#line=2;content=1").text, "Pi user text");
    assert_eq!(
        at("pi.jsonl#line=2;content=2").role.as_deref(),
        Some("omitted-asset")
    );
    assert_eq!(
        at("pi.jsonl#line=2;content=2").text,
        "{\"kind\":\"image\",\"mimeType\":\"image/png\",\"status\":\"not-materialized\"}"
    );
    assert_eq!(
        at("pi.jsonl#line=3;content=2").role.as_deref(),
        Some("excluded-reasoning")
    );
    assert_eq!(
        at("pi.jsonl#line=3;content=2").text,
        "{\"type\":\"thinking\"}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&at("pi.jsonl#line=3;content=3").text).unwrap(),
        json!({"id":"call-pi","name":"fictional_tool","arguments":{"query":"fictional"}})
    );
    assert_eq!(
        at("pi.jsonl#line=4;content=1").role.as_deref(),
        Some("tool-result")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&at("pi.jsonl#line=4;result").text).unwrap(),
        json!({"toolCallId":"call-pi","toolName":"fictional_tool","isError":false})
    );
}

fn assert_pi_metadata_spans(spans: &[Span]) {
    let at = |locator: &str| spans.iter().find(|span| span.locator == locator).unwrap();
    for line in [5, 6, 8, 14] {
        assert_eq!(
            at(&format!("pi.jsonl#line={line}")).role.as_deref(),
            Some("lifecycle")
        );
    }
    assert_eq!(at("pi.jsonl#line=7").role.as_deref(), Some("metadata"));
    for line in [9, 11] {
        assert_eq!(
            at(&format!("pi.jsonl#line={line}")).role.as_deref(),
            Some("tool-result")
        );
    }
    assert_eq!(at("pi.jsonl#line=12;content=1").text, "Terminal completed");
    assert_eq!(at("pi.jsonl#line=13;content=1").text, "Delegate result");
    assert_eq!(
        serde_json::from_str::<Value>(&at("pi.jsonl#line=12;result").text).unwrap(),
        json!({"type":"background-terminal-result","id":"terminal-job","status":"failed",
            "title":"Terminal","exitCode":7,"signal":"TERM"})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&at("pi.jsonl#line=13;result").text).unwrap(),
        json!({"type":"subagent-result","id":"delegate-job","status":"completed",
            "title":"Delegate","exitCode":null,"signal":null})
    );
    assert!(
        at("pi.jsonl#line=11")
            .text
            .contains("Synthetic aside error")
    );
    assert_eq!(
        at("pi.jsonl#line=10").role.as_deref(),
        Some("excluded-reasoning")
    );
}

fn assert_pi_private_fields_excluded(units: &[EvidenceUnit]) {
    let compiled = serde_json::to_string(units).unwrap();
    for excluded in [
        "NEVER_COPY_PI_IMAGE",
        "NEVER_COPY_PI_THINKING",
        "NEVER_COPY_TOOL_REASONING",
        "NEVER_COPY_COMPACTION_SUMMARY",
        "NEVER_COPY_RECAP_BODY",
        "NEVER_COPY_RECAP_REASONING",
        "NEVER_COPY_UNSTABLE_BTW_FIELD",
        "NEVER_COPY_READY_BODY",
        "NEVER_COPY_TERMINAL_TRANSIENT",
    ] {
        assert!(!compiled.contains(excluded));
    }
}

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
