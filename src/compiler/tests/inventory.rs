use super::*;

#[test]
fn conversation_inventory_emits_versioned_compiler_inputs() {
    let temp = tempfile::tempdir().unwrap();
    private_root(temp.path());
    let source = private_dir(temp.path(), "source");
    write_private(
        &source.join("chat.csv "),
        b"thread,time,speaker,message\nfictional,00:00,Ada,First\n",
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
