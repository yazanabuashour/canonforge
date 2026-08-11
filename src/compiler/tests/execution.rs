use super::*;

mod pi;

#[test]
fn execution_frontend_excludes_private_platform_records() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "execution:one",
        "execution-history",
        &json!({"files":["history.jsonl"]}),
    );
    let history = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"fictional-session\"}}\n{\"timestamp\":\"fictional\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Preserve safe evidence\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"web_search_call\",\"call_id\":\"call-1\",\"query\":\"fictional lookup\",\"status\":\"completed\",\"developer_instructions\":\"PRIVATE TOOL FIELD\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"call_id\":\"call-2\",\"name\":\"fictional_tool\",\"arguments\":\"{}\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call-2\",\"output\":\"paired result\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"local_shell_call\",\"call_id\":\"call-3\",\"action\":\"fictional command\",\"status\":\"completed\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"local_shell_call_output\",\"call_id\":\"call-3\",\"output\":\"fictional shell result\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"tool_search_call\",\"call_id\":\"call-4\",\"arguments\":{\"query\":\"fictional tool\"},\"internal_chat_message_metadata_passthrough\":\"PRIVATE TOOL SEARCH STATE\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"tool_search_output\",\"call_id\":\"call-4\",\"tools\":[{\"name\":\"fictional_search\"}],\"internal_chat_message_metadata_passthrough\":\"PRIVATE TOOL SEARCH OUTPUT\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"mcp_tool_call_end\",\"call_id\":\"call-5\",\"invocation\":{\"tool\":\"fictional_mcp\"},\"result\":\"fictional MCP result\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"summary\":\"PRIVATE REASONING\",\"encrypted_content\":\"PRIVATE STATE\"}}\n{\"type\":\"compacted\",\"payload\":{\"message\":\"PRIVATE COMPACTION\",\"replacement_history\":[]}}\n{\"type\":\"world_state\",\"payload\":{\"full\":true,\"state\":\"PRIVATE WORLD STATE\"}}\n{\"type\":\"inter_agent_communication_metadata\",\"payload\":{\"trigger_turn\":{\"body\":\"PRIVATE INTER-AGENT STATE\"}}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"new_lifecycle\",\"developer_instructions\":\"PRIVATE INSTRUCTIONS\"}}\n";
    write_private(&source.join("history.jsonl"), history);
    write_private(
        &checksums,
        format!("{}  ./history.jsonl\n", digest(history)).as_bytes(),
    );
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let units = load_package(&package).unwrap();
    assert!(
        units[0]
            .spans
            .iter()
            .any(|span| span.text == "Preserve safe evidence")
    );
    assert!(units[0].spans.iter().any(|span| {
        span.role.as_deref() == Some("excluded-reasoning")
            && span.text == "{\"type\":\"reasoning\"}"
    }));
    assert!(
        units[0]
            .spans
            .iter()
            .any(|span| span.text.contains("fictional lookup"))
    );
    assert_eq!(
        units[0]
            .spans
            .iter()
            .filter(|span| span.text.contains("\"call_id\":\"call-2\""))
            .count(),
        2
    );
    assert_eq!(
        units[0]
            .spans
            .iter()
            .filter(|span| span.text.contains("\"call_id\":\"call-3\""))
            .count(),
        2
    );
    assert!(
        units[0]
            .spans
            .iter()
            .all(|span| !span.text.contains("PRIVATE"))
    );

    let header = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"fictional-session\"}}\n";
    for (unknown_record, output) in [
        (
            b"{\"type\":\"response_item\",\"payload\":{\"type\":\"mystery_tool_call\",\"input\":\"must not disappear\"}}\n"
                .as_slice(),
            "unknown-response-tool-package",
        ),
        (
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"mystery_tool_call\",\"input\":\"must not disappear\"}}\n"
                .as_slice(),
            "unknown-event-tool-package",
        ),
        (
            b"{\"type\":\"mystery_tool_call\",\"payload\":{\"input\":\"must not disappear\"}}\n"
                .as_slice(),
            "unknown-top-level-tool-package",
        ),
        (
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\"}}\n".as_slice(),
            "missing-event-message-package",
        ),
        (
            b"{\"type\":\"new_record\",\"developer_instructions\":\"must not disappear\"}\n"
                .as_slice(),
            "unknown-record-package",
        ),
        (
            b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_audio\",\"audio\":\"must not disappear\"}]}}\n"
                .as_slice(),
            "non-text-message-package",
        ),
    ] {
        let invalid_history = [header.as_slice(), unknown_record].concat();
        write_private(&source.join("history.jsonl"), &invalid_history);
        write_private(
            &checksums,
            format!("{}  ./history.jsonl\n", digest(&invalid_history)).as_bytes(),
        );
        assert!(
            compile(
                &assignments,
                &source,
                &checksums,
                &temp.path().join(output)
            )
            .is_err()
        );
    }
}

#[test]
fn codex_messages_preserve_text_and_mark_images_in_content_order() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "execution:codex-assets",
        "execution-history",
        &json!({"files":["history.jsonl"]}),
    );
    let history = jsonl(&[
        json!({"type":"session_meta","payload":{"id":"codex-assets"}}),
        json!({
            "timestamp": "fictional-time",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    {"type":"input_text","text":"Before image"},
                    {
                        "type":"input_image",
                        "image_url":"data:image/png;base64,NEVER_COPY_CODEX_IMAGE"
                    },
                    {"type":"input_text","text":"After image"}
                ]
            }
        }),
        json!({
            "type":"response_item",
            "payload":{
                "type":"agent_message",
                "author":"fictional-agent",
                "recipient":"fictional-recipient",
                "content":[
                    {"type":"input_text","text":"Delegate text"},
                    {"type":"encrypted_content","data":"NEVER_COPY_AGENT_STATE"}
                ]
            }
        }),
    ]);
    write_source(&source, &checksums, "history.jsonl", &history);
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let units = load_package(&package).unwrap();
    assert_eq!(units[0].spans[1].text, "Before image");
    assert_eq!(units[0].spans[2].role.as_deref(), Some("omitted-asset"));
    assert_eq!(units[0].spans[2].locator, "history.jsonl#line=2;content=2");
    assert_eq!(
        units[0].spans[2].text,
        "{\"kind\":\"image\",\"status\":\"not-materialized\"}"
    );
    assert_eq!(units[0].spans[3].text, "After image");
    assert_eq!(units[0].spans[4].text, "Delegate text");
    assert_eq!(
        units[0].spans[5].role.as_deref(),
        Some("excluded-platform-instruction")
    );
    let compiled = serde_json::to_string(&units).unwrap();
    assert!(compiled.contains("fictional-time"));
    assert!(!compiled.contains("data:image"));
    assert!(!compiled.contains("NEVER_COPY_AGENT_STATE"));
}

#[test]
fn codex_marks_structured_startup_context_without_inspecting_its_text() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "execution:codex-context",
        "execution-history",
        &json!({"files":["history.jsonl"]}),
    );
    let history = jsonl(&[
        json!({"type":"session_meta","payload":{"id":"codex-context"}}),
        json!({"type":"event_msg","payload":{"type":"task_started"}}),
        json!({
            "type":"response_item",
            "payload":{
                "type":"message",
                "role":"developer",
                "content":[{"type":"input_text","text":"private platform policy"}]
            }
        }),
        json!({
            "timestamp":"context-time",
            "type":"response_item",
            "payload":{
                "type":"message",
                "role":"user",
                "content":[{"type":"input_text","text":"arbitrary injected context"}]
            }
        }),
        json!({"type":"world_state","payload":{"full":true}}),
        json!({"type":"turn_context","payload":{"cwd":"/fictional"}}),
        json!({
            "timestamp":"user-time",
            "type":"response_item",
            "payload":{
                "type":"message",
                "role":"user",
                "content":[{"type":"input_text","text":"Human-authored request"}]
            }
        }),
    ]);
    write_source(&source, &checksums, "history.jsonl", &history);
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let units = load_package(&package).unwrap();
    let at = |locator: &str| {
        units[0]
            .spans
            .iter()
            .find(|span| span.locator == locator)
            .unwrap()
    };

    assert_eq!(
        at("history.jsonl#line=4;content=1").role.as_deref(),
        Some("excluded-platform-instruction")
    );
    assert_eq!(
        at("history.jsonl#line=4;content=1").text,
        EXCLUDED_PLATFORM_TEXT
    );
    assert_eq!(
        at("history.jsonl#line=7;content=1").role.as_deref(),
        Some("user")
    );
    assert!(
        !serde_json::to_string(&units)
            .unwrap()
            .contains("arbitrary injected context")
    );
}

#[test]
fn codex_marks_only_adjacent_exact_provider_mirrors() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "execution:codex-mirrors",
        "execution-history",
        &json!({"files":["history.jsonl"]}),
    );
    let history = jsonl(&[
        json!({"type":"session_meta","payload":{"id":"codex-mirrors"}}),
        json!({
            "timestamp":"user-time",
            "type":"response_item",
            "payload":{
                "type":"message","role":"user",
                "content":[{"type":"input_text","text":"Human-authored request"}]
            }
        }),
        json!({
            "timestamp":"user-time","type":"event_msg",
            "payload":{"type":"user_message","message":"Human-authored request"}
        }),
        json!({
            "timestamp":"assistant-time","type":"event_msg",
            "payload":{"type":"agent_message","message":"Assistant response"}
        }),
        json!({
            "timestamp":"assistant-time","type":"response_item",
            "payload":{
                "type":"message","role":"assistant",
                "content":[{"type":"output_text","text":"Assistant response"}]
            }
        }),
        json!({
            "timestamp":"first-time","type":"event_msg",
            "payload":{"type":"agent_message","message":"Same text, distinct events"}
        }),
        json!({
            "timestamp":"second-time","type":"response_item",
            "payload":{
                "type":"message","role":"assistant",
                "content":[{"type":"output_text","text":"Same text, distinct events"}]
            }
        }),
    ]);
    write_source(&source, &checksums, "history.jsonl", &history);
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let units = load_package(&package).unwrap();
    let at = |locator: &str| {
        units[0]
            .spans
            .iter()
            .find(|span| span.locator == locator)
            .unwrap()
    };

    for line in [3, 4] {
        assert_eq!(
            at(&format!("history.jsonl#line={line}")).role.as_deref(),
            Some("excluded-provider-mirror")
        );
    }
    assert_eq!(
        at("history.jsonl#line=2;content=1").role.as_deref(),
        Some("user")
    );
    assert_eq!(
        at("history.jsonl#line=5;content=1").role.as_deref(),
        Some("assistant")
    );
    assert_eq!(
        at("history.jsonl#line=6").role.as_deref(),
        Some("assistant")
    );
    assert_eq!(
        at("history.jsonl#line=7;content=1").role.as_deref(),
        Some("assistant")
    );
}
