use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use super::{
    OMITTED_IMAGE_TEXT, PlannedUnit, RawSpan, RecordSpan, SourceExtraction, SourceUse,
    VerifiedSource,
    extraction::planned_assignment,
    json_support::{locator_str, parse_unique_json, scalar_text},
};

pub(super) fn chatgpt_source_extractions(
    source: &VerifiedSource,
    uses: &[&SourceUse],
    units: &[PlannedUnit],
) -> Result<Vec<SourceExtraction>> {
    let document = parse_unique_json(&source.bytes, &source.receipt.path)?;
    let conversations = document
        .as_array()
        .context("ChatGPT export root must be an array")?;
    let targets = uses
        .iter()
        .map(|source_use| {
            locator_str(
                &planned_assignment(units, source_use.unit_index)?.locator,
                "conversation_id",
            )
        })
        .collect::<Result<HashSet<_>>>()?;
    let mut selected = HashMap::new();
    for conversation in conversations {
        let mut conversation_ids = HashSet::new();
        for conversation_id in [
            conversation.get("id").and_then(Value::as_str),
            conversation.get("conversation_id").and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        {
            if targets.contains(conversation_id) && conversation_ids.insert(conversation_id) {
                ensure!(
                    selected.insert(conversation_id, conversation).is_none(),
                    "conversation {conversation_id} is duplicated"
                );
            }
        }
    }
    uses.iter()
        .map(|source_use| {
            let unit = planned_assignment(units, source_use.unit_index)?;
            let conversation_id = locator_str(&unit.locator, "conversation_id")?;
            let conversation = selected
                .get(conversation_id)
                .with_context(|| format!("conversation {conversation_id} was not found"))?;
            Ok(SourceExtraction {
                unit_index: source_use.unit_index,
                source_index: source_use.source_index,
                raw_spans: chatgpt_spans(conversation_id, conversation)?,
                raw_attachments: Vec::new(),
                execution_header: None,
            })
        })
        .collect()
}

fn chatgpt_spans(conversation_id: &str, conversation: &Value) -> Result<Vec<RawSpan>> {
    let mapping = conversation
        .get("mapping")
        .and_then(Value::as_object)
        .context("conversation mapping is missing")?;
    let mut chain = Vec::new();
    let mut cursor = Some(
        conversation
            .get("current_node")
            .and_then(Value::as_str)
            .context("conversation current_node is missing")?
            .to_owned(),
    );
    let mut seen = HashSet::new();
    while let Some(id) = cursor {
        ensure!(seen.insert(id.clone()), "conversation parent cycle");
        let node = mapping
            .get(&id)
            .with_context(|| format!("conversation node {id} is missing"))?;
        let parent = match node.get("parent") {
            None | Some(Value::Null) => None,
            Some(Value::String(parent)) => Some(parent.clone()),
            Some(_) => bail!("conversation node {id} has an invalid parent"),
        };
        chain.push((id, node));
        cursor = parent;
    }
    chain.reverse();
    let mut spans = vec![RawSpan {
        locator: format!("conversation={conversation_id}#metadata"),
        role: Some("metadata".into()),
        timestamp: conversation.get("update_time").map(scalar_text),
        text: format!(
            "Title: {}\nCreated: {}\nUpdated: {}",
            conversation
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("(untitled)"),
            conversation
                .get("create_time")
                .map(scalar_text)
                .unwrap_or_default(),
            conversation
                .get("update_time")
                .map(scalar_text)
                .unwrap_or_default()
        ),
    }];
    for (node_id, node) in chain {
        let Some(message) = node.get("message") else {
            continue;
        };
        let role = message
            .pointer("/author/role")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !matches!(role, "user" | "assistant" | "tool") {
            continue;
        }
        let locator = format!(
            "conversation={conversation_id};node={node_id};message={}",
            message
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
        for part in chatgpt_message_parts(message)? {
            spans.push(RawSpan {
                locator: format!("{locator}{}", part.locator_suffix),
                role: part.role,
                timestamp: part.timestamp,
                text: part.text,
            });
        }
    }
    ensure!(
        spans.len() > 1,
        "conversation {conversation_id} produced no user, assistant, or tool spans"
    );
    Ok(spans)
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "ChatGPT content part indices are finite one-based diagnostic positions"
)]
fn chatgpt_message_parts(message: &Value) -> Result<Vec<RecordSpan>> {
    let Some(content) = message.get("content") else {
        return Ok(Vec::new());
    };
    let role = message
        .pointer("/author/role")
        .and_then(Value::as_str)
        .context("ChatGPT message role is missing")?;
    let timestamp = message.get("create_time").map(scalar_text);
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
            for (index, part) in parts.iter().enumerate() {
                let locator_suffix = format!(";part={}", index + 1);
                let (part_role, text) = match part {
                    Value::String(text) => (role, text.to_owned()),
                    Value::Object(object) => {
                        let part_type = object
                            .get("content_type")
                            .or_else(|| object.get("type"))
                            .and_then(Value::as_str);
                        if part_type == Some("image_asset_pointer") {
                            ("omitted-asset", OMITTED_IMAGE_TEXT.to_owned())
                        } else if matches!(part_type, None | Some("text")) {
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
                    _ => bail!("unsupported ChatGPT content part at index {}", index + 1),
                };
                spans.push(RecordSpan {
                    locator_suffix,
                    role: Some(part_role.into()),
                    timestamp: timestamp.clone(),
                    text,
                });
            }
        }
        (None, None) => {}
    }
    Ok(spans)
}
