use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::record_span;
use crate::compiler::{RecordSpan, json_support::scalar_text};

pub(super) fn pi_record(value: &Value) -> Result<Vec<RecordSpan>> {
    let record_type = value
        .get("type")
        .and_then(Value::as_str)
        .context("Pi execution record type is missing or invalid")?;
    let timestamp = value.get("timestamp").map(scalar_text);
    Ok(match record_type {
        "session" => record_span(
            "metadata",
            timestamp,
            json!({
                "type": record_type,
                "version": value.get("version"),
                "id": value.get("id"),
                "cwd": value.get("cwd"),
                "parentSession": value.get("parentSession"),
            })
            .to_string(),
        ),
        "message" => pi_message(value, timestamp)?,
        "model_change" => record_span(
            "lifecycle",
            timestamp,
            json!({
                "type": record_type,
                "id": value.get("id"),
                "parentId": value.get("parentId"),
                "provider": value.get("provider"),
                "modelId": value.get("modelId"),
            })
            .to_string(),
        ),
        "thinking_level_change" => record_span(
            "lifecycle",
            timestamp,
            json!({
                "type": record_type,
                "id": value.get("id"),
                "parentId": value.get("parentId"),
                "thinkingLevel": value.get("thinkingLevel"),
            })
            .to_string(),
        ),
        "session_info" => record_span(
            "metadata",
            timestamp,
            json!({
                "type": record_type,
                "id": value.get("id"),
                "parentId": value.get("parentId"),
                "name": value.get("name"),
            })
            .to_string(),
        ),
        "compaction" => record_span(
            "lifecycle",
            timestamp,
            json!({
                "type": record_type,
                "id": value.get("id"),
                "parentId": value.get("parentId"),
                "firstKeptEntryId": value.get("firstKeptEntryId"),
                "tokensBefore": value.get("tokensBefore"),
            })
            .to_string(),
        ),
        "custom" | "custom_message" => pi_custom_record(value, timestamp)?,
        _ => bail!("unsupported Pi execution record type {record_type:?}"),
    })
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "Pi message content indices are finite one-based diagnostic positions"
)]
fn pi_message(value: &Value, timestamp: Option<String>) -> Result<Vec<RecordSpan>> {
    let message = value
        .get("message")
        .and_then(Value::as_object)
        .context("Pi message payload is missing or invalid")?;
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .context("Pi message role is missing or invalid")?;
    ensure!(
        matches!(role, "user" | "assistant" | "toolResult"),
        "unsupported Pi message role {role:?}"
    );
    let content = message
        .get("content")
        .context("Pi message content is missing")?;
    let mut spans = if role == "toolResult" {
        vec![RecordSpan {
            locator_suffix: ";result".into(),
            role: Some("tool-result".into()),
            timestamp: timestamp.clone(),
            text: json!({
                "toolCallId": message
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .context("Pi toolResult toolCallId is missing or invalid")?,
                "toolName": message
                    .get("toolName")
                    .and_then(Value::as_str)
                    .context("Pi toolResult toolName is missing or invalid")?,
                "isError": message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .context("Pi toolResult isError is missing or invalid")?,
            })
            .to_string(),
        }]
    } else {
        Vec::new()
    };
    if let Some(text) = content.as_str() {
        ensure!(role == "user", "Pi {role} message content must be an array");
        return Ok(vec![RecordSpan {
            locator_suffix: ";content=1".into(),
            role: Some("user".into()),
            timestamp,
            text: text.to_owned(),
        }]);
    }
    let items = content
        .as_array()
        .context("Pi message content is not a string or array")?;
    if items.is_empty() {
        ensure!(role == "assistant", "Pi {role} message content is empty");
        return Ok(vec![RecordSpan {
            locator_suffix: ";error".into(),
            role: Some("assistant".into()),
            timestamp,
            text: message
                .get("errorMessage")
                .and_then(Value::as_str)
                .context("Pi assistant message has empty content and no errorMessage")?
                .to_owned(),
        }]);
    }
    spans.extend(
        items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let (item_role, text) = pi_message_content(item, role)?;
                Ok(RecordSpan {
                    locator_suffix: format!(";content={}", index + 1),
                    role: Some(item_role.into()),
                    timestamp: timestamp.clone(),
                    text,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    );
    Ok(spans)
}

fn pi_message_content(item: &Value, role: &str) -> Result<(&'static str, String)> {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .context("Pi message content type is missing or invalid")?;
    match item_type {
        "text" => Ok((
            match role {
                "user" => "user",
                "assistant" => "assistant",
                "toolResult" => "tool-result",
                _ => bail!("unsupported Pi message role {role:?}"),
            },
            item.get("text")
                .and_then(Value::as_str)
                .context("Pi text content is missing or invalid")?
                .to_owned(),
        )),
        "image" => {
            ensure!(
                matches!(role, "user" | "toolResult"),
                "Pi image content is invalid for role {role:?}"
            );
            let mime_type = item
                .get("mimeType")
                .and_then(Value::as_str)
                .context("Pi image content mimeType is missing or invalid")?;
            Ok((
                "omitted-asset",
                json!({
                    "kind": "image",
                    "mimeType": mime_type,
                    "status": "not-materialized",
                })
                .to_string(),
            ))
        }
        "thinking" => {
            ensure!(
                role == "assistant",
                "Pi thinking content is invalid for role {role:?}"
            );
            Ok((
                "excluded-reasoning",
                json!({"type": "thinking"}).to_string(),
            ))
        }
        "toolCall" => {
            ensure!(
                role == "assistant",
                "Pi toolCall content is invalid for role {role:?}"
            );
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .context("Pi toolCall id is missing or invalid")?;
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .context("Pi toolCall name is missing or invalid")?;
            let arguments = item
                .get("arguments")
                .context("Pi toolCall arguments are missing")?;
            Ok((
                "tool-call",
                json!({"id": id, "name": name, "arguments": arguments}).to_string(),
            ))
        }
        _ => bail!("unsupported Pi message content type {item_type:?}"),
    }
}

fn pi_custom_field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .get(key)
        .or_else(|| value.get("data").and_then(|data| data.get(key)))
        .or_else(|| value.get("details").and_then(|details| details.get(key)))
}

fn pi_custom_record(value: &Value, timestamp: Option<String>) -> Result<Vec<RecordSpan>> {
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
