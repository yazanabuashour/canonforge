use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result, ensure};

use super::{
    AttachmentDisposition, AttachmentSummary, DispositionSummary, MANIFEST_SCHEMA_VERSION,
    ManifestPart, SourceFile,
};
use crate::compiler::compile_workflow::safe_join;
use crate::protected_fs::digest_bound_private_file;

pub(super) fn artifact_path(directory: &str, sha256: &str) -> Result<String> {
    let prefix = sha256.get(..2).context("artifact digest is too short")?;
    let path = format!("{directory}/{prefix}/{sha256}");
    safe_join(Path::new("."), &path)?;
    Ok(path)
}

pub(super) fn validate_artifact_receipt(source: &SourceFile, directory: &str) -> Result<()> {
    ensure!(
        source.sha256.len() == 64
            && source.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            && source.path == artifact_path(directory, &source.sha256)?,
        "invalid content-addressed artifact receipt {}",
        source.path
    );
    Ok(())
}

pub(in crate::compiler) fn verify_artifact(root: &Path, source: &SourceFile) -> Result<()> {
    let path = safe_join(root, &source.path)?;
    let found = digest_bound_private_file(&path)
        .with_context(|| format!("verify materialized email artifact {}", source.path))?;
    ensure!(
        found.sha256 == source.sha256 && found.bytes == source.bytes,
        "materialized email artifact {} digest mismatch: expected sha256 {} and {} bytes, found sha256 {} and {} bytes",
        source.path,
        source.sha256,
        source.bytes,
        found.sha256,
        found.bytes
    );
    Ok(())
}

pub(super) fn relative_utf8(path: &Path, label: &str) -> Result<String> {
    let value = path
        .to_str()
        .with_context(|| format!("{label} must be UTF-8"))?
        .to_owned();
    safe_join(Path::new("."), &value)?;
    Ok(value)
}
pub(super) fn summarize(parts: &[ManifestPart], parsed_messages: u64) -> Result<AttachmentSummary> {
    let mut summary = AttachmentSummary {
        schema_version: MANIFEST_SCHEMA_VERSION,
        parsed_messages,
        attachment_occurrences: u64::try_from(parts.len())
            .context("attachment occurrence count overflow")?,
        unique_blobs: 0,
        total_decoded_bytes: 0,
        unique_decoded_bytes: 0,
        duplicate_bytes_avoided: 0,
        by_media_type: BTreeMap::new(),
        by_disposition: DispositionSummary::default(),
        malformed_or_undecodable_parts: 0,
    };
    let mut unique = BTreeSet::new();
    for part in parts {
        let media = summary
            .by_media_type
            .entry(part.media_type.clone())
            .or_default();
        media.occurrences = media
            .occurrences
            .checked_add(1)
            .context("media occurrence count overflow")?;
        let (counter, overflow) = match part.disposition {
            AttachmentDisposition::Attachment => (
                &mut summary.by_disposition.attachment,
                "attachment count overflow",
            ),
            AttachmentDisposition::Inline => {
                (&mut summary.by_disposition.inline, "inline count overflow")
            }
        };
        *counter = counter.checked_add(1).context(overflow)?;
        if let Some(source) = &part.source {
            media.decoded_bytes = media
                .decoded_bytes
                .checked_add(source.bytes)
                .context("media decoded bytes overflow")?;
            summary.total_decoded_bytes = summary
                .total_decoded_bytes
                .checked_add(source.bytes)
                .context("total decoded bytes overflow")?;
            if unique.insert(&source.sha256) {
                summary.unique_blobs = summary
                    .unique_blobs
                    .checked_add(1)
                    .context("unique blob count overflow")?;
                summary.unique_decoded_bytes = summary
                    .unique_decoded_bytes
                    .checked_add(source.bytes)
                    .context("unique decoded bytes overflow")?;
            }
        } else {
            summary.malformed_or_undecodable_parts = summary
                .malformed_or_undecodable_parts
                .checked_add(1)
                .context("malformed part count overflow")?;
        }
    }
    summary.duplicate_bytes_avoided = summary
        .total_decoded_bytes
        .checked_sub(summary.unique_decoded_bytes)
        .context("unique decoded bytes exceed total decoded bytes")?;
    Ok(summary)
}
