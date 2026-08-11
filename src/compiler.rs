use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod chatgpt;
mod compile_workflow;
mod conversation_table;
mod docling;
mod email;
mod email_attachments;
mod execution;
mod extraction;
mod inventory;
mod json_support;
mod markdown;
mod package;

#[cfg(test)]
pub use compile_workflow::compile;
pub use compile_workflow::{compile_with_email_attachments, materialize_email_attachments};
use email_attachments::{AttachmentDisposition, EmailAttachmentManifests};
use execution::ExecutionFormat;
pub use inventory::inventory_conversation_tables;
pub use package::{inspect, validate};
#[cfg(test)]
use std::cell::Cell;

const EVIDENCE_SCHEMA_VERSION: u8 = 3;
const ATTACHMENT_DECODE_ERROR: &str = "malformed-or-undecodable-transfer";
const SOURCE_ASSIGNMENT_SCHEMA_VERSION: u8 = 1;
const CONVERSATION_INVENTORY_SCHEMA_VERSION: u8 = 2;

#[cfg(test)]
thread_local! {
    static VERIFIED_SOURCE_READS: Cell<usize> = const { Cell::new(0) };
    static PARSED_SOURCE_PASSES: Cell<usize> = const { Cell::new(0) };
}
const SOURCE_ASSIGNMENT_SCHEMA: &str =
    include_str!("../skill/compile-knowledge/assets/source-assignment.schema.json");
const PACKAGE_MANIFEST_SCHEMA: &str =
    include_str!("../skill/compile-knowledge/assets/evidence-package-manifest.schema.json");
const EVIDENCE_UNIT_SCHEMA: &str =
    include_str!("../skill/compile-knowledge/assets/evidence-unit.schema.json");

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Assignment {
    schema_version: u8,
    units: Vec<AssignedUnit>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssignedUnit {
    unit_id: String,
    source_type: String,
    locator: Value,
    metadata: BTreeMap<String, Value>,
}

#[derive(Serialize)]
struct ConversationInventoryManifest {
    schema_version: u8,
    source_type: String,
    source_files: Vec<ConversationInventoryFile>,
    selection: Option<ConversationSelectionFile>,
    units: usize,
}

#[derive(Serialize)]
struct ConversationInventoryFile {
    path: String,
    sha256: String,
    bytes: u64,
    conversations: usize,
    messages: usize,
}

#[derive(Serialize)]
struct ConversationSelectionFile {
    path: String,
    sha256: String,
    bytes: u64,
    selected_conversations: usize,
}

struct ConversationRow {
    record: usize,
    thread: String,
    time: String,
    speaker: String,
    message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceFile {
    path: String,
    sha256: String,
    bytes: u64,
}

struct VerifiedSource {
    receipt: SourceFile,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceRole {
    Markdown,
    ChatGpt,
    Email,
    ConversationTable,
    Docling,
    ReceiptOnly,
    Execution,
}

#[derive(Clone, Copy)]
struct SourceUse {
    unit_index: usize,
    source_index: usize,
    role: SourceRole,
}

struct SourcePlan {
    path: String,
    parsers: Vec<SourceRole>,
    uses: Vec<SourceUse>,
}

struct PlannedUnit {
    unit: Option<AssignedUnit>,
    receipts: Vec<Option<SourceFile>>,
    raw_spans: Vec<Option<Vec<RawSpan>>>,
    raw_attachments: Vec<Option<Vec<RawAttachment>>>,
    execution_headers: Vec<Option<ExecutionHeader>>,
    identities: HashSet<(u64, u64)>,
    remaining_sources: usize,
}

#[derive(Clone)]
struct ExecutionHeader {
    format: ExecutionFormat,
    identity: String,
    path: String,
}

struct SourceExtraction {
    unit_index: usize,
    source_index: usize,
    raw_spans: Vec<RawSpan>,
    raw_attachments: Vec<RawAttachment>,
    execution_header: Option<ExecutionHeader>,
}

struct ExtractionContext<'a> {
    source_root: &'a Path,
    attachment_manifests: &'a EmailAttachmentManifests,
    planned_source_paths: &'a HashSet<String>,
    attachment_receipts: &'a mut HashMap<String, SourceFile>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Span {
    id: String,
    locator: String,
    role: Option<String>,
    timestamp: Option<String>,
    text_sha256: String,
    text: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Attachment {
    id: String,
    span_id: String,
    locator: String,
    filename: Option<String>,
    media_type: String,
    disposition: AttachmentDisposition,
    content_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<SourceFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct EvidenceUnitCoreV1<'a> {
    schema_version: u8,
    unit_id: &'a str,
    source_type: &'a str,
    source_locator: &'a Value,
    metadata: &'a BTreeMap<String, Value>,
    sources: &'a [SourceFile],
    spans: &'a [Span],
}

#[derive(Serialize)]
struct EvidenceUnitCore<'a> {
    schema_version: u8,
    unit_id: &'a str,
    source_type: &'a str,
    source_locator: &'a Value,
    metadata: &'a BTreeMap<String, Value>,
    sources: &'a [SourceFile],
    spans: &'a [Span],
    attachments: &'a [Attachment],
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidenceUnit {
    schema_version: u8,
    unit_id: String,
    source_type: String,
    source_locator: Value,
    metadata: BTreeMap<String, Value>,
    sources: Vec<SourceFile>,
    spans: Vec<Span>,
    #[serde(default)]
    attachments: Vec<Attachment>,
    unit_sha256: String,
}

impl EvidenceUnit {
    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        if self.schema_version == 1 {
            serde_json::to_vec(&EvidenceUnitCoreV1 {
                schema_version: self.schema_version,
                unit_id: &self.unit_id,
                source_type: &self.source_type,
                source_locator: &self.source_locator,
                metadata: &self.metadata,
                sources: &self.sources,
                spans: &self.spans,
            })
            .map_err(Into::into)
        } else {
            serde_json::to_vec(&EvidenceUnitCore {
                schema_version: self.schema_version,
                unit_id: &self.unit_id,
                source_type: &self.source_type,
                source_locator: &self.source_locator,
                metadata: &self.metadata,
                sources: &self.sources,
                spans: &self.spans,
                attachments: &self.attachments,
            })
            .map_err(Into::into)
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidencePackageManifest {
    schema_version: u8,
    units: Vec<EvidencePackageEntry>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidencePackageEntry {
    unit_id: String,
    source_type: String,
    unit_sha256: String,
    path: String,
}

#[derive(Serialize)]
struct PackageInspection {
    schema_version: u8,
    units: usize,
    source_types: BTreeMap<String, usize>,
    source_files: usize,
    spans: usize,
    attachments: usize,
    materialized_attachments: usize,
    unavailable_attachments: usize,
}

#[derive(Clone)]
struct RawSpan {
    locator: String,
    role: Option<String>,
    timestamp: Option<String>,
    text: String,
}

#[derive(Clone)]
struct RawAttachment {
    parent_locator: String,
    locator: String,
    filename: Option<String>,
    media_type: String,
    disposition: AttachmentDisposition,
    content_id: Option<String>,
    source: Option<SourceFile>,
    error: Option<String>,
}

struct RecordSpan {
    locator_suffix: String,
    role: Option<String>,
    timestamp: Option<String>,
    text: String,
}

const OMITTED_IMAGE_TEXT: &str = "{\"kind\":\"image\",\"status\":\"not-materialized\"}";
const EXCLUDED_PLATFORM_TEXT: &str = "[platform instruction body excluded from evidence view]";

#[cfg(test)]
mod tests;
