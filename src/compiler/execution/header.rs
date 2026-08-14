use anyhow::{Context, Result, bail};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::compiler) enum ExecutionFormat {
    Codex,
    Pi,
}

pub(super) fn execution_format(value: &Value, path: &str) -> Result<ExecutionFormat> {
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

pub(super) fn session_identity(
    value: &Value,
    format: ExecutionFormat,
    path: &str,
) -> Result<String> {
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
