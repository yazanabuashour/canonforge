use super::*;

#[test]
fn docling_json_is_a_supported_frontend() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "docling:one",
        "docling-json",
        &json!({"file":"document.json","original_file":"document.pdf"}),
    );
    let document = serde_json::to_vec(&json!({
        "body": {
            "children": [
                {"$ref": "#/texts/0"},
                {"$ref": "#/tables/0"},
                {"$ref": "#/texts/1"}
            ]
        },
        "furniture": {"children": []},
        "texts": [
            {"self_ref": "#/texts/0", "children": [], "label": "paragraph", "text": "First text"},
            {"self_ref": "#/texts/1", "children": [], "label": "paragraph", "text": "Last text"}
        ],
        "tables": [{"self_ref": "#/tables/0", "children": [], "data": {"cells": ["A", "B"]}}]
    }))
    .unwrap();
    write_private(&source.join("document.json"), &document);
    let original = b"synthetic PDF placeholder";
    write_private(&source.join("document.pdf"), original);
    write_private(
        &checksums,
        format!(
            "{}  ./document.json\n{}  ./document.pdf\n",
            digest(&document),
            digest(original)
        )
        .as_bytes(),
    );
    let package = temp.path().join("package");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let units = load_package(&package).unwrap();
    assert_eq!(
        units[0]
            .sources
            .iter()
            .map(|source| source.path.as_str())
            .collect::<Vec<_>>(),
        ["document.json", "document.pdf"]
    );
    assert_eq!(units[0].spans.len(), 3);
    assert_eq!(units[0].spans[0].text, "First text");
    assert_eq!(
        serde_json::from_str::<Value>(&units[0].spans[1].text).unwrap(),
        json!({"self_ref": "#/tables/0", "children": [], "data": {"cells": ["A", "B"]}})
    );
    assert_eq!(units[0].spans[2].text, "Last text");

    let contradictory = serde_json::to_vec(&json!({
        "body": {"children": [{"$ref": "#/groups/0"}]},
        "furniture": {"children": []},
        "groups": [{
            "self_ref": "#/groups/other",
            "children": [{"$ref": "#/texts/0"}]
        }],
        "texts": [{
            "self_ref": "#/texts/0",
            "children": [],
            "text": "First text"
        }]
    }))
    .unwrap();
    let mut duplicate: Value = serde_json::from_slice(&document).unwrap();
    duplicate["body"]["children"][2] = json!({"$ref":"#/texts/0"});
    for (name, document, reason, reference) in [
        (
            "contradictory",
            contradictory,
            "Docling reference does not match self_ref",
            "#/groups/0",
        ),
        (
            "duplicate",
            serde_json::to_vec(&duplicate).unwrap(),
            "Docling reference cycle or duplicate",
            "#/texts/0",
        ),
    ] {
        write_private(&source.join("document.json"), &document);
        write_private(
            &checksums,
            format!(
                "{}  ./document.json\n{}  ./document.pdf\n",
                digest(&document),
                digest(original)
            )
            .as_bytes(),
        );
        let output = temp.path().join(format!("{name}-package"));
        let error = compile(&assignments, &source, &checksums, &output).unwrap_err();
        let error = format!("{error:#}");
        for detail in ["extract Docling source document.json", reason, reference] {
            assert!(error.contains(detail), "missing {detail}: {error}");
        }
        assert!(!error.contains("First text"));
        assert!(!output.exists());
    }
}

#[test]
fn docling_requires_top_level_references() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "docling:nested",
        "docling-json",
        &json!({"file":"document.json","original_file":"document.pdf"}),
    );
    let document = serde_json::to_vec(&json!({
        "body": {"children": [{"$ref": "#/texts/0/nested"}]},
        "furniture": {"children": []},
        "texts": [{
            "self_ref": "#/texts/0",
            "children": [],
            "text": "Top-level fictional text",
            "nested": {
                "self_ref": "#/texts/0/nested",
                "text": "Nested substitute"
            }
        }]
    }))
    .unwrap();
    let original = b"synthetic PDF placeholder";
    write_private(&source.join("document.json"), &document);
    write_private(&source.join("document.pdf"), original);
    write_private(
        &checksums,
        format!(
            "{}  ./document.json\n{}  ./document.pdf\n",
            digest(&document),
            digest(original)
        )
        .as_bytes(),
    );
    assert!(
        compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("package")
        )
        .is_err()
    );
}

#[test]
fn docling_original_must_be_a_distinct_file() {
    let (temp, source, assignments, checksums) = frontend_fixture(
        "docling:hardlink",
        "docling-json",
        &json!({"file":"document.json","original_file":"document.pdf"}),
    );
    let document = source.join("document.json");
    write_private(
        &document,
        serde_json::to_vec(&json!({
            "body":{"children":[{"$ref":"#/texts/0"}]},
            "texts":[{"self_ref":"#/texts/0","children":[],"text":"Synthetic"}]
        }))
        .unwrap()
        .as_slice(),
    );
    fs::hard_link(&document, source.join("document.pdf")).unwrap();
    let bytes = fs::read(&document).unwrap();
    write_private(
        &checksums,
        format!(
            "{}  ./document.json\n{}  ./document.pdf\n",
            digest(&bytes),
            digest(&bytes)
        )
        .as_bytes(),
    );
    assert!(
        compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("package")
        )
        .is_err()
    );
}
