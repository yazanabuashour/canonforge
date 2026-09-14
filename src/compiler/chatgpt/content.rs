use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};

use super::super::{OMITTED_IMAGE_TEXT, RecordSpan};

#[expect(
    clippy::arithmetic_side_effects,
    reason = "ChatGPT content part indices are finite one-based diagnostic positions"
)]
pub(super) fn parts(
    content: &Map<String, Value>,
    role: &str,
    timestamp: Option<String>,
) -> Result<Vec<RecordSpan>> {
    let mut spans = Vec::new();
    match (content.get("text"), content.get("parts")) {
        (Some(_), Some(_)) => bail!("ChatGPT message content is ambiguous"),
        (Some(text), None) => spans.push(RecordSpan {
            locator_suffix: ";part=1".into(),
            role: Some(role.into()),
            timestamp,
            text: text
                .as_str()
                .context("ChatGPT message text is invalid")?
                .to_owned(),
        }),
        (None, Some(parts)) => {
            let parts = parts
                .as_array()
                .context("ChatGPT message parts are invalid")?;
            ensure!(!parts.is_empty(), "ChatGPT message parts are empty");
            for (index, part) in parts.iter().enumerate() {
                let locator_suffix = format!(";part={}", index + 1);
                let (part_role, text) = match part {
                    Value::String(text) => (role, text.to_owned()),
                    Value::Object(object) => {
                        ensure!(
                            !(object.contains_key("content_type") && object.contains_key("type")),
                            "ChatGPT content part type is ambiguous"
                        );
                        let part_type = object
                            .get("content_type")
                            .or_else(|| object.get("type"))
                            .map(|value| value.as_str().context("invalid ChatGPT part type"))
                            .transpose()?;
                        if part_type == Some("image_asset_pointer") {
                            ("omitted-asset", OMITTED_IMAGE_TEXT.to_owned())
                        } else if matches!(part_type, None | Some("text")) {
                            ensure!(
                                object.keys().all(|key| matches!(
                                    key.as_str(),
                                    "content_type" | "type" | "text"
                                )),
                                "unsupported ChatGPT text part field"
                            );
                            (
                                role,
                                object
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .context("ChatGPT text part is missing text")?
                                    .to_owned(),
                            )
                        } else {
                            bail!("unsupported ChatGPT content part type {part_type:?}")
                        }
                    }
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => {
                        bail!("unsupported ChatGPT content part at index {}", index + 1)
                    }
                };
                spans.push(RecordSpan {
                    locator_suffix,
                    role: Some(part_role.into()),
                    timestamp: timestamp.clone(),
                    text,
                });
            }
        }
        (None, None) => bail!("ChatGPT message content has no supported body"),
    }
    Ok(spans)
}
