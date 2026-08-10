use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use mail_parser::{
    DateTime, Message, MessageParser, MimeHeaders, PartType, mailbox::mbox::MessageIterator,
};
use serde::{Deserialize, Serialize};

use super::{
    RawSpan, SourceFile, VerifiedSource, contract_validator, digest, email_message_span,
    read_validated_json, validate_contract_value,
};
use crate::protected_fs::{
    BoundPrivateDirectory, digest_bound_private_file, ensure_output_separate,
    ensure_private_relative_directory, open_private_bound_directory, private_staging_writer,
    publish_content_addressed_blob, read_bound_private_file,
};

const MANIFEST_SCHEMA_VERSION: u8 = 1;
const MANIFEST_SCHEMA: &str =
    include_str!("../../skill/compile-knowledge/assets/email-attachment-manifest.schema.json");
const RECEIPT_SCHEMA: &str =
    include_str!("../../skill/compile-knowledge/assets/email-attachment-receipt.schema.json");

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EmailAttachmentManifest {
    schema_version: u8,
    source: SourceFile,
    artifact_dir: String,
    pub(super) summary: AttachmentSummary,
    pub(super) parts: Vec<ManifestPart>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AttachmentSummary {
    pub(super) schema_version: u8,
    pub(super) parsed_messages: u64,
    pub(super) attachment_occurrences: u64,
    pub(super) unique_blobs: u64,
    pub(super) total_decoded_bytes: u64,
    pub(super) unique_decoded_bytes: u64,
    pub(super) duplicate_bytes_avoided: u64,
    pub(super) by_media_type: BTreeMap<String, MediaSummary>,
    pub(super) by_disposition: DispositionSummary,
    pub(super) malformed_or_undecodable_parts: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MediaSummary {
    occurrences: u64,
    decoded_bytes: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DispositionSummary {
    attachment: u64,
    inline: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManifestPart {
    id: String,
    pub(super) message: u64,
    pub(super) thread_id: String,
    part: String,
    pub(super) locator: String,
    pub(super) filename: Option<String>,
    pub(super) media_type: String,
    pub(super) disposition: AttachmentDisposition,
    pub(super) content_id: Option<String>,
    pub(super) source: Option<SourceFile>,
    pub(super) error: Option<String>,
}

impl ManifestPart {
    pub(super) fn failure_context(&self, source_path: &str) -> String {
        format!(
            "email MIME part failure: source_path={source_path:?}, message_ordinal={}, thread_id={:?}, mime_path={:?}, media_type={:?}, disposition={}, filename={:?}",
            self.message,
            self.thread_id,
            self.part,
            self.media_type,
            self.disposition.as_str(),
            self.filename,
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum AttachmentDisposition {
    Attachment,
    Inline,
}

#[derive(Default)]
pub(super) struct EmailAttachmentManifests {
    by_source: HashMap<String, EmailAttachmentManifest>,
}

pub(super) struct MailboxProjection {
    pub(super) manifest: EmailAttachmentManifest,
    pub(super) spans_by_thread: HashMap<String, Vec<RawSpan>>,
}

struct DecodedPart {
    path: String,
    filename: Option<String>,
    media_type: String,
    disposition: AttachmentDisposition,
    content_id: Option<String>,
    bytes: Option<Vec<u8>>,
}

struct MessageIdentity<'a> {
    source_path: &'a str,
    ordinal: usize,
    thread_id: Option<&'a str>,
}

impl AttachmentDisposition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Attachment => "attachment",
            Self::Inline => "inline",
        }
    }
}

impl EmailAttachmentManifests {
    pub(super) fn load(paths: &[PathBuf]) -> Result<Self> {
        let mut by_source = HashMap::new();
        let validator = contract_validator(MANIFEST_SCHEMA)?;
        for path in paths {
            let manifest: EmailAttachmentManifest =
                read_validated_json(path, &validator, "email attachment manifest")?;
            manifest.validate()?;
            ensure!(
                by_source
                    .insert(manifest.source.path.clone(), manifest)
                    .is_none(),
                "duplicate email attachment manifest for source"
            );
        }
        Ok(Self { by_source })
    }

    pub(super) fn get(&self, source: &str) -> Option<&EmailAttachmentManifest> {
        self.by_source.get(source)
    }

    pub(super) fn reject_unused(&self, email_sources: &HashSet<String>) -> Result<()> {
        for source in self.by_source.keys() {
            ensure!(
                email_sources.contains(source),
                "email attachment manifest does not match an assigned email source: {source}"
            );
        }
        Ok(())
    }
}

impl EmailAttachmentManifest {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == MANIFEST_SCHEMA_VERSION,
            "unsupported email attachment manifest schema version {}",
            self.schema_version
        );
        super::safe_join(Path::new("."), &self.source.path)?;
        super::safe_join(Path::new("."), &self.artifact_dir)?;
        ensure!(
            self.source.sha256.len() == 64
                && self
                    .source
                    .sha256
                    .bytes()
                    .all(|byte| { byte.is_ascii_digit() || matches!(byte, b'a'..=b'f') }),
            "email attachment manifest has an invalid source SHA-256"
        );
        let mut ids = HashSet::new();
        let mut blobs = HashMap::new();
        for part in &self.parts {
            ensure!(ids.insert(&part.id), "duplicate attachment occurrence ID");
            ensure!(
                part.message > 0
                    && !part.thread_id.is_empty()
                    && valid_part_path(&part.part)
                    && part.locator
                        == format!(
                            "{}#message={};thread={};part={}",
                            self.source.path, part.message, part.thread_id, part.part
                        ),
                "invalid attachment occurrence locator {}",
                part.locator
            );
            ensure!(
                part.source.is_some() != part.error.is_some(),
                "attachment occurrence must contain exactly one source or error"
            );
            if let Some(source) = &part.source {
                validate_artifact_receipt(source, &self.artifact_dir)?;
                ensure!(
                    blobs
                        .insert(&source.sha256, source)
                        .is_none_or(|found| found == source),
                    "artifact occurrences disagree about receipt {}",
                    source.path
                );
            } else {
                ensure!(
                    part.error.as_deref() == Some(super::ATTACHMENT_DECODE_ERROR),
                    "unknown attachment materialization error"
                );
            }
        }
        ensure!(
            summarize(&self.parts, self.summary.parsed_messages)? == self.summary,
            "email attachment manifest aggregate receipt does not match its occurrences"
        );
        Ok(())
    }

    pub(super) fn artifact_dir(&self) -> &str {
        &self.artifact_dir
    }
}

fn valid_part_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('.')
            .all(|component| component.parse::<u64>().is_ok_and(|value| value > 0))
}

pub(super) fn materialize(
    source_root: &Path,
    file: &Path,
    artifact_dir: &Path,
    output_manifest: &Path,
) -> Result<AttachmentSummary> {
    ensure_output_separate(output_manifest, &[(source_root, "source root")])?;
    let source_root = open_private_bound_directory(source_root)?;
    let file = relative_utf8(file, "email source file")?;
    let artifact_dir = relative_utf8(artifact_dir, "artifact directory")?;
    let source_path = super::safe_join(source_root.path(), &file)?;
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
    let mut writer = private_staging_writer(output_manifest)?;
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
    super::safe_join(Path::new("."), artifact_dir)?;
    let parser = MessageParser::default();
    let mut parts = Vec::new();
    let mut spans_by_thread: HashMap<String, Vec<RawSpan>> = HashMap::new();
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
            publish_content_addressed_blob(
                &super::safe_join(root.path(), &relative)?,
                &sha256,
                &bytes,
            )?;
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

fn attachment_parts(
    message: &Message<'_>,
    identity: &MessageIdentity<'_>,
) -> Result<Vec<DecodedPart>> {
    let attachments = message.attachments.iter().copied().collect::<HashSet<_>>();
    let mut decoded = Vec::new();
    match message.parts.first().map(|part| &part.body) {
        Some(PartType::Multipart(children)) => {
            let inherited = message
                .parts
                .first()
                .and_then(|part| classify_part(part, attachments.contains(&0)));
            visit_children(
                message,
                children,
                "",
                &attachments,
                inherited,
                identity,
                &mut decoded,
            )?;
        }
        Some(_) => visit_part(message, 0, "1", &attachments, None, identity, &mut decoded)?,
        None => bail!(
            "{}; parsed MIME message contains no root part",
            part_failure_context(identity, "1", "unavailable", None, None)
        ),
    }
    Ok(decoded)
}

fn visit_children(
    message: &Message<'_>,
    children: &[u32],
    parent: &str,
    attachments: &HashSet<u32>,
    inherited: Option<AttachmentDisposition>,
    identity: &MessageIdentity<'_>,
    decoded: &mut Vec<DecodedPart>,
) -> Result<()> {
    for (index, part_id) in children.iter().enumerate() {
        let child = index.checked_add(1).with_context(|| {
            format!(
                "{}; MIME part index overflow",
                part_failure_context(identity, parent, "unavailable", inherited, None)
            )
        })?;
        let path = if parent.is_empty() {
            child.to_string()
        } else {
            format!("{parent}.{child}")
        };
        visit_part(
            message,
            *part_id,
            &path,
            attachments,
            inherited,
            identity,
            decoded,
        )?;
    }
    Ok(())
}

fn visit_part(
    message: &Message<'_>,
    part_id: u32,
    path: &str,
    attachments: &HashSet<u32>,
    inherited: Option<AttachmentDisposition>,
    identity: &MessageIdentity<'_>,
    decoded: &mut Vec<DecodedPart>,
) -> Result<()> {
    let part = message.part(part_id).with_context(|| {
        format!(
            "{}; MIME tree references a missing part",
            part_failure_context(identity, path, "unavailable", inherited, None)
        )
    })?;
    let disposition = classify_part(part, attachments.contains(&part_id)).or(inherited);
    if let PartType::Multipart(children) = &part.body {
        ensure!(
            disposition.is_none() || !children.is_empty(),
            "{}; classified multipart contains no leaf parts",
            part_failure_context(
                identity,
                path,
                &media_type(part),
                disposition,
                part.attachment_name(),
            )
        );
        return visit_children(
            message,
            children,
            path,
            attachments,
            disposition,
            identity,
            decoded,
        );
    }
    let Some(disposition) = disposition else {
        return Ok(());
    };
    decoded.push(DecodedPart {
        path: path.to_owned(),
        filename: part.attachment_name().map(str::to_owned),
        media_type: media_type(part),
        disposition,
        content_id: part.content_id().map(str::to_owned),
        bytes: decode_part(message, part),
    });
    Ok(())
}

fn part_failure_context(
    identity: &MessageIdentity<'_>,
    path: &str,
    media_type: &str,
    disposition: Option<AttachmentDisposition>,
    filename: Option<&str>,
) -> String {
    format!(
        "email MIME part failure: source_path={:?}, message_ordinal={}, thread_id={:?}, mime_path={path:?}, media_type={media_type:?}, disposition={}, filename={filename:?}",
        identity.source_path,
        identity.ordinal,
        identity.thread_id.unwrap_or("unavailable"),
        disposition.map_or("unclassified", AttachmentDisposition::as_str),
    )
}

fn classify_part(
    part: &mail_parser::MessagePart<'_>,
    listed_attachment: bool,
) -> Option<AttachmentDisposition> {
    let disposition = part
        .content_disposition()
        .map(|value| value.c_type.as_ref());
    if disposition.is_some_and(|value| value.eq_ignore_ascii_case("attachment")) {
        return Some(AttachmentDisposition::Attachment);
    }
    if disposition.is_some_and(|value| value.eq_ignore_ascii_case("inline"))
        || matches!(part.body, PartType::InlineBinary(_))
        || part.content_id().is_some()
    {
        return Some(AttachmentDisposition::Inline);
    }
    (listed_attachment || part.attachment_name().is_some())
        .then_some(AttachmentDisposition::Attachment)
}

fn media_type(part: &mail_parser::MessagePart<'_>) -> String {
    if let Some(content_type) = part.content_type() {
        return content_type.c_subtype.as_ref().map_or_else(
            || content_type.c_type.to_ascii_lowercase(),
            |subtype| {
                format!(
                    "{}/{}",
                    content_type.c_type.to_ascii_lowercase(),
                    subtype.to_ascii_lowercase()
                )
            },
        );
    }
    match part.body {
        PartType::Text(_) => "text/plain",
        PartType::Html(_) => "text/html",
        PartType::Message(_) => "message/rfc822",
        PartType::Binary(_) | PartType::InlineBinary(_) | PartType::Multipart(_) => {
            "application/octet-stream"
        }
    }
    .to_owned()
}

fn decode_part(message: &Message<'_>, part: &mail_parser::MessagePart<'_>) -> Option<Vec<u8>> {
    if part.is_encoding_problem {
        return None;
    }
    let transfer = part.content_transfer_encoding().map(str::trim);
    if transfer.is_some_and(|value| {
        !value.eq_ignore_ascii_case("7bit")
            && !value.eq_ignore_ascii_case("8bit")
            && !value.eq_ignore_ascii_case("binary")
            && !value.eq_ignore_ascii_case("base64")
            && !value.eq_ignore_ascii_case("quoted-printable")
    }) {
        return None;
    }
    match &part.body {
        PartType::Binary(bytes) | PartType::InlineBinary(bytes) => return Some(bytes.to_vec()),
        PartType::Message(nested) => return Some(nested.raw_message.to_vec()),
        PartType::Text(_) | PartType::Html(_) => {}
        PartType::Multipart(_) => return None,
    }
    let start = usize::try_from(part.offset_body).ok()?;
    let end = usize::try_from(part.offset_end).ok()?;
    let bytes = message.raw_message.get(start..end)?;
    match transfer {
        None => Some(bytes.to_vec()),
        Some(value)
            if value.eq_ignore_ascii_case("7bit")
                || value.eq_ignore_ascii_case("8bit")
                || value.eq_ignore_ascii_case("binary") =>
        {
            Some(bytes.to_vec())
        }
        Some(value) if value.eq_ignore_ascii_case("base64") => {
            mail_parser::decoders::base64::base64_decode(bytes)
        }
        Some(value) if value.eq_ignore_ascii_case("quoted-printable") => {
            mail_parser::decoders::quoted_printable::quoted_printable_decode(bytes)
        }
        Some(_) => None,
    }
}

fn summarize(parts: &[ManifestPart], parsed_messages: u64) -> Result<AttachmentSummary> {
    let mut summary = AttachmentSummary {
        schema_version: MANIFEST_SCHEMA_VERSION,
        parsed_messages,
        attachment_occurrences: u64::try_from(parts.len())
            .context("attachment occurrence count overflow")?,
        unique_blobs: 0,
        total_decoded_bytes: 0,
        unique_decoded_bytes: 0,
        duplicate_bytes_avoided: 0,
        by_media_type: BTreeMap::new(),
        by_disposition: DispositionSummary::default(),
        malformed_or_undecodable_parts: 0,
    };
    let mut unique = BTreeSet::new();
    for part in parts {
        let media = summary
            .by_media_type
            .entry(part.media_type.clone())
            .or_default();
        media.occurrences = media
            .occurrences
            .checked_add(1)
            .context("media occurrence count overflow")?;
        let (counter, overflow) = match part.disposition {
            AttachmentDisposition::Attachment => (
                &mut summary.by_disposition.attachment,
                "attachment count overflow",
            ),
            AttachmentDisposition::Inline => {
                (&mut summary.by_disposition.inline, "inline count overflow")
            }
        };
        *counter = counter.checked_add(1).context(overflow)?;
        if let Some(source) = &part.source {
            media.decoded_bytes = media
                .decoded_bytes
                .checked_add(source.bytes)
                .context("media decoded bytes overflow")?;
            summary.total_decoded_bytes = summary
                .total_decoded_bytes
                .checked_add(source.bytes)
                .context("total decoded bytes overflow")?;
            if unique.insert(&source.sha256) {
                summary.unique_blobs = summary
                    .unique_blobs
                    .checked_add(1)
                    .context("unique blob count overflow")?;
                summary.unique_decoded_bytes = summary
                    .unique_decoded_bytes
                    .checked_add(source.bytes)
                    .context("unique decoded bytes overflow")?;
            }
        } else {
            summary.malformed_or_undecodable_parts = summary
                .malformed_or_undecodable_parts
                .checked_add(1)
                .context("malformed part count overflow")?;
        }
    }
    summary.duplicate_bytes_avoided = summary
        .total_decoded_bytes
        .checked_sub(summary.unique_decoded_bytes)
        .context("unique decoded bytes exceed total decoded bytes")?;
    Ok(summary)
}

fn artifact_path(directory: &str, sha256: &str) -> Result<String> {
    let prefix = sha256.get(..2).context("artifact digest is too short")?;
    let path = format!("{directory}/{prefix}/{sha256}");
    super::safe_join(Path::new("."), &path)?;
    Ok(path)
}

fn validate_artifact_receipt(source: &SourceFile, directory: &str) -> Result<()> {
    ensure!(
        source.sha256.len() == 64
            && source.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            && source.path == artifact_path(directory, &source.sha256)?,
        "invalid content-addressed artifact receipt {}",
        source.path
    );
    Ok(())
}

pub(super) fn verify_artifact(root: &Path, source: &SourceFile) -> Result<()> {
    let path = super::safe_join(root, &source.path)?;
    let found = digest_bound_private_file(&path)
        .with_context(|| format!("verify materialized email artifact {}", source.path))?;
    ensure!(
        found.sha256 == source.sha256 && found.bytes == source.bytes,
        "materialized email artifact {} digest mismatch: expected sha256 {} and {} bytes, found sha256 {} and {} bytes",
        source.path,
        source.sha256,
        source.bytes,
        found.sha256,
        found.bytes
    );
    Ok(())
}

fn relative_utf8(path: &Path, label: &str) -> Result<String> {
    let value = path
        .to_str()
        .with_context(|| format!("{label} must be UTF-8"))?
        .to_owned();
    super::safe_join(Path::new("."), &value)?;
    Ok(value)
}
