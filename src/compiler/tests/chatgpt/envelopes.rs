use super::*;

#[test]
fn chatgpt_envelopes_preserve_dialogue_and_mark_explicit_exclusions() {
    let messages = [
        Value::Null,
        json!({"author":{"role":"system"},"content":{"parts":["Fictional platform secret"]}}),
        json!({"author":{"role":"user"},"content":{"content_type":"user_editable_context","user_instructions":"Fictional context secret"}}),
        json!({"author":{"role":"assistant"},"channel":"analysis","content":{"content_type":"thoughts","thoughts":[{"content":"Fictional thought secret"}]}}),
        json!({"author":{"role":"assistant"},"metadata":{"is_visually_hidden_from_conversation":true},"content":{"content_type":"reasoning_recap","content":"Fictional recap secret"}}),
        json!({"author":{"role":"user"},"channel":null,"metadata":{"is_visually_hidden_from_conversation":false},"content":{"parts":["Literal dialogue"]}}),
        json!({"author":{"role":"assistant"},"content":{"content_type":"code","language":"python","text":"print('fictional')"}}),
        json!({"author":{"role":"tool"},"content":{"content_type":"execution_output","text":"fictional"}}),
        json!({"author":{"role":"assistant"},"content":{"content_type":"text","parts":[""]}}),
    ];
    let (temp, source, assignments, checksums) = frontend_fixture(
        "chatgpt:envelopes",
        "conversation-chatgpt",
        &json!({"file":"chatgpt.json","conversation_id":"envelopes"}),
    );
    let mut mapping = serde_json::Map::new();
    let mut parent = Value::Null;
    for (index, message) in messages.iter().enumerate() {
        let id = format!("node-{index}");
        mapping.insert(id.clone(), json!({"parent":parent,"message":message}));
        parent = json!(id);
    }
    let document = json!([{"id":"envelopes","current_node":parent,"mapping":mapping}]);
    let compile_document = |document: &Value, name: &str| {
        write_source(
            &source,
            &checksums,
            "chatgpt.json",
            &serde_json::to_vec(document).unwrap(),
        );
        compile(&assignments, &source, &checksums, &temp.path().join(name))
    };
    compile_document(&document, "valid").unwrap();
    let units = load_package(&temp.path().join("valid")).unwrap();
    let spans = &units[0].spans;
    assert_eq!(spans.len(), messages.len());
    assert_eq!(
        spans[1].role.as_deref(),
        Some("excluded-platform-instruction")
    );
    assert_eq!(
        spans[2].role.as_deref(),
        Some("excluded-platform-instruction")
    );
    assert_eq!(spans[3].role.as_deref(), Some("excluded-reasoning"));
    assert_eq!(spans[4].role.as_deref(), Some("excluded-reasoning"));
    assert_eq!(
        spans[3].locator,
        "conversation=envelopes;node=node-3;message=unknown;content"
    );
    assert_eq!(spans[5].text, "Literal dialogue");
    assert_eq!(spans[6].text, "print('fictional')");
    assert_eq!(spans[7].text, "fictional");
    assert_eq!(spans[8].text, "");
    assert!(!serde_json::to_string(&units).unwrap().contains("secret"));

    // Keep represented siblings: one unsupported message must still reject the unit.
    for (index, message) in [
        json!("invalid envelope"),
        json!({"content":{"parts":["missing role"]}}),
        json!({"author":{"role":"future"},"content":{"parts":["unknown role"]}}),
        json!({"author":{"role":"user"}}),
        json!({"author":{"role":"user"},"content":null}),
        json!({"author":{"role":"user"},"content":{"result":"unsupported body"}}),
        json!({"author":{"role":"user"},"content":{"content_type":"future","text":"apparently supported body"}}),
        json!({"author":{"role":"user"},"content":{"content_type":null,"parts":["invalid type"]}}),
        json!({"author":{"role":"user"},"content":{"text":"one","parts":["two"]}}),
        json!({"author":{"role":"user"},"content":{"parts":[]}}),
        json!({"author":{"role":"user"},"content":{"text":"one","result":"unrepresented body"}}),
        json!({"author":{"role":"user"},"content":{"content_type":"thoughts","text":"wrong role"}}),
        json!({"author":{"role":"assistant"},"content":{"content_type":"user_editable_context"}}),
        json!({"author":{"role":"assistant"},"channel":"final","content":{"text":"unknown channel semantics"}}),
        json!({"author":{"role":"user"},"metadata":{"is_visually_hidden_from_conversation":true},"content":{"text":"ambiguous hidden message"}}),
        json!({"author":{"role":"user"},"metadata":{"is_user_system_message":true},"content":{"text":"ambiguous context"}}),
        json!({"author":{"role":"user"},"content":{"parts":[{"content_type":7,"text":"invalid discriminator"}]}}),
        json!({"author":{"role":"user"},"content":{"parts":[{"content_type":"text","type":"image_asset_pointer","text":"ambiguous discriminator"}]}}),
        json!({"author":{"role":"user"},"content":{"parts":[{"text":"one","result":"unrepresented part"}]}}),
    ].into_iter().enumerate() {
        let mut invalid = document.clone();
        invalid[0]["mapping"]["node-5"]["message"] = message;
        let output = format!("invalid-{index}");
        let error = compile_document(&invalid, &output).unwrap_err();
        assert!(format!("{error:#}").contains("invalid ChatGPT message at conversation=envelopes;node=node-5"), "{error:#}");
        assert!(!temp.path().join(output).exists());
    }
}
