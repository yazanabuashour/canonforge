use std::collections::HashSet;

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use super::{RawSpan, VerifiedSource};

const DOCLING_CONTENT_COLLECTIONS: [&str; 7] = [
    "texts",
    "tables",
    "pictures",
    "key_value_items",
    "form_items",
    "field_regions",
    "field_items",
];

pub(super) fn docling_spans(source: &VerifiedSource, document: &Value) -> Result<Vec<RawSpan>> {
    let object = document
        .as_object()
        .context("Docling JSON root must be an object")?;
    let mut spans = Vec::new();
    let mut seen = HashSet::new();
    let body = object.get("body").context("Docling body is missing")?;
    let mut references = Vec::new();
    if let Some(furniture) = object.get("furniture") {
        push_docling_children(furniture, &mut references)?;
    }
    push_docling_children(body, &mut references)?;
    while let Some(reference) = references.pop() {
        let pointer = reference
            .strip_prefix('#')
            .filter(|pointer| pointer.starts_with('/'))
            .context("Docling child reference is not a JSON pointer")?;
        let relative_pointer = pointer
            .strip_prefix('/')
            .context("Docling child reference is not a JSON pointer")?;
        let (collection, index) = relative_pointer
            .split_once('/')
            .context("Docling child reference must identify one top-level item")?;
        ensure!(
            !index.is_empty() && !index.contains('/'),
            "Docling child reference must identify one top-level item: {reference:?}"
        );
        let item = document
            .pointer(pointer)
            .with_context(|| format!("Docling child reference does not resolve: {reference:?}"))?;
        ensure!(
            seen.insert(reference.clone()),
            "Docling reference cycle or duplicate"
        );
        ensure!(
            item.get("self_ref").and_then(Value::as_str) == Some(reference.as_str()),
            "Docling reference does not match self_ref: {reference:?}"
        );
        if collection == "groups" {
            push_docling_children(item, &mut references)?;
            continue;
        }
        ensure!(
            DOCLING_CONTENT_COLLECTIONS.contains(&collection),
            "unsupported Docling body reference: {reference:?}"
        );
        let role = item.get("label").and_then(Value::as_str).map_or_else(
            || collection.trim_end_matches('s').to_owned(),
            str::to_owned,
        );
        let text = if collection == "texts" {
            item.get("text")
                .and_then(Value::as_str)
                .map_or_else(|| item.to_string(), str::to_owned)
        } else {
            item.to_string()
        };
        spans.push(RawSpan {
            locator: reference,
            role: Some(format!("docling:{role}")),
            timestamp: None,
            text,
        });
        if item.get("children").is_some() {
            push_docling_children(item, &mut references)?;
        }
    }
    let expected = DOCLING_CONTENT_COLLECTIONS
        .iter()
        .map(|collection| {
            object.get(*collection).map_or(Ok(0), |items| {
                items
                    .as_array()
                    .map(Vec::len)
                    .context("Docling content collection must be an array")
            })
        })
        .sum::<Result<usize>>()?;
    ensure!(
        spans.len() == expected,
        "Docling body and furniture do not reference every supported content item"
    );
    ensure!(
        !spans.is_empty(),
        "Docling document contains no supported content items: {}",
        source.receipt.path
    );
    Ok(spans)
}

fn push_docling_children(parent: &Value, stack: &mut Vec<String>) -> Result<()> {
    let children = parent
        .get("children")
        .and_then(Value::as_array)
        .context("Docling node children are missing or invalid")?;
    for child in children.iter().rev() {
        let reference = child
            .get("$ref")
            .and_then(Value::as_str)
            .context("Docling child reference is missing $ref")?;
        stack.push(reference.into());
    }
    Ok(())
}
