use anyhow::{Context, Result, ensure};

use super::{AssignedUnit, RawSpan, VerifiedSource, json_support::locator_usize};

#[expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "line byte offsets are derived from the same buffer and checked before slicing"
)]
pub(super) fn markdown_spans(
    unit: &AssignedUnit,
    source: &VerifiedSource,
    lines: &[&str],
) -> Result<Vec<RawSpan>> {
    let start = locator_usize(&unit.locator, "line")?;
    ensure!(
        start > 0 && start <= lines.len(),
        "Markdown line is out of range"
    );
    let level = heading_level(lines[start - 1]).context("Markdown locator is not a heading")?;
    let mut end = lines.len();
    let mut fence = None;
    for (index, line) in lines.iter().enumerate().skip(start) {
        if let Some((marker, minimum)) = fence {
            if closes_fence(line, marker, minimum) {
                fence = None;
            }
            continue;
        }
        if let Some(opening) = opening_fence(line) {
            fence = Some(opening);
            continue;
        }
        if heading_level(line).is_some_and(|candidate| candidate <= level) {
            end = index;
            break;
        }
    }
    let mut spans = Vec::new();
    let mut block_start = start - 1;
    while block_start < end {
        while block_start < end && lines[block_start].trim().is_empty() {
            block_start += 1;
        }
        if block_start == end {
            break;
        }
        let mut block_end = block_start;
        let mut block_fence = None;
        while block_end < end {
            let line = lines[block_end];
            if let Some((marker, minimum)) = block_fence {
                if closes_fence(line, marker, minimum) {
                    block_fence = None;
                }
                block_end += 1;
                continue;
            }
            if let Some(opening) = opening_fence(line) {
                block_fence = Some(opening);
                block_end += 1;
                continue;
            }
            if line.trim().is_empty() {
                break;
            }
            block_end += 1;
        }
        spans.push(RawSpan {
            locator: format!(
                "{}#line={}-{}",
                source.receipt.path,
                block_start + 1,
                block_end
            ),
            role: Some("document".into()),
            timestamp: None,
            text: lines[block_start..block_end].join("\n"),
        });
        block_start = block_end;
    }
    Ok(spans)
}

fn heading_level(line: &str) -> Option<usize> {
    let count = line.bytes().take_while(|byte| *byte == b'#').count();
    (count > 0 && line.as_bytes().get(count) == Some(&b' ')).then_some(count)
}

fn fence_run(line: &str) -> Option<(u8, usize, &[u8])> {
    let bytes = line.as_bytes();
    let indentation = bytes.iter().take_while(|byte| **byte == b' ').count();
    if indentation > 3 {
        return None;
    }
    let content = bytes.get(indentation..)?;
    let marker = *content.first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let length = content.iter().take_while(|byte| **byte == marker).count();
    if length < 3 {
        return None;
    }
    Some((marker, length, content.get(length..)?))
}

fn opening_fence(line: &str) -> Option<(u8, usize)> {
    let (marker, length, suffix) = fence_run(line)?;
    (marker != b'`' || !suffix.contains(&b'`')).then_some((marker, length))
}

fn closes_fence(line: &str, marker: u8, minimum: usize) -> bool {
    fence_run(line).is_some_and(|(candidate, length, suffix)| {
        candidate == marker
            && length >= minimum
            && suffix.iter().all(|byte| matches!(byte, b' ' | b'\t'))
    })
}
