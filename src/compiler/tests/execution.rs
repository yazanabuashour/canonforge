use super::*;

mod codex;
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
