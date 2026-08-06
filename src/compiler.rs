use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{self, BufRead, BufReader, Cursor, Write},
    os::unix::fs::DirBuilderExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use mail_parser::{DateTime, Message, MessageParser, MimeHeaders, mailbox::mbox::MessageIterator};
use serde::{
    Deserialize, Serialize,
    de::{Error as _, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::protected_fs::{
    PrivateDirectory, digest_bound_private_file, ensure_output_separate,
    open_private_bound_directory, private_staging_writer, read_bound_private_file, sync_directory,
};
#[cfg(test)]
use crate::protected_fs::{private_mode, read_bound_private_json as read_json};

const SCHEMA_VERSION: u8 = 1;
const CONVERSATION_INVENTORY_SCHEMA_VERSION: u8 = 2;
const SOURCE_ASSIGNMENT_SCHEMA: &str =
    include_str!("../skill/compile-knowledge/assets/source-assignment.schema.json");
const PACKAGE_MANIFEST_SCHEMA: &str =
    include_str!("../skill/compile-knowledge/assets/evidence-package-manifest.schema.json");
const EVIDENCE_UNIT_SCHEMA: &str =
    include_str!("../skill/compile-knowledge/assets/evidence-unit.schema.json");

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Assignment {
    schema_version: u8,
    units: Vec<AssignedUnit>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssignedUnit {
    unit_id: String,
    source_type: String,
    locator: Value,
    metadata: BTreeMap<String, Value>,
}

#[derive(Serialize)]
struct ConversationInventoryManifest {
    schema_version: u8,
    source_type: String,
    source_files: Vec<ConversationInventoryFile>,
    selection: Option<ConversationSelectionFile>,
    units: usize,
}

#[derive(Serialize)]
struct ConversationInventoryFile {
    path: String,
    sha256: String,
    bytes: u64,
    conversations: usize,
    messages: usize,
}

#[derive(Serialize)]
struct ConversationSelectionFile {
    path: String,
    sha256: String,
    bytes: u64,
    selected_conversations: usize,
}

struct ConversationRow {
    record: usize,
    thread: String,
    time: String,
    speaker: String,
    message: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceFile {
    path: String,
    sha256: String,
    bytes: u64,
}

struct VerifiedSource {
    receipt: SourceFile,
    bytes: Vec<u8>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Span {
    id: String,
    locator: String,
    role: Option<String>,
    timestamp: Option<String>,
    text_sha256: String,
    text: String,
}

#[derive(Serialize)]
struct EvidenceUnitCore<'a> {
    schema_version: u8,
    unit_id: &'a str,
    source_type: &'a str,
    source_locator: &'a Value,
    metadata: &'a BTreeMap<String, Value>,
    sources: &'a [SourceFile],
    spans: &'a [Span],
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidenceUnit {
    schema_version: u8,
    unit_id: String,
    source_type: String,
    source_locator: Value,
    metadata: BTreeMap<String, Value>,
    sources: Vec<SourceFile>,
    spans: Vec<Span>,
    unit_sha256: String,
}

impl EvidenceUnit {
    fn core(&self) -> EvidenceUnitCore<'_> {
        EvidenceUnitCore {
            schema_version: self.schema_version,
            unit_id: &self.unit_id,
            source_type: &self.source_type,
            source_locator: &self.source_locator,
            metadata: &self.metadata,
            sources: &self.sources,
            spans: &self.spans,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidencePackageManifest {
    schema_version: u8,
    units: Vec<EvidencePackageEntry>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidencePackageEntry {
    unit_id: String,
    source_type: String,
    unit_sha256: String,
    path: String,
}

#[derive(Serialize)]
struct PackageInspection {
    schema_version: u8,
    units: usize,
    source_types: BTreeMap<String, usize>,
    source_files: usize,
    spans: usize,
}

fn write_staging_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut writer = private_staging_writer(path)?;
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.finish()
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    ensure!(
        !relative
            .bytes()
            .any(|byte| matches!(byte, 0 | b'\r' | b'\n')),
        "unsafe relative path: {relative:?}"
    );
    let relative = Path::new(relative);
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "unsafe relative path: {}",
        relative.display()
    );
    Ok(root.join(relative))
}

pub fn compile(
    assignments: &Path,
    source_root: &Path,
    checksums: &Path,
    output: &Path,
) -> Result<()> {
    ensure_output_separate(
        output,
        &[
            (assignments, "assignment"),
            (source_root, "source root"),
            (checksums, "checksum index"),
        ],
    )?;
    let staging = PrivateDirectory::new(output)?;
    let source_root = open_private_bound_directory(source_root)?;
    let assignment_validator = contract_validator(SOURCE_ASSIGNMENT_SCHEMA)?;
    let assignment: Assignment =
        read_validated_json(assignments, &assignment_validator, "source assignment")?;
    let checksum_index = checksum_index(checksums)?;
    let unit_validator = contract_validator(EVIDENCE_UNIT_SCHEMA)?;
    let units_directory = staging.path().join("units");
    fs::DirBuilder::new().mode(0o700).create(&units_directory)?;
    let mut entries = Vec::new();
    let mut unit_ids = HashSet::new();
    for unit in assignment.units {
        ensure!(
            unit_ids.insert(unit.unit_id.clone()),
            "duplicate assigned unit {}",
            unit.unit_id
        );
        let source_paths = source_paths(&unit.source_type, &unit.locator)?;
        let snapshots = verified_sources(
            &source_paths,
            &unit.source_type,
            source_root.path(),
            &checksum_index,
        )?;
        let spans = extract_spans(&unit, &snapshots)?;
        let sources = snapshots
            .into_iter()
            .map(|source| source.receipt)
            .collect::<Vec<_>>();
        ensure!(!spans.is_empty(), "unit {} produced no spans", unit.unit_id);
        let mut evidence = EvidenceUnit {
            schema_version: SCHEMA_VERSION,
            unit_id: unit.unit_id.clone(),
            source_type: unit.source_type.clone(),
            source_locator: unit.locator,
            metadata: unit.metadata,
            sources,
            spans,
            unit_sha256: String::new(),
        };
        evidence.unit_sha256 = digest(&serde_json::to_vec(&evidence.core())?);
        validate_contract_value(
            &serde_json::to_value(&evidence)?,
            &unit_validator,
            "evidence unit",
        )?;
        let path = format!("units/{}.json", digest(unit.unit_id.as_bytes()));
        write_staging_json(&staging.path().join(&path), &evidence)?;
        entries.push(EvidencePackageEntry {
            unit_id: unit.unit_id,
            source_type: unit.source_type,
            unit_sha256: evidence.unit_sha256,
            path,
        });
    }
    write_staging_json(
        &staging.path().join("manifest.json"),
        &EvidencePackageManifest {
            schema_version: SCHEMA_VERSION,
            units: entries,
        },
    )?;
    sync_directory(&units_directory)?;
    staging.finish()
}

pub fn validate(package: &Path) -> Result<()> {
    load_package(package).map(|_| ())
}

pub fn inspect(package: &Path) -> Result<()> {
    let units = load_package(package)?;
    let inspection = package_inspection(&units);
    serde_json::to_writer_pretty(io::stdout().lock(), &inspection)?;
    println!();
    Ok(())
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "counts cannot exceed the already-allocated evidence vectors they summarize"
)]
fn package_inspection(units: &[EvidenceUnit]) -> PackageInspection {
    let mut source_types = BTreeMap::new();
    let mut source_files = BTreeSet::new();
    let mut spans = 0_usize;
    for unit in units {
        *source_types.entry(unit.source_type.clone()).or_default() += 1;
        source_files.extend(
            unit.sources
                .iter()
                .map(|source| (&source.path, &source.sha256)),
        );
        spans += unit.spans.len();
    }
    PackageInspection {
        schema_version: SCHEMA_VERSION,
        units: units.len(),
        source_types,
        source_files: source_files.len(),
        spans,
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "conversation inventory is one ordered source receipt"
)]
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
    let mut relative_files = files
        .iter()
        .map(|path| {
            let relative = path
                .to_str()
                .context("conversation table path must be UTF-8")?
                .replace('\\', "/");
            safe_join(source_root.path(), &relative)?;
            Ok(relative)
        })
        .collect::<Result<Vec<_>>>()?;
    relative_files.sort();
    relative_files.dedup();
    ensure!(
        relative_files.len() == files.len(),
        "duplicate conversation table path"
    );
    let selection = if let Some(path) = selection_table {
        let relative = path
            .to_str()
            .context("selection table path must be UTF-8")?
            .replace('\\', "/");
        let path = safe_join(source_root.path(), &relative)?;
        let snapshot = read_bound_private_file(&path)?;
        let selected = conversation_selection(&snapshot.bytes, &relative)?;
        Some((relative, snapshot, selected))
    } else {
        None
    };

    let mut units = Vec::new();
    let mut source_files = Vec::new();
    let mut selected = HashSet::new();
    for relative in relative_files {
        let path = safe_join(source_root.path(), &relative)?;
        let snapshot = read_bound_private_file(&path)?;
        let rows = conversation_rows(&snapshot.bytes, &relative)?;
        let community = Path::new(&relative)
            .file_stem()
            .and_then(|value| value.to_str())
            .context("conversation table filename must have a UTF-8 stem")?;
        let threads = rows
            .iter()
            .map(|row| row.thread.as_str())
            .filter(|thread| {
                selection.as_ref().is_none_or(|(_, _, selected)| {
                    selected.contains(&(community.into(), (*thread).into()))
                })
            })
            .collect::<BTreeSet<_>>();
        ensure!(
            !threads.is_empty(),
            "conversation table contains no threads: {relative}"
        );
        for thread in &threads {
            selected.insert((community.to_owned(), (*thread).to_owned()));
            units.push(AssignedUnit {
                unit_id: format!(
                    "conversation-table:{}:{}",
                    encode_unit_component(relative.trim_end_matches(".csv")),
                    encode_unit_component(thread)
                ),
                source_type: "conversation-table".into(),
                locator: json!({"file": relative, "conversation_id": thread}),
                metadata: BTreeMap::new(),
            });
        }
        source_files.push(ConversationInventoryFile {
            path: relative,
            sha256: digest(&snapshot.bytes),
            bytes: u64::try_from(snapshot.bytes.len()).context("source byte count overflow")?,
            conversations: threads.len(),
            messages: rows.len(),
        });
    }
    if let Some((_, _, expected)) = &selection {
        ensure!(
            selected == *expected,
            "selection table contains conversations absent from the supplied source files"
        );
    }
    let mut unit_ids = HashSet::new();
    ensure!(
        units
            .iter()
            .all(|unit| unit_ids.insert(unit.unit_id.as_str())),
        "conversation table unit IDs collide"
    );
    let unit_count = units.len();
    let assignment = Assignment {
        schema_version: SCHEMA_VERSION,
        units,
    };
    validate_contract_value(
        &serde_json::to_value(&assignment)?,
        &contract_validator(SOURCE_ASSIGNMENT_SCHEMA)?,
        "generated source assignment",
    )?;
    write_staging_json(&staging.path().join("assignments.json"), &assignment)?;
    let selection_file = if let Some((relative, snapshot, selected)) = selection {
        Some(ConversationSelectionFile {
            path: relative,
            sha256: digest(&snapshot.bytes),
            bytes: u64::try_from(snapshot.bytes.len()).context("selection byte count overflow")?,
            selected_conversations: selected.len(),
        })
    } else {
        None
    };
    let manifest = ConversationInventoryManifest {
        schema_version: CONVERSATION_INVENTORY_SCHEMA_VERSION,
        source_type: "conversation-table".into(),
        source_files,
        selection: selection_file,
        units: unit_count,
    };
    write_staging_json(&staging.path().join("manifest.json"), &manifest)?;
    let mut sums = private_staging_writer(&staging.path().join("SHA256SUMS"))?;
    for file in &manifest.source_files {
        writeln!(sums, "{}  ./{}", file.sha256, file.path)?;
    }
    if let Some(selection) = &manifest.selection {
        writeln!(sums, "{}  ./{}", selection.sha256, selection.path)?;
    }
    sums.finish()?;
    staging.finish()
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

fn load_package(root: &Path) -> Result<Vec<EvidenceUnit>> {
    let package = open_private_bound_directory(root)?;
    let root = package.path();
    let members = fs::read_dir(root)?
        .map(|entry| {
            let entry = entry?;
            ensure!(
                entry.file_type()?.is_file() && entry.file_name() == "manifest.json"
                    || entry.file_type()?.is_dir() && entry.file_name() == "units",
                "unlisted or invalid evidence-package member: {}",
                entry.path().display()
            );
            Ok(entry.file_name())
        })
        .collect::<Result<HashSet<_>>>()?;
    ensure!(
        members == HashSet::from(["manifest.json".into(), "units".into()]),
        "evidence package must contain exactly manifest.json and units/"
    );
    let manifest_validator = contract_validator(PACKAGE_MANIFEST_SCHEMA)?;
    let manifest: EvidencePackageManifest = read_validated_json(
        &root.join("manifest.json"),
        &manifest_validator,
        "evidence-package manifest",
    )?;
    let units_directory = open_private_bound_directory(&root.join("units"))?;
    let actual_paths = fs::read_dir(units_directory.path())?
        .map(|entry| {
            let entry = entry?;
            ensure!(
                entry.file_type()?.is_file(),
                "evidence-package units directory contains a non-file: {}",
                entry.path().display()
            );
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("evidence-unit filename must be UTF-8"))?;
            Ok(format!("units/{name}"))
        })
        .collect::<Result<HashSet<_>>>()?;
    let expected_paths = manifest
        .units
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<HashSet<_>>();
    ensure!(
        actual_paths == expected_paths,
        "evidence-package unit files do not exactly match the manifest"
    );
    let unit_validator = contract_validator(EVIDENCE_UNIT_SCHEMA)?;
    let mut loaded = Vec::new();
    let mut ids = HashSet::new();
    for entry in &manifest.units {
        ensure!(ids.insert(entry.unit_id.clone()), "duplicate manifest unit");
        let path = safe_join(root, &entry.path)?;
        let unit: EvidenceUnit = read_validated_json(&path, &unit_validator, "evidence unit")?;
        validate_unit(entry, &unit)?;
        loaded.push(unit);
    }
    Ok(loaded)
}

fn validate_unit(entry: &EvidencePackageEntry, unit: &EvidenceUnit) -> Result<()> {
    ensure!(
        unit.unit_id == entry.unit_id
            && unit.source_type == entry.source_type
            && unit.unit_sha256 == entry.unit_sha256,
        "evidence-package manifest binding mismatch for {}",
        entry.unit_id
    );
    ensure!(
        digest(&serde_json::to_vec(&unit.core())?) == unit.unit_sha256,
        "evidence-unit checksum mismatch for {}",
        unit.unit_id
    );
    let expected_sources = source_paths(&unit.source_type, &unit.source_locator)?;
    ensure!(
        unit.sources
            .iter()
            .map(|source| source.path.as_str())
            .eq(expected_sources.iter().map(String::as_str)),
        "source receipts do not match the locator for {}",
        unit.unit_id
    );
    let mut span_ids = HashSet::new();
    for span in &unit.spans {
        ensure!(
            span_ids.insert(span.id.as_str()),
            "duplicate span ID in {}",
            unit.unit_id
        );
        ensure!(
            digest(span.text.as_bytes()) == span.text_sha256,
            "span checksum mismatch in {}",
            unit.unit_id
        );
    }
    Ok(())
}

fn source_paths(source_type: &str, locator: &Value) -> Result<Vec<String>> {
    let paths = match source_type {
        "canonical-markdown"
        | "conversation-chatgpt"
        | "conversation-email"
        | "conversation-table" => vec![locator_str(locator, "file")?.to_owned()],
        "docling-json" => {
            let document = locator_str(locator, "file")?;
            let original = locator_str(locator, "original_file")?;
            ensure!(
                document != original,
                "Docling JSON and original source must be different files"
            );
            vec![document.to_owned(), original.to_owned()]
        }
        "execution-history" => locator_strings(locator, "files")?
            .into_iter()
            .map(str::to_owned)
            .collect(),
        value => bail!("unsupported evidence source type {value}"),
    };
    Ok(paths)
}

fn verified_sources(
    paths: &[String],
    source_type: &str,
    source_root: &Path,
    checksums: &HashMap<String, String>,
) -> Result<Vec<VerifiedSource>> {
    let mut identities = HashSet::new();
    let mut snapshots = Vec::new();
    for (index, relative) in paths.iter().enumerate() {
        let path = safe_join(source_root, relative)?;
        if source_type == "docling-json" && index > 0 {
            let snapshot = digest_bound_private_file(&path)?;
            ensure!(
                identities.insert((snapshot.device, snapshot.inode)),
                "source paths resolve to the same file: {relative}"
            );
            ensure!(
                checksums.get(relative) == Some(&snapshot.sha256),
                "checksum mismatch or missing checksum for {relative}"
            );
            snapshots.push(VerifiedSource {
                receipt: SourceFile {
                    path: relative.clone(),
                    sha256: snapshot.sha256,
                    bytes: snapshot.bytes,
                },
                bytes: Vec::new(),
            });
            continue;
        }
        let snapshot = read_bound_private_file(&path)?;
        ensure!(
            identities.insert((snapshot.device, snapshot.inode)),
            "source paths resolve to the same file: {relative}"
        );
        let sha256 = digest(&snapshot.bytes);
        ensure!(
            checksums.get(relative) == Some(&sha256),
            "checksum mismatch or missing checksum for {relative}"
        );
        snapshots.push(VerifiedSource {
            receipt: SourceFile {
                path: relative.clone(),
                sha256,
                bytes: u64::try_from(snapshot.bytes.len()).context("source byte count overflow")?,
            },
            bytes: snapshot.bytes,
        });
    }
    Ok(snapshots)
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "checksum diagnostics use one-based line numbers over an in-memory file"
)]
fn checksum_index(path: &Path) -> Result<HashMap<String, String>> {
    let bytes = read_bound_private_file(path)?.bytes;
    let text = std::str::from_utf8(&bytes).context("checksum index must be UTF-8")?;
    let mut checksums = HashMap::new();
    for (index, line) in text.lines().enumerate() {
        let digest = line
            .get(..64)
            .with_context(|| format!("invalid checksum line {}", index + 1))?;
        let remainder = line
            .get(64..)
            .with_context(|| format!("invalid checksum line {}", index + 1))?;
        ensure!(
            remainder.starts_with(' ') || remainder.starts_with('\t'),
            "invalid checksum line {}",
            index + 1
        );
        let name = remainder.trim_start_matches([' ', '\t']);
        let name = name.strip_prefix("./").unwrap_or(name).to_owned();
        ensure!(
            digest.len() == 64
                && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                && !name.is_empty()
                && !name.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
                && checksums
                    .insert(name, digest.to_ascii_lowercase())
                    .is_none(),
            "invalid or duplicate checksum line {}",
            index + 1
        );
    }
    Ok(checksums)
}

#[expect(
    clippy::indexing_slicing,
    clippy::unreachable,
    reason = "source type and path cardinality are exhaustively validated before extraction"
)]
fn extract_spans(unit: &AssignedUnit, sources: &[VerifiedSource]) -> Result<Vec<Span>> {
    let raw = match unit.source_type.as_str() {
        "canonical-markdown" => markdown_spans(unit, &sources[0])?,
        "conversation-chatgpt" => chatgpt_spans(unit, &sources[0])?,
        "conversation-email" => email_spans(unit, &sources[0])?,
        "conversation-table" => conversation_table_spans(unit, &sources[0])?,
        "docling-json" => docling_spans(&sources[0])?,
        "execution-history" => execution_spans(sources)?,
        _ => unreachable!(),
    };
    raw.into_iter()
        .enumerate()
        .map(|(index, raw)| {
            Ok(Span {
                id: format!(
                    "s{:06}",
                    index.checked_add(1).context("span index overflow")?
                ),
                locator: raw.locator,
                role: raw.role,
                timestamp: raw.timestamp,
                text_sha256: digest(raw.text.as_bytes()),
                text: raw.text,
            })
        })
        .collect()
}

struct RawSpan {
    locator: String,
    role: Option<String>,
    timestamp: Option<String>,
    text: String,
}

struct RecordSpan {
    locator_suffix: String,
    role: Option<String>,
    timestamp: Option<String>,
    text: String,
}

const OMITTED_IMAGE_TEXT: &str = "{\"kind\":\"image\",\"status\":\"not-materialized\"}";
const EXCLUDED_PLATFORM_TEXT: &str = "[platform instruction body excluded from evidence view]";

#[expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "line byte offsets are derived from the same buffer and checked before slicing"
)]
fn markdown_spans(unit: &AssignedUnit, source: &VerifiedSource) -> Result<Vec<RawSpan>> {
    let text = std::str::from_utf8(&source.bytes).context("Markdown source must be UTF-8")?;
    let lines = text.lines().collect::<Vec<_>>();
    let start = locator_usize(&unit.locator, "line")?;
    ensure!(
        start > 0 && start <= lines.len(),
        "Markdown line is out of range"
    );
    let level = heading_level(lines[start - 1]).context("Markdown locator is not a heading")?;
    let mut end = lines.len();
    let mut fence = None;
    for (index, line) in lines.iter().enumerate().skip(start) {
        if let Some((marker, minimum)) = fence {
            if closes_fence(line, marker, minimum) {
                fence = None;
            }
            continue;
        }
        if let Some(opening) = opening_fence(line) {
            fence = Some(opening);
            continue;
        }
        if heading_level(line).is_some_and(|candidate| candidate <= level) {
            end = index;
            break;
        }
    }
    let mut spans = Vec::new();
    let mut block_start = start - 1;
    while block_start < end {
        while block_start < end && lines[block_start].trim().is_empty() {
            block_start += 1;
        }
        if block_start == end {
            break;
        }
        let mut block_end = block_start;
        let mut block_fence = None;
        while block_end < end {
            let line = lines[block_end];
            if let Some((marker, minimum)) = block_fence {
                if closes_fence(line, marker, minimum) {
                    block_fence = None;
                }
                block_end += 1;
                continue;
            }
            if let Some(opening) = opening_fence(line) {
                block_fence = Some(opening);
                block_end += 1;
                continue;
            }
            if line.trim().is_empty() {
                break;
            }
            block_end += 1;
        }
        spans.push(RawSpan {
            locator: format!(
                "{}#line={}-{}",
                source.receipt.path,
                block_start + 1,
                block_end
            ),
            role: Some("document".into()),
            timestamp: None,
            text: lines[block_start..block_end].join("\n"),
        });
        block_start = block_end;
    }
    Ok(spans)
}

fn heading_level(line: &str) -> Option<usize> {
    let count = line.bytes().take_while(|byte| *byte == b'#').count();
    (count > 0 && line.as_bytes().get(count) == Some(&b' ')).then_some(count)
}

fn fence_run(line: &str) -> Option<(u8, usize, &[u8])> {
    let bytes = line.as_bytes();
    let indentation = bytes.iter().take_while(|byte| **byte == b' ').count();
    if indentation > 3 {
        return None;
    }
    let content = bytes.get(indentation..)?;
    let marker = *content.first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let length = content.iter().take_while(|byte| **byte == marker).count();
    if length < 3 {
        return None;
    }
    Some((marker, length, content.get(length..)?))
}

fn opening_fence(line: &str) -> Option<(u8, usize)> {
    let (marker, length, suffix) = fence_run(line)?;
    (marker != b'`' || !suffix.contains(&b'`')).then_some((marker, length))
}

fn closes_fence(line: &str, marker: u8, minimum: usize) -> bool {
    fence_run(line).is_some_and(|(candidate, length, suffix)| {
        candidate == marker
            && length >= minimum
            && suffix.iter().all(|byte| matches!(byte, b' ' | b'\t'))
    })
}

fn conversation_table_spans(unit: &AssignedUnit, source: &VerifiedSource) -> Result<Vec<RawSpan>> {
    let conversation_id = locator_str(&unit.locator, "conversation_id")?;
    let rows = conversation_rows(&source.bytes, &source.receipt.path)?;
    let spans = rows
        .into_iter()
        .filter(|row| row.thread == conversation_id)
        .map(|row| RawSpan {
            locator: format!(
                "{}#record={};conversation_id={}",
                source.receipt.path,
                row.record,
                encode_unit_component(conversation_id)
            ),
            role: Some(if row.speaker.is_empty() {
                "unknown".into()
            } else {
                row.speaker
            }),
            timestamp: Some(format!("relative:{}", row.time)),
            text: row.message,
        })
        .collect::<Vec<_>>();
    ensure!(
        !spans.is_empty(),
        "conversation {conversation_id} was not found in {}",
        source.receipt.path
    );
    Ok(spans)
}

#[expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "header indices and one-based row numbers are validated before CSV field access"
)]
fn conversation_rows(bytes: &[u8], label: &str) -> Result<Vec<ConversationRow>> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(bytes);
    let headers = reader
        .headers()
        .with_context(|| format!("read headers from {label}"))?;
    let actual = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            if index == 0 {
                header.trim_start_matches('\u{feff}')
            } else {
                header
            }
        })
        .collect::<Vec<_>>();
    ensure!(
        actual == ["thread", "time", "speaker", "message"],
        "unsupported conversation table headers in {label}: expected thread,time,speaker,message"
    );
    let mut rows = Vec::new();
    for (index, record) in reader.records().enumerate() {
        let record = record.with_context(|| format!("read record {} from {label}", index + 2))?;
        ensure!(
            record.len() == 4,
            "record {} in {label} does not have four fields",
            index + 2
        );
        if record.iter().all(str::is_empty) {
            continue;
        }
        let thread = record[0].to_owned();
        ensure!(
            !thread.is_empty(),
            "record {} in {label} has no thread identity",
            index + 2
        );
        rows.push(ConversationRow {
            record: index + 2,
            thread,
            time: record[1].to_owned(),
            speaker: record[2].to_owned(),
            message: record[3].to_owned(),
        });
    }
    Ok(rows)
}

#[expect(
    clippy::as_conversions,
    clippy::format_push_string,
    reason = "percent encoding widens ASCII bytes to their numeric hex representation"
)]
fn encode_unit_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn chatgpt_spans(unit: &AssignedUnit, source: &VerifiedSource) -> Result<Vec<RawSpan>> {
    let document = parse_unique_json(&source.bytes, &source.receipt.path)?;
    let conversations = document
        .as_array()
        .context("ChatGPT export root must be an array")?;
    let conversation_id = locator_str(&unit.locator, "conversation_id")?;
    let mut matches = conversations.iter().filter(|value| {
        value.get("id").and_then(Value::as_str) == Some(conversation_id)
            || value.get("conversation_id").and_then(Value::as_str) == Some(conversation_id)
    });
    let conversation = matches
        .next()
        .with_context(|| format!("conversation {conversation_id} was not found"))?;
    ensure!(
        matches.next().is_none(),
        "conversation {conversation_id} is duplicated"
    );
    let mapping = conversation
        .get("mapping")
        .and_then(Value::as_object)
        .context("conversation mapping is missing")?;
    let mut chain = Vec::new();
    let mut cursor = Some(
        conversation
            .get("current_node")
            .and_then(Value::as_str)
            .context("conversation current_node is missing")?
            .to_owned(),
    );
    let mut seen = HashSet::new();
    while let Some(id) = cursor {
        ensure!(seen.insert(id.clone()), "conversation parent cycle");
        let node = mapping
            .get(&id)
            .with_context(|| format!("conversation node {id} is missing"))?;
        let parent = match node.get("parent") {
            None | Some(Value::Null) => None,
            Some(Value::String(parent)) => Some(parent.clone()),
            Some(_) => bail!("conversation node {id} has an invalid parent"),
        };
        chain.push((id, node));
        cursor = parent;
    }
    chain.reverse();
    let mut spans = vec![RawSpan {
        locator: format!("conversation={conversation_id}#metadata"),
        role: Some("metadata".into()),
        timestamp: conversation.get("update_time").map(scalar_text),
        text: format!(
            "Title: {}\nCreated: {}\nUpdated: {}",
            conversation
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("(untitled)"),
            conversation
                .get("create_time")
                .map(scalar_text)
                .unwrap_or_default(),
            conversation
                .get("update_time")
                .map(scalar_text)
                .unwrap_or_default()
        ),
    }];
    for (node_id, node) in chain {
        let Some(message) = node.get("message") else {
            continue;
        };
        let role = message
            .pointer("/author/role")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !matches!(role, "user" | "assistant" | "tool") {
            continue;
        }
        let locator = format!(
            "conversation={conversation_id};node={node_id};message={}",
            message
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
        for part in chatgpt_message_parts(message)? {
            spans.push(RawSpan {
                locator: format!("{locator}{}", part.locator_suffix),
                role: part.role,
                timestamp: part.timestamp,
                text: part.text,
            });
        }
    }
    ensure!(
        spans.len() > 1,
        "conversation {conversation_id} produced no user, assistant, or tool spans"
    );
    Ok(spans)
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "ChatGPT content part indices are finite one-based diagnostic positions"
)]
fn chatgpt_message_parts(message: &Value) -> Result<Vec<RecordSpan>> {
    let Some(content) = message.get("content") else {
        return Ok(Vec::new());
    };
    let role = message
        .pointer("/author/role")
        .and_then(Value::as_str)
        .context("ChatGPT message role is missing")?;
    let timestamp = message.get("create_time").map(scalar_text);
    let mut spans = Vec::new();
    match (content.get("text"), content.get("parts")) {
        (Some(_), Some(_)) => bail!("ChatGPT message content is ambiguous"),
        (Some(text), None) => spans.push(RecordSpan {
            locator_suffix: ";part=1".into(),
            role: Some(role.into()),
            timestamp,
            text: text
                .as_str()
                .context("ChatGPT message text is invalid")?
                .to_owned(),
        }),
        (None, Some(parts)) => {
            let parts = parts
                .as_array()
                .context("ChatGPT message parts are invalid")?;
            for (index, part) in parts.iter().enumerate() {
                let locator_suffix = format!(";part={}", index + 1);
                let (part_role, text) = match part {
                    Value::String(text) => (role, text.to_owned()),
                    Value::Object(object) => {
                        let part_type = object
                            .get("content_type")
                            .or_else(|| object.get("type"))
                            .and_then(Value::as_str);
                        if part_type == Some("image_asset_pointer") {
                            ("omitted-asset", OMITTED_IMAGE_TEXT.to_owned())
                        } else if matches!(part_type, None | Some("text")) {
                            (
                                role,
                                object
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .context("ChatGPT text part is missing text")?
                                    .to_owned(),
                            )
                        } else {
                            bail!("unsupported ChatGPT content part type {part_type:?}")
                        }
                    }
                    _ => bail!("unsupported ChatGPT content part at index {}", index + 1),
                };
                spans.push(RecordSpan {
                    locator_suffix,
                    role: Some(part_role.into()),
                    timestamp: timestamp.clone(),
                    text,
                });
            }
        }
        (None, None) => {}
    }
    Ok(spans)
}

const DOCLING_CONTENT_COLLECTIONS: [&str; 7] = [
    "texts",
    "tables",
    "pictures",
    "key_value_items",
    "form_items",
    "field_regions",
    "field_items",
];

fn docling_spans(source: &VerifiedSource) -> Result<Vec<RawSpan>> {
    let document = parse_unique_json(&source.bytes, &source.receipt.path)?;
    let object = document
        .as_object()
        .context("Docling JSON root must be an object")?;
    let mut spans = Vec::new();
    let mut seen = HashSet::new();
    let body = object.get("body").context("Docling body is missing")?;
    let mut references = Vec::new();
    if let Some(furniture) = object.get("furniture") {
        push_docling_children(furniture, &mut references)?;
    }
    push_docling_children(body, &mut references)?;
    while let Some(reference) = references.pop() {
        let pointer = reference
            .strip_prefix('#')
            .filter(|pointer| pointer.starts_with('/'))
            .context("Docling child reference is not a JSON pointer")?;
        let relative_pointer = pointer
            .strip_prefix('/')
            .context("Docling child reference is not a JSON pointer")?;
        let (collection, index) = relative_pointer
            .split_once('/')
            .context("Docling child reference must identify one top-level item")?;
        ensure!(
            !index.is_empty() && !index.contains('/'),
            "Docling child reference must identify one top-level item: {reference:?}"
        );
        let item = document
            .pointer(pointer)
            .with_context(|| format!("Docling child reference does not resolve: {reference:?}"))?;
        ensure!(
            seen.insert(reference.clone()),
            "Docling reference cycle or duplicate"
        );
        ensure!(
            item.get("self_ref").and_then(Value::as_str) == Some(reference.as_str()),
            "Docling reference does not match self_ref: {reference:?}"
        );
        if collection == "groups" {
            push_docling_children(item, &mut references)?;
            continue;
        }
        ensure!(
            DOCLING_CONTENT_COLLECTIONS.contains(&collection),
            "unsupported Docling body reference: {reference:?}"
        );
        let role = item.get("label").and_then(Value::as_str).map_or_else(
            || collection.trim_end_matches('s').to_owned(),
            str::to_owned,
        );
        let text = if collection == "texts" {
            item.get("text")
                .and_then(Value::as_str)
                .map_or_else(|| item.to_string(), str::to_owned)
        } else {
            item.to_string()
        };
        spans.push(RawSpan {
            locator: reference,
            role: Some(format!("docling:{role}")),
            timestamp: None,
            text,
        });
        if item.get("children").is_some() {
            push_docling_children(item, &mut references)?;
        }
    }
    let expected = DOCLING_CONTENT_COLLECTIONS
        .iter()
        .map(|collection| {
            object.get(*collection).map_or(Ok(0), |items| {
                items
                    .as_array()
                    .map(Vec::len)
                    .context("Docling content collection must be an array")
            })
        })
        .sum::<Result<usize>>()?;
    ensure!(
        spans.len() == expected,
        "Docling body and furniture do not reference every supported content item"
    );
    ensure!(
        !spans.is_empty(),
        "Docling document contains no supported content items: {}",
        source.receipt.path
    );
    Ok(spans)
}

fn push_docling_children(parent: &Value, stack: &mut Vec<String>) -> Result<()> {
    let children = parent
        .get("children")
        .and_then(Value::as_array)
        .context("Docling node children are missing or invalid")?;
    for child in children.iter().rev() {
        let reference = child
            .get("$ref")
            .and_then(Value::as_str)
            .context("Docling child reference is missing $ref")?;
        stack.push(reference.into());
    }
    Ok(())
}

#[expect(
    clippy::arithmetic_side_effects,
    clippy::format_push_string,
    reason = "message indices are bounded and the normalized body is assembled linearly"
)]
fn email_spans(unit: &AssignedUnit, source: &VerifiedSource) -> Result<Vec<RawSpan>> {
    let parser = MessageParser::default();
    let thread_id = locator_str(&unit.locator, "thread_id")?;
    let mut spans = Vec::new();
    ensure!(
        source.bytes.starts_with(b"From "),
        "MBOX does not begin with an envelope line: {}",
        source.receipt.path
    );
    let mut envelopes = source
        .bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| line.starts_with(b"From "));
    for (ordinal, raw_message) in MessageIterator::new(Cursor::new(&source.bytes)).enumerate() {
        let raw_message =
            raw_message.with_context(|| format!("parse MBOX envelope {}", source.receipt.path))?;
        let envelope = std::str::from_utf8(
            envelopes
                .next()
                .context("MBOX parser produced a message without an envelope")?,
        )
        .context("MBOX envelope is not UTF-8")?;
        let (sender, date) = envelope
            .strip_prefix("From ")
            .and_then(|value| value.split_once(' '))
            .context("MBOX envelope is missing its sender or date")?;
        let mut fields = date.split_whitespace();
        let parsed_date = match (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) {
            (Some(weekday), Some(month), Some(day), Some(time), Some(year)) => {
                DateTime::parse_rfc822(&format!("{weekday}, {day} {month} {year} {time} +0000"))
            }
            _ => None,
        };
        ensure!(
            !sender.trim().is_empty() && parsed_date.is_some_and(|value| value.is_valid()),
            "invalid MBOX envelope {} message {}",
            source.receipt.path,
            ordinal + 1
        );
        let message = parser
            .parse(raw_message.contents())
            .with_context(|| format!("parse {} message {}", source.receipt.path, ordinal + 1))?;
        if message
            .header("X-GM-THRID")
            .and_then(|value| value.as_text())
            .map(str::trim)
            != Some(thread_id)
        {
            continue;
        }
        let body = selected_body(&message).unwrap_or_default();
        let from = message
            .from()
            .and_then(|addresses| addresses.first())
            .and_then(|address| address.address.as_deref())
            .unwrap_or("unknown");
        let attachments = message
            .attachments
            .iter()
            .filter_map(|part| message.part(*part)?.attachment_name())
            .collect::<Vec<_>>();
        let mut text = format!(
            "Subject: {}\nFrom: {from}\nDate: {}",
            message.subject().unwrap_or("(no subject)"),
            message
                .date()
                .map(mail_parser::DateTime::to_rfc3339)
                .unwrap_or_default()
        );
        if !attachments.is_empty() {
            text.push_str(&format!("\nAttachments: {}", attachments.join(", ")));
        }
        if !body.trim().is_empty() {
            text.push_str("\n\n");
            text.push_str(body.trim());
        }
        spans.push(RawSpan {
            locator: format!(
                "{}#message={};thread={thread_id}",
                source.receipt.path,
                ordinal + 1
            ),
            role: Some(from.into()),
            timestamp: message.date().map(mail_parser::DateTime::to_rfc3339),
            text,
        });
    }
    ensure!(!spans.is_empty(), "Gmail thread {thread_id} was not found");
    Ok(spans)
}

fn selected_body(message: &Message<'_>) -> Option<String> {
    message
        .body_text(0)
        .map(std::borrow::Cow::into_owned)
        .or_else(|| {
            message
                .body_html(0)
                .map(|html| mail_parser::decoders::html::html_to_text(html.as_ref()))
        })
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "execution record indices are finite one-based diagnostic positions"
)]
fn execution_spans(sources: &[VerifiedSource]) -> Result<Vec<RawSpan>> {
    let mut spans = Vec::new();
    let mut unit_format: Option<ExecutionFormat> = None;
    let mut unit_session_identity = None;
    for source in sources {
        let reader = BufReader::new(Cursor::new(&source.bytes));
        let mut source_format = None;
        let mut pending_codex_dialogue = Vec::new();
        for (index, line) in reader.lines().enumerate() {
            let line = line?;
            let value = parse_unique_json(
                line.as_bytes(),
                &format!("{} line {}", source.receipt.path, index + 1),
            )?;
            let format = if index == 0 {
                let format = execution_format(&value, &source.receipt.path)?;
                if let Some(expected) = unit_format {
                    ensure!(
                        expected == format,
                        "mixed execution-history formats: expected {expected:?} but {} begins with {format:?}",
                        source.receipt.path,
                    );
                } else {
                    unit_format = Some(format);
                }
                let identity = session_identity(&value, format, &source.receipt.path)?;
                if let Some(expected) = &unit_session_identity {
                    ensure!(
                        expected == &identity,
                        "execution history {} has inconsistent session identity {identity:?}; expected {expected:?}",
                        source.receipt.path
                    );
                } else {
                    unit_session_identity = Some(identity);
                }
                source_format = Some(format);
                format
            } else {
                source_format.context("execution history is missing a session header")?
            };
            if format == ExecutionFormat::Codex && is_codex_dialogue(&value) {
                pending_codex_dialogue.push((index, value));
                continue;
            }
            let before_world_state = format == ExecutionFormat::Codex
                && value.get("type").and_then(Value::as_str) == Some("world_state");
            flush_codex_dialogue(
                &mut spans,
                &source.receipt.path,
                &mut pending_codex_dialogue,
                before_world_state,
            )?;
            append_execution_records(
                &mut spans,
                &source.receipt.path,
                index,
                match format {
                    ExecutionFormat::Codex => codex_record(&value, false, false),
                    ExecutionFormat::Pi => pi_record(&value),
                }?,
            );
        }
        flush_codex_dialogue(
            &mut spans,
            &source.receipt.path,
            &mut pending_codex_dialogue,
            false,
        )?;
        ensure!(
            source_format.is_some(),
            "execution history {} is missing a session header",
            source.receipt.path
        );
    }
    Ok(spans)
}

fn is_codex_dialogue(value: &Value) -> bool {
    let top = value.get("type").and_then(Value::as_str);
    let subtype = value
        .get("payload")
        .and_then(|payload| payload.get("type"))
        .and_then(Value::as_str);
    matches!(
        (top, subtype),
        (Some("response_item"), Some("message"))
            | (Some("event_msg"), Some("user_message" | "agent_message"))
    )
}

fn flush_codex_dialogue(
    spans: &mut Vec<RawSpan>,
    path: &str,
    pending: &mut Vec<(usize, Value)>,
    before_world_state: bool,
) -> Result<()> {
    let mut records = std::mem::take(pending).into_iter().peekable();
    while let Some((index, value)) = records.next() {
        let mirrored = records
            .peek()
            .is_some_and(|(_, next)| codex_records_are_mirrors(&value, next));
        append_execution_records(
            spans,
            path,
            index,
            codex_record(
                &value,
                before_world_state,
                mirrored && is_codex_event(&value),
            )?,
        );
        if mirrored {
            let (next_index, next) = records
                .next()
                .context("mirrored execution record disappeared")?;
            append_execution_records(
                spans,
                path,
                next_index,
                codex_record(&next, before_world_state, is_codex_event(&next))?,
            );
        }
    }
    Ok(())
}

fn is_codex_event(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("event_msg")
}

fn codex_records_are_mirrors(first: &Value, second: &Value) -> bool {
    let Some((first_role, first_text, first_timestamp, first_is_event)) =
        codex_dialogue_identity(first)
    else {
        return false;
    };
    let Some((second_role, second_text, second_timestamp, second_is_event)) =
        codex_dialogue_identity(second)
    else {
        return false;
    };
    first_is_event != second_is_event
        && first_role == second_role
        && first_text == second_text
        && first_timestamp.is_some()
        && first_timestamp == second_timestamp
}

fn codex_dialogue_identity(value: &Value) -> Option<(&str, &str, Option<&str>, bool)> {
    let top = value.get("type")?.as_str()?;
    let payload = value.get("payload")?;
    let subtype = payload.get("type")?.as_str()?;
    let timestamp = value.get("timestamp").and_then(Value::as_str);
    match (top, subtype) {
        ("event_msg", "user_message" | "agent_message") => Some((
            if subtype == "user_message" {
                "user"
            } else {
                "assistant"
            },
            payload.get("message")?.as_str()?,
            timestamp,
            true,
        )),
        ("response_item", "message") => {
            let role = payload.get("role")?.as_str()?;
            if !matches!(role, "user" | "assistant") {
                return None;
            }
            let content = payload.get("content")?.as_array()?;
            let [item] = content.as_slice() else {
                return None;
            };
            let item_type = item.get("type")?.as_str()?;
            if !matches!(item_type, "input_text" | "output_text") {
                return None;
            }
            Some((role, item.get("text")?.as_str()?, timestamp, false))
        }
        _ => None,
    }
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "execution record indices are finite zero-based source positions"
)]
fn append_execution_records(
    spans: &mut Vec<RawSpan>,
    path: &str,
    index: usize,
    records: Vec<RecordSpan>,
) {
    for record in records {
        spans.push(RawSpan {
            locator: format!("{path}#line={}{}", index + 1, record.locator_suffix),
            role: record.role,
            timestamp: record.timestamp,
            text: record.text,
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecutionFormat {
    Codex,
    Pi,
}

fn execution_format(value: &Value, path: &str) -> Result<ExecutionFormat> {
    let observed = value.get("type").and_then(Value::as_str);
    match observed {
        Some("session_meta") => Ok(ExecutionFormat::Codex),
        Some("session") => Ok(ExecutionFormat::Pi),
        _ => bail!(
            "execution history {path} must begin with a session_meta or session record; observed type {observed:?}"
        ),
    }
}

fn optional_string<'a>(
    value: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>> {
    value
        .get(key)
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("session identity {key} must be a string"))
        })
        .transpose()
}

fn session_identity(value: &Value, format: ExecutionFormat, path: &str) -> Result<String> {
    let identity = match format {
        ExecutionFormat::Codex => value
            .get("payload")
            .and_then(Value::as_object)
            .context("Codex session_meta payload is missing or invalid")?,
        ExecutionFormat::Pi => value
            .as_object()
            .context("Pi session header is not an object")?,
    };
    let id = optional_string(identity, "id")?;
    let session_id = optional_string(identity, "session_id")?;
    session_id
        .or(id)
        .map(str::to_owned)
        .with_context(|| format!("execution history {path} session header has no identity"))
}

fn record_span(role: &str, timestamp: Option<String>, text: String) -> Vec<RecordSpan> {
    vec![RecordSpan {
        locator_suffix: String::new(),
        role: Some(role.into()),
        timestamp,
        text,
    }]
}

fn codex_record(
    value: &Value,
    before_world_state: bool,
    mirrored_event: bool,
) -> Result<Vec<RecordSpan>> {
    let top = value.get("type").and_then(Value::as_str).unwrap_or("");
    let payload = value.get("payload").unwrap_or(&Value::Null);
    let subtype = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let timestamp = value.get("timestamp").map(scalar_text);
    Ok(match (top, subtype) {
        ("event_msg", "user_message" | "agent_message") => record_span(
            if mirrored_event {
                "excluded-provider-mirror"
            } else if subtype == "user_message" {
                "user"
            } else {
                "assistant"
            },
            timestamp,
            payload
                .get("message")
                .and_then(Value::as_str)
                .context("execution event message is missing or not a string")?
                .to_owned(),
        ),
        ("response_item", "message") => codex_message(payload, timestamp, before_world_state)?,
        ("response_item", "agent_message") => codex_agent_message(payload, timestamp.as_deref())?,
        ("response_item", "function_call" | "custom_tool_call" | "tool_search_call") => {
            record_span("tool-call", timestamp, tool_event_text(payload, subtype))
        }
        (
            "response_item",
            "function_call_output"
            | "custom_tool_call_output"
            | "web_search_call_output"
            | "computer_call_output"
            | "local_shell_call_output"
            | "mcp_tool_call_output"
            | "tool_search_output",
        )
        | ("event_msg", "mcp_tool_call_end") => {
            record_span("tool-result", timestamp, tool_event_text(payload, subtype))
        }
        (
            "response_item",
            "web_search_call" | "computer_call" | "local_shell_call" | "mcp_tool_call",
        ) => record_span("tool-event", timestamp, tool_event_text(payload, subtype)),
        ("response_item", "reasoning") => record_span(
            "excluded-reasoning",
            timestamp,
            json!({"type": subtype}).to_string(),
        ),
        ("compacted" | "world_state" | "inter_agent_communication_metadata", _)
        | ("event_msg", "token_count") => Vec::new(),
        _ if top.contains("call")
            || top.contains("tool")
            || subtype.contains("call")
            || subtype.contains("tool") =>
        {
            bail!("unsupported execution tool record type top={top:?} subtype={subtype:?}")
        }
        ("event_msg", _) if !subtype.is_empty() => {
            record_span("lifecycle", timestamp, lifecycle_text(payload, subtype))
        }
        ("session_meta", _) => record_span(
            "metadata",
            timestamp,
            json!({
                "id": payload.get("id"),
                "session_id": payload.get("session_id"),
                "cwd": payload.get("cwd"),
                "timestamp": payload.get("timestamp"),
                "source": payload.get("source"),
                "git": payload.get("git"),
            })
            .to_string(),
        ),
        ("turn_context", _) => record_span(
            "metadata",
            timestamp,
            json!({
                "cwd": payload.get("cwd"),
                "current_date": payload.get("current_date"),
                "model": payload.get("model"),
            })
            .to_string(),
        ),
        _ => bail!("unsupported execution record type top={top:?} subtype={subtype:?}"),
    })
}

fn tool_event_text(payload: &Value, subtype: &str) -> String {
    json!({
        "type": subtype,
        "id": payload.get("id"),
        "call_id": payload.get("call_id"),
        "name": payload.get("name"),
        "server": payload.get("server"),
        "tool": payload.get("tool"),
        "tools": payload.get("tools"),
        "status": payload.get("status"),
        "arguments": payload.get("arguments"),
        "invocation": payload.get("invocation"),
        "input": payload.get("input"),
        "action": payload.get("action"),
        "query": payload.get("query"),
        "result": payload.get("result"),
        "error": payload.get("error"),
        "output": payload.get("output"),
    })
    .to_string()
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "execution content indices are finite one-based diagnostic positions"
)]
fn codex_message(
    payload: &Value,
    timestamp: Option<String>,
    before_world_state: bool,
) -> Result<Vec<RecordSpan>> {
    let role = payload
        .get("role")
        .and_then(Value::as_str)
        .context("execution message role is missing or invalid")?;
    if matches!(role, "system" | "developer") {
        return Ok(record_span(
            "excluded-platform-instruction",
            timestamp,
            EXCLUDED_PLATFORM_TEXT.into(),
        ));
    }
    ensure!(
        matches!(role, "user" | "assistant"),
        "unsupported execution message role {role:?}"
    );
    let content = payload
        .get("content")
        .and_then(Value::as_array)
        .context("execution user or assistant message content is missing or invalid")?;
    if content.is_empty() {
        return Ok(record_span(
            role,
            timestamp,
            json!({"type": "message"}).to_string(),
        ));
    }
    content
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .context("execution message content type is missing or invalid")?;
            let (item_role, text) = match item_type {
                "input_text" | "output_text" if before_world_state && role == "user" => (
                    "excluded-platform-instruction",
                    EXCLUDED_PLATFORM_TEXT.to_owned(),
                ),
                "input_text" | "output_text" => (
                    role,
                    item.get("text")
                        .and_then(Value::as_str)
                        .context("execution text content is missing or invalid")?
                        .to_owned(),
                ),
                "input_image" => ("omitted-asset", OMITTED_IMAGE_TEXT.to_owned()),
                _ => bail!("unsupported execution message content type {item_type:?}"),
            };
            Ok(RecordSpan {
                locator_suffix: format!(";content={}", index + 1),
                role: Some(item_role.into()),
                timestamp: timestamp.clone(),
                text,
            })
        })
        .collect()
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "agent message content indices are finite one-based diagnostic positions"
)]
fn codex_agent_message(payload: &Value, timestamp: Option<&str>) -> Result<Vec<RecordSpan>> {
    let content = payload
        .get("content")
        .and_then(Value::as_array)
        .context("execution agent message content is missing or invalid")?;
    ensure!(
        !content.is_empty(),
        "execution agent message content is empty"
    );
    content
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .context("execution agent message content type is missing or invalid")?;
            let (role, text) = match item_type {
                "input_text" => (
                    "assistant",
                    item.get("text")
                        .and_then(Value::as_str)
                        .context("execution agent message text is missing or invalid")?
                        .to_owned(),
                ),
                "input_image" => ("omitted-asset", OMITTED_IMAGE_TEXT.to_owned()),
                "encrypted_content" => (
                    "excluded-platform-instruction",
                    json!({"type": item_type}).to_string(),
                ),
                _ => bail!("unsupported execution agent message content type {item_type:?}"),
            };
            Ok(RecordSpan {
                locator_suffix: format!(";content={}", index + 1),
                role: Some(role.into()),
                timestamp: timestamp.map(str::to_owned),
                text,
            })
        })
        .collect()
}

fn pi_record(value: &Value) -> Result<Vec<RecordSpan>> {
    let record_type = value
        .get("type")
        .and_then(Value::as_str)
        .context("Pi execution record type is missing or invalid")?;
    let timestamp = value.get("timestamp").map(scalar_text);
    Ok(match record_type {
        "session" => record_span(
            "metadata",
            timestamp,
            json!({
                "type": record_type,
                "version": value.get("version"),
                "id": value.get("id"),
                "cwd": value.get("cwd"),
                "parentSession": value.get("parentSession"),
            })
            .to_string(),
        ),
        "message" => pi_message(value, timestamp)?,
        "model_change" => record_span(
            "lifecycle",
            timestamp,
            json!({
                "type": record_type,
                "id": value.get("id"),
                "parentId": value.get("parentId"),
                "provider": value.get("provider"),
                "modelId": value.get("modelId"),
            })
            .to_string(),
        ),
        "thinking_level_change" => record_span(
            "lifecycle",
            timestamp,
            json!({
                "type": record_type,
                "id": value.get("id"),
                "parentId": value.get("parentId"),
                "thinkingLevel": value.get("thinkingLevel"),
            })
            .to_string(),
        ),
        "session_info" => record_span(
            "metadata",
            timestamp,
            json!({
                "type": record_type,
                "id": value.get("id"),
                "parentId": value.get("parentId"),
                "name": value.get("name"),
            })
            .to_string(),
        ),
        "compaction" => record_span(
            "lifecycle",
            timestamp,
            json!({
                "type": record_type,
                "id": value.get("id"),
                "parentId": value.get("parentId"),
                "firstKeptEntryId": value.get("firstKeptEntryId"),
                "tokensBefore": value.get("tokensBefore"),
            })
            .to_string(),
        ),
        "custom" | "custom_message" => pi_custom_record(value, timestamp)?,
        _ => bail!("unsupported Pi execution record type {record_type:?}"),
    })
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "Pi message content indices are finite one-based diagnostic positions"
)]
fn pi_message(value: &Value, timestamp: Option<String>) -> Result<Vec<RecordSpan>> {
    let message = value
        .get("message")
        .and_then(Value::as_object)
        .context("Pi message payload is missing or invalid")?;
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .context("Pi message role is missing or invalid")?;
    ensure!(
        matches!(role, "user" | "assistant" | "toolResult"),
        "unsupported Pi message role {role:?}"
    );
    let content = message
        .get("content")
        .context("Pi message content is missing")?;
    let mut spans = if role == "toolResult" {
        vec![RecordSpan {
            locator_suffix: ";result".into(),
            role: Some("tool-result".into()),
            timestamp: timestamp.clone(),
            text: json!({
                "toolCallId": message
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .context("Pi toolResult toolCallId is missing or invalid")?,
                "toolName": message
                    .get("toolName")
                    .and_then(Value::as_str)
                    .context("Pi toolResult toolName is missing or invalid")?,
                "isError": message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .context("Pi toolResult isError is missing or invalid")?,
            })
            .to_string(),
        }]
    } else {
        Vec::new()
    };
    if let Some(text) = content.as_str() {
        ensure!(role == "user", "Pi {role} message content must be an array");
        return Ok(vec![RecordSpan {
            locator_suffix: ";content=1".into(),
            role: Some("user".into()),
            timestamp,
            text: text.to_owned(),
        }]);
    }
    let items = content
        .as_array()
        .context("Pi message content is not a string or array")?;
    if items.is_empty() {
        ensure!(role == "assistant", "Pi {role} message content is empty");
        return Ok(vec![RecordSpan {
            locator_suffix: ";error".into(),
            role: Some("assistant".into()),
            timestamp,
            text: message
                .get("errorMessage")
                .and_then(Value::as_str)
                .context("Pi assistant message has empty content and no errorMessage")?
                .to_owned(),
        }]);
    }
    spans.extend(
        items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let (item_role, text) = pi_message_content(item, role)?;
                Ok(RecordSpan {
                    locator_suffix: format!(";content={}", index + 1),
                    role: Some(item_role.into()),
                    timestamp: timestamp.clone(),
                    text,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    );
    Ok(spans)
}

fn pi_message_content(item: &Value, role: &str) -> Result<(&'static str, String)> {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .context("Pi message content type is missing or invalid")?;
    match item_type {
        "text" => Ok((
            match role {
                "user" => "user",
                "assistant" => "assistant",
                "toolResult" => "tool-result",
                _ => bail!("unsupported Pi message role {role:?}"),
            },
            item.get("text")
                .and_then(Value::as_str)
                .context("Pi text content is missing or invalid")?
                .to_owned(),
        )),
        "image" => {
            ensure!(
                matches!(role, "user" | "toolResult"),
                "Pi image content is invalid for role {role:?}"
            );
            let mime_type = item
                .get("mimeType")
                .and_then(Value::as_str)
                .context("Pi image content mimeType is missing or invalid")?;
            Ok((
                "omitted-asset",
                json!({
                    "kind": "image",
                    "mimeType": mime_type,
                    "status": "not-materialized",
                })
                .to_string(),
            ))
        }
        "thinking" => {
            ensure!(
                role == "assistant",
                "Pi thinking content is invalid for role {role:?}"
            );
            Ok((
                "excluded-reasoning",
                json!({"type": "thinking"}).to_string(),
            ))
        }
        "toolCall" => {
            ensure!(
                role == "assistant",
                "Pi toolCall content is invalid for role {role:?}"
            );
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .context("Pi toolCall id is missing or invalid")?;
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .context("Pi toolCall name is missing or invalid")?;
            let arguments = item
                .get("arguments")
                .context("Pi toolCall arguments are missing")?;
            Ok((
                "tool-call",
                json!({"id": id, "name": name, "arguments": arguments}).to_string(),
            ))
        }
        _ => bail!("unsupported Pi message content type {item_type:?}"),
    }
}

fn pi_custom_field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .get(key)
        .or_else(|| value.get("data").and_then(|data| data.get(key)))
        .or_else(|| value.get("details").and_then(|details| details.get(key)))
}

fn pi_custom_record(value: &Value, timestamp: Option<String>) -> Result<Vec<RecordSpan>> {
    let custom_type = value
        .get("customType")
        .and_then(Value::as_str)
        .context("Pi custom record customType is missing or invalid")?;
    Ok(match custom_type {
        "summary-recap" => record_span(
            "excluded-reasoning",
            timestamp,
            json!({"type": custom_type}).to_string(),
        ),
        "web-search-content-ready" => record_span(
            "lifecycle",
            timestamp,
            json!({"type": custom_type}).to_string(),
        ),
        "btw-result" => record_span(
            "tool-result",
            timestamp,
            json!({
                "type": custom_type,
                "status": pi_custom_field(value, "status"),
                "title": pi_custom_field(value, "title"),
                "answer": pi_custom_field(value, "answer"),
                "error": pi_custom_field(value, "error")
                    .or_else(|| pi_custom_field(value, "errorText")),
            })
            .to_string(),
        ),
        "web-search-results" => pi_custom_result(value, timestamp)?,
        "background-terminal-result" | "subagent-result" => {
            let details = value
                .get("details")
                .and_then(Value::as_object)
                .context("Pi custom message result details are missing or invalid")?;
            let mut spans = vec![RecordSpan {
                locator_suffix: ";result".into(),
                role: Some("tool-result".into()),
                timestamp: timestamp.clone(),
                text: json!({
                    "type": custom_type,
                    "id": details.get("id"),
                    "status": details.get("status"),
                    "title": details.get("title"),
                    "exitCode": details.get("exitCode"),
                    "signal": details.get("signal"),
                })
                .to_string(),
            }];
            spans.extend(pi_custom_result(value, timestamp)?);
            spans
        }
        _ => bail!("unsupported Pi custom record type {custom_type:?}"),
    })
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "Pi custom content indices are finite one-based diagnostic positions"
)]
fn pi_custom_result(value: &Value, timestamp: Option<String>) -> Result<Vec<RecordSpan>> {
    if let Some(data) = value.get("data") {
        return Ok(record_span("tool-result", timestamp, data.to_string()));
    }
    let content = value
        .get("content")
        .context("Pi custom result has no data or content")?;
    if let Some(text) = content.as_str() {
        return Ok(vec![RecordSpan {
            locator_suffix: ";content=1".into(),
            role: Some("tool-result".into()),
            timestamp,
            text: text.to_owned(),
        }]);
    }
    let items = content
        .as_array()
        .context("Pi custom result content is not a string or array")?;
    ensure!(!items.is_empty(), "Pi custom result content is empty");
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .context("Pi custom result content type is missing or invalid")?;
            let (role, text) = match item_type {
                "text" => (
                    "tool-result",
                    item.get("text")
                        .and_then(Value::as_str)
                        .context("Pi custom result text is missing or invalid")?
                        .to_owned(),
                ),
                "image" => {
                    let mime_type = item
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .context("Pi custom result image mimeType is missing or invalid")?;
                    (
                        "omitted-asset",
                        json!({
                            "kind": "image",
                            "mimeType": mime_type,
                            "status": "not-materialized",
                        })
                        .to_string(),
                    )
                }
                _ => bail!("unsupported Pi custom result content type {item_type:?}"),
            };
            Ok(RecordSpan {
                locator_suffix: format!(";content={}", index + 1),
                role: Some(role.into()),
                timestamp: timestamp.clone(),
                text,
            })
        })
        .collect()
}

fn lifecycle_text(payload: &Value, subtype: &str) -> String {
    match subtype {
        "task_started" => json!({
            "type": subtype,
            "turn_id": payload.get("turn_id"),
            "started_at": payload.get("started_at"),
            "model_context_window": payload.get("model_context_window"),
            "collaboration_mode_kind": payload.get("collaboration_mode_kind"),
        })
        .to_string(),
        "task_complete" => json!({
            "type": subtype,
            "turn_id": payload.get("turn_id"),
            "completed_at": payload.get("completed_at"),
            "duration_ms": payload.get("duration_ms"),
            "last_agent_message": payload.get("last_agent_message"),
        })
        .to_string(),
        "patch_apply_end" => json!({
            "type": subtype,
            "call_id": payload.get("call_id"),
            "stdout": payload.get("stdout"),
            "stderr": payload.get("stderr"),
            "success": payload.get("success"),
            "status": payload.get("status"),
        })
        .to_string(),
        "web_search_end" => json!({
            "type": subtype,
            "call_id": payload.get("call_id"),
            "query": payload.get("query"),
            "status": payload.get("status"),
        })
        .to_string(),
        "turn_aborted" => json!({
            "type": subtype,
            "reason": payload.get("reason"),
        })
        .to_string(),
        _ => json!({"type": subtype}).to_string(),
    }
}

fn canonicalize_json_numbers(value: &mut Value, label: &str) -> Result<()> {
    match value {
        Value::Number(number) => *number = canonical_integer(number, label)?,
        Value::Array(values) => {
            for value in values {
                canonicalize_json_numbers(value, label)?;
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                canonicalize_json_numbers(value, label)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
    Ok(())
}

fn canonical_integer(number: &serde_json::Number, label: &str) -> Result<serde_json::Number> {
    const MAX_EXACT_JSON_INTEGER: i64 = 9_007_199_254_740_991;

    let token = number.as_str();
    let (negative, unsigned) = token
        .strip_prefix('-')
        .map_or((false, token), |value| (true, value));
    let (coefficient, exponent) = unsigned
        .split_once('e')
        .or_else(|| unsigned.split_once('E'))
        .map_or((unsigned, None), |(value, exponent)| {
            (value, Some(exponent))
        });
    let (whole, fraction) = coefficient
        .split_once('.')
        .map_or((coefficient, ""), |parts| parts);
    let digits = format!("{whole}{fraction}");
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return Ok(0.into());
    }
    let exponent = exponent
        .map_or(Ok(0_i64), str::parse::<i64>)
        .with_context(|| format!("{label} contains an out-of-range JSON exponent"))?;
    let fraction_digits = i64::try_from(fraction.len())
        .with_context(|| format!("{label} contains an oversized JSON number"))?;
    let shift = exponent
        .checked_sub(fraction_digits)
        .with_context(|| format!("{label} contains an out-of-range JSON exponent"))?;
    let integer = if shift < 0 {
        let removed = usize::try_from(
            shift
                .checked_neg()
                .context("negative decimal shift cannot be negated")?,
        )
        .with_context(|| format!("{label} contains an oversized JSON number"))?;
        let kept = significant
            .len()
            .checked_sub(removed)
            .with_context(|| format!("{label} contains a non-integral number"))?;
        ensure!(
            significant
                .get(kept..)
                .is_some_and(|suffix| suffix.bytes().all(|byte| byte == b'0')),
            "{label} contains a non-integral number"
        );
        significant
            .get(..kept)
            .context("integer prefix is not valid UTF-8")?
            .to_owned()
    } else {
        let appended = usize::try_from(shift)
            .with_context(|| format!("{label} contains an oversized JSON number"))?;
        ensure!(
            significant
                .len()
                .checked_add(appended)
                .is_some_and(|length| { length <= MAX_EXACT_JSON_INTEGER.to_string().len() }),
            "{label} contains an out-of-range integer"
        );
        format!("{significant}{}", "0".repeat(appended))
    };
    let magnitude = integer
        .parse::<i64>()
        .with_context(|| format!("{label} contains an out-of-range integer"))?;
    ensure!(
        magnitude <= MAX_EXACT_JSON_INTEGER,
        "{label} contains an out-of-range integer"
    );
    Ok(if negative {
        magnitude
            .checked_neg()
            .context("JSON integer cannot be negated")?
            .into()
    } else {
        magnitude.into()
    })
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

fn locator_str<'a>(locator: &'a Value, key: &str) -> Result<&'a str> {
    locator
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("locator is missing string field {key}"))
}

fn locator_strings<'a>(locator: &'a Value, key: &str) -> Result<Vec<&'a str>> {
    locator
        .get(key)
        .and_then(Value::as_array)
        .context("locator is missing an array field")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("locator field {key} contains a non-string"))
        })
        .collect()
}

fn locator_usize(locator: &Value, key: &str) -> Result<usize> {
    locator
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .with_context(|| format!("locator is missing integer field {key}"))
}

struct DuplicateChecked;

impl<'de> Deserialize<'de> for DuplicateChecked {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct DuplicateCheckedVisitor;

        impl<'de> Visitor<'de> for DuplicateCheckedVisitor {
            type Value = DuplicateChecked;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate object members")
            }

            fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_u64<E>(self, _value: u64) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_str<E>(self, _value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(DuplicateChecked)
            }

            fn visit_string<E>(self, _value: String) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                while sequence.next_element::<DuplicateChecked>()?.is_some() {}
                Ok(DuplicateChecked)
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut keys = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(A::Error::custom("duplicate JSON object member"));
                    }
                    map.next_value::<DuplicateChecked>()?;
                }
                Ok(DuplicateChecked)
            }
        }

        deserializer.deserialize_any(DuplicateCheckedVisitor)
    }
}

fn parse_unique_json(bytes: &[u8], label: &str) -> Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    DuplicateChecked::deserialize(&mut deserializer).with_context(|| format!("parse {label}"))?;
    deserializer
        .end()
        .with_context(|| format!("parse trailing data in {label}"))?;
    serde_json::from_slice(bytes).with_context(|| format!("parse {label}"))
}

fn contract_validator(schema: &str) -> Result<jsonschema::Validator> {
    let schema: Value = serde_json::from_str(schema).context("parse embedded contract schema")?;
    jsonschema::validator_for(&schema).context("compile embedded contract schema")
}

fn read_validated_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    validator: &jsonschema::Validator,
    label: &str,
) -> Result<T> {
    let snapshot = read_bound_private_file(path)?;
    let mut value = parse_unique_json(&snapshot.bytes, &path.display().to_string())?;
    validate_contract_value(&value, validator, label)?;
    canonicalize_json_numbers(&mut value, label)?;
    serde_json::from_value(value).with_context(|| format!("parse {label} {}", path.display()))
}

fn validate_contract_value(
    value: &Value,
    validator: &jsonschema::Validator,
    label: &str,
) -> Result<()> {
    if let Err(error) = validator.validate(value) {
        bail!(
            "{label} violates its schema at {}: {}",
            error.instance_path(),
            error.masked()
        );
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
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
        let text = b"# Alpha\n\nFictional evidence.\n\n## Detail\n\nMore evidence.\n\n# Beta\n\nExcluded.\n";
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

    #[test]
    fn compile_validate_and_detect_tampering() {
        let (temp, source, assignments, checksums) = markdown_fixture();
        set_mode(temp.path(), 0o755);
        let mut assignment: Value = read_json(&assignments).unwrap();
        assignment["units"][0]["locator"]["line"] = json!(1.0);
        assignment["units"][0]["metadata"]["integral"] = json!(2.0);
        write_private(
            &assignments,
            serde_json::to_vec(&assignment).unwrap().as_slice(),
        );
        let package = temp.path().join("package");
        let stale = private_dir(temp.path(), "package.staging");
        write_private(&stale.join("partial"), b"incomplete");
        assert!(compile(&assignments, &source, &checksums, &package).is_err());
        assert!(stale.join("partial").exists());
        fs::remove_dir_all(&stale).unwrap();
        let unrelated_marker_sibling = temp.path().join("package.staging.owner.json.tmp");
        write_private(&unrelated_marker_sibling, b"{");
        compile(&assignments, &source, &checksums, &package).unwrap();
        assert!(unrelated_marker_sibling.exists());
        validate(&package).unwrap();
        assert!(PrivateDirectory::new(&package.join("nested")).is_err());
        compile(&assignments, &source, &checksums, &package).unwrap();

        let manifest: EvidencePackageManifest = read_json(&package.join("manifest.json")).unwrap();
        let unit_path = package.join(&manifest.units[0].path);
        let mut unit: Value = read_json(&unit_path).unwrap();
        assert_eq!(unit["source_locator"]["line"], json!(1));
        assert_eq!(unit["metadata"]["integral"], json!(2));
        unit["spans"][0]["text"] = "tampered".into();
        write_private(&unit_path, serde_json::to_vec(&unit).unwrap().as_slice());
        assert!(validate(&package).is_err());
    }

    #[test]
    fn validation_rejects_unmanifested_and_schema_invalid_units() {
        let (temp, source, assignments, checksums) = markdown_fixture();
        let package = temp.path().join("package");
        compile(&assignments, &source, &checksums, &package).unwrap();
        write_private(&package.join("units/extra.json"), b"{}");
        assert!(validate(&package).is_err());
        fs::remove_file(package.join("units/extra.json")).unwrap();

        let mut manifest: EvidencePackageManifest =
            read_json(&package.join("manifest.json")).unwrap();
        let unit_path = package.join(&manifest.units[0].path);
        let mut unit: EvidenceUnit = read_json(&unit_path).unwrap();
        unit.sources.clear();
        unit.unit_sha256 = digest(&serde_json::to_vec(&unit.core()).unwrap());
        manifest.units[0].unit_sha256.clone_from(&unit.unit_sha256);
        write_private(&unit_path, serde_json::to_vec(&unit).unwrap().as_slice());
        write_private(
            &package.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap().as_slice(),
        );
        assert!(validate(&package).is_err());

        unit.sources = vec![SourceFile {
            path: "unrelated.md".into(),
            sha256: "0".repeat(64),
            bytes: 0,
        }];
        unit.unit_sha256 = digest(&serde_json::to_vec(&unit.core()).unwrap());
        manifest.units[0].unit_sha256.clone_from(&unit.unit_sha256);
        write_private(&unit_path, serde_json::to_vec(&unit).unwrap().as_slice());
        write_private(
            &package.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap().as_slice(),
        );
        assert!(validate(&package).is_err());
    }

    #[test]
    fn compiled_package_matches_public_schemas() {
        let (_temp, _source, assignments, _checksums) = markdown_fixture();
        let assignment: Value = read_json(&assignments).unwrap();
        let mut incomplete_docling = assignment;
        incomplete_docling["units"][0]["source_type"] = "docling-json".into();
        assert!(!matches_schema(
            "source-assignment.schema.json",
            &incomplete_docling
        ));
        let converted = json!({
            "schema_version": 1,
            "units": [{
                "unit_id": "markdown:converted",
                "source_type": "canonical-markdown",
                "locator": {"file": "notes.md", "line": 1},
                "metadata": {"heading": "Former locator value"}
            }]
        });
        assert!(matches_schema("source-assignment.schema.json", &converted));
        let mut unconverted = converted;
        unconverted["units"][0]["locator"]["heading"] = "Legacy field".into();
        assert!(!matches_schema(
            "source-assignment.schema.json",
            &unconverted
        ));
        let mut unit = json!({
            "schema_version": 1,
            "unit_id": "markdown:one",
            "source_type": "canonical-markdown",
            "source_locator": {"file": "notes.md", "line": 1},
            "metadata": {},
            "sources": [{"path": "notes.md", "sha256": "0".repeat(64), "bytes": 1}],
            "spans": [{
                "id": "s000001",
                "locator": "notes.md#line=1",
                "role": null,
                "timestamp": null,
                "text_sha256": "1".repeat(64),
                "text": "evidence"
            }],
            "unit_sha256": "2".repeat(64)
        });
        assert!(matches_schema("evidence-unit.schema.json", &unit));
        unit["unexpected"] = true.into();
        assert!(!matches_schema("evidence-unit.schema.json", &unit));
        assert!(matches_schema(
            "package-inspection.schema.json",
            &serde_json::to_value(PackageInspection {
                schema_version: SCHEMA_VERSION,
                units: 1,
                source_types: BTreeMap::from([("canonical-markdown".into(), 1)]),
                source_files: 1,
                spans: 3,
            })
            .unwrap()
        ));
    }

    #[test]
    fn inspection_counts_shared_source_once() {
        let (temp, source, assignments, checksums) = markdown_fixture();
        let mut assignment: Value = read_json(&assignments).unwrap();
        assignment["units"].as_array_mut().unwrap().push(json!({
            "unit_id": "markdown:beta",
            "source_type": "canonical-markdown",
            "locator": {"file": "notes.md", "line": 9},
            "metadata": {"collection": "fictional"}
        }));
        write_private(
            &assignments,
            serde_json::to_vec(&assignment).unwrap().as_slice(),
        );
        let package = temp.path().join("package");
        compile(&assignments, &source, &checksums, &package).unwrap();
        let inspection = package_inspection(&load_package(&package).unwrap());
        assert_eq!(inspection.units, 2);
        assert_eq!(inspection.source_files, 1);
    }

    #[test]
    fn canonical_evidence_unit_digest_vector_is_stable() {
        let source_locator = json!({"file": "notes/é.md", "line": 7});
        let metadata = BTreeMap::from([
            ("count".into(), json!(2)),
            (
                "nested".into(),
                json!({"enabled": true, "labels": ["α", "line\nbreak"]}),
            ),
        ]);
        let sources = [SourceFile {
            path: "notes/é.md".into(),
            sha256: "0".repeat(64),
            bytes: 12,
        }];
        let spans = [Span {
            id: "unit:é#span=1".into(),
            locator: "notes/é.md#line=7".into(),
            role: Some("heading".into()),
            timestamp: None,
            text_sha256: "1".repeat(64),
            text: "Café\n\u{1}".into(),
        }];
        assert_eq!(
            digest(
                &serde_json::to_vec(&EvidenceUnitCore {
                    schema_version: SCHEMA_VERSION,
                    unit_id: "unit:é",
                    source_type: "canonical-markdown",
                    source_locator: &source_locator,
                    metadata: &metadata,
                    sources: &sources,
                    spans: &spans,
                })
                .unwrap()
            ),
            "bc005fbef2e63ad0573fe528041e725fe6b532ab49a47efa9ab6b732fc213067"
        );
    }

    #[test]
    fn compilation_preserves_markdown_fences_and_large_spans() {
        let (temp, source, assignments, checksums) = markdown_fixture();
        let text = format!(
            "# Alpha\n\n```text\n# not a heading\n\nblank line preserved\n```\n\nAfter fence\n\n{}\n\n# Beta\n\nExcluded.\n",
            "x".repeat(70_000)
        );
        write_private(&source.join("notes.md"), text.as_bytes());
        write_private(
            &checksums,
            format!("{}  ./notes.md\n", digest(text.as_bytes())).as_bytes(),
        );
        let package = temp.path().join("package");
        compile(&assignments, &source, &checksums, &package).unwrap();
        let units = load_package(&package).unwrap();
        assert!(
            units[0]
                .spans
                .iter()
                .any(|span| span.text.len() > 64 * 1024)
        );
        assert!(units[0].spans.iter().any(|span| {
            span.text
                .contains("```text\n# not a heading\n\nblank line preserved\n```")
        }));
        assert!(
            units[0]
                .spans
                .iter()
                .all(|span| !span.text.contains("Excluded."))
        );
    }

    #[test]
    fn compilation_rejects_generated_units_outside_the_contract() {
        let (temp, source, assignments, checksums) = markdown_fixture();
        let text = b"# Alpha\n\ninvalid\0evidence\n";
        write_private(&source.join("notes.md"), text);
        write_private(
            &checksums,
            format!("{}  ./notes.md\n", digest(text)).as_bytes(),
        );
        let package = temp.path().join("package");
        assert!(compile(&assignments, &source, &checksums, &package).is_err());
        assert!(!package.exists());
        assert!(!temp.path().join("package.staging").exists());
    }

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
        assert!(
            compile_document(json!([conversation(json!(7))]), "invalid-parent-package").is_err()
        );
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

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one chronological Pi fixture covers every supported ordered record shape"
    )]
    fn pi_execution_history_projects_supported_records_without_media_or_reasoning() {
        let (temp, source, assignments, checksums) = frontend_fixture(
            "execution:pi",
            "execution-history",
            &json!({"files":["pi.jsonl"]}),
        );
        let history = jsonl(&[
            json!({
                "type":"session",
                "version":3,
                "id":"pi-session",
                "timestamp":"time-1",
                "cwd":"/fictional/project"
            }),
            json!({
                "type":"message",
                "id":"entry-user",
                "parentId":null,
                "timestamp":"time-2",
                "message":{
                    "role":"user",
                    "content":[
                        {"type":"text","text":"Pi user text"},
                        {
                            "type":"image",
                            "mimeType":"image/png",
                            "data":"NEVER_COPY_PI_IMAGE"
                        }
                    ]
                }
            }),
            json!({
                "type":"message",
                "id":"entry-assistant",
                "parentId":"entry-user",
                "timestamp":"time-3",
                "message":{
                    "role":"assistant",
                    "content":[
                        {"type":"text","text":"Pi assistant text"},
                        {"type":"thinking","thinking":"NEVER_COPY_PI_THINKING"},
                        {
                            "type":"toolCall",
                            "id":"call-pi",
                            "name":"fictional_tool",
                            "arguments":{"query":"fictional"},
                            "thoughtSignature":"NEVER_COPY_TOOL_REASONING"
                        }
                    ]
                }
            }),
            json!({
                "type":"message",
                "id":"entry-result",
                "parentId":"entry-assistant",
                "timestamp":"time-4",
                "message":{
                    "role":"toolResult",
                    "toolCallId":"call-pi",
                    "toolName":"fictional_tool",
                    "content":[{"type":"text","text":"Pi tool result"}],
                    "isError":false
                }
            }),
            json!({
                "type":"model_change","id":"model","parentId":"entry-result",
                "timestamp":"time-5","provider":"fictional","modelId":"model-one"
            }),
            json!({
                "type":"thinking_level_change","id":"level","parentId":"model",
                "timestamp":"time-6","thinkingLevel":"high"
            }),
            json!({
                "type":"session_info","id":"info","parentId":"level",
                "timestamp":"time-7","name":"Fictional session"
            }),
            json!({
                "type":"compaction","id":"compact","parentId":"info",
                "timestamp":"time-8","summary":"NEVER_COPY_COMPACTION_SUMMARY",
                "firstKeptEntryId":"entry-result","tokensBefore":1200
            }),
            json!({
                "type":"custom","id":"search","parentId":"compact","timestamp":"time-9",
                "customType":"web-search-results",
                "data":{"query":"fictional query","results":[{"title":"Synthetic result"}]}
            }),
            json!({
                "type":"custom_message","id":"recap","parentId":"search","timestamp":"time-10",
                "customType":"summary-recap","content":"NEVER_COPY_RECAP_BODY",
                "details":{"reasoning":"NEVER_COPY_RECAP_REASONING"}
            }),
            json!({
                "type":"custom","id":"btw","parentId":"recap","timestamp":"time-11",
                "customType":"btw-result",
                "data":{
                    "status":"completed","title":"Aside","answer":"Stable answer",
                    "errorText":"Synthetic aside error",
                    "transient":"NEVER_COPY_UNSTABLE_BTW_FIELD"
                }
            }),
            json!({
                "type":"custom_message","id":"terminal","parentId":"btw","timestamp":"time-12",
                "customType":"background-terminal-result","content":"Terminal completed",
                "details":{
                    "id":"terminal-job","status":"failed","title":"Terminal",
                    "exitCode":7,"signal":"TERM","transient":"NEVER_COPY_TERMINAL_TRANSIENT"
                }
            }),
            json!({
                "type":"custom_message","id":"subagent","parentId":"terminal","timestamp":"time-13",
                "customType":"subagent-result","content":"Delegate result",
                "details":{"id":"delegate-job","status":"completed","title":"Delegate"}
            }),
            json!({
                "type":"custom","id":"ready","parentId":"subagent","timestamp":"time-14",
                "customType":"web-search-content-ready","data":{"body":"NEVER_COPY_READY_BODY"}
            }),
        ]);
        write_source(&source, &checksums, "pi.jsonl", &history);
        let package = temp.path().join("package");
        compile(&assignments, &source, &checksums, &package).unwrap();
        let units = load_package(&package).unwrap();
        let spans = &units[0].spans;
        let at = |locator: &str| spans.iter().find(|span| span.locator == locator).unwrap();
        assert_eq!(at("pi.jsonl#line=2;content=1").text, "Pi user text");
        assert_eq!(
            at("pi.jsonl#line=2;content=2").role.as_deref(),
            Some("omitted-asset")
        );
        assert_eq!(
            at("pi.jsonl#line=2;content=2").text,
            "{\"kind\":\"image\",\"mimeType\":\"image/png\",\"status\":\"not-materialized\"}"
        );
        assert_eq!(
            at("pi.jsonl#line=3;content=2").role.as_deref(),
            Some("excluded-reasoning")
        );
        assert_eq!(
            at("pi.jsonl#line=3;content=2").text,
            "{\"type\":\"thinking\"}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&at("pi.jsonl#line=3;content=3").text).unwrap(),
            json!({"id":"call-pi","name":"fictional_tool","arguments":{"query":"fictional"}})
        );
        assert_eq!(
            at("pi.jsonl#line=4;content=1").role.as_deref(),
            Some("tool-result")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&at("pi.jsonl#line=4;result").text).unwrap(),
            json!({"toolCallId":"call-pi","toolName":"fictional_tool","isError":false})
        );
        for line in [5, 6, 8, 14] {
            assert_eq!(
                at(&format!("pi.jsonl#line={line}")).role.as_deref(),
                Some("lifecycle")
            );
        }
        assert_eq!(at("pi.jsonl#line=7").role.as_deref(), Some("metadata"));
        for line in [9, 11] {
            assert_eq!(
                at(&format!("pi.jsonl#line={line}")).role.as_deref(),
                Some("tool-result")
            );
        }
        assert_eq!(at("pi.jsonl#line=12;content=1").text, "Terminal completed");
        assert_eq!(at("pi.jsonl#line=13;content=1").text, "Delegate result");
        assert_eq!(
            serde_json::from_str::<Value>(&at("pi.jsonl#line=12;result").text).unwrap(),
            json!({
                "type":"background-terminal-result","id":"terminal-job",
                "status":"failed","title":"Terminal","exitCode":7,"signal":"TERM"
            })
        );
        assert_eq!(
            serde_json::from_str::<Value>(&at("pi.jsonl#line=13;result").text).unwrap(),
            json!({
                "type":"subagent-result","id":"delegate-job",
                "status":"completed","title":"Delegate","exitCode":null,"signal":null
            })
        );
        assert!(
            at("pi.jsonl#line=11")
                .text
                .contains("Synthetic aside error")
        );
        assert_eq!(
            at("pi.jsonl#line=10").role.as_deref(),
            Some("excluded-reasoning")
        );
        let compiled = serde_json::to_string(&units).unwrap();
        for excluded in [
            "NEVER_COPY_PI_IMAGE",
            "NEVER_COPY_PI_THINKING",
            "NEVER_COPY_TOOL_REASONING",
            "NEVER_COPY_COMPACTION_SUMMARY",
            "NEVER_COPY_RECAP_BODY",
            "NEVER_COPY_RECAP_REASONING",
            "NEVER_COPY_UNSTABLE_BTW_FIELD",
            "NEVER_COPY_READY_BODY",
            "NEVER_COPY_TERMINAL_TRANSIENT",
        ] {
            assert!(!compiled.contains(excluded));
        }
    }

    #[test]
    fn pi_execution_history_fails_closed_on_unknown_content_and_role_pairings() {
        let (temp, source, assignments, checksums) = frontend_fixture(
            "execution:pi-invalid",
            "execution-history",
            &json!({"files":["pi.jsonl"]}),
        );
        let header = json!({"type":"session","version":3,"id":"pi-invalid"});
        for (record, output) in [
            (
                json!({
                    "type":"message","message":{"role":"user","content":[{
                        "type":"audio","data":"must not disappear"
                    }]}
                }),
                "unknown-content",
            ),
            (
                json!({
                    "type":"message","message":{"role":"user","content":[{
                        "type":"toolCall","id":"call","name":"tool","arguments":{}
                    }]}
                }),
                "role-mismatch",
            ),
            (
                json!({"type":"custom","customType":"unknown-extension","data":{}}),
                "unknown-custom",
            ),
            (json!({"type":"unknown-record"}), "unknown-record"),
        ] {
            let history = jsonl(&[header.clone(), record]);
            write_source(&source, &checksums, "pi.jsonl", &history);
            assert!(compile(&assignments, &source, &checksums, &temp.path().join(output)).is_err());
        }
    }

    #[test]
    fn pi_execution_history_preserves_assistant_transport_errors() {
        let (temp, source, assignments, checksums) = frontend_fixture(
            "execution:pi-error",
            "execution-history",
            &json!({"files":["pi.jsonl"]}),
        );
        let history = jsonl(&[
            json!({"type":"session","version":3,"id":"pi-error"}),
            json!({
                "type":"message",
                "timestamp":"fictional-time",
                "message":{
                    "role":"assistant",
                    "content":[],
                    "stopReason":"error",
                    "errorMessage":"Synthetic provider failure"
                }
            }),
        ]);
        write_source(&source, &checksums, "pi.jsonl", &history);
        let package = temp.path().join("package");
        compile(&assignments, &source, &checksums, &package).unwrap();
        let units = load_package(&package).unwrap();
        assert_eq!(units[0].spans[1].locator, "pi.jsonl#line=2;error");
        assert_eq!(units[0].spans[1].role.as_deref(), Some("assistant"));
        assert_eq!(units[0].spans[1].text, "Synthetic provider failure");
    }

    #[test]
    fn execution_history_requires_a_consistent_recognized_header() {
        let (temp, source, assignments, checksums) = frontend_fixture(
            "execution:headers",
            "execution-history",
            &json!({"files":["first.jsonl","second.jsonl"]}),
        );
        let write_histories = |first: &[u8], second: &[u8]| {
            write_private(&source.join("first.jsonl"), first);
            write_private(&source.join("second.jsonl"), second);
            write_private(
                &checksums,
                format!(
                    "{}  ./first.jsonl\n{}  ./second.jsonl\n",
                    digest(first),
                    digest(second)
                )
                .as_bytes(),
            );
        };
        let missing_header = jsonl(&[json!({
            "type":"event_msg","payload":{"type":"user_message","message":"text"}
        })]);
        write_histories(&missing_header, &missing_header);
        let error = compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("missing-header"),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("first.jsonl"));
        assert!(message.contains("event_msg"));

        let codex = jsonl(&[json!({
            "type":"session_meta","payload":{"id":"codex-session","session_id":"group-one"}
        })]);
        let pi = jsonl(&[json!({"type":"session","version":3,"id":"pi-session"})]);
        write_histories(&codex, &pi);
        assert!(
            compile(
                &assignments,
                &source,
                &checksums,
                &temp.path().join("mixed-formats")
            )
            .is_err()
        );

        let inconsistent = jsonl(&[json!({
            "type":"session_meta",
            "payload":{"id":"delegate-session","session_id":"group-two"}
        })]);
        write_histories(&inconsistent, &codex);
        let error = compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("inconsistent-identities"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("inconsistent session identity"));
    }

    #[test]
    fn email_frontend_requires_complete_mbox_framing() {
        let (temp, source, assignments, checksums) = frontend_fixture(
            "email:42",
            "conversation-email",
            &json!({"file":"mail.mbox","thread_id":"42"}),
        );
        let mailbox = b"From ada@example.test Sat Jan 01 00:00:00 2022\nX-GM-THRID: 42\nFrom: Ada <ada@example.test>\nSubject: Fictional note\nDate: Sat, 1 Jan 2022 00:00:00 +0000\nContent-Type: text/plain; charset=utf-8\n\nComplete fictional evidence.\n";
        write_private(&source.join("mail.mbox"), mailbox);
        write_private(
            &checksums,
            format!("{}  ./mail.mbox\n", digest(mailbox)).as_bytes(),
        );
        let package = temp.path().join("package");
        compile(&assignments, &source, &checksums, &package).unwrap();
        let units = load_package(&package).unwrap();
        assert!(
            units[0].spans[0]
                .text
                .contains("Complete fictional evidence")
        );

        let epoch_mailbox = b"From ada@example.test Thu Jan 01 00:00:00 1970\nX-GM-THRID: 42\nFrom: Ada <ada@example.test>\nSubject: Fictional note\nDate: Thu, 1 Jan 1970 00:00:00 +0000\nContent-Type: text/plain; charset=utf-8\n\nEpoch evidence.\n";
        write_private(&source.join("mail.mbox"), epoch_mailbox);
        write_private(
            &checksums,
            format!("{}  ./mail.mbox\n", digest(epoch_mailbox)).as_bytes(),
        );
        compile(
            &assignments,
            &source,
            &checksums,
            &temp.path().join("epoch-package"),
        )
        .unwrap();

        let malformed = [b"unconsumed prefix\n".as_slice(), mailbox].concat();
        write_private(&source.join("mail.mbox"), &malformed);
        write_private(
            &checksums,
            format!("{}  ./mail.mbox\n", digest(&malformed)).as_bytes(),
        );
        assert!(
            compile(
                &assignments,
                &source,
                &checksums,
                &temp.path().join("malformed-package")
            )
            .is_err()
        );

        let malformed_envelope = b"From ada@example.test not-a-valid-date\nX-GM-THRID: 42\nFrom: Ada <ada@example.test>\nSubject: Fictional note\nDate: Sat, 1 Jan 2022 00:00:00 +0000\nContent-Type: text/plain; charset=utf-8\n\nComplete fictional evidence.\n";
        write_private(&source.join("mail.mbox"), malformed_envelope);
        write_private(
            &checksums,
            format!("{}  ./mail.mbox\n", digest(malformed_envelope)).as_bytes(),
        );
        assert!(
            compile(
                &assignments,
                &source,
                &checksums,
                &temp.path().join("malformed-envelope-package")
            )
            .is_err()
        );
    }

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
        assert_eq!(units[0].sources.len(), 2);
        assert_eq!(units[0].spans.len(), 3);
        assert_eq!(units[0].spans[0].text, "First text");
        assert!(units[0].spans[1].text.contains("cells"));
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
                "text": "Ambiguous fictional text"
            }]
        }))
        .unwrap();
        write_private(&source.join("document.json"), &contradictory);
        write_private(
            &checksums,
            format!(
                "{}  ./document.json\n{}  ./document.pdf\n",
                digest(&contradictory),
                digest(original)
            )
            .as_bytes(),
        );
        assert!(
            compile(
                &assignments,
                &source,
                &checksums,
                &temp.path().join("contradictory-package")
            )
            .is_err()
        );
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
        inventory_conversation_tables(&source, &[PathBuf::from("chat.csv ")], None, &output)
            .unwrap();
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
}
