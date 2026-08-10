use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, hash_map::Entry},
    fs,
    io::{self, BufRead, BufReader, Cursor, Write},
    os::unix::fs::DirBuilderExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use mail_parser::{Message, MimeHeaders, PartType};
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

mod email_attachments;

#[cfg(test)]
use crate::protected_fs::{private_mode, read_bound_private_json as read_json};
use email_attachments::{AttachmentDisposition, EmailAttachmentManifests, ManifestPart};
#[cfg(test)]
use std::cell::Cell;

const EVIDENCE_SCHEMA_VERSION: u8 = 3;
const ATTACHMENT_DECODE_ERROR: &str = "malformed-or-undecodable-transfer";
const SOURCE_ASSIGNMENT_SCHEMA_VERSION: u8 = 1;
const CONVERSATION_INVENTORY_SCHEMA_VERSION: u8 = 2;

#[cfg(test)]
thread_local! {
    static VERIFIED_SOURCE_READS: Cell<usize> = const { Cell::new(0) };
    static PARSED_SOURCE_PASSES: Cell<usize> = const { Cell::new(0) };
}
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceRole {
    Markdown,
    ChatGpt,
    Email,
    ConversationTable,
    Docling,
    ReceiptOnly,
    Execution,
}

#[derive(Clone, Copy)]
struct SourceUse {
    unit_index: usize,
    source_index: usize,
    role: SourceRole,
}

struct SourcePlan {
    path: String,
    parsers: Vec<SourceRole>,
    uses: Vec<SourceUse>,
}

struct PlannedUnit {
    unit: Option<AssignedUnit>,
    receipts: Vec<Option<SourceFile>>,
    raw_spans: Vec<Option<Vec<RawSpan>>>,
    raw_attachments: Vec<Option<Vec<RawAttachment>>>,
    execution_headers: Vec<Option<ExecutionHeader>>,
    identities: HashSet<(u64, u64)>,
    remaining_sources: usize,
}

#[derive(Clone)]
struct ExecutionHeader {
    format: ExecutionFormat,
    identity: String,
    path: String,
}

struct SourceExtraction {
    unit_index: usize,
    source_index: usize,
    raw_spans: Vec<RawSpan>,
    raw_attachments: Vec<RawAttachment>,
    execution_header: Option<ExecutionHeader>,
}

struct ExtractionContext<'a> {
    source_root: &'a Path,
    attachment_manifests: &'a EmailAttachmentManifests,
    planned_source_paths: &'a HashSet<String>,
    attachment_receipts: &'a mut HashMap<String, SourceFile>,
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

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Attachment {
    id: String,
    span_id: String,
    locator: String,
    filename: Option<String>,
    media_type: String,
    disposition: AttachmentDisposition,
    content_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<SourceFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct EvidenceUnitCoreV1<'a> {
    schema_version: u8,
    unit_id: &'a str,
    source_type: &'a str,
    source_locator: &'a Value,
    metadata: &'a BTreeMap<String, Value>,
    sources: &'a [SourceFile],
    spans: &'a [Span],
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
    attachments: &'a [Attachment],
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
    #[serde(default)]
    attachments: Vec<Attachment>,
    unit_sha256: String,
}

impl EvidenceUnit {
    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        if self.schema_version == 1 {
            serde_json::to_vec(&EvidenceUnitCoreV1 {
                schema_version: self.schema_version,
                unit_id: &self.unit_id,
                source_type: &self.source_type,
                source_locator: &self.source_locator,
                metadata: &self.metadata,
                sources: &self.sources,
                spans: &self.spans,
            })
            .map_err(Into::into)
        } else {
            serde_json::to_vec(&EvidenceUnitCore {
                schema_version: self.schema_version,
                unit_id: &self.unit_id,
                source_type: &self.source_type,
                source_locator: &self.source_locator,
                metadata: &self.metadata,
                sources: &self.sources,
                spans: &self.spans,
                attachments: &self.attachments,
            })
            .map_err(Into::into)
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
    attachments: usize,
    materialized_attachments: usize,
    unavailable_attachments: usize,
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

#[cfg(test)]
pub fn compile(
    assignments: &Path,
    source_root: &Path,
    checksums: &Path,
    output: &Path,
) -> Result<()> {
    compile_with_email_attachments(assignments, source_root, checksums, &[], output)
}

pub fn compile_with_email_attachments(
    assignments: &Path,
    source_root: &Path,
    checksums: &Path,
    email_attachment_manifests: &[PathBuf],
    output: &Path,
) -> Result<()> {
    let mut inputs = vec![
        (assignments, "assignment"),
        (source_root, "source root"),
        (checksums, "checksum index"),
    ];
    inputs.extend(
        email_attachment_manifests
            .iter()
            .map(|path| (path.as_path(), "email attachment manifest")),
    );
    ensure_output_separate(output, &inputs)?;
    let staging = PrivateDirectory::new(output)?;
    let source_root = open_private_bound_directory(source_root)?;
    let attachment_manifests = EmailAttachmentManifests::load(email_attachment_manifests)?;
    let assignment_validator = contract_validator(SOURCE_ASSIGNMENT_SCHEMA)?;
    let assignment: Assignment =
        read_validated_json(assignments, &assignment_validator, "source assignment")?;
    let checksum_index = checksum_index(checksums)?;
    let unit_validator = contract_validator(EVIDENCE_UNIT_SCHEMA)?;
    let units_directory = staging.path().join("units");
    fs::DirBuilder::new().mode(0o700).create(&units_directory)?;
    let (mut units, sources) = compile_plan(assignment.units)?;
    let email_sources = sources
        .iter()
        .filter(|plan| plan.parsers.contains(&SourceRole::Email))
        .map(|plan| plan.path.clone())
        .collect::<HashSet<_>>();
    attachment_manifests.reject_unused(&email_sources)?;
    let planned_source_paths = sources
        .iter()
        .map(|plan| plan.path.clone())
        .collect::<HashSet<_>>();
    let mut attachment_receipts = HashMap::new();
    let mut entries = (0..units.len()).map(|_| None).collect::<Vec<_>>();
    for source in sources {
        let ready = process_source(
            &source,
            source_root.path(),
            &checksum_index,
            &attachment_manifests,
            &planned_source_paths,
            &mut attachment_receipts,
            &mut units,
        )?;
        for unit_index in ready {
            let entry = write_planned_unit(
                units
                    .get_mut(unit_index)
                    .context("ready unit index is outside the compile plan")?,
                &staging,
                &unit_validator,
            )?;
            let slot = entries
                .get_mut(unit_index)
                .context("manifest entry index is outside the compile plan")?;
            ensure!(slot.replace(entry).is_none(), "unit was compiled twice");
        }
    }
    let entries = entries
        .into_iter()
        .map(|entry| entry.context("assigned unit was not compiled"))
        .collect::<Result<Vec<_>>>()?;
    write_staging_json(
        &staging.path().join("manifest.json"),
        &EvidencePackageManifest {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            units: entries,
        },
    )?;
    sync_directory(&units_directory)?;
    staging.finish()
}

pub fn materialize_email_attachments(
    source_root: &Path,
    file: &Path,
    artifact_dir: &Path,
    output_manifest: &Path,
) -> Result<()> {
    let summary = email_attachments::materialize(source_root, file, artifact_dir, output_manifest)?;
    serde_json::to_writer_pretty(io::stdout().lock(), &summary)?;
    println!();
    Ok(())
}

fn compile_plan(units: Vec<AssignedUnit>) -> Result<(Vec<PlannedUnit>, Vec<SourcePlan>)> {
    let mut planned_units = Vec::with_capacity(units.len());
    let mut source_plans = Vec::new();
    let mut source_indices = HashMap::new();
    let mut unit_ids = HashSet::new();
    for (unit_index, unit) in units.into_iter().enumerate() {
        ensure!(
            unit_ids.insert(unit.unit_id.clone()),
            "duplicate assigned unit {}",
            unit.unit_id
        );
        let paths = source_paths(&unit.source_type, &unit.locator)?;
        let roles = source_roles(&unit.source_type, paths.len())?;
        ensure!(
            paths.len() == roles.len(),
            "source role count does not match source paths for {}",
            unit.unit_id
        );
        let source_count = paths.len();
        for (source_index, (path, role)) in paths.into_iter().zip(roles).enumerate() {
            let plan_index = match source_indices.entry(path.clone()) {
                Entry::Occupied(entry) => *entry.get(),
                Entry::Vacant(entry) => {
                    let index = source_plans.len();
                    entry.insert(index);
                    source_plans.push(SourcePlan {
                        path: path.clone(),
                        parsers: Vec::new(),
                        uses: Vec::new(),
                    });
                    index
                }
            };
            let plan = source_plans
                .get_mut(plan_index)
                .context("source plan index is outside the compile plan")?;
            if role != SourceRole::ReceiptOnly && !plan.parsers.contains(&role) {
                plan.parsers.push(role);
            }
            plan.uses.push(SourceUse {
                unit_index,
                source_index,
                role,
            });
        }
        planned_units.push(PlannedUnit {
            unit: Some(unit),
            receipts: (0..source_count).map(|_| None).collect(),
            raw_spans: (0..source_count).map(|_| None).collect(),
            raw_attachments: (0..source_count).map(|_| None).collect(),
            execution_headers: (0..source_count).map(|_| None).collect(),
            identities: HashSet::new(),
            remaining_sources: source_count,
        });
    }
    Ok((planned_units, source_plans))
}

fn source_roles(source_type: &str, source_count: usize) -> Result<Vec<SourceRole>> {
    let role = match source_type {
        "canonical-markdown" => SourceRole::Markdown,
        "conversation-chatgpt" => SourceRole::ChatGpt,
        "conversation-email" => SourceRole::Email,
        "conversation-table" => SourceRole::ConversationTable,
        "execution-history" => SourceRole::Execution,
        "docling-json" => {
            ensure!(
                source_count == 2,
                "Docling assignment must have two sources"
            );
            return Ok(vec![SourceRole::Docling, SourceRole::ReceiptOnly]);
        }
        value => bail!("unsupported evidence source type {value}"),
    };
    Ok(vec![role; source_count])
}

fn process_source(
    plan: &SourcePlan,
    source_root: &Path,
    checksums: &HashMap<String, String>,
    attachment_manifests: &EmailAttachmentManifests,
    planned_source_paths: &HashSet<String>,
    attachment_receipts: &mut HashMap<String, SourceFile>,
    units: &mut [PlannedUnit],
) -> Result<Vec<usize>> {
    let path = safe_join(source_root, &plan.path)?;
    let (source, identity) =
        verified_source(&path, &plan.path, !plan.parsers.is_empty(), checksums)?;
    match attachment_receipts.entry(plan.path.clone()) {
        Entry::Occupied(expected) => ensure!(
            expected.get() == &source.receipt,
            "verified source receipt disagrees with email attachment receipt: {}",
            plan.path
        ),
        Entry::Vacant(slot) => {
            slot.insert(source.receipt.clone());
        }
    }
    for source_use in &plan.uses {
        let unit = units
            .get_mut(source_use.unit_index)
            .context("source use unit index is outside the compile plan")?;
        ensure!(
            unit.identities.insert(identity),
            "source paths resolve to the same file: {}",
            plan.path
        );
        let receipt = unit
            .receipts
            .get_mut(source_use.source_index)
            .context("source receipt index is outside the compile plan")?;
        ensure!(
            receipt.replace(source.receipt.clone()).is_none(),
            "source receipt was assigned twice"
        );
    }
    for &parser in &plan.parsers {
        let mut context = ExtractionContext {
            source_root,
            attachment_manifests,
            planned_source_paths,
            attachment_receipts,
        };
        for extraction in extract_source(parser, &source, &mut context, &plan.uses, units)? {
            let unit = units
                .get_mut(extraction.unit_index)
                .context("source extraction unit index is outside the compile plan")?;
            let span_slot = unit
                .raw_spans
                .get_mut(extraction.source_index)
                .context("source span index is outside the compile plan")?;
            ensure!(
                span_slot.replace(extraction.raw_spans).is_none(),
                "source spans were extracted twice"
            );
            let attachment_slot = unit
                .raw_attachments
                .get_mut(extraction.source_index)
                .context("source attachment index is outside the compile plan")?;
            ensure!(
                attachment_slot
                    .replace(extraction.raw_attachments)
                    .is_none(),
                "source attachments were extracted twice"
            );
            if let Some(header) = extraction.execution_header {
                let header_slot = unit
                    .execution_headers
                    .get_mut(extraction.source_index)
                    .context("execution header index is outside the compile plan")?;
                ensure!(
                    header_slot.replace(header).is_none(),
                    "execution header was extracted twice"
                );
            }
        }
    }
    let mut ready = Vec::new();
    for source_use in &plan.uses {
        let unit = units
            .get_mut(source_use.unit_index)
            .context("source use unit index is outside the compile plan")?;
        unit.remaining_sources = unit
            .remaining_sources
            .checked_sub(1)
            .context("unit source count underflowed")?;
        if unit.remaining_sources == 0 {
            ready.push(source_use.unit_index);
        }
    }
    Ok(ready)
}

fn verified_source(
    path: &Path,
    relative: &str,
    read_bytes: bool,
    checksums: &HashMap<String, String>,
) -> Result<(VerifiedSource, (u64, u64))> {
    #[cfg(test)]
    VERIFIED_SOURCE_READS.set(VERIFIED_SOURCE_READS.get().saturating_add(1));
    if read_bytes {
        let snapshot = read_bound_private_file(path)?;
        let sha256 = digest(&snapshot.bytes);
        ensure!(
            checksums.get(relative) == Some(&sha256),
            "checksum mismatch or missing checksum for {relative}"
        );
        let bytes = u64::try_from(snapshot.bytes.len()).context("source byte count overflow")?;
        return Ok((
            VerifiedSource {
                receipt: SourceFile {
                    path: relative.into(),
                    sha256,
                    bytes,
                },
                bytes: snapshot.bytes,
            },
            (snapshot.device, snapshot.inode),
        ));
    }
    let snapshot = digest_bound_private_file(path)?;
    ensure!(
        checksums.get(relative) == Some(&snapshot.sha256),
        "checksum mismatch or missing checksum for {relative}"
    );
    Ok((
        VerifiedSource {
            receipt: SourceFile {
                path: relative.into(),
                sha256: snapshot.sha256,
                bytes: snapshot.bytes,
            },
            bytes: Vec::new(),
        },
        (snapshot.device, snapshot.inode),
    ))
}

fn write_planned_unit(
    plan: &mut PlannedUnit,
    staging: &PrivateDirectory,
    unit_validator: &jsonschema::Validator,
) -> Result<EvidencePackageEntry> {
    ensure!(plan.remaining_sources == 0, "unit still has unread sources");
    let unit = plan
        .unit
        .take()
        .context("assigned unit was already compiled")?;
    let mut sources = std::mem::take(&mut plan.receipts)
        .into_iter()
        .map(|receipt| receipt.context("assigned source has no receipt"))
        .collect::<Result<Vec<_>>>()?;
    let raw_spans = if unit.source_type == "execution-history" {
        validate_execution_headers(&plan.execution_headers)?;
        std::mem::take(&mut plan.raw_spans)
            .into_iter()
            .map(|spans| spans.context("execution source has no spans"))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect()
    } else {
        std::mem::take(&mut plan.raw_spans)
            .into_iter()
            .flatten()
            .flatten()
            .collect()
    };
    let spans = number_spans(raw_spans)?;
    ensure!(!spans.is_empty(), "unit {} produced no spans", unit.unit_id);
    let raw_attachments = std::mem::take(&mut plan.raw_attachments)
        .into_iter()
        .flatten()
        .flatten()
        .collect::<Vec<_>>();
    let attachments = number_attachments(raw_attachments, &spans, &mut sources)?;
    ensure!(
        unit.source_type == "conversation-email" || attachments.is_empty(),
        "non-email unit produced attachments"
    );
    let mut evidence = EvidenceUnit {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        unit_id: unit.unit_id.clone(),
        source_type: unit.source_type.clone(),
        source_locator: unit.locator,
        metadata: unit.metadata,
        sources,
        spans,
        attachments,
        unit_sha256: String::new(),
    };
    evidence.unit_sha256 = digest(&evidence.canonical_bytes()?);
    validate_contract_value(
        &serde_json::to_value(&evidence)?,
        unit_validator,
        "evidence unit",
    )?;
    let path = format!("units/{}.json", digest(unit.unit_id.as_bytes()));
    write_staging_json(&staging.path().join(&path), &evidence)?;
    Ok(EvidencePackageEntry {
        unit_id: unit.unit_id,
        source_type: unit.source_type,
        unit_sha256: evidence.unit_sha256,
        path,
    })
}

fn validate_execution_headers(headers: &[Option<ExecutionHeader>]) -> Result<()> {
    let first = headers
        .first()
        .and_then(Option::as_ref)
        .context("execution history is missing a session header")?;
    for header in headers.iter().skip(1) {
        let header = header
            .as_ref()
            .context("execution history is missing a session header")?;
        ensure!(
            header.format == first.format,
            "mixed execution-history formats: expected {:?} but {} begins with {:?}",
            first.format,
            header.path,
            header.format
        );
        ensure!(
            header.identity == first.identity,
            "execution history {} has inconsistent session identity {:?}; expected {:?}",
            header.path,
            header.identity,
            first.identity
        );
    }
    Ok(())
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
    let mut attachments = 0_usize;
    let mut materialized_attachments = 0_usize;
    for unit in units {
        *source_types.entry(unit.source_type.clone()).or_default() += 1;
        source_files.extend(
            unit.sources
                .iter()
                .map(|source| (&source.path, &source.sha256)),
        );
        spans += unit.spans.len();
        attachments += unit.attachments.len();
        materialized_attachments += unit
            .attachments
            .iter()
            .filter(|attachment| attachment.source.is_some())
            .count();
    }
    PackageInspection {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        units: units.len(),
        source_types,
        source_files: source_files.len(),
        spans,
        attachments,
        materialized_attachments,
        unavailable_attachments: attachments - materialized_attachments,
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
        schema_version: SOURCE_ASSIGNMENT_SCHEMA_VERSION,
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
        ensure!(
            unit.schema_version == manifest.schema_version,
            "evidence-unit schema version does not match its package manifest: {}",
            entry.unit_id
        );
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
        digest(&unit.canonical_bytes()?) == unit.unit_sha256,
        "evidence-unit checksum mismatch for {}",
        unit.unit_id
    );
    let expected_sources = source_paths(&unit.source_type, &unit.source_locator)?;
    ensure!(
        unit.sources
            .iter()
            .take(expected_sources.len())
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
    ensure!(
        unit.source_type == "conversation-email" || unit.attachments.is_empty(),
        "non-email unit contains attachments: {}",
        unit.unit_id
    );
    let span_by_id = unit
        .spans
        .iter()
        .map(|span| (span.id.as_str(), span))
        .collect::<HashMap<_, _>>();
    let mut attachment_ids = HashSet::new();
    let mut artifact_paths = HashSet::new();
    let mut expected_artifacts = Vec::new();
    for attachment in &unit.attachments {
        ensure!(
            attachment_ids.insert(attachment.id.as_str()),
            "duplicate attachment ID in {}",
            unit.unit_id
        );
        let parent = span_by_id
            .get(attachment.span_id.as_str())
            .with_context(|| format!("invalid attachment span reference in {}", unit.unit_id))?;
        ensure!(
            attachment
                .locator
                .strip_prefix(&parent.locator)
                .is_some_and(valid_mime_part_suffix),
            "attachment locator does not extend its parent message locator in {}",
            unit.unit_id
        );
        if let Some(source) = &attachment.source {
            validate_content_addressed_source(source)?;
            if artifact_paths.insert(source.path.as_str()) {
                expected_artifacts.push(source);
            } else {
                ensure!(
                    expected_artifacts.contains(&source),
                    "attachment source receipts disagree in {}",
                    unit.unit_id
                );
            }
        }
    }
    ensure!(
        unit.sources
            .iter()
            .skip(expected_sources.len())
            .eq(expected_artifacts.into_iter()),
        "artifact sources do not match first attachment occurrence order in {}",
        unit.unit_id
    );
    Ok(())
}

fn valid_mime_part_suffix(suffix: &str) -> bool {
    suffix.strip_prefix(";part=").is_some_and(|path| {
        !path.is_empty()
            && path
                .split('.')
                .all(|component| component.parse::<u64>().is_ok_and(|value| value > 0))
    })
}

fn validate_content_addressed_source(source: &SourceFile) -> Result<()> {
    safe_join(Path::new("."), &source.path)?;
    let path = Path::new(&source.path);
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("artifact source has no UTF-8 digest filename")?;
    let prefix = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .context("artifact source has no digest-prefix directory")?;
    ensure!(
        filename == source.sha256
            && source.sha256.get(..2) == Some(prefix)
            && source.sha256.len() == 64
            && source
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "artifact source path does not match its SHA-256 receipt: {}",
        source.path
    );
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
    clippy::unreachable,
    reason = "source type and path cardinality are exhaustively validated before extraction"
)]
fn extract_source(
    role: SourceRole,
    source: &VerifiedSource,
    context: &mut ExtractionContext<'_>,
    source_uses: &[SourceUse],
    units: &[PlannedUnit],
) -> Result<Vec<SourceExtraction>> {
    #[cfg(test)]
    PARSED_SOURCE_PASSES.set(PARSED_SOURCE_PASSES.get().saturating_add(1));
    let uses = source_uses
        .iter()
        .filter(|source_use| source_use.role == role)
        .collect::<Vec<_>>();
    match role {
        SourceRole::Markdown => {
            let text =
                std::str::from_utf8(&source.bytes).context("Markdown source must be UTF-8")?;
            let lines = text.lines().collect::<Vec<_>>();
            uses.into_iter()
                .map(|source_use| {
                    Ok(SourceExtraction {
                        unit_index: source_use.unit_index,
                        source_index: source_use.source_index,
                        raw_spans: markdown_spans(
                            planned_assignment(units, source_use.unit_index)?,
                            source,
                            &lines,
                        )?,
                        raw_attachments: Vec::new(),
                        execution_header: None,
                    })
                })
                .collect()
        }
        SourceRole::ConversationTable => {
            let rows = conversation_rows(&source.bytes, &source.receipt.path)?;
            let mut by_thread: HashMap<&str, Vec<&ConversationRow>> = HashMap::new();
            for row in &rows {
                by_thread.entry(&row.thread).or_default().push(row);
            }
            uses.into_iter()
                .map(|source_use| {
                    let unit = planned_assignment(units, source_use.unit_index)?;
                    let conversation_id = locator_str(&unit.locator, "conversation_id")?;
                    Ok(SourceExtraction {
                        unit_index: source_use.unit_index,
                        source_index: source_use.source_index,
                        raw_spans: conversation_table_spans(
                            conversation_id,
                            source,
                            by_thread.get(conversation_id).map(Vec::as_slice),
                        )?,
                        raw_attachments: Vec::new(),
                        execution_header: None,
                    })
                })
                .collect()
        }
        SourceRole::ChatGpt => chatgpt_source_extractions(source, &uses, units),
        SourceRole::Email => email_source_extractions(source, context, &uses, units),
        SourceRole::Docling => {
            let document = parse_unique_json(&source.bytes, &source.receipt.path)?;
            let spans = docling_spans(source, &document)?;
            Ok(uses
                .into_iter()
                .map(|source_use| SourceExtraction {
                    unit_index: source_use.unit_index,
                    source_index: source_use.source_index,
                    raw_spans: spans.clone(),
                    raw_attachments: Vec::new(),
                    execution_header: None,
                })
                .collect())
        }
        SourceRole::Execution => {
            let (header, spans) = execution_source(source)?;
            Ok(uses
                .into_iter()
                .map(|source_use| SourceExtraction {
                    unit_index: source_use.unit_index,
                    source_index: source_use.source_index,
                    raw_spans: spans.clone(),
                    raw_attachments: Vec::new(),
                    execution_header: Some(header.clone()),
                })
                .collect())
        }
        SourceRole::ReceiptOnly => unreachable!(),
    }
}

fn planned_assignment(units: &[PlannedUnit], index: usize) -> Result<&AssignedUnit> {
    units
        .get(index)
        .and_then(|unit| unit.unit.as_ref())
        .context("source use refers to a compiled or missing unit")
}

fn number_spans(raw: Vec<RawSpan>) -> Result<Vec<Span>> {
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

fn number_attachments(
    raw: Vec<RawAttachment>,
    spans: &[Span],
    sources: &mut Vec<SourceFile>,
) -> Result<Vec<Attachment>> {
    let span_ids = spans
        .iter()
        .map(|span| (span.locator.as_str(), span.id.as_str()))
        .collect::<HashMap<_, _>>();
    let mut artifact_sources = HashSet::new();
    raw.into_iter()
        .enumerate()
        .map(|(index, raw)| {
            let span_id = (*span_ids.get(raw.parent_locator.as_str()).with_context(|| {
                format!(
                    "attachment {} has no parent message span {}",
                    raw.locator, raw.parent_locator
                )
            })?)
            .to_string();
            if let Some(source) = &raw.source {
                if artifact_sources.insert(source.path.clone()) {
                    ensure!(
                        !sources.iter().any(|found| found.path == source.path),
                        "artifact source path collides with an assigned source: {}",
                        source.path
                    );
                    sources.push(source.clone());
                } else {
                    ensure!(
                        sources.iter().any(|found| found == source),
                        "artifact occurrences disagree about source receipt {}",
                        source.path
                    );
                }
            }
            Ok(Attachment {
                id: format!(
                    "a{:06}",
                    index.checked_add(1).context("attachment index overflow")?
                ),
                span_id,
                locator: raw.locator,
                filename: raw.filename,
                media_type: raw.media_type,
                disposition: raw.disposition,
                content_id: raw.content_id,
                source: raw.source,
                error: raw.error,
            })
        })
        .collect()
}

#[derive(Clone)]
struct RawSpan {
    locator: String,
    role: Option<String>,
    timestamp: Option<String>,
    text: String,
}

#[derive(Clone)]
struct RawAttachment {
    parent_locator: String,
    locator: String,
    filename: Option<String>,
    media_type: String,
    disposition: AttachmentDisposition,
    content_id: Option<String>,
    source: Option<SourceFile>,
    error: Option<String>,
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
fn markdown_spans(
    unit: &AssignedUnit,
    source: &VerifiedSource,
    lines: &[&str],
) -> Result<Vec<RawSpan>> {
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

fn conversation_table_spans(
    conversation_id: &str,
    source: &VerifiedSource,
    rows: Option<&[&ConversationRow]>,
) -> Result<Vec<RawSpan>> {
    let spans = rows
        .unwrap_or_default()
        .iter()
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
                row.speaker.clone()
            }),
            timestamp: Some(format!("relative:{}", row.time)),
            text: row.message.clone(),
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

fn chatgpt_source_extractions(
    source: &VerifiedSource,
    uses: &[&SourceUse],
    units: &[PlannedUnit],
) -> Result<Vec<SourceExtraction>> {
    let document = parse_unique_json(&source.bytes, &source.receipt.path)?;
    let conversations = document
        .as_array()
        .context("ChatGPT export root must be an array")?;
    let targets = uses
        .iter()
        .map(|source_use| {
            locator_str(
                &planned_assignment(units, source_use.unit_index)?.locator,
                "conversation_id",
            )
        })
        .collect::<Result<HashSet<_>>>()?;
    let mut selected = HashMap::new();
    for conversation in conversations {
        let mut conversation_ids = HashSet::new();
        for conversation_id in [
            conversation.get("id").and_then(Value::as_str),
            conversation.get("conversation_id").and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        {
            if targets.contains(conversation_id) && conversation_ids.insert(conversation_id) {
                ensure!(
                    selected.insert(conversation_id, conversation).is_none(),
                    "conversation {conversation_id} is duplicated"
                );
            }
        }
    }
    uses.iter()
        .map(|source_use| {
            let unit = planned_assignment(units, source_use.unit_index)?;
            let conversation_id = locator_str(&unit.locator, "conversation_id")?;
            let conversation = selected
                .get(conversation_id)
                .with_context(|| format!("conversation {conversation_id} was not found"))?;
            Ok(SourceExtraction {
                unit_index: source_use.unit_index,
                source_index: source_use.source_index,
                raw_spans: chatgpt_spans(conversation_id, conversation)?,
                raw_attachments: Vec::new(),
                execution_header: None,
            })
        })
        .collect()
}

fn chatgpt_spans(conversation_id: &str, conversation: &Value) -> Result<Vec<RawSpan>> {
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

fn docling_spans(source: &VerifiedSource, document: &Value) -> Result<Vec<RawSpan>> {
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

fn email_source_extractions(
    source: &VerifiedSource,
    context: &mut ExtractionContext<'_>,
    uses: &[&SourceUse],
    units: &[PlannedUnit],
) -> Result<Vec<SourceExtraction>> {
    let mut targets: HashMap<&str, Vec<&SourceUse>> = HashMap::new();
    for source_use in uses {
        let unit = planned_assignment(units, source_use.unit_index)?;
        targets
            .entry(locator_str(&unit.locator, "thread_id")?)
            .or_default()
            .push(source_use);
    }
    let supplied = context.attachment_manifests.get(&source.receipt.path);
    let artifact_dir = supplied.map_or("_artifacts/sha256", |manifest| manifest.artifact_dir());
    let selected_threads = targets.keys().copied().collect::<HashSet<_>>();
    let mut projection =
        email_attachments::project_mailbox(source, artifact_dir, None, &selected_threads)?;
    if let Some(supplied) = supplied {
        ensure!(
            projection.manifest == *supplied,
            "email attachment manifest does not match observed MIME parts for {}",
            source.receipt.path
        );
    } else {
        ensure!(
            projection.manifest.parts.is_empty(),
            "email source {} contains attachments but has no --email-attachment-manifest",
            source.receipt.path
        );
    }
    let mut extractions = Vec::with_capacity(uses.len());
    for (thread_id, thread_uses) in targets {
        let thread_spans = projection
            .spans_by_thread
            .remove(thread_id)
            .context("Gmail thread projection disappeared")?;
        ensure!(
            !thread_spans.is_empty(),
            "Gmail thread {thread_id} was not found"
        );
        let attachments = projection
            .manifest
            .parts
            .iter()
            .filter(|part| part.thread_id == thread_id)
            .map(|part| {
                raw_attachment(
                    part,
                    &source.receipt.path,
                    context.source_root,
                    context.planned_source_paths,
                    context.attachment_receipts,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        for source_use in thread_uses {
            extractions.push(SourceExtraction {
                unit_index: source_use.unit_index,
                source_index: source_use.source_index,
                raw_spans: thread_spans.clone(),
                raw_attachments: attachments.clone(),
                execution_header: None,
            });
        }
    }
    Ok(extractions)
}

fn raw_attachment(
    part: &ManifestPart,
    source_path: &str,
    source_root: &Path,
    planned_source_paths: &HashSet<String>,
    receipts: &mut HashMap<String, SourceFile>,
) -> Result<RawAttachment> {
    if let Some(source) = &part.source {
        match receipts.entry(source.path.clone()) {
            Entry::Occupied(found) => {
                ensure!(
                    found.get() == source,
                    "{}; email attachment manifests disagree about source receipt {}",
                    part.failure_context(source_path),
                    source.path
                );
            }
            Entry::Vacant(slot) => {
                if !planned_source_paths.contains(&source.path) {
                    email_attachments::verify_artifact(source_root, source)
                        .with_context(|| part.failure_context(source_path))?;
                }
                slot.insert(source.clone());
            }
        }
    }
    let parent_locator = part
        .locator
        .split_once(";part=")
        .map(|(parent, _)| parent.to_owned())
        .with_context(|| {
            format!(
                "{}; attachment locator has no MIME part path",
                part.failure_context(source_path)
            )
        })?;
    Ok(RawAttachment {
        parent_locator,
        locator: part.locator.clone(),
        filename: part.filename.clone(),
        media_type: part.media_type.clone(),
        disposition: part.disposition,
        content_id: part.content_id.clone(),
        source: part.source.clone(),
        error: part.error.clone(),
    })
}

#[expect(
    clippy::arithmetic_side_effects,
    clippy::format_push_string,
    reason = "message indices are bounded by the parsed mailbox"
)]
fn email_message_span(
    source: &VerifiedSource,
    ordinal: usize,
    thread_id: &str,
    message: &Message<'_>,
) -> RawSpan {
    let body = selected_body(message).unwrap_or_default();
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
    RawSpan {
        locator: format!(
            "{}#message={};thread={thread_id}",
            source.receipt.path,
            ordinal + 1
        ),
        role: Some(from.into()),
        timestamp: message.date().map(mail_parser::DateTime::to_rfc3339),
        text,
    }
}

fn selected_body(message: &Message<'_>) -> Option<String> {
    message
        .text_part(0)
        .and_then(selected_email_part)
        .or_else(|| message.html_part(0).and_then(selected_email_part))
}

fn selected_email_part(part: &mail_parser::MessagePart<'_>) -> Option<String> {
    match &part.body {
        PartType::Text(text) => Some(normalize_email_spacers(&replace_email_break_tags(
            &decode_email_entities(text),
        ))),
        PartType::Html(html) => Some(normalize_email_spacers(
            &mail_parser::decoders::html::html_to_text(html),
        )),
        _ => None,
    }
}

fn decode_email_entities(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '&' {
            output.push(character);
            continue;
        }
        let mut token = String::from("&");
        let mut complete = false;
        while let Some(next) = characters.peek().copied() {
            if next == '&' || next.is_whitespace() {
                break;
            }
            token.push(next);
            characters.next();
            if next == ';' {
                complete = true;
                break;
            }
        }
        if complete {
            mail_parser::decoders::html::add_html_token(&mut output, token.as_bytes(), false);
        } else {
            output.push_str(&token);
        }
    }
    output
}

fn replace_email_break_tags(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '<' {
            output.push(character);
            continue;
        }
        let mut tag = String::new();
        let mut complete = false;
        while let Some(next) = characters.peek().copied() {
            if next == '<' || matches!(next, '\n' | '\r') {
                break;
            }
            characters.next();
            if next == '>' {
                complete = true;
                break;
            }
            tag.push(next);
        }
        if complete
            && tag
                .trim()
                .trim_end_matches('/')
                .trim()
                .eq_ignore_ascii_case("br")
        {
            output.push('\n');
        } else {
            output.push('<');
            output.push_str(&tag);
            if complete {
                output.push('>');
            }
        }
    }
    output
}

fn normalize_email_spacers(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut after_hair_space = false;
    for character in input.chars() {
        match character {
            '\u{034f}' | '\u{200b}' | '\u{feff}' => {}
            '\u{200a}' => {
                while output.ends_with([' ', '\t']) {
                    output.pop();
                }
                output.push(' ');
                after_hair_space = true;
            }
            ' ' | '\t' if after_hair_space => {}
            _ => {
                output.push(character);
                after_hair_space = false;
            }
        }
    }
    output
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "execution record indices are finite one-based diagnostic positions"
)]
fn execution_source(source: &VerifiedSource) -> Result<(ExecutionHeader, Vec<RawSpan>)> {
    let mut spans = Vec::new();
    let reader = BufReader::new(Cursor::new(&source.bytes));
    let mut header = None;
    let mut pending_codex_dialogue = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let value = parse_unique_json(
            line.as_bytes(),
            &format!("{} line {}", source.receipt.path, index + 1),
        )?;
        let format = if index == 0 {
            let format = execution_format(&value, &source.receipt.path)?;
            header = Some(ExecutionHeader {
                format,
                identity: session_identity(&value, format, &source.receipt.path)?,
                path: source.receipt.path.clone(),
            });
            format
        } else {
            header
                .as_ref()
                .map(|header| header.format)
                .context("execution history is missing a session header")?
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
    let header = header.with_context(|| {
        format!(
            "execution history {} is missing a session header",
            source.receipt.path
        )
    })?;
    Ok((header, spans))
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
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

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

    fn rewrite_package_unit(package: &Path, mutate: impl FnOnce(&mut EvidenceUnit)) {
        let mut manifest: EvidencePackageManifest =
            read_json(&package.join("manifest.json")).unwrap();
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
        let mut manifest: EvidencePackageManifest =
            read_json(&package.join("manifest.json")).unwrap();
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
        unit.unit_sha256 = digest(&unit.canonical_bytes().unwrap());
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
        unit.unit_sha256 = digest(&unit.canonical_bytes().unwrap());
        manifest.units[0].unit_sha256.clone_from(&unit.unit_sha256);
        write_private(&unit_path, serde_json::to_vec(&unit).unwrap().as_slice());
        write_private(
            &package.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap().as_slice(),
        );
        assert!(validate(&package).is_err());
    }

    #[test]
    fn validation_and_loading_accept_evidence_v1_and_v2() {
        let (temp, source, assignments, checksums) = markdown_fixture();
        let package_v1 = temp.path().join("package-v1");
        compile(&assignments, &source, &checksums, &package_v1).unwrap();
        rewrite_package_schema_version(&package_v1, 1);
        let units = load_package(&package_v1).unwrap();
        assert_eq!(units[0].schema_version, 1);
        assert!(units[0].attachments.is_empty());

        let (temp, source, assignments, checksums, manifest) = attachment_fixture();
        let package_v2 = temp.path().join("package-v2");
        compile_with_email_attachments(
            &assignments,
            &source,
            &checksums,
            std::slice::from_ref(&manifest),
            &package_v2,
        )
        .unwrap();
        rewrite_package_schema_version(&package_v2, 2);
        let units = load_package(&package_v2).unwrap();
        assert!(units.iter().all(|unit| unit.schema_version == 2));
        assert!(
            units
                .iter()
                .flat_map(|unit| &unit.attachments)
                .all(|attachment| attachment.source.is_some() && attachment.error.is_none())
        );
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
            "schema_version": 2,
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
            "attachments": [],
            "unit_sha256": "2".repeat(64)
        });
        assert!(matches_schema("evidence-unit.schema.json", &unit));
        let mut invalid_v2 = unit.clone();
        invalid_v2["source_type"] = "conversation-email".into();
        invalid_v2["source_locator"] = json!({"file":"mail.mbox","thread_id":"100"});
        invalid_v2["attachments"] = json!([{
            "id":"a000001",
            "span_id":"s000001",
            "locator":"mail.mbox#message=1;thread=100;part=2",
            "filename":null,
            "media_type":"text/html",
            "disposition":"attachment",
            "content_id":null,
            "source":{"path":"artifact","sha256":"0".repeat(64),"bytes":1},
            "error":ATTACHMENT_DECODE_ERROR
        }]);
        assert!(!matches_schema("evidence-unit.schema.json", &invalid_v2));
        unit["unexpected"] = true.into();
        assert!(!matches_schema("evidence-unit.schema.json", &unit));
        assert!(matches_schema(
            "package-inspection.schema.json",
            &serde_json::to_value(PackageInspection {
                schema_version: EVIDENCE_SCHEMA_VERSION,
                units: 1,
                source_types: BTreeMap::from([("canonical-markdown".into(), 1)]),
                source_files: 1,
                spans: 3,
                attachments: 0,
                materialized_attachments: 0,
                unavailable_attachments: 0,
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
        let attachments = [];
        assert_eq!(
            digest(
                &serde_json::to_vec(&EvidenceUnitCore {
                    schema_version: EVIDENCE_SCHEMA_VERSION,
                    unit_id: "unit:é",
                    source_type: "canonical-markdown",
                    source_locator: &source_locator,
                    metadata: &metadata,
                    sources: &sources,
                    spans: &spans,
                    attachments: &attachments,
                })
                .unwrap()
            ),
            "a3cefec463d9c9e84e22149cff7daac493f4819e1c2019c1a0948663e997097c"
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
        let mailbox = b"From ada@example.test Sat Jan 01 00:00:00 2022\nX-GM-THRID: 42\nFrom: Ada <ada@example.test>\nSubject: Fictional note\nDate: Sat, 1 Jan 2022 00:00:00 +0000\nContent-Type: text/plain; charset=utf-8\n\nR&D Complete&#847;&hairsp;&#8202;fictional 1 < 2<br>evidence.\nItem      Qty\n  coat     1\nFrom ada@example.test Sun Jan 02 00:00:00 2022\nX-GM-THRID: 42\nFrom: Ada <ada@example.test>\nSubject: Fictional HTML note\nDate: Sun, 2 Jan 2022 00:00:00 +0000\nContent-Type: text/html; charset=utf-8\n\n<p>Show &amp;lt;br&amp;gt;</p>\n";
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
                .contains("R&D Complete fictional 1 < 2\nevidence.\nItem      Qty\n  coat     1")
        );
        assert!(units[0].spans[1].text.contains("Show &lt;br&gt;"));

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

    fn attachment_mailbox() -> Vec<u8> {
        concat!(
            "From ada@example.test Sat Jan 01 00:00:00 2022\n",
            "X-GM-THRID: 100\n",
            "From: Ada <ada@example.test>\n",
            "Subject: Fictional multipart one\n",
            "Date: Sat, 1 Jan 2022 00:00:00 +0000\n",
            "MIME-Version: 1.0\n",
            "Content-Type: multipart/mixed; boundary=outer-one\n\n",
            "--outer-one\n",
            "Content-Type: text/plain; charset=utf-8\n\n",
            "First fictional message.\n",
            "--outer-one\n",
            "Content-Type: application/pdf\n",
            "Content-Disposition: attachment; filename=first.pdf\n",
            "Content-Transfer-Encoding: base64\n\n",
            "JVBERi1maWN0aW9uYWw=\n",
            "--outer-one\n",
            "Content-Type: multipart/related; boundary=inner-one\n\n",
            "--inner-one\n",
            "Content-Type: text/html; charset=utf-8\n\n",
            "<p>Inline fictional message.</p>\n",
            "--inner-one\n",
            "Content-Type: image/jpeg\n",
            "Content-Disposition: inline\n",
            "Content-ID: <fictional-image>\n",
            "Content-Transfer-Encoding: quoted-printable\n\n",
            "=FF=D8=FFfictional\n",
            "--inner-one\n",
            "Content-Type: application/octet-stream\n",
            "Content-Disposition: attachment\n",
            "Content-Transfer-Encoding: base64\n\n",
            "\n",
            "--inner-one\n",
            "Content-Type: text/plain\n",
            "Content-Disposition: attachment; filename=quoted.txt\n",
            "Content-Transfer-Encoding: quoted-printable\n\n",
            "same=20bytes\n",
            "--inner-one--\n",
            "--outer-one--\n",
            "From grace@example.test Sun Jan 02 00:00:00 2022\n",
            "X-GM-THRID: 200\n",
            "From: Grace <grace@example.test>\n",
            "Subject: Fictional multipart two\n",
            "Date: Sun, 2 Jan 2022 00:00:00 +0000\n",
            "MIME-Version: 1.0\n",
            "Content-Type: multipart/mixed; boundary=outer-two\n\n",
            "--outer-two\n",
            "Content-Type: text/plain; charset=utf-8\n\n",
            "Second fictional message.\n",
            "--outer-two\n",
            "Content-Type: application/pdf\n",
            "Content-Disposition: attachment; filename=renamed.pdf\n",
            "Content-Transfer-Encoding: base64\n\n",
            "JVBERi1maWN0aW9uYWw=\n",
            "--outer-two\n",
            "Content-Type: application/pdf\n",
            "Content-Disposition: attachment; filename=first.pdf\n",
            "Content-Transfer-Encoding: base64\n\n",
            "JVBERi1kaWZmZXJlbnQ=\n",
            "--outer-two\n",
            "Content-Type: application/octet-stream\n",
            "Content-Disposition: attachment; filename=\"../unsafe/path-\u{1}-雪.bin\"\n",
            "Content-Transfer-Encoding: base64\n\n",
            "c2FmZS1ieXRlcw==\n",
            "--outer-two--\n",
            "From ada@example.test Mon Jan 03 00:00:00 2022\n",
            "X-GM-THRID: 100\n",
            "From: Ada <ada@example.test>\n",
            "Subject: Fictional follow-up\n",
            "Date: Mon, 3 Jan 2022 00:00:00 +0000\n",
            "MIME-Version: 1.0\n",
            "Content-Type: multipart/mixed; boundary=outer-three\n\n",
            "--outer-three\n",
            "Content-Type: text/plain; charset=utf-8\n\n",
            "Follow-up in the first fictional thread.\n",
            "--outer-three\n",
            "Content-Type: application/pdf\n",
            "Content-Disposition: attachment; filename=third.pdf\n",
            "Content-Transfer-Encoding: base64\n\n",
            "JVBERi1maWN0aW9uYWw=\n",
            "--outer-three--\n"
        )
        .as_bytes()
        .to_vec()
    }

    fn attachment_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
        let (temp, source, assignments, checksums) = frontend_fixture(
            "email:100",
            "conversation-email",
            &json!({"file":"mail.mbox","thread_id":"100"}),
        );
        let mailbox = attachment_mailbox();
        write_source(&source, &checksums, "mail.mbox", &mailbox);
        let mut assignment: Value = read_json(&assignments).unwrap();
        assignment["units"].as_array_mut().unwrap().push(json!({
            "unit_id":"email:200",
            "source_type":"conversation-email",
            "locator":{"file":"mail.mbox","thread_id":"200"},
            "metadata":{}
        }));
        write_private(&assignments, &serde_json::to_vec(&assignment).unwrap());
        let manifest = temp.path().join("attachments.json");
        email_attachments::materialize(
            &source,
            Path::new("mail.mbox"),
            Path::new("_artifacts/sha256"),
            &manifest,
        )
        .unwrap();
        (temp, source, assignments, checksums, manifest)
    }

    fn malformed_attachment_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
        let (temp, source, assignments, checksums) = frontend_fixture(
            "email:100",
            "conversation-email",
            &json!({"file":"mail.mbox","thread_id":"100"}),
        );
        let mailbox = b"From ada@example.test Sat Jan 01 00:00:00 2022\nX-GM-THRID: 100\nFrom: Ada <ada@example.test>\nSubject: Fictional malformed attachment\nDate: Sat, 1 Jan 2022 00:00:00 +0000\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=bad\n\n--bad\nContent-Type: text/plain\n\nSearchable fictional parent text.\n--bad\nContent-Type: text/html\nContent-Disposition: attachment\nContent-Transfer-Encoding: base64\n\n%%%not-base64%%%\n--bad--\n";
        write_source(&source, &checksums, "mail.mbox", mailbox);
        let manifest = temp.path().join("malformed.json");
        email_attachments::materialize(
            &source,
            Path::new("mail.mbox"),
            Path::new("_artifacts/sha256"),
            &manifest,
        )
        .unwrap();
        (temp, source, assignments, checksums, manifest)
    }

    #[test]
    fn nested_multipart_attachment_materializes_only_leaf_parts() {
        let (temp, source, _assignments, checksums) = frontend_fixture(
            "email:300",
            "conversation-email",
            &json!({"file":"mail.mbox","thread_id":"300"}),
        );
        let mailbox = concat!(
            "From lin@example.test Tue Jan 04 00:00:00 2022\n",
            "X-GM-THRID: 300\n",
            "From: Lin <lin@example.test>\n",
            "Subject: Fictional nested multipart attachment\n",
            "Date: Tue, 4 Jan 2022 00:00:00 +0000\n",
            "MIME-Version: 1.0\n",
            "Content-Type: multipart/mixed; boundary=outer-fictional\n\n",
            "--outer-fictional\n",
            "Content-Type: text/plain; charset=utf-8\n\n",
            "Message body.\n",
            "--outer-fictional\n",
            "Content-Type: multipart/related; boundary=nested-fictional\n",
            "Content-Disposition: attachment; filename=bundle.mime\n\n",
            "--nested-fictional\n",
            "Content-Type: text/plain\n",
            "Content-Transfer-Encoding: quoted-printable\n\n",
            "Inherited=20attachment\n",
            "--nested-fictional\n",
            "Content-Type: image/jpeg\n",
            "Content-Disposition: inline; filename=preview.jpg\n",
            "Content-ID: <fictional-preview>\n",
            "Content-Transfer-Encoding: base64\n\n",
            "/9j/ZmljdGlvbmFs\n",
            "--nested-fictional--\n",
            "--outer-fictional--\n"
        );
        write_source(&source, &checksums, "mail.mbox", mailbox.as_bytes());
        let manifest_path = temp.path().join("nested-attachments.json");
        email_attachments::materialize(
            &source,
            Path::new("mail.mbox"),
            Path::new("_artifacts/sha256"),
            &manifest_path,
        )
        .unwrap();
        let manifest: Value = read_json(&manifest_path).unwrap();
        let parts = manifest["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0]["locator"],
            "mail.mbox#message=1;thread=300;part=2.1"
        );
        assert_eq!(parts[0]["disposition"], "attachment");
        assert_eq!(parts[0]["media_type"], "text/plain");
        assert!(parts[0]["filename"].is_null());
        assert_eq!(
            parts[1]["locator"],
            "mail.mbox#message=1;thread=300;part=2.2"
        );
        assert_eq!(parts[1]["disposition"], "inline");
        assert_eq!(parts[1]["filename"], "preview.jpg");
        assert_eq!(parts[1]["content_id"], "fictional-preview");

        let boundary = b"nested-fictional";
        for part in parts {
            let artifact = source.join(part["source"]["path"].as_str().unwrap());
            let bytes = fs::read(artifact).unwrap();
            assert!(
                !bytes
                    .windows(boundary.len())
                    .any(|window| window == boundary)
            );
        }
    }

    #[test]
    fn leafless_multipart_attachment_fails_closed() {
        let (temp, source, _assignments, checksums) = frontend_fixture(
            "email:301",
            "conversation-email",
            &json!({"file":"mail.mbox","thread_id":"301"}),
        );
        let mailbox = concat!(
            "From noa@example.test Wed Jan 05 00:00:00 2022\n",
            "X-GM-THRID: 301\n",
            "From: Noa <noa@example.test>\n",
            "Subject: Fictional empty multipart attachment\n",
            "Date: Wed, 5 Jan 2022 00:00:00 +0000\n",
            "MIME-Version: 1.0\n",
            "Content-Type: multipart/mixed; boundary=outer-empty\n\n",
            "--outer-empty\n",
            "Content-Type: text/plain\n\n",
            "Message body.\n",
            "--outer-empty\n",
            "Content-Type: multipart/mixed; boundary=empty-fictional\n",
            "Content-Disposition: attachment; filename=empty-bundle.mime\n\n",
            "--empty-fictional--\n",
            "--outer-empty--\n"
        );
        write_source(&source, &checksums, "mail.mbox", mailbox.as_bytes());
        let error = email_attachments::materialize(
            &source,
            Path::new("mail.mbox"),
            Path::new("_artifacts/sha256"),
            &temp.path().join("leafless.json"),
        )
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("classified multipart contains no leaf parts"));
        assert!(error.contains("mime_path=\"2\""));
        assert!(error.contains("disposition=attachment"));
        assert!(error.contains("filename=Some(\"empty-bundle.mime\")"));
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one end-to-end fixture verifies the ordered materialization and compilation contract"
    )]
    fn email_attachments_materialize_deduplicate_and_compile_exact_occurrences() {
        let (temp, source, assignments, checksums, manifest_path) = attachment_fixture();
        let before = fs::read(&manifest_path).unwrap();
        email_attachments::materialize(
            &source,
            Path::new("mail.mbox"),
            Path::new("_artifacts/sha256"),
            &manifest_path,
        )
        .unwrap();
        assert_eq!(fs::read(&manifest_path).unwrap(), before);
        let manifest: Value = read_json(&manifest_path).unwrap();
        assert_eq!(manifest["summary"]["parsed_messages"], 3);
        assert_eq!(manifest["summary"]["attachment_occurrences"], 8);
        assert_eq!(manifest["summary"]["unique_blobs"], 6);
        assert_eq!(manifest["summary"]["by_disposition"]["attachment"], 7);
        assert_eq!(manifest["summary"]["by_disposition"]["inline"], 1);
        assert_eq!(
            manifest["parts"][1]["locator"],
            "mail.mbox#message=1;thread=100;part=3.2"
        );
        assert_eq!(manifest["parts"][1]["content_id"], "fictional-image");
        assert!(manifest["parts"][2]["filename"].is_null());
        assert_eq!(manifest["parts"][3]["media_type"], "text/plain");
        assert_ne!(
            manifest["parts"][0]["source"],
            manifest["parts"][5]["source"]
        );
        assert!(
            manifest["parts"][6]["filename"]
                .as_str()
                .unwrap()
                .contains("../unsafe/path-")
        );
        assert!(matches_schema(
            "email-attachment-manifest.schema.json",
            &manifest
        ));
        assert!(matches_schema(
            "email-attachment-receipt.schema.json",
            &manifest["summary"]
        ));
        assert_eq!(
            fs::metadata(&manifest_path).unwrap().permissions().mode() & 0o077,
            0
        );
        let artifact_paths = manifest["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| part["source"]["path"].as_str().unwrap())
            .collect::<HashSet<_>>();
        for relative in artifact_paths {
            let path = source.join(relative);
            assert!(path.is_file());
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        }
        assert!(!source.join("unsafe").exists());

        let original_file = manifest["parts"][0]["source"]["path"].as_str().unwrap();
        let document = serde_json::to_vec(&json!({
            "body":{"children":[{"$ref":"#/texts/0"}]},
            "furniture":{"children":[]},
            "texts":[{
                "self_ref":"#/texts/0","children":[],"label":"paragraph",
                "text":"Externally extracted fictional document"
            }]
        }))
        .unwrap();
        write_private(&source.join("document.json"), &document);
        let mut assignment: Value = read_json(&assignments).unwrap();
        assignment["units"].as_array_mut().unwrap().push(json!({
            "unit_id":"docling:first-pdf",
            "source_type":"docling-json",
            "locator":{"file":"document.json","original_file":original_file},
            "metadata":{}
        }));
        write_private(&assignments, &serde_json::to_vec(&assignment).unwrap());
        let checksums_text = fs::read_to_string(&checksums).unwrap();
        write_private(
            &checksums,
            format!(
                "{checksums_text}{}  ./document.json\n{}  ./{original_file}\n",
                digest(&document),
                manifest["parts"][0]["source"]["sha256"].as_str().unwrap()
            )
            .as_bytes(),
        );

        let package = temp.path().join("package");
        assert!(
            compile(
                &assignments,
                &source,
                &checksums,
                &temp.path().join("missing-manifest")
            )
            .is_err()
        );
        compile_with_email_attachments(
            &assignments,
            &source,
            &checksums,
            std::slice::from_ref(&manifest_path),
            &package,
        )
        .unwrap();
        let units = load_package(&package).unwrap();
        assert_eq!(units[0].attachments.len(), 5);
        assert_eq!(units[0].attachments[0].span_id, "s000001");
        assert_eq!(
            units[0].attachments[1].locator,
            "mail.mbox#message=1;thread=100;part=3.2"
        );
        assert_eq!(
            units[0].attachments[1].disposition,
            AttachmentDisposition::Inline
        );
        assert_eq!(
            units[0].attachments[1].content_id.as_deref(),
            Some("fictional-image")
        );
        assert_eq!(units[0].attachments[4].span_id, "s000002");
        assert_eq!(
            units[0].attachments[4].locator,
            "mail.mbox#message=3;thread=100;part=2"
        );
        assert_eq!(units[0].sources.len(), 5);
        assert_eq!(units[1].attachments.len(), 3);
        assert_eq!(
            units[0].attachments[0].source,
            units[1].attachments[0].source
        );
        assert_ne!(
            units[0].attachments[0].filename,
            units[1].attachments[0].filename
        );
        assert_eq!(
            units[0].attachments[0].source.as_ref(),
            Some(&units[2].sources[1])
        );
        validate(&package).unwrap();

        let digest_path = manifest["parts"][0]["source"]["path"].as_str().unwrap();
        let staging_path = PathBuf::from(format!("{}.staging", source.join(digest_path).display()));
        write_private(&staging_path, b"interrupted staging");
        email_attachments::materialize(
            &source,
            Path::new("mail.mbox"),
            Path::new("_artifacts/sha256"),
            &manifest_path,
        )
        .unwrap();
        assert!(!staging_path.exists());

        let changed = b"changed planned artifact";
        write_private(&source.join(digest_path), changed);
        let changed_digest = digest(changed);
        let updated_checksums = fs::read_to_string(&checksums)
            .unwrap()
            .lines()
            .map(|line| {
                if line.ends_with(&format!("./{digest_path}")) {
                    format!("{changed_digest}  ./{digest_path}")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        write_private(&checksums, format!("{updated_checksums}\n").as_bytes());
        assert!(
            compile_with_email_attachments(
                &assignments,
                &source,
                &checksums,
                std::slice::from_ref(&manifest_path),
                &temp.path().join("contradictory-receipts"),
            )
            .is_err()
        );
    }

    #[test]
    fn email_attachment_artifact_failures_are_visible_and_leave_no_staging() {
        let (temp, source, assignments, checksums, manifest_path) = attachment_fixture();
        let manifest: Value = read_json(&manifest_path).unwrap();
        let artifact = source.join(manifest["parts"][0]["source"]["path"].as_str().unwrap());
        let saved = fs::read(&artifact).unwrap();
        fs::remove_file(&artifact).unwrap();
        assert!(
            compile_with_email_attachments(
                &assignments,
                &source,
                &checksums,
                std::slice::from_ref(&manifest_path),
                &temp.path().join("missing-package"),
            )
            .is_err()
        );
        write_private(&artifact, &saved);
        write_private(&artifact, b"tampered artifact bytes");
        let staging = PathBuf::from(format!("{}.staging", artifact.display()));
        write_private(&staging, b"interrupted staging");
        let error = email_attachments::materialize(
            &source,
            Path::new("mail.mbox"),
            Path::new("_artifacts/sha256"),
            &manifest_path,
        )
        .unwrap_err();
        let error = format!("{error:#}");
        for detail in [
            "source_path=\"mail.mbox\"",
            "message_ordinal=1",
            "thread_id=\"100\"",
            "mime_path=\"2\"",
            "media_type=\"application/pdf\"",
            "disposition=attachment",
            "filename=Some(\"first.pdf\")",
        ] {
            assert!(
                error.contains(detail),
                "missing failure detail {detail}: {error}"
            );
        }
        assert!(!staging.exists());
        assert!(
            compile_with_email_attachments(
                &assignments,
                &source,
                &checksums,
                std::slice::from_ref(&manifest_path),
                &temp.path().join("tampered-package"),
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_selected_attachment_compiles_as_unavailable() {
        let (temp, source, assignments, checksums, manifest) = malformed_attachment_fixture();
        let package = temp.path().join("package");
        compile_with_email_attachments(
            &assignments,
            &source,
            &checksums,
            std::slice::from_ref(&manifest),
            &package,
        )
        .unwrap();
        compile_with_email_attachments(
            &assignments,
            &source,
            &checksums,
            std::slice::from_ref(&manifest),
            &package,
        )
        .unwrap();
        let units = load_package(&package).unwrap();
        let unit = &units[0];
        assert!(
            unit.spans
                .iter()
                .any(|span| span.text.contains("Searchable fictional parent text"))
        );
        assert_eq!(unit.sources.len(), 1);
        assert_eq!(unit.attachments.len(), 1);
        let attachment = &unit.attachments[0];
        assert_eq!(attachment.span_id, "s000001");
        assert_eq!(unit.spans[0].locator, "mail.mbox#message=1;thread=100");
        assert_eq!(attachment.locator, "mail.mbox#message=1;thread=100;part=2");
        assert!(attachment.filename.is_none());
        assert_eq!(attachment.media_type, "text/html");
        assert_eq!(attachment.disposition, AttachmentDisposition::Attachment);
        assert!(attachment.source.is_none());
        assert_eq!(attachment.error.as_deref(), Some(ATTACHMENT_DECODE_ERROR));
        let inspection = package_inspection(&units);
        assert_eq!(inspection.materialized_attachments, 0);
        assert_eq!(inspection.unavailable_attachments, 1);
    }

    #[test]
    fn evidence_v3_rejects_invalid_attachment_availability() {
        let (temp, source, assignments, checksums, manifest) = malformed_attachment_fixture();
        let compile_package = |name: &str| {
            let package = temp.path().join(name);
            compile_with_email_attachments(
                &assignments,
                &source,
                &checksums,
                std::slice::from_ref(&manifest),
                &package,
            )
            .unwrap();
            package
        };

        let both = compile_package("both");
        rewrite_package_unit(&both, |unit| {
            unit.attachments[0].source = Some(unit.sources[0].clone());
        });
        assert!(validate(&both).is_err());

        let neither = compile_package("neither");
        rewrite_package_unit(&neither, |unit| {
            unit.attachments[0].error = None;
        });
        assert!(validate(&neither).is_err());

        let unknown = compile_package("unknown");
        rewrite_package_unit(&unknown, |unit| {
            unit.attachments[0].error = Some("fictional-error".into());
        });
        assert!(validate(&unknown).is_err());
    }

    #[test]
    fn evidence_v3_rejects_invalid_attachment_relationships() {
        let (temp, source, assignments, checksums, manifest_path) = attachment_fixture();
        let compile_package = |name: &str| {
            let package = temp.path().join(name);
            compile_with_email_attachments(
                &assignments,
                &source,
                &checksums,
                std::slice::from_ref(&manifest_path),
                &package,
            )
            .unwrap();
            package
        };

        let duplicate = compile_package("duplicate-id");
        rewrite_package_unit(&duplicate, |unit| {
            let id = unit.attachments[0].id.clone();
            unit.attachments[1].id = id;
        });
        assert!(validate(&duplicate).is_err());

        let invalid_parent = compile_package("invalid-parent");
        rewrite_package_unit(&invalid_parent, |unit| {
            unit.attachments[0].span_id = "s999999".into();
        });
        assert!(validate(&invalid_parent).is_err());

        let missing_source = compile_package("missing-source");
        rewrite_package_unit(&missing_source, |unit| {
            unit.sources.pop();
        });
        assert!(validate(&missing_source).is_err());

        let mismatched_digest = compile_package("mismatched-digest");
        rewrite_package_unit(&mismatched_digest, |unit| {
            unit.attachments[0].source.as_mut().unwrap().sha256 = "0".repeat(64);
        });
        assert!(validate(&mismatched_digest).is_err());

        let unsafe_path = compile_package("unsafe-path");
        rewrite_package_unit(&unsafe_path, |unit| {
            unit.attachments[0].source.as_mut().unwrap().path = "../outside".into();
        });
        assert!(validate(&unsafe_path).is_err());

        let unknown = compile_package("unknown-field");
        let manifest: EvidencePackageManifest = read_json(&unknown.join("manifest.json")).unwrap();
        let unit_path = unknown.join(&manifest.units[0].path);
        let mut unit: Value = read_json(&unit_path).unwrap();
        unit["attachments"][0]["unexpected"] = true.into();
        write_private(&unit_path, &serde_json::to_vec(&unit).unwrap());
        assert!(validate(&unknown).is_err());
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
