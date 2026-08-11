use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};
use mail_parser::{Message, MimeHeaders, PartType};

use super::AttachmentDisposition;

pub(super) struct DecodedPart {
    pub(super) path: String,
    pub(super) filename: Option<String>,
    pub(super) media_type: String,
    pub(super) disposition: AttachmentDisposition,
    pub(super) content_id: Option<String>,
    pub(super) bytes: Option<Vec<u8>>,
}

pub(super) struct MessageIdentity<'a> {
    pub(super) source_path: &'a str,
    pub(super) ordinal: usize,
    pub(super) thread_id: Option<&'a str>,
}
pub(super) fn attachment_parts(
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

pub(super) fn part_failure_context(
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
