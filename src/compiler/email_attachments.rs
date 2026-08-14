use std::{
    collections::{BTreeMap, HashSet},
    io::{Cursor, Write},
    path::Path,
};

use super::{
    RawSpan, SourceFile, VerifiedSource,
    compile_workflow::safe_join,
    email::email_message_span,
    json_support::{contract_validator, digest, validate_contract_value},
};
use crate::protected_fs::{
    BoundOutput, BoundPrivateDirectory, ensure_output_separate, ensure_private_relative_directory,
    open_private_bound_directory, publish_content_addressed_blob, read_bound_private_file,
};
use anyhow::{Context, Result, bail, ensure};
use mail_parser::{DateTime, Message, MessageParser, mailbox::mbox::MessageIterator};

mod manifest;
mod mime;
mod receipts;

pub(super) use manifest::{
    AttachmentDisposition, AttachmentSummary, EmailAttachmentManifests, ManifestPart,
};
use manifest::{
    DispositionSummary, EmailAttachmentManifest, MANIFEST_SCHEMA, MANIFEST_SCHEMA_VERSION,
    MailboxProjection,
};
use mime::{DecodedPart, MessageIdentity, attachment_parts, part_failure_context};
pub(super) use receipts::verify_artifact;
use receipts::{artifact_path, relative_utf8, summarize};

const RECEIPT_SCHEMA: &str =
    include_str!("../../skill/compile-knowledge/assets/email-attachment-receipt.schema.json");

pub(super) fn materialize(
    source_root: &Path,
    file: &Path,
    artifact_dir: &Path,
    output_manifest: &Path,
) -> Result<AttachmentSummary> {
    ensure_output_separate(output_manifest, &[(source_root, "source root")])?;
    let output = BoundOutput::open(output_manifest)?;
    let source_root = open_private_bound_directory(source_root)?;
    let file = relative_utf8(file, "email source file")?;
    let artifact_dir = relative_utf8(artifact_dir, "artifact directory")?;
    let source_path = safe_join(source_root.path(), &file)?;
    let snapshot = read_bound_private_file(&source_path)?;
    let source = VerifiedSource {
        receipt: SourceFile {
            path: file,
            sha256: digest(&snapshot.bytes),
            bytes: u64::try_from(snapshot.bytes.len()).context("source byte count overflow")?,
        },
        bytes: snapshot.bytes,
    };
    ensure_private_relative_directory(&source_root, Path::new(&artifact_dir))?;
    let projection = project_mailbox(&source, &artifact_dir, Some(&source_root), &HashSet::new())?;
    validate_contract_value(
        &serde_json::to_value(&projection.manifest)?,
        &contract_validator(MANIFEST_SCHEMA)?,
        "generated email attachment manifest",
    )?;
    validate_contract_value(
        &serde_json::to_value(&projection.manifest.summary)?,
        &contract_validator(RECEIPT_SCHEMA)?,
        "generated email attachment receipt",
    )?;
    let mut writer = output.into_guarded_public_writer()?;
    serde_json::to_writer_pretty(&mut writer, &projection.manifest)?;
    writer.write_all(b"\n")?;
    writer.finish()?;
    Ok(projection.manifest.summary)
}

pub(super) fn project_mailbox(
    source: &VerifiedSource,
    artifact_dir: &str,
    publish_root: Option<&BoundPrivateDirectory>,
    span_threads: &HashSet<&str>,
) -> Result<MailboxProjection> {
    safe_join(Path::new("."), artifact_dir)?;
    let parser = MessageParser::default();
    let mut parts = Vec::new();
    let mut spans_by_thread: BTreeMap<String, Vec<RawSpan>> = BTreeMap::new();
    let mut parsed_messages = 0_u64;
    for_each_mbox_message(source, &parser, |ordinal, message| {
        let message_number = ordinal.checked_add(1).context("message index overflow")?;
        parsed_messages = parsed_messages
            .checked_add(1)
            .context("parsed message count overflow")?;
        let thread_id = message
            .header("X-GM-THRID")
            .and_then(|value| value.as_text())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if let Some(thread_id) = &thread_id
            && span_threads.contains(thread_id.as_str())
        {
            spans_by_thread
                .entry(thread_id.clone())
                .or_default()
                .push(email_message_span(source, ordinal, thread_id, message));
        }
        let identity = MessageIdentity {
            source_path: &source.receipt.path,
            ordinal: message_number,
            thread_id: thread_id.as_deref(),
        };
        let decoded = attachment_parts(message, &identity)?;
        if thread_id.is_none()
            && let Some(part) = decoded.first()
        {
            bail!(
                "{}; X-GM-THRID header is missing",
                part_failure_context(
                    &identity,
                    &part.path,
                    &part.media_type,
                    Some(part.disposition),
                    part.filename.as_deref(),
                )
            );
        }
        for decoded in decoded {
            let failure_context = part_failure_context(
                &identity,
                &decoded.path,
                &decoded.media_type,
                Some(decoded.disposition),
                decoded.filename.as_deref(),
            );
            let occurrence = parts
                .len()
                .checked_add(1)
                .context("attachment occurrence count overflow")
                .with_context(|| failure_context.clone())?;
            parts.push(
                manifest_part(&identity, artifact_dir, publish_root, occurrence, decoded)
                    .with_context(|| failure_context)?,
            );
        }
        Ok(())
    })?;
    let summary = summarize(&parts, parsed_messages)?;
    Ok(MailboxProjection {
        manifest: EmailAttachmentManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            source: source.receipt.clone(),
            artifact_dir: artifact_dir.to_owned(),
            summary,
            parts,
        },
        spans_by_thread,
    })
}

fn manifest_part(
    identity: &MessageIdentity<'_>,
    artifact_dir: &str,
    publish_root: Option<&BoundPrivateDirectory>,
    occurrence: usize,
    decoded: DecodedPart,
) -> Result<ManifestPart> {
    let thread_id = identity.thread_id.context("X-GM-THRID header is missing")?;
    let locator = format!(
        "{}#message={};thread={thread_id};part={}",
        identity.source_path, identity.ordinal, decoded.path
    );
    let (source, error) = if let Some(bytes) = decoded.bytes {
        let sha256 = digest(&bytes);
        let relative = artifact_path(artifact_dir, &sha256)?;
        if let Some(root) = publish_root {
            let prefix = Path::new(&relative)
                .parent()
                .context("artifact path has no parent")?;
            ensure_private_relative_directory(root, prefix)?;
            publish_content_addressed_blob(&safe_join(root.path(), &relative)?, &sha256, &bytes)?;
        }
        (
            Some(SourceFile {
                path: relative,
                sha256,
                bytes: u64::try_from(bytes.len())
                    .context("decoded attachment byte count overflow")?,
            }),
            None,
        )
    } else {
        (None, Some(super::ATTACHMENT_DECODE_ERROR.to_owned()))
    };
    Ok(ManifestPart {
        id: format!("o{occurrence:06}"),
        message: u64::try_from(identity.ordinal).context("message number overflow")?,
        thread_id: thread_id.to_owned(),
        part: decoded.path,
        locator,
        filename: decoded.filename,
        media_type: decoded.media_type,
        disposition: decoded.disposition,
        content_id: decoded.content_id,
        source,
        error,
    })
}

fn for_each_mbox_message(
    source: &VerifiedSource,
    parser: &MessageParser,
    mut visitor: impl FnMut(usize, &Message<'_>) -> Result<()>,
) -> Result<()> {
    ensure!(
        source.bytes.starts_with(b"From "),
        "MBOX does not begin with an envelope line: {}",
        source.receipt.path
    );
    let mut envelopes = source
        .bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| line.starts_with(b"From "));
    for (ordinal, raw_message) in MessageIterator::new(Cursor::new(&source.bytes)).enumerate() {
        let message_number = ordinal.checked_add(1).context("message index overflow")?;
        let raw_message =
            raw_message.with_context(|| format!("parse MBOX envelope {}", source.receipt.path))?;
        validate_envelope(
            envelopes
                .next()
                .context("MBOX parser produced a message without an envelope")?,
            source,
            message_number,
        )?;
        let message = parser
            .parse(raw_message.contents())
            .with_context(|| format!("parse {} message {message_number}", source.receipt.path))?;
        visitor(ordinal, &message)?;
    }
    Ok(())
}

fn validate_envelope(
    envelope: &[u8],
    source: &VerifiedSource,
    message_number: usize,
) -> Result<()> {
    let envelope = std::str::from_utf8(envelope).context("MBOX envelope is not UTF-8")?;
    let (sender, date) = envelope
        .strip_prefix("From ")
        .and_then(|value| value.split_once(' '))
        .context("MBOX envelope is missing its sender or date")?;
    let mut fields = date.split_whitespace();
    let parsed_date = match (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) {
        (Some(weekday), Some(month), Some(day), Some(time), Some(year)) => {
            DateTime::parse_rfc822(&format!("{weekday}, {day} {month} {year} {time} +0000"))
        }
        _ => None,
    };
    ensure!(
        !sender.trim().is_empty() && parsed_date.is_some_and(|value| value.is_valid()),
        "invalid MBOX envelope {} message {}",
        source.receipt.path,
        message_number
    );
    Ok(())
}
