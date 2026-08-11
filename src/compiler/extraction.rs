#[cfg(test)]
use super::PARSED_SOURCE_PASSES;
use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};

use super::{
    AssignedUnit, Attachment, ConversationRow, ExtractionContext, PlannedUnit, RawAttachment,
    RawSpan, SourceExtraction, SourceFile, SourceRole, SourceUse, Span, VerifiedSource,
    chatgpt::chatgpt_source_extractions,
    conversation_table::{conversation_rows, conversation_table_spans},
    docling::docling_spans,
    email::email_source_extractions,
    execution::execution_source,
    json_support::{digest, locator_str, parse_unique_json},
    markdown::markdown_spans,
};

#[expect(
    clippy::unreachable,
    reason = "source type and path cardinality are exhaustively validated before extraction"
)]
pub(super) fn extract_source(
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

pub(super) fn planned_assignment(units: &[PlannedUnit], index: usize) -> Result<&AssignedUnit> {
    units
        .get(index)
        .and_then(|unit| unit.unit.as_ref())
        .context("source use refers to a compiled or missing unit")
}

pub(super) fn number_spans(raw: Vec<RawSpan>) -> Result<Vec<Span>> {
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

pub(super) fn number_attachments(
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
