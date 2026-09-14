mod content;

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use super::{
    ExtractionRequest, RawSpan, RecordSpan, SourceExtraction, VerifiedSource,
    json_support::{locator_str, parse_unique_json, scalar_text},
};

pub(super) fn chatgpt_source_extractions(
    source: &VerifiedSource,
    requests: &[ExtractionRequest<'_>],
) -> Result<Vec<SourceExtraction>> {
    let document = parse_unique_json(&source.bytes, &source.receipt.path)?;
    let conversations = document
        .as_array()
        .context("ChatGPT export root must be an array")?;
    let targets = requests
        .iter()
        .map(|request| locator_str(&request.assignment.locator, "conversation_id"))
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
    requests
        .iter()
        .map(|request| {
            let conversation_id = locator_str(&request.assignment.locator, "conversation_id")?;
            let conversation = selected
                .get(conversation_id)
                .with_context(|| format!("conversation {conversation_id} was not found"))?;
            Ok(SourceExtraction {
                unit_index: request.unit_index,
                source_index: request.source_index,
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
        if message.is_null() {
            continue;
        }
        let locator = format!(
            "conversation={conversation_id};node={node_id};message={}",
            message
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
        for part in chatgpt_message_parts(message)
            .with_context(|| format!("invalid ChatGPT message at {locator}"))?
        {
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

fn chatgpt_message_parts(message: &Value) -> Result<Vec<RecordSpan>> {
    ensure!(message.is_object(), "ChatGPT message must be an object");
    let role = message
        .pointer("/author/role")
        .and_then(Value::as_str)
        .context("ChatGPT message role is missing")?;
    ensure!(
        matches!(role, "system" | "user" | "assistant" | "tool"),
        "unsupported ChatGPT message role"
    );
    let timestamp = message.get("create_time").map(scalar_text);
    if role == "system" {
        return Ok(vec![RecordSpan {
            locator_suffix: ";content".into(),
            role: Some("excluded-platform-instruction".into()),
            timestamp,
            text: super::EXCLUDED_PLATFORM_TEXT.into(),
        }]);
    }
    let content = message
        .get("content")
        .and_then(Value::as_object)
        .context("ChatGPT message content must be an object")?;
    let content_type = content
        .get("content_type")
        .map(|value| value.as_str().context("invalid ChatGPT content type"))
        .transpose()?;
    let excluded_role = match content_type {
        Some("thoughts" | "reasoning_recap") => {
            ensure!(
                role == "assistant",
                "ChatGPT reasoning requires assistant role"
            );
            Some("excluded-reasoning")
        }
        Some("user_editable_context") => {
            ensure!(
                role == "user",
                "ChatGPT editable context requires user role"
            );
            Some("excluded-platform-instruction")
        }
        None | Some("text" | "multimodal_text" | "code" | "execution_output") => None,
        Some(_) => bail!("unsupported ChatGPT message content type"),
    };
    if let Some(excluded_role) = excluded_role {
        return Ok(vec![RecordSpan {
            locator_suffix: ";content".into(),
            role: Some(excluded_role.into()),
            timestamp,
            text: serde_json::json!({"type": content_type}).to_string(),
        }]);
    }
    ensure!(
        message.get("channel").is_none_or(Value::is_null),
        "unsupported ChatGPT message channel"
    );
    if let Some(metadata) = message.get("metadata").filter(|value| !value.is_null()) {
        let metadata = metadata
            .as_object()
            .context("invalid ChatGPT message metadata")?;
        for flag in [
            "is_visually_hidden_from_conversation",
            "is_user_system_message",
        ] {
            ensure!(
                metadata
                    .get(flag)
                    .is_none_or(|value| value == &Value::Bool(false)),
                "unsupported ChatGPT message metadata flag {flag} without an exclusion type"
            );
        }
    }
    ensure!(
        content.keys().all(|key| matches!(
            key.as_str(),
            "content_type" | "text" | "parts" | "language" | "response_format_name"
        )),
        "unsupported ChatGPT message content field"
    );
    content::parts(content, role, timestamp)
}
