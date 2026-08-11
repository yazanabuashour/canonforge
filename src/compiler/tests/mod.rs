use super::*;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};
use tempfile::TempDir;

use super::{
    json_support::digest,
    package::{load_package, package_inspection},
};
use crate::protected_fs::{PrivateDirectory, private_mode, read_bound_private_json as read_json};

mod chatgpt;
mod docling;
mod email;
mod execution;
mod inventory;
mod package;

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn private_root(path: &Path) {
    set_mode(path, 0o700);
}

fn private_dir(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    fs::create_dir(&path).unwrap();
    set_mode(&path, 0o700);
    path
}

fn write_private(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    private_mode(path).unwrap();
}

fn markdown_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    private_root(temp.path());
    let source = private_dir(temp.path(), "source");
    let markdown = source.join("notes.md");
    let text =
        b"# Alpha\n\nFictional evidence.\n\n## Detail\n\nMore evidence.\n\n# Beta\n\nExcluded.\n";
    write_private(&markdown, text);
    let assignments = temp.path().join("assignments.json");
    write_private(
        &assignments,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "units": [{
                "unit_id": "markdown:alpha",
                "source_type": "canonical-markdown",
                "locator": {"file": "notes.md", "line": 1},
                "metadata": {"collection": "fictional"}
            }]
        }))
        .unwrap()
        .as_slice(),
    );
    let checksums = temp.path().join("SHA256SUMS");
    write_private(
        &checksums,
        format!("{}  ./notes.md\n", digest(text)).as_bytes(),
    );
    (temp, source, assignments, checksums)
}

fn frontend_fixture(
    unit_id: &str,
    source_type: &str,
    locator: &Value,
) -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    private_root(temp.path());
    let source = private_dir(temp.path(), "source");
    let assignments = temp.path().join("assignments.json");
    write_private(
        &assignments,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "units": [{
                "unit_id": unit_id,
                "source_type": source_type,
                "locator": locator,
                "metadata": {}
            }]
        }))
        .unwrap()
        .as_slice(),
    );
    let checksums = temp.path().join("SHA256SUMS");
    (temp, source, assignments, checksums)
}

fn jsonl(records: &[Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, record).unwrap();
        bytes.push(b'\n');
    }
    bytes
}

fn write_source(source: &Path, checksums: &Path, relative: &str, bytes: &[u8]) {
    write_private(&source.join(relative), bytes);
    write_private(
        checksums,
        format!("{}  ./{relative}\n", digest(bytes)).as_bytes(),
    );
}

fn schema(name: &str) -> Value {
    serde_json::from_slice(
        &fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("skill/compile-knowledge/assets")
                .join(name),
        )
        .unwrap(),
    )
    .unwrap()
}

fn matches_schema(name: &str, value: &Value) -> bool {
    jsonschema::validator_for(&schema(name))
        .unwrap()
        .is_valid(value)
}

fn rewrite_package_unit(package: &Path, mutate: impl FnOnce(&mut EvidenceUnit)) {
    let mut manifest: EvidencePackageManifest = read_json(&package.join("manifest.json")).unwrap();
    let unit_path = package.join(&manifest.units[0].path);
    let mut unit: EvidenceUnit = read_json(&unit_path).unwrap();
    mutate(&mut unit);
    unit.unit_sha256 = digest(&unit.canonical_bytes().unwrap());
    manifest.units[0].unit_sha256.clone_from(&unit.unit_sha256);
    write_private(&unit_path, &serde_json::to_vec(&unit).unwrap());
    write_private(
        &package.join("manifest.json"),
        &serde_json::to_vec(&manifest).unwrap(),
    );
}

fn rewrite_package_schema_version(package: &Path, schema_version: u8) {
    let mut manifest: EvidencePackageManifest = read_json(&package.join("manifest.json")).unwrap();
    for entry in &mut manifest.units {
        let unit_path = package.join(&entry.path);
        let mut value: Value = read_json(&unit_path).unwrap();
        value["schema_version"] = schema_version.into();
        if schema_version == 1 {
            value.as_object_mut().unwrap().remove("attachments");
        }
        let unit: EvidenceUnit = serde_json::from_value(value.clone()).unwrap();
        let unit_sha256 = digest(&unit.canonical_bytes().unwrap());
        value["unit_sha256"] = unit_sha256.clone().into();
        entry.unit_sha256 = unit_sha256;
        write_private(&unit_path, &serde_json::to_vec(&value).unwrap());
    }
    manifest.schema_version = schema_version;
    write_private(
        &package.join("manifest.json"),
        &serde_json::to_vec(&manifest).unwrap(),
    );
}
