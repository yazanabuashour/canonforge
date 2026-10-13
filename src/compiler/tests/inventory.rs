use super::*;

#[test]
fn conversation_inventory_emits_versioned_compiler_inputs() {
    let temp = tempfile::tempdir().unwrap();
    private_root(temp.path());
    let source = private_dir(temp.path(), "source");
    write_private(
        &source.join("chat.csv "),
        b"thread,time,speaker,message\nfictional,00:00,Ada,First\nother,00:01,Ben,Second\n",
    );
    let output = temp.path().join("inventory");
    inventory_conversation_tables(&source, &[PathBuf::from("chat.csv ")], None, &output).unwrap();
    let manifest: Value = read_json(&output.join("manifest.json")).unwrap();
    assert!(matches_schema(
        "conversation-inventory-manifest.schema.json",
        &manifest
    ));
    compile(
        &output.join("assignments.json"),
        &source,
        &output.join("SHA256SUMS"),
        &temp.path().join("inventory-package"),
    )
    .unwrap();

    let literal_file = r"alias\chat.csv ";
    let alias = private_dir(&source, "alias");
    write_private(
        &source.join(literal_file),
        b"thread,time,speaker,message\nfictional,00:00,Ada,Literal\n",
    );
    write_private(
        &alias.join("chat.csv "),
        b"thread,time,speaker,message\nsubstitute,00:00,Ben,Substitute\n",
    );
    let literal_output = temp.path().join("literal-inventory");
    inventory_conversation_tables(
        &source,
        &[PathBuf::from(literal_file)],
        None,
        &literal_output,
    )
    .unwrap();
    let manifest: Value = read_json(&literal_output.join("manifest.json")).unwrap();
    assert_eq!(manifest["source_files"][0]["path"], literal_file);
    let literal_package = temp.path().join("literal-package");
    compile(
        &literal_output.join("assignments.json"),
        &source,
        &literal_output.join("SHA256SUMS"),
        &literal_package,
    )
    .unwrap();
    let units = load_package(&literal_package).unwrap();
    assert!(units[0].spans.iter().any(|span| span.text == "Literal"));

    let selection_file = r"selection\rules.csv";
    let selection_alias = private_dir(&source, "selection");
    write_private(
        &source.join(selection_file),
        b"community,conversation_id\nchat,fictional\n",
    );
    write_private(
        &selection_alias.join("rules.csv"),
        b"community,conversation_id\nchat,other\n",
    );
    let selected_output = temp.path().join("selected-inventory");
    inventory_conversation_tables(
        &source,
        &[PathBuf::from("chat.csv ")],
        Some(Path::new(selection_file)),
        &selected_output,
    )
    .unwrap();
    let manifest: Value = read_json(&selected_output.join("manifest.json")).unwrap();
    assert_eq!(manifest["selection"]["path"], selection_file);
    let assignment: Value = read_json(&selected_output.join("assignments.json")).unwrap();
    assert_eq!(assignment["units"].as_array().unwrap().len(), 1);
    assert_eq!(
        assignment["units"][0]["locator"]["conversation_id"],
        "fictional"
    );
    compile(
        &selected_output.join("assignments.json"),
        &source,
        &selected_output.join("SHA256SUMS"),
        &temp.path().join("selected-package"),
    )
    .unwrap();

    write_private(
        &source.join("chat.csv "),
        b"thread,time,speaker,message\nfictional\0thread,00:00,Ada,Rejected\n",
    );
    let invalid_output = temp.path().join("invalid-inventory");
    assert!(
        inventory_conversation_tables(
            &source,
            &[PathBuf::from("chat.csv ")],
            None,
            &invalid_output
        )
        .is_err()
    );
    assert!(!invalid_output.exists());
}

#[test]
fn duplicate_unit_ids_fail_closed() {
    let (temp, source, assignments, checksums) = markdown_fixture();
    let mut value: Value = read_json(&assignments).unwrap();
    let duplicate = value["units"][0].clone();
    value["units"].as_array_mut().unwrap().push(duplicate);
    write_private(&assignments, serde_json::to_vec(&value).unwrap().as_slice());
    assert!(
        compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("package")
        )
        .is_err()
    );
    value["units"].as_array_mut().unwrap().truncate(1);
    value["units"][0]["unit_id"] = json!("");
    value["units"][0]["metadata"]["secret"] = json!("PRIVATE SCHEMA VALUE");
    write_private(&assignments, serde_json::to_vec(&value).unwrap().as_slice());
    let error = compile(
        &assignments,
        &source,
        &checksums,
        &temp.path().join("empty-id-package"),
    )
    .unwrap_err()
    .to_string();
    assert!(!error.contains("PRIVATE SCHEMA VALUE"));
    write_private(
        &assignments,
        br#"{"schema_version":1,"units":[{"unit_id":"markdown:alpha","unit_id":"markdown:other","source_type":"canonical-markdown","locator":{"file":"notes.md","line":1},"metadata":{}}]}"#,
    );
    assert!(
        compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("duplicate-member-package")
        )
        .is_err()
    );
    write_private(
        &assignments,
        br#"{"schema_version":1,"units":[{"unit_id":"markdown:alpha","source_type":"canonical-markdown","locator":{"file":"notes.md","line":1},"metadata":{"ambiguous":0.99999999999999999}}]}"#,
    );
    assert!(
        compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("fractional-number-package")
        )
        .is_err()
    );
    write_private(
        &assignments,
        br#"{"schema_version":1,"units":[{"unit_id":"markdown:alpha","source_type":"canonical-markdown","locator":{"file":"notes.md","line":9007199254740993.0},"metadata":{}}]}"#,
    );
    assert!(
        compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("inexact-integer-package")
        )
        .is_err()
    );
}
