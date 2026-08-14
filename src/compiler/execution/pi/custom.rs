use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::super::record_span;
use crate::compiler::RecordSpan;

fn pi_custom_field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .get(key)
        .or_else(|| value.get("data").and_then(|data| data.get(key)))
        .or_else(|| value.get("details").and_then(|details| details.get(key)))
}

pub(super) fn pi_custom_record(
    value: &Value,
    timestamp: Option<String>,
) -> Result<Vec<RecordSpan>> {
    let custom_type = value
        .get("customType")
        .and_then(Value::as_str)
        .context("Pi custom record customType is missing or invalid")?;
    Ok(match custom_type {
        "summary-recap" => record_span(
            "excluded-reasoning",
            timestamp,
            json!({"type": custom_type}).to_string(),
        ),
        "web-search-content-ready" => record_span(
            "lifecycle",
            timestamp,
            json!({"type": custom_type}).to_string(),
        ),
        "btw-result" => record_span(
            "tool-result",
            timestamp,
            json!({
                "type": custom_type,
                "status": pi_custom_field(value, "status"),
                "title": pi_custom_field(value, "title"),
                "answer": pi_custom_field(value, "answer"),
                "error": pi_custom_field(value, "error")
                    .or_else(|| pi_custom_field(value, "errorText")),
            })
            .to_string(),
        ),
        "web-search-results" => pi_custom_result(value, timestamp)?,
        "background-terminal-result" | "subagent-result" => {
            let details = value
                .get("details")
                .and_then(Value::as_object)
                .context("Pi custom message result details are missing or invalid")?;
            let mut spans = vec![RecordSpan {
                locator_suffix: ";result".into(),
                role: Some("tool-result".into()),
                timestamp: timestamp.clone(),
                text: json!({
                    "type": custom_type,
                    "id": details.get("id"),
                    "status": details.get("status"),
                    "title": details.get("title"),
                    "exitCode": details.get("exitCode"),
                    "signal": details.get("signal"),
                })
                .to_string(),
            }];
            spans.extend(pi_custom_result(value, timestamp)?);
            spans
        }
        _ => bail!("unsupported Pi custom record type {custom_type:?}"),
    })
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "Pi custom content indices are finite one-based diagnostic positions"
)]
fn pi_custom_result(value: &Value, timestamp: Option<String>) -> Result<Vec<RecordSpan>> {
    if let Some(data) = value.get("data") {
        return Ok(record_span("tool-result", timestamp, data.to_string()));
    }
    let content = value
        .get("content")
        .context("Pi custom result has no data or content")?;
    if let Some(text) = content.as_str() {
        return Ok(vec![RecordSpan {
            locator_suffix: ";content=1".into(),
            role: Some("tool-result".into()),
            timestamp,
            text: text.to_owned(),
        }]);
    }
    let items = content
        .as_array()
        .context("Pi custom result content is not a string or array")?;
    ensure!(!items.is_empty(), "Pi custom result content is empty");
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .context("Pi custom result content type is missing or invalid")?;
            let (role, text) = match item_type {
                "text" => (
                    "tool-result",
                    item.get("text")
                        .and_then(Value::as_str)
                        .context("Pi custom result text is missing or invalid")?
                        .to_owned(),
                ),
                "image" => {
                    let mime_type = item
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .context("Pi custom result image mimeType is missing or invalid")?;
                    (
                        "omitted-asset",
                        json!({
                            "kind": "image",
                            "mimeType": mime_type,
                            "status": "not-materialized",
                        })
                        .to_string(),
                    )
                }
                _ => bail!("unsupported Pi custom result content type {item_type:?}"),
            };
            Ok(RecordSpan {
                locator_suffix: format!(";content={}", index + 1),
                role: Some(role.into()),
                timestamp: timestamp.clone(),
                text,
            })
        })
        .collect()
}
