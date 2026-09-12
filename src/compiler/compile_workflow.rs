use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::protected_fs::{PrivateDirectory, ensure_output_separate, open_private_bound_directory};

mod planning;
mod sources;
mod unit;

use planning::compile_plan;
use sources::process_source;

use super::{
    Assignment, SOURCE_ASSIGNMENT_SCHEMA, SourceRole,
    email_attachments::{self, EmailAttachmentManifests},
    json_support::{contract_validator, read_validated_json},
    package::PackageWriter,
    source_receipts::SourceReceipts,
};

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
    let mut writer = PackageWriter::new(staging, &assignment.units)?;
    let (mut units, sources) = compile_plan(assignment.units)?;
    let email_sources = sources
        .iter()
        .filter(|plan| plan.parsers.contains(&SourceRole::Email))
        .map(|plan| plan.path.clone())
        .collect::<HashSet<_>>();
    attachment_manifests.reject_unused(&email_sources)?;
    let mut receipts = SourceReceipts::new(
        source_root.path(),
        checksums,
        sources.iter().map(|plan| plan.path.clone()).collect(),
    )?;
    for source in sources {
        for unit_index in process_source(&source, &mut receipts, &attachment_manifests, &mut units)?
        {
            let evidence = units
                .get_mut(unit_index)
                .context("ready unit index is outside the compile plan")?
                .finish()?;
            writer.write(unit_index, evidence)?;
        }
    }
    receipts.finish()?;
    writer.finish()
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
