use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};

use crate::compiler::{Attachment, RawAttachment, RawSpan, SourceFile, Span, json_support::digest};

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
