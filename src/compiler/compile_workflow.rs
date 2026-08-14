use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    os::unix::fs::DirBuilderExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::protected_fs::{
    PrivateDirectory, ensure_output_separate, open_private_bound_directory, private_staging_writer,
    sync_directory,
};

mod planning;
mod sources;

use planning::compile_plan;
use sources::process_source;

use super::{
    Assignment, EVIDENCE_SCHEMA_VERSION, EVIDENCE_UNIT_SCHEMA, EvidencePackageEntry,
    EvidencePackageManifest, EvidenceUnit, ExecutionHeader, PlannedUnit, SOURCE_ASSIGNMENT_SCHEMA,
    SourceRole,
    email_attachments::{self, EmailAttachmentManifests},
    extraction::{number_attachments, number_spans},
    json_support::{contract_validator, digest, read_validated_json, validate_contract_value},
    package::checksum_index,
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
