use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::{
    super::{
        RawSpan, SourceFile,
        compile_workflow::safe_join,
        json_support::{contract_validator, read_validated_json},
    },
    receipts::{summarize, validate_artifact_receipt},
};

pub(super) const MANIFEST_SCHEMA_VERSION: u8 = 1;
pub(super) const MANIFEST_SCHEMA: &str =
    include_str!("../../../skill/compile-knowledge/assets/email-attachment-manifest.schema.json");

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::compiler) struct EmailAttachmentManifest {
    pub schema_version: u8,
    pub source: SourceFile,
    pub artifact_dir: String,
    pub summary: AttachmentSummary,
    pub parts: Vec<ManifestPart>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::compiler) struct AttachmentSummary {
    pub schema_version: u8,
    pub parsed_messages: u64,
    pub attachment_occurrences: u64,
    pub unique_blobs: u64,
    pub total_decoded_bytes: u64,
    pub unique_decoded_bytes: u64,
    pub duplicate_bytes_avoided: u64,
    pub by_media_type: BTreeMap<String, MediaSummary>,
    pub by_disposition: DispositionSummary,
    pub malformed_or_undecodable_parts: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::compiler) struct MediaSummary {
    pub occurrences: u64,
    pub decoded_bytes: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::compiler) struct DispositionSummary {
    pub attachment: u64,
    pub inline: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::compiler) struct ManifestPart {
    pub id: String,
    pub message: u64,
    pub thread_id: String,
    pub part: String,
    pub locator: String,
    pub filename: Option<String>,
    pub media_type: String,
    pub disposition: AttachmentDisposition,
    pub content_id: Option<String>,
    pub source: Option<SourceFile>,
    pub error: Option<String>,
}

impl ManifestPart {
    pub(in crate::compiler) fn failure_context(&self, source_path: &str) -> String {
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
pub(in crate::compiler) enum AttachmentDisposition {
    Attachment,
    Inline,
}

#[derive(Default)]
pub(in crate::compiler) struct EmailAttachmentManifests {
    by_source: BTreeMap<String, EmailAttachmentManifest>,
}

pub(in crate::compiler) struct MailboxProjection {
    pub manifest: EmailAttachmentManifest,
    pub spans_by_thread: BTreeMap<String, Vec<RawSpan>>,
}

impl AttachmentDisposition {
    pub(in crate::compiler) const fn as_str(self) -> &'static str {
        match self {
            Self::Attachment => "attachment",
            Self::Inline => "inline",
        }
    }
}

impl EmailAttachmentManifests {
    pub(in crate::compiler) fn load(paths: &[PathBuf]) -> Result<Self> {
        let mut by_source = BTreeMap::new();
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

    pub(in crate::compiler) fn get(&self, source: &str) -> Option<&EmailAttachmentManifest> {
        self.by_source.get(source)
    }

    pub(in crate::compiler) fn reject_unused(&self, email_sources: &HashSet<String>) -> Result<()> {
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
        safe_join(Path::new("."), &self.source.path)?;
        safe_join(Path::new("."), &self.artifact_dir)?;
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
        let mut blobs = BTreeMap::new();
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
                    part.error.as_deref() == Some(super::super::ATTACHMENT_DECODE_ERROR),
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

    pub(in crate::compiler) fn artifact_dir(&self) -> &str {
        &self.artifact_dir
    }
}

fn valid_part_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('.')
            .all(|component| component.parse::<u64>().is_ok_and(|value| value > 0))
}
