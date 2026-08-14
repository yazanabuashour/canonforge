#[cfg(test)]
use super::super::VERIFIED_SOURCE_READS;
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    path::Path,
};

use anyhow::{Context, Result, ensure};

use super::{
    super::{ExtractionContext, PlannedUnit, SourceFile, SourcePlan, VerifiedSource},
    safe_join,
};
use crate::{
    compiler::{
        email_attachments::EmailAttachmentManifests, extraction::extract_source,
        json_support::digest,
    },
    protected_fs::{digest_bound_private_file, read_bound_private_file},
};

pub(super) fn process_source(
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
