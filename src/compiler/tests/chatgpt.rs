use super::*;

#[test]
fn chatgpt_frontend_requires_one_unambiguous_ancestry() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "chatgpt:one",
        "conversation-chatgpt",
        &json!({"file":"chatgpt.json","conversation_id":"conversation-1"}),
    );
    let conversation = |parent| {
        json!({
            "id":"conversation-1",
            "current_node":"node-1",
            "mapping":{"node-1":{
                "parent":parent,
                "message":{"id":"message-1","author":{"role":"user"},"content":{"parts":["Fictional request"]}}
            }}
        })
    };
    let compile_document = |document: Value, output: &str| {
        let bytes = serde_json::to_vec(&document)?;
        write_private(&source.join("chatgpt.json"), &bytes);
        write_private(
            &checksums,
            format!("{}  ./chatgpt.json\n", digest(&bytes)).as_bytes(),
        );
        compile(&assignments, &source, &checksums, &temp.path().join(output))
    };
    compile_document(json!([conversation(Value::Null)]), "valid-package").unwrap();
    assert!(
        compile_document(
            json!([conversation(Value::Null), conversation(Value::Null)]),
            "duplicate-package"
        )
        .is_err()
    );
    assert!(compile_document(json!([conversation(json!(7))]), "invalid-parent-package").is_err());
}

#[test]
fn shared_chatgpt_source_is_read_and_parsed_once_without_reordering_units() {
    let temp = tempfile::tempdir().unwrap();
    private_root(temp.path());
    let source = private_dir(temp.path(), "source");
    let assignments = temp.path().join("assignments.json");
    write_private(
        &assignments,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "units": [
                {
                    "unit_id":"chatgpt:second","source_type":"conversation-chatgpt",
                    "locator":{"file":"chatgpt.json","conversation_id":"conversation-2"},
                    "metadata":{}
                },
                {
                    "unit_id":"chatgpt:first","source_type":"conversation-chatgpt",
                    "locator":{"file":"chatgpt.json","conversation_id":"conversation-1"},
                    "metadata":{}
                }
            ]
        }))
        .unwrap()
        .as_slice(),
    );
    let conversation = |id: &str, text: &str| {
        json!({
            "id":id,"current_node":"node",
            "mapping":{"node":{
                "parent":null,
                "message":{
                    "id":format!("message-{id}"),"author":{"role":"user"},
                    "content":{"parts":[text]}
                }
            }}
        })
    };
    let document = serde_json::to_vec(&json!([
        conversation("conversation-1", "First fictional conversation"),
        conversation("conversation-2", "Second fictional conversation")
    ]))
    .unwrap();
    let checksums = temp.path().join("SHA256SUMS");
    write_source(&source, &checksums, "chatgpt.json", &document);
    VERIFIED_SOURCE_READS.set(0);
    PARSED_SOURCE_PASSES.set(0);
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    assert_eq!(VERIFIED_SOURCE_READS.get(), 1);
    assert_eq!(PARSED_SOURCE_PASSES.get(), 1);
    let units = load_package(&package).unwrap();
    assert_eq!(units[0].unit_id, "chatgpt:second");
    assert_eq!(units[0].spans[1].text, "Second fictional conversation");
    assert_eq!(units[1].unit_id, "chatgpt:first");
    assert_eq!(units[1].spans[1].text, "First fictional conversation");
}

#[test]
fn shared_source_across_profiles_is_read_once_and_parsed_once_per_profile() {
    let temp = tempfile::tempdir().unwrap();
    private_root(temp.path());
    let source = private_dir(temp.path(), "source");
    let assignments = temp.path().join("assignments.json");
    write_private(
        &assignments,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "units": [
                {
                    "unit_id":"docling:shared","source_type":"docling-json",
                    "locator":{"file":"shared.jsonl","original_file":"original.bin"},
                    "metadata":{}
                },
                {
                    "unit_id":"execution:shared","source_type":"execution-history",
                    "locator":{"files":["shared.jsonl"]},"metadata":{}
                }
            ]
        }))
        .unwrap()
        .as_slice(),
    );
    let document = serde_json::to_vec(&json!({
        "type":"session_meta",
        "payload":{"id":"shared-fictional-session"},
        "body":{"children":[{"$ref":"#/texts/0"}]},
        "furniture":{"children":[]},
        "texts":[{
            "self_ref":"#/texts/0","children":[],"label":"paragraph",
            "text":"Shared fictional document"
        }]
    }))
    .unwrap();
    let original = b"fictional original bytes";
    write_private(&source.join("shared.jsonl"), &document);
    write_private(&source.join("original.bin"), original);
    let checksums = temp.path().join("SHA256SUMS");
    write_private(
        &checksums,
        format!(
            "{}  ./shared.jsonl\n{}  ./original.bin\n",
            digest(&document),
            digest(original)
        )
        .as_bytes(),
    );
    VERIFIED_SOURCE_READS.set(0);
    PARSED_SOURCE_PASSES.set(0);
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    assert_eq!(VERIFIED_SOURCE_READS.get(), 2);
    assert_eq!(PARSED_SOURCE_PASSES.get(), 2);
    let units = load_package(&package).unwrap();
    assert_eq!(units[0].unit_id, "docling:shared");
    assert_eq!(units[0].spans[0].text, "Shared fictional document");
    assert_eq!(units[1].unit_id, "execution:shared");
    assert_eq!(units[1].spans[0].role.as_deref(), Some("metadata"));
}

#[test]
fn chatgpt_frontend_marks_asset_parts_without_copying_pointers() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "chatgpt:assets",
        "conversation-chatgpt",
        &json!({"file":"chatgpt.json","conversation_id":"conversation-assets"}),
    );
    let conversation = |parts| {
        json!([{
            "id": "conversation-assets",
            "current_node": "node-assets",
            "mapping": {"node-assets": {
                "parent": null,
                "message": {
                    "id": "message-assets",
                    "author": {"role": "user"},
                    "content": {"parts": parts}
                }
            }}
        }])
    };
    let document = serde_json::to_vec(&conversation(json!([
        "Before image",
        {
            "content_type": "image_asset_pointer",
            "asset_pointer": "sediment://never-copy-this-pointer"
        },
        {"content_type": "text", "text": "After image"}
    ])))
    .unwrap();
    write_source(&source, &checksums, "chatgpt.json", &document);
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let units = load_package(&package).unwrap();
    assert_eq!(units[0].spans[1].text, "Before image");
    assert_eq!(units[0].spans[2].role.as_deref(), Some("omitted-asset"));
    assert_eq!(
        units[0].spans[2].locator,
        "conversation=conversation-assets;node=node-assets;message=message-assets;part=2"
    );
    assert_eq!(
        units[0].spans[2].text,
        "{\"kind\":\"image\",\"status\":\"not-materialized\"}"
    );
    assert_eq!(units[0].spans[3].text, "After image");
    assert!(
        !serde_json::to_string(&units)
            .unwrap()
            .contains("sediment://")
    );

    let unsupported = serde_json::to_vec(&conversation(json!([{
        "content_type": "audio_asset_pointer",
        "asset_pointer": "fictional-audio"
    }])))
    .unwrap();
    write_source(&source, &checksums, "chatgpt.json", &unsupported);
    assert!(
        compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("unsupported-package")
        )
        .is_err()
    );
}
