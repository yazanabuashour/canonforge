use anyhow::{Context, Result, ensure};

use super::{ConversationRow, RawSpan, VerifiedSource};

pub(super) fn conversation_table_spans(
    conversation_id: &str,
    source: &VerifiedSource,
    rows: Option<&[&ConversationRow]>,
) -> Result<Vec<RawSpan>> {
    let spans = rows
        .unwrap_or_default()
        .iter()
        .map(|row| RawSpan {
            locator: format!(
                "{}#record={};conversation_id={}",
                source.receipt.path,
                row.record,
                encode_unit_component(conversation_id)
            ),
            role: Some(if row.speaker.is_empty() {
                "unknown".into()
            } else {
                row.speaker.clone()
            }),
            timestamp: Some(format!("relative:{}", row.time)),
            text: row.message.clone(),
        })
        .collect::<Vec<_>>();
    ensure!(
        !spans.is_empty(),
        "conversation {conversation_id} was not found in {}",
        source.receipt.path
    );
    Ok(spans)
}

#[expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "header indices and one-based row numbers are validated before CSV field access"
)]
pub(super) fn conversation_rows(bytes: &[u8], label: &str) -> Result<Vec<ConversationRow>> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(bytes);
    let headers = reader
        .headers()
        .with_context(|| format!("read headers from {label}"))?;
    let actual = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            if index == 0 {
                header.trim_start_matches('\u{feff}')
            } else {
                header
            }
        })
        .collect::<Vec<_>>();
    ensure!(
        actual == ["thread", "time", "speaker", "message"],
        "unsupported conversation table headers in {label}: expected thread,time,speaker,message"
    );
    let mut rows = Vec::new();
    for (index, record) in reader.records().enumerate() {
        let record = record.with_context(|| format!("read record {} from {label}", index + 2))?;
        ensure!(
            record.len() == 4,
            "record {} in {label} does not have four fields",
            index + 2
        );
        if record.iter().all(str::is_empty) {
            continue;
        }
        let thread = record[0].to_owned();
        ensure!(
            !thread.is_empty(),
            "record {} in {label} has no thread identity",
            index + 2
        );
        rows.push(ConversationRow {
            record: index + 2,
            thread,
            time: record[1].to_owned(),
            speaker: record[2].to_owned(),
            message: record[3].to_owned(),
        });
    }
    Ok(rows)
}

#[expect(
    clippy::as_conversions,
    clippy::format_push_string,
    reason = "percent encoding widens ASCII bytes to their numeric hex representation"
)]
pub(super) fn encode_unit_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}
