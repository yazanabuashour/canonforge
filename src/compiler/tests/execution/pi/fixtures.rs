use super::super::*;

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

pub(super) fn pi_supported_history() -> Vec<u8> {
    let mut records = vec![json!({
        "type":"session", "version":3, "id":"pi-session", "timestamp":"time-1",
        "cwd":"/fictional/project"
    })];
    records.extend(pi_message_records());
    records.extend(pi_lifecycle_records());
    records.extend(pi_custom_records());
    jsonl(&records)
}

pub(super) fn assert_pi_message_spans(spans: &[Span]) {
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

pub(super) fn assert_pi_metadata_spans(spans: &[Span]) {
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

pub(super) fn assert_pi_private_fields_excluded(units: &[EvidenceUnit]) {
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
