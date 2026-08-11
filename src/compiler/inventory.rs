use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde_json::json;

use crate::protected_fs::{
    PrivateDirectory, ensure_output_separate, open_private_bound_directory, private_staging_writer,
    read_bound_private_file,
};

use super::{
    AssignedUnit, Assignment, CONVERSATION_INVENTORY_SCHEMA_VERSION, ConversationInventoryFile,
    ConversationInventoryManifest, ConversationSelectionFile, SOURCE_ASSIGNMENT_SCHEMA,
    SOURCE_ASSIGNMENT_SCHEMA_VERSION,
    compile_workflow::{safe_join, write_staging_json},
    conversation_table::{conversation_rows, encode_unit_component},
    json_support::{contract_validator, digest, validate_contract_value},
};

pub fn inventory_conversation_tables(
    source_root: &Path,
    files: &[PathBuf],
    selection_table: Option<&Path>,
    output: &Path,
) -> Result<()> {
    ensure_output_separate(output, &[(source_root, "source root")])?;
    ensure!(
        !files.is_empty(),
        "at least one conversation table is required"
    );
    let staging = PrivateDirectory::new(output)?;
    let source_root = open_private_bound_directory(source_root)?;
    let relative_files = normalized_source_paths(source_root.path(), files)?;
    let selection = load_selection(source_root.path(), selection_table)?;

    let mut units = Vec::new();
    let mut source_files = Vec::new();
    let mut selected = HashSet::new();
    for relative in relative_files {
        let inventory = inventory_source_file(
            source_root.path(),
            relative,
            selection.as_ref().map(|value| &value.selected),
        )?;
        selected.extend(inventory.selected);
        units.extend(inventory.units);
        source_files.push(inventory.file);
    }
    if let Some(expected) = &selection {
        ensure!(
            selected == expected.selected,
            "selection table contains conversations absent from the supplied source files"
        );
    }
    publish_inventory(staging, units, source_files, selection)
}

struct LoadedSelection {
    receipt: ConversationSelectionFile,
    selected: HashSet<(String, String)>,
}

struct SourceInventory {
    units: Vec<AssignedUnit>,
    file: ConversationInventoryFile,
    selected: HashSet<(String, String)>,
}

fn normalized_source_paths(source_root: &Path, files: &[PathBuf]) -> Result<Vec<String>> {
    let mut relative_files = files
        .iter()
        .map(|path| {
            let relative = path
                .to_str()
                .context("conversation table path must be UTF-8")?
                .replace('\\', "/");
            safe_join(source_root, &relative)?;
            Ok(relative)
        })
        .collect::<Result<Vec<_>>>()?;
    relative_files.sort();
    relative_files.dedup();
    ensure!(
        relative_files.len() == files.len(),
        "duplicate conversation table path"
    );
    Ok(relative_files)
}

fn load_selection(source_root: &Path, path: Option<&Path>) -> Result<Option<LoadedSelection>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let relative = path
        .to_str()
        .context("selection table path must be UTF-8")?
        .replace('\\', "/");
    let snapshot = read_bound_private_file(&safe_join(source_root, &relative)?)?;
    let selected = conversation_selection(&snapshot.bytes, &relative)?;
    let receipt = ConversationSelectionFile {
        path: relative,
        sha256: digest(&snapshot.bytes),
        bytes: u64::try_from(snapshot.bytes.len()).context("selection byte count overflow")?,
        selected_conversations: selected.len(),
    };
    Ok(Some(LoadedSelection { receipt, selected }))
}

fn inventory_source_file(
    source_root: &Path,
    relative: String,
    selection: Option<&HashSet<(String, String)>>,
) -> Result<SourceInventory> {
    let snapshot = read_bound_private_file(&safe_join(source_root, &relative)?)?;
    let rows = conversation_rows(&snapshot.bytes, &relative)?;
    let community = Path::new(&relative)
        .file_stem()
        .and_then(|value| value.to_str())
        .context("conversation table filename must have a UTF-8 stem")?;
    let threads = rows
        .iter()
        .map(|row| row.thread.as_str())
        .filter(|thread| {
            selection
                .is_none_or(|selected| selected.contains(&(community.into(), (*thread).into())))
        })
        .collect::<BTreeSet<_>>();
    ensure!(
        !threads.is_empty(),
        "conversation table contains no threads: {relative}"
    );
    let selected = threads
        .iter()
        .map(|thread| (community.to_owned(), (*thread).to_owned()))
        .collect::<HashSet<_>>();
    let units = threads
        .iter()
        .map(|thread| AssignedUnit {
            unit_id: format!(
                "conversation-table:{}:{}",
                encode_unit_component(relative.trim_end_matches(".csv")),
                encode_unit_component(thread)
            ),
            source_type: "conversation-table".into(),
            locator: json!({"file": relative, "conversation_id": thread}),
            metadata: BTreeMap::new(),
        })
        .collect();
    let file = ConversationInventoryFile {
        path: relative,
        sha256: digest(&snapshot.bytes),
        bytes: u64::try_from(snapshot.bytes.len()).context("source byte count overflow")?,
        conversations: threads.len(),
        messages: rows.len(),
    };
    Ok(SourceInventory {
        units,
        file,
        selected,
    })
}

fn publish_inventory(
    staging: PrivateDirectory,
    units: Vec<AssignedUnit>,
    source_files: Vec<ConversationInventoryFile>,
    selection: Option<LoadedSelection>,
) -> Result<()> {
    let mut unit_ids = HashSet::new();
    ensure!(
        units
            .iter()
            .all(|unit| unit_ids.insert(unit.unit_id.as_str())),
        "conversation table unit IDs collide"
    );
    let unit_count = units.len();
    let assignment = Assignment {
        schema_version: SOURCE_ASSIGNMENT_SCHEMA_VERSION,
        units,
    };
    validate_contract_value(
        &serde_json::to_value(&assignment)?,
        &contract_validator(SOURCE_ASSIGNMENT_SCHEMA)?,
        "generated source assignment",
    )?;
    write_staging_json(&staging.path().join("assignments.json"), &assignment)?;
    let manifest = ConversationInventoryManifest {
        schema_version: CONVERSATION_INVENTORY_SCHEMA_VERSION,
        source_type: "conversation-table".into(),
        source_files,
        selection: selection.map(|value| value.receipt),
        units: unit_count,
    };
    write_staging_json(&staging.path().join("manifest.json"), &manifest)?;
    write_inventory_checksums(staging.path(), &manifest)?;
    staging.finish()
}

fn write_inventory_checksums(path: &Path, manifest: &ConversationInventoryManifest) -> Result<()> {
    let mut sums = private_staging_writer(&path.join("SHA256SUMS"))?;
    for file in &manifest.source_files {
        writeln!(sums, "{}  ./{}", file.sha256, file.path)?;
    }
    if let Some(selection) = &manifest.selection {
        writeln!(sums, "{}  ./{}", selection.sha256, selection.path)?;
    }
    sums.finish()
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "CSV diagnostic row numbers are one-based indices over an in-memory file"
)]
fn conversation_selection(bytes: &[u8], label: &str) -> Result<HashSet<(String, String)>> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(bytes);
    let headers = reader
        .headers()
        .with_context(|| format!("read headers from {label}"))?
        .iter()
        .enumerate()
        .map(|(index, header)| {
            if index == 0 {
                header.trim_start_matches('\u{feff}').to_owned()
            } else {
                header.to_owned()
            }
        })
        .collect::<Vec<_>>();
    let conversation = headers
        .iter()
        .position(|header| header == "conversation_id")
        .context("selection table has no conversation_id column")?;
    let community = headers
        .iter()
        .position(|header| header == "community")
        .context("selection table has no community column")?;
    let mut selected = HashSet::new();
    for (index, record) in reader.records().enumerate() {
        let record = record.with_context(|| format!("read record {} from {label}", index + 2))?;
        let id = record
            .get(conversation)
            .context("selection record has no conversation_id")?;
        let community = record
            .get(community)
            .context("selection record has no community")?;
        ensure!(
            !id.is_empty()
                && !community.is_empty()
                && selected.insert((community.into(), id.into())),
            "selection record {} is empty or duplicated",
            index + 2
        );
    }
    ensure!(!selected.is_empty(), "selection table is empty");
    Ok(selected)
}
