use std::io::{BufRead, BufReader, Cursor};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{
    ExecutionHeader, RawSpan, RecordSpan, VerifiedSource, json_support::parse_unique_json,
};

mod codex;
mod pi;

use codex::codex_record;
use pi::pi_record;

#[expect(
    clippy::arithmetic_side_effects,
    reason = "execution record indices are finite one-based diagnostic positions"
)]
pub(super) fn execution_source(source: &VerifiedSource) -> Result<(ExecutionHeader, Vec<RawSpan>)> {
    let mut spans = Vec::new();
    let reader = BufReader::new(Cursor::new(&source.bytes));
    let mut header = None;
    let mut pending_codex_dialogue = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let value = parse_unique_json(
            line.as_bytes(),
            &format!("{} line {}", source.receipt.path, index + 1),
        )?;
        let format = if index == 0 {
            let format = execution_format(&value, &source.receipt.path)?;
            header = Some(ExecutionHeader {
                format,
                identity: session_identity(&value, format, &source.receipt.path)?,
                path: source.receipt.path.clone(),
            });
            format
        } else {
            header
                .as_ref()
                .map(|header| header.format)
                .context("execution history is missing a session header")?
        };
        if format == ExecutionFormat::Codex && is_codex_dialogue(&value) {
            pending_codex_dialogue.push((index, value));
            continue;
        }
        let before_world_state = format == ExecutionFormat::Codex
            && value.get("type").and_then(Value::as_str) == Some("world_state");
        flush_codex_dialogue(
            &mut spans,
            &source.receipt.path,
            &mut pending_codex_dialogue,
            before_world_state,
        )?;
        append_execution_records(
            &mut spans,
            &source.receipt.path,
            index,
            match format {
                ExecutionFormat::Codex => codex_record(&value, false, false),
                ExecutionFormat::Pi => pi_record(&value),
            }?,
        );
    }
    flush_codex_dialogue(
        &mut spans,
        &source.receipt.path,
        &mut pending_codex_dialogue,
        false,
    )?;
    let header = header.with_context(|| {
        format!(
            "execution history {} is missing a session header",
            source.receipt.path
        )
    })?;
    Ok((header, spans))
}

fn is_codex_dialogue(value: &Value) -> bool {
    let top = value.get("type").and_then(Value::as_str);
    let subtype = value
        .get("payload")
        .and_then(|payload| payload.get("type"))
        .and_then(Value::as_str);
    matches!(
        (top, subtype),
        (Some("response_item"), Some("message"))
            | (Some("event_msg"), Some("user_message" | "agent_message"))
    )
}

fn flush_codex_dialogue(
    spans: &mut Vec<RawSpan>,
    path: &str,
    pending: &mut Vec<(usize, Value)>,
    before_world_state: bool,
) -> Result<()> {
    let mut records = std::mem::take(pending).into_iter().peekable();
    while let Some((index, value)) = records.next() {
        let mirrored = records
            .peek()
            .is_some_and(|(_, next)| codex_records_are_mirrors(&value, next));
        append_execution_records(
            spans,
            path,
            index,
            codex_record(
                &value,
                before_world_state,
                mirrored && is_codex_event(&value),
            )?,
        );
        if mirrored {
            let (next_index, next) = records
                .next()
                .context("mirrored execution record disappeared")?;
            append_execution_records(
                spans,
                path,
                next_index,
                codex_record(&next, before_world_state, is_codex_event(&next))?,
            );
        }
    }
    Ok(())
}

fn is_codex_event(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("event_msg")
}

fn codex_records_are_mirrors(first: &Value, second: &Value) -> bool {
    let Some((first_role, first_text, first_timestamp, first_is_event)) =
        codex_dialogue_identity(first)
    else {
        return false;
    };
    let Some((second_role, second_text, second_timestamp, second_is_event)) =
        codex_dialogue_identity(second)
    else {
        return false;
    };
    first_is_event != second_is_event
        && first_role == second_role
        && first_text == second_text
        && first_timestamp.is_some()
        && first_timestamp == second_timestamp
}

fn codex_dialogue_identity(value: &Value) -> Option<(&str, &str, Option<&str>, bool)> {
    let top = value.get("type")?.as_str()?;
    let payload = value.get("payload")?;
    let subtype = payload.get("type")?.as_str()?;
    let timestamp = value.get("timestamp").and_then(Value::as_str);
    match (top, subtype) {
        ("event_msg", "user_message" | "agent_message") => Some((
            if subtype == "user_message" {
                "user"
            } else {
                "assistant"
            },
            payload.get("message")?.as_str()?,
            timestamp,
            true,
        )),
        ("response_item", "message") => {
            let role = payload.get("role")?.as_str()?;
            if !matches!(role, "user" | "assistant") {
                return None;
            }
            let content = payload.get("content")?.as_array()?;
            let [item] = content.as_slice() else {
                return None;
            };
            let item_type = item.get("type")?.as_str()?;
            if !matches!(item_type, "input_text" | "output_text") {
                return None;
            }
            Some((role, item.get("text")?.as_str()?, timestamp, false))
        }
        _ => None,
    }
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "execution record indices are finite zero-based source positions"
)]
fn append_execution_records(
    spans: &mut Vec<RawSpan>,
    path: &str,
    index: usize,
    records: Vec<RecordSpan>,
) {
    for record in records {
        spans.push(RawSpan {
            locator: format!("{path}#line={}{}", index + 1, record.locator_suffix),
            role: record.role,
            timestamp: record.timestamp,
            text: record.text,
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExecutionFormat {
    Codex,
    Pi,
}

fn execution_format(value: &Value, path: &str) -> Result<ExecutionFormat> {
    let observed = value.get("type").and_then(Value::as_str);
    match observed {
        Some("session_meta") => Ok(ExecutionFormat::Codex),
        Some("session") => Ok(ExecutionFormat::Pi),
        _ => bail!(
            "execution history {path} must begin with a session_meta or session record; observed type {observed:?}"
        ),
    }
}

fn optional_string<'a>(
    value: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>> {
    value
        .get(key)
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("session identity {key} must be a string"))
        })
        .transpose()
}

fn session_identity(value: &Value, format: ExecutionFormat, path: &str) -> Result<String> {
    let identity = match format {
        ExecutionFormat::Codex => value
            .get("payload")
            .and_then(Value::as_object)
            .context("Codex session_meta payload is missing or invalid")?,
        ExecutionFormat::Pi => value
            .as_object()
            .context("Pi session header is not an object")?,
    };
    let id = optional_string(identity, "id")?;
    let session_id = optional_string(identity, "session_id")?;
    session_id
        .or(id)
        .map(str::to_owned)
        .with_context(|| format!("execution history {path} session header has no identity"))
}

fn record_span(role: &str, timestamp: Option<String>, text: String) -> Vec<RecordSpan> {
    vec![RecordSpan {
        locator_suffix: String::new(),
        role: Some(role.into()),
        timestamp,
        text,
    }]
}

fn lifecycle_text(payload: &Value, subtype: &str) -> String {
    match subtype {
        "task_started" => json!({
            "type": subtype,
            "turn_id": payload.get("turn_id"),
            "started_at": payload.get("started_at"),
            "model_context_window": payload.get("model_context_window"),
            "collaboration_mode_kind": payload.get("collaboration_mode_kind"),
        })
        .to_string(),
        "task_complete" => json!({
            "type": subtype,
            "turn_id": payload.get("turn_id"),
            "completed_at": payload.get("completed_at"),
            "duration_ms": payload.get("duration_ms"),
            "last_agent_message": payload.get("last_agent_message"),
        })
        .to_string(),
        "patch_apply_end" => json!({
            "type": subtype,
            "call_id": payload.get("call_id"),
            "stdout": payload.get("stdout"),
            "stderr": payload.get("stderr"),
            "success": payload.get("success"),
            "status": payload.get("status"),
        })
        .to_string(),
        "web_search_end" => json!({
            "type": subtype,
            "call_id": payload.get("call_id"),
            "query": payload.get("query"),
            "status": payload.get("status"),
        })
        .to_string(),
        "turn_aborted" => json!({
            "type": subtype,
            "reason": payload.get("reason"),
        })
        .to_string(),
        _ => json!({"type": subtype}).to_string(),
    }
}
