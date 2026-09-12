use std::collections::HashSet;

use anyhow::{Context, Result, ensure};

use super::super::{
    AssignedUnit, EVIDENCE_SCHEMA_VERSION, EvidenceUnit, ExecutionHeader, SourceExtraction,
    SourceFile, SourceRole,
};

mod spans;

use spans::{number_attachments, number_spans};

pub(super) struct PlannedUnit {
    assignment: Option<AssignedUnit>,
    sources: Vec<SourceSlot>,
    identities: HashSet<(u64, u64)>,
    remaining_sources: usize,
}

struct SourceSlot {
    role: SourceRole,
    receipt: Option<SourceFile>,
    extraction: Option<SourceExtraction>,
}

impl PlannedUnit {
    pub(super) fn new(assignment: AssignedUnit, roles: Vec<SourceRole>) -> Self {
        Self {
            assignment: Some(assignment),
            remaining_sources: roles.len(),
            sources: roles
                .into_iter()
                .map(|role| SourceSlot {
                    role,
                    receipt: None,
                    extraction: None,
                })
                .collect(),
            identities: HashSet::new(),
        }
    }

    pub(super) fn assignment(&self) -> Result<&AssignedUnit> {
        self.assignment
            .as_ref()
            .context("source use refers to a compiled or missing unit")
    }

    pub(super) fn record_source(
        &mut self,
        index: usize,
        receipt: SourceFile,
        identity: (u64, u64),
    ) -> Result<bool> {
        ensure!(
            self.identities.insert(identity),
            "source paths resolve to the same file: {}",
            receipt.path
        );
        let slot = self
            .sources
            .get_mut(index)
            .context("source receipt index is outside the compile plan")?;
        ensure!(
            slot.receipt.replace(receipt).is_none(),
            "source receipt was assigned twice"
        );
        self.remaining_sources = self
            .remaining_sources
            .checked_sub(1)
            .context("unit source count underflowed")?;
        Ok(self.remaining_sources == 0)
    }

    pub(super) fn record_extraction(&mut self, extraction: SourceExtraction) -> Result<()> {
        let slot = self
            .sources
            .get_mut(extraction.source_index)
            .context("source extraction index is outside the compile plan")?;
        ensure!(
            slot.role != SourceRole::ReceiptOnly,
            "receipt-only source produced evidence"
        );
        ensure!(
            slot.extraction.replace(extraction).is_none(),
            "source was extracted twice"
        );
        Ok(())
    }

    pub(super) fn finish(&mut self) -> Result<EvidenceUnit> {
        ensure!(self.remaining_sources == 0, "unit still has unread sources");
        let unit = self
            .assignment
            .take()
            .context("assigned unit was already compiled")?;
        let mut sources = Vec::with_capacity(self.sources.len());
        let mut raw_spans = Vec::new();
        let mut raw_attachments = Vec::new();
        let mut execution_headers = Vec::new();
        for slot in std::mem::take(&mut self.sources) {
            sources.push(slot.receipt.context("assigned source has no receipt")?);
            if slot.role == SourceRole::ReceiptOnly {
                ensure!(
                    slot.extraction.is_none(),
                    "receipt-only source produced evidence"
                );
                continue;
            }
            let extraction = slot
                .extraction
                .context("assigned source has no extraction")?;
            raw_spans.extend(extraction.raw_spans);
            raw_attachments.extend(extraction.raw_attachments);
            execution_headers.push(extraction.execution_header);
        }
        if unit.source_type == "execution-history" {
            validate_execution_headers(&execution_headers)?;
        }
        let spans = number_spans(raw_spans)?;
        ensure!(!spans.is_empty(), "unit {} produced no spans", unit.unit_id);
        let attachments = number_attachments(raw_attachments, &spans, &mut sources)?;
        ensure!(
            unit.source_type == "conversation-email" || attachments.is_empty(),
            "non-email unit produced attachments"
        );
        Ok(EvidenceUnit {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            unit_id: unit.unit_id,
            source_type: unit.source_type,
            source_locator: unit.locator,
            metadata: unit.metadata,
            sources,
            spans,
            attachments,
            unit_sha256: String::new(),
        })
    }
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
