use anyhow::{Context, Result};

use super::{
    super::{
        ExtractionContext, ExtractionRequest, email_attachments::EmailAttachmentManifests,
        extraction::extract_source, source_receipts::SourceReceipts,
    },
    planning::SourcePlan,
    unit::PlannedUnit,
};

pub(super) fn process_source(
    plan: &SourcePlan,
    receipts: &mut SourceReceipts<'_>,
    attachment_manifests: &EmailAttachmentManifests,
    units: &mut [PlannedUnit],
) -> Result<Vec<usize>> {
    let (source, identity) = receipts.read(&plan.path, !plan.parsers.is_empty())?;
    let mut ready = Vec::new();
    for source_use in &plan.uses {
        let unit = units
            .get_mut(source_use.unit_index)
            .context("source use unit index is outside the compile plan")?;
        if unit.record_source(source_use.source_index, source.receipt.clone(), identity)? {
            ready.push(source_use.unit_index);
        }
    }
    for &parser in &plan.parsers {
        let requests = plan
            .uses
            .iter()
            .filter(|source_use| source_use.role == parser)
            .map(|source_use| {
                let unit = units
                    .get(source_use.unit_index)
                    .context("source use unit index is outside the compile plan")?;
                Ok(ExtractionRequest {
                    unit_index: source_use.unit_index,
                    source_index: source_use.source_index,
                    assignment: unit.assignment()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut context = ExtractionContext {
            attachment_manifests,
            receipts,
        };
        for extraction in extract_source(parser, &source, &mut context, &requests)? {
            units
                .get_mut(extraction.unit_index)
                .context("source extraction unit index is outside the compile plan")?
                .record_extraction(extraction)?;
        }
    }
    Ok(ready)
}
