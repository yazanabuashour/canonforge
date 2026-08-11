#[cfg(test)]
use super::VERIFIED_SOURCE_READS;
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    fs,
    io::{self, Write},
    os::unix::fs::DirBuilderExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::protected_fs::{
    PrivateDirectory, digest_bound_private_file, ensure_output_separate,
    open_private_bound_directory, private_staging_writer, read_bound_private_file, sync_directory,
};

use super::{
    AssignedUnit, Assignment, EVIDENCE_SCHEMA_VERSION, EVIDENCE_UNIT_SCHEMA, EvidencePackageEntry,
    EvidencePackageManifest, EvidenceUnit, ExecutionHeader, ExtractionContext, PlannedUnit,
    SOURCE_ASSIGNMENT_SCHEMA, SourceFile, SourcePlan, SourceRole, SourceUse, VerifiedSource,
    email_attachments::{self, EmailAttachmentManifests},
    extraction::{extract_source, number_attachments, number_spans},
    json_support::{contract_validator, digest, read_validated_json, validate_contract_value},
    package::{checksum_index, source_paths},
};

pub(super) fn write_staging_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut writer = private_staging_writer(path)?;
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.finish()
}

pub(super) fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
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
