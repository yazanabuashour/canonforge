use super::*;

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
