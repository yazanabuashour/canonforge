use std::{
    collections::{BTreeMap, HashMap, HashSet, hash_map::Entry},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use mail_parser::{Message, MimeHeaders, PartType};

use super::{
    ExtractionContext, PlannedUnit, RawAttachment, RawSpan, SourceExtraction, SourceFile,
    SourceUse, VerifiedSource,
    email_attachments::{self, ManifestPart},
    extraction::planned_assignment,
    json_support::locator_str,
};

pub(super) fn email_source_extractions(
    source: &VerifiedSource,
    context: &mut ExtractionContext<'_>,
    uses: &[&SourceUse],
    units: &[PlannedUnit],
) -> Result<Vec<SourceExtraction>> {
    let mut targets: BTreeMap<&str, Vec<&SourceUse>> = BTreeMap::new();
    for source_use in uses {
        let unit = planned_assignment(units, source_use.unit_index)?;
        targets
            .entry(locator_str(&unit.locator, "thread_id")?)
            .or_default()
            .push(source_use);
    }
    let supplied = context.attachment_manifests.get(&source.receipt.path);
    let artifact_dir = supplied.map_or("_artifacts/sha256", |manifest| manifest.artifact_dir());
    let selected_threads = targets.keys().copied().collect::<HashSet<_>>();
    let mut projection =
        email_attachments::project_mailbox(source, artifact_dir, None, &selected_threads)?;
    if let Some(supplied) = supplied {
        ensure!(
            projection.manifest == *supplied,
            "email attachment manifest does not match observed MIME parts for {}",
            source.receipt.path
        );
    } else {
        ensure!(
            projection.manifest.parts.is_empty(),
            "email source {} contains attachments but has no --email-attachment-manifest",
            source.receipt.path
        );
    }
    let mut extractions = Vec::with_capacity(uses.len());
    for (thread_id, thread_uses) in targets {
        let thread_spans = projection
            .spans_by_thread
            .remove(thread_id)
            .context("Gmail thread projection disappeared")?;
        ensure!(
            !thread_spans.is_empty(),
            "Gmail thread {thread_id} was not found"
        );
        let attachments = projection
            .manifest
            .parts
            .iter()
            .filter(|part| part.thread_id == thread_id)
            .map(|part| {
                raw_attachment(
                    part,
                    &source.receipt.path,
                    context.source_root,
                    context.planned_source_paths,
                    context.attachment_receipts,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        for source_use in thread_uses {
            extractions.push(SourceExtraction {
                unit_index: source_use.unit_index,
                source_index: source_use.source_index,
                raw_spans: thread_spans.clone(),
                raw_attachments: attachments.clone(),
                execution_header: None,
            });
        }
    }
    Ok(extractions)
}

fn raw_attachment(
    part: &ManifestPart,
    source_path: &str,
    source_root: &Path,
    planned_source_paths: &HashSet<String>,
    receipts: &mut HashMap<String, SourceFile>,
) -> Result<RawAttachment> {
    if let Some(source) = &part.source {
        match receipts.entry(source.path.clone()) {
            Entry::Occupied(found) => {
                ensure!(
                    found.get() == source,
                    "{}; email attachment manifests disagree about source receipt {}",
                    part.failure_context(source_path),
                    source.path
                );
            }
            Entry::Vacant(slot) => {
                if !planned_source_paths.contains(&source.path) {
                    email_attachments::verify_artifact(source_root, source)
                        .with_context(|| part.failure_context(source_path))?;
                }
                slot.insert(source.clone());
            }
        }
    }
    let parent_locator = part
        .locator
        .split_once(";part=")
        .map(|(parent, _)| parent.to_owned())
        .with_context(|| {
            format!(
                "{}; attachment locator has no MIME part path",
                part.failure_context(source_path)
            )
        })?;
    Ok(RawAttachment {
        parent_locator,
        locator: part.locator.clone(),
        filename: part.filename.clone(),
        media_type: part.media_type.clone(),
        disposition: part.disposition,
        content_id: part.content_id.clone(),
        source: part.source.clone(),
        error: part.error.clone(),
    })
}

#[expect(
    clippy::arithmetic_side_effects,
    clippy::format_push_string,
    reason = "message indices are bounded by the parsed mailbox"
)]
pub(super) fn email_message_span(
    source: &VerifiedSource,
    ordinal: usize,
    thread_id: &str,
    message: &Message<'_>,
) -> RawSpan {
    let body = selected_body(message).unwrap_or_default();
    let from = message
        .from()
        .and_then(|addresses| addresses.first())
        .and_then(|address| address.address.as_deref())
        .unwrap_or("unknown");
    let attachments = message
        .attachments
        .iter()
        .filter_map(|part| message.part(*part)?.attachment_name())
        .collect::<Vec<_>>();
    let mut text = format!(
        "Subject: {}\nFrom: {from}\nDate: {}",
        message.subject().unwrap_or("(no subject)"),
        message
            .date()
            .map(mail_parser::DateTime::to_rfc3339)
            .unwrap_or_default()
    );
    if !attachments.is_empty() {
        text.push_str(&format!("\nAttachments: {}", attachments.join(", ")));
    }
    if !body.trim().is_empty() {
        text.push_str("\n\n");
        text.push_str(body.trim());
    }
    RawSpan {
        locator: format!(
            "{}#message={};thread={thread_id}",
            source.receipt.path,
            ordinal + 1
        ),
        role: Some(from.into()),
        timestamp: message.date().map(mail_parser::DateTime::to_rfc3339),
        text,
    }
}

fn selected_body(message: &Message<'_>) -> Option<String> {
    message
        .text_part(0)
        .and_then(selected_email_part)
        .or_else(|| message.html_part(0).and_then(selected_email_part))
}

fn selected_email_part(part: &mail_parser::MessagePart<'_>) -> Option<String> {
    match &part.body {
        PartType::Text(text) => Some(normalize_email_spacers(&replace_email_break_tags(
            &decode_email_entities(text),
        ))),
        PartType::Html(html) => Some(normalize_email_spacers(
            &mail_parser::decoders::html::html_to_text(html),
        )),
        PartType::Binary(_)
        | PartType::InlineBinary(_)
        | PartType::Message(_)
        | PartType::Multipart(_) => None,
    }
}

fn decode_email_entities(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '&' {
            output.push(character);
            continue;
        }
        let mut token = String::from("&");
        let mut complete = false;
        while let Some(next) = characters.peek().copied() {
            if next == '&' || next.is_whitespace() {
                break;
            }
            token.push(next);
            characters.next();
            if next == ';' {
                complete = true;
                break;
            }
        }
        if complete {
            mail_parser::decoders::html::add_html_token(&mut output, token.as_bytes(), false);
        } else {
            output.push_str(&token);
        }
    }
    output
}

fn replace_email_break_tags(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '<' {
            output.push(character);
            continue;
        }
        let mut tag = String::new();
        let mut complete = false;
        while let Some(next) = characters.peek().copied() {
            if next == '<' || matches!(next, '\n' | '\r') {
                break;
            }
            characters.next();
            if next == '>' {
                complete = true;
                break;
            }
            tag.push(next);
        }
        if complete
            && tag
                .trim()
                .trim_end_matches('/')
                .trim()
                .eq_ignore_ascii_case("br")
        {
            output.push('\n');
        } else {
            output.push('<');
            output.push_str(&tag);
            if complete {
                output.push('>');
            }
        }
    }
    output
}

fn normalize_email_spacers(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut after_hair_space = false;
    for character in input.chars() {
        match character {
            '\u{034f}' | '\u{200b}' | '\u{feff}' => {}
            '\u{200a}' => {
                while output.ends_with([' ', '\t']) {
                    output.pop();
                }
                output.push(' ');
                after_hair_space = true;
            }
            ' ' | '\t' if after_hair_space => {}
            _ => {
                output.push(character);
                after_hair_space = false;
            }
        }
    }
    output
}
