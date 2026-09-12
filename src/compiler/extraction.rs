#[cfg(test)]
use super::PARSED_SOURCE_PASSES;
use std::collections::HashMap;

use anyhow::{Context, Result, bail};

use super::{
    ConversationRow, ExtractionContext, ExtractionRequest, SourceExtraction, SourceRole,
    VerifiedSource,
    chatgpt::chatgpt_source_extractions,
    conversation_table::{conversation_rows, conversation_table_spans},
    docling::docling_spans,
    email::email_source_extractions,
    execution::execution_source,
    json_support::{locator_str, parse_unique_json},
    markdown::markdown_spans,
};

pub(super) fn extract_source(
    role: SourceRole,
    source: &VerifiedSource,
    context: &mut ExtractionContext<'_, '_>,
    requests: &[ExtractionRequest<'_>],
) -> Result<Vec<SourceExtraction>> {
    #[cfg(test)]
    PARSED_SOURCE_PASSES.set(PARSED_SOURCE_PASSES.get().saturating_add(1));
    match role {
        SourceRole::Markdown => {
            let text =
                std::str::from_utf8(&source.bytes).context("Markdown source must be UTF-8")?;
            let lines = text.lines().collect::<Vec<_>>();
            requests
                .iter()
                .map(|request| {
                    Ok(SourceExtraction {
                        unit_index: request.unit_index,
                        source_index: request.source_index,
                        raw_spans: markdown_spans(request.assignment, source, &lines)?,
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
            requests
                .iter()
                .map(|request| {
                    let conversation_id =
                        locator_str(&request.assignment.locator, "conversation_id")?;
                    Ok(SourceExtraction {
                        unit_index: request.unit_index,
                        source_index: request.source_index,
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
        SourceRole::ChatGpt => chatgpt_source_extractions(source, requests),
        SourceRole::Email => email_source_extractions(source, context, requests),
        SourceRole::Docling => {
            let document = parse_unique_json(&source.bytes, &source.receipt.path)?;
            let spans = docling_spans(source, &document)?;
            Ok(requests
                .iter()
                .map(|request| SourceExtraction {
                    unit_index: request.unit_index,
                    source_index: request.source_index,
                    raw_spans: spans.clone(),
                    raw_attachments: Vec::new(),
                    execution_header: None,
                })
                .collect())
        }
        SourceRole::Execution => {
            let (header, spans) = execution_source(source)?;
            Ok(requests
                .iter()
                .map(|request| SourceExtraction {
                    unit_index: request.unit_index,
                    source_index: request.source_index,
                    raw_spans: spans.clone(),
                    raw_attachments: Vec::new(),
                    execution_header: Some(header.clone()),
                })
                .collect())
        }
        SourceRole::ReceiptOnly => bail!("receipt-only source cannot be parsed"),
    }
}
