use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::{lifecycle_text, record_span};
use crate::compiler::{
    EXCLUDED_PLATFORM_TEXT, OMITTED_IMAGE_TEXT, RecordSpan, json_support::scalar_text,
};

pub(super) fn codex_record(
    value: &Value,
    before_world_state: bool,
    mirrored_event: bool,
) -> Result<Vec<RecordSpan>> {
    let top = value.get("type").and_then(Value::as_str).unwrap_or("");
    let payload = value.get("payload").unwrap_or(&Value::Null);
    let subtype = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let timestamp = value.get("timestamp").map(scalar_text);
    Ok(match (top, subtype) {
        ("event_msg", "user_message" | "agent_message") => record_span(
            if mirrored_event {
                "excluded-provider-mirror"
            } else if subtype == "user_message" {
                "user"
            } else {
                "assistant"
            },
            timestamp,
            payload
                .get("message")
                .and_then(Value::as_str)
                .context("execution event message is missing or not a string")?
                .to_owned(),
        ),
        ("response_item", "message") => codex_message(payload, timestamp, before_world_state)?,
        ("response_item", "agent_message") => codex_agent_message(payload, timestamp.as_deref())?,
        ("response_item", "function_call" | "custom_tool_call" | "tool_search_call") => {
            record_span("tool-call", timestamp, tool_event_text(payload, subtype))
        }
        (
            "response_item",
            "function_call_output"
            | "custom_tool_call_output"
            | "web_search_call_output"
            | "computer_call_output"
            | "local_shell_call_output"
            | "mcp_tool_call_output"
            | "tool_search_output",
        )
        | ("event_msg", "mcp_tool_call_end") => {
            record_span("tool-result", timestamp, tool_event_text(payload, subtype))
        }
        (
            "response_item",
            "web_search_call" | "computer_call" | "local_shell_call" | "mcp_tool_call",
        ) => record_span("tool-event", timestamp, tool_event_text(payload, subtype)),
        ("response_item", "reasoning") => record_span(
            "excluded-reasoning",
            timestamp,
            json!({"type": subtype}).to_string(),
        ),
        ("compacted" | "world_state" | "inter_agent_communication_metadata", _)
        | ("event_msg", "token_count") => Vec::new(),
        _ if top.contains("call")
            || top.contains("tool")
            || subtype.contains("call")
            || subtype.contains("tool") =>
        {
            bail!("unsupported execution tool record type top={top:?} subtype={subtype:?}")
        }
        ("event_msg", _) if !subtype.is_empty() => {
            record_span("lifecycle", timestamp, lifecycle_text(payload, subtype))
        }
        ("session_meta", _) => record_span(
            "metadata",
            timestamp,
            json!({
                "id": payload.get("id"),
                "session_id": payload.get("session_id"),
                "cwd": payload.get("cwd"),
                "timestamp": payload.get("timestamp"),
                "source": payload.get("source"),
                "git": payload.get("git"),
            })
            .to_string(),
        ),
        ("turn_context", _) => record_span(
            "metadata",
            timestamp,
            json!({
                "cwd": payload.get("cwd"),
                "current_date": payload.get("current_date"),
                "model": payload.get("model"),
            })
            .to_string(),
        ),
        _ => bail!("unsupported execution record type top={top:?} subtype={subtype:?}"),
    })
}

fn tool_event_text(payload: &Value, subtype: &str) -> String {
    json!({
        "type": subtype,
        "id": payload.get("id"),
        "call_id": payload.get("call_id"),
        "name": payload.get("name"),
        "server": payload.get("server"),
        "tool": payload.get("tool"),
        "tools": payload.get("tools"),
        "status": payload.get("status"),
        "arguments": payload.get("arguments"),
        "invocation": payload.get("invocation"),
        "input": payload.get("input"),
        "action": payload.get("action"),
        "query": payload.get("query"),
        "result": payload.get("result"),
        "error": payload.get("error"),
        "output": payload.get("output"),
    })
    .to_string()
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "execution content indices are finite one-based diagnostic positions"
)]
fn codex_message(
    payload: &Value,
    timestamp: Option<String>,
    before_world_state: bool,
) -> Result<Vec<RecordSpan>> {
    let role = payload
        .get("role")
        .and_then(Value::as_str)
        .context("execution message role is missing or invalid")?;
    if matches!(role, "system" | "developer") {
        return Ok(record_span(
            "excluded-platform-instruction",
            timestamp,
            EXCLUDED_PLATFORM_TEXT.into(),
        ));
    }
    ensure!(
        matches!(role, "user" | "assistant"),
        "unsupported execution message role {role:?}"
    );
    let content = payload
        .get("content")
        .and_then(Value::as_array)
        .context("execution user or assistant message content is missing or invalid")?;
    if content.is_empty() {
        return Ok(record_span(
            role,
            timestamp,
            json!({"type": "message"}).to_string(),
        ));
    }
    content
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .context("execution message content type is missing or invalid")?;
            let (item_role, text) = match item_type {
                "input_text" | "output_text" if before_world_state && role == "user" => (
                    "excluded-platform-instruction",
                    EXCLUDED_PLATFORM_TEXT.to_owned(),
                ),
                "input_text" | "output_text" => (
                    role,
                    item.get("text")
                        .and_then(Value::as_str)
                        .context("execution text content is missing or invalid")?
                        .to_owned(),
                ),
                "input_image" => ("omitted-asset", OMITTED_IMAGE_TEXT.to_owned()),
                _ => bail!("unsupported execution message content type {item_type:?}"),
            };
            Ok(RecordSpan {
                locator_suffix: format!(";content={}", index + 1),
                role: Some(item_role.into()),
                timestamp: timestamp.clone(),
                text,
            })
        })
        .collect()
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "agent message content indices are finite one-based diagnostic positions"
)]
fn codex_agent_message(payload: &Value, timestamp: Option<&str>) -> Result<Vec<RecordSpan>> {
    let content = payload
        .get("content")
        .and_then(Value::as_array)
        .context("execution agent message content is missing or invalid")?;
    ensure!(
        !content.is_empty(),
        "execution agent message content is empty"
    );
    content
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .context("execution agent message content type is missing or invalid")?;
            let (role, text) = match item_type {
                "input_text" => (
                    "assistant",
                    item.get("text")
                        .and_then(Value::as_str)
                        .context("execution agent message text is missing or invalid")?
                        .to_owned(),
                ),
                "input_image" => ("omitted-asset", OMITTED_IMAGE_TEXT.to_owned()),
                "encrypted_content" => (
                    "excluded-platform-instruction",
                    json!({"type": item_type}).to_string(),
                ),
                _ => bail!("unsupported execution agent message content type {item_type:?}"),
            };
            Ok(RecordSpan {
                locator_suffix: format!(";content={}", index + 1),
                role: Some(role.into()),
                timestamp: timestamp.map(str::to_owned),
                text,
            })
        })
        .collect()
}
