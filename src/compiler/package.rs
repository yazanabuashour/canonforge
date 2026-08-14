use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs, io,
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use crate::protected_fs::open_private_bound_directory;

mod checksum;

pub(super) use checksum::checksum_index;

use super::{
    EVIDENCE_SCHEMA_VERSION, EVIDENCE_UNIT_SCHEMA, EvidencePackageEntry, EvidencePackageManifest,
    EvidenceUnit, PACKAGE_MANIFEST_SCHEMA, PackageInspection, SourceFile,
    compile_workflow::safe_join,
    json_support::{contract_validator, digest, locator_str, locator_strings, read_validated_json},
};

pub fn validate(package: &Path) -> Result<()> {
    load_package(package).map(|_| ())
}

pub fn inspect(package: &Path) -> Result<()> {
    let units = load_package(package)?;
    let inspection = package_inspection(&units);
    serde_json::to_writer_pretty(io::stdout().lock(), &inspection)?;
    println!();
    Ok(())
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "counts cannot exceed the already-allocated evidence vectors they summarize"
)]
pub(super) fn package_inspection(units: &[EvidenceUnit]) -> PackageInspection {
    let mut source_types = BTreeMap::new();
    let mut source_files = BTreeSet::new();
    let mut spans = 0_usize;
    let mut attachments = 0_usize;
    let mut materialized_attachments = 0_usize;
    for unit in units {
        *source_types.entry(unit.source_type.clone()).or_default() += 1;
        source_files.extend(
            unit.sources
                .iter()
                .map(|source| (&source.path, &source.sha256)),
        );
        spans += unit.spans.len();
        attachments += unit.attachments.len();
        materialized_attachments += unit
            .attachments
            .iter()
            .filter(|attachment| attachment.source.is_some())
            .count();
    }
    PackageInspection {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        units: units.len(),
        source_types,
        source_files: source_files.len(),
        spans,
        attachments,
        materialized_attachments,
        unavailable_attachments: attachments - materialized_attachments,
    }
}
pub(super) fn load_package(root: &Path) -> Result<Vec<EvidenceUnit>> {
    let package = open_private_bound_directory(root)?;
    let root = package.path();
    let members = fs::read_dir(root)?
        .map(|entry| {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let regular_file = !file_type.is_symlink() && entry.metadata()?.is_file();
            ensure!(
                regular_file && entry.file_name() == "manifest.json"
                    || file_type.is_dir() && entry.file_name() == "units",
                "unlisted or invalid evidence-package member: {}",
                entry.path().display()
            );
            Ok(entry.file_name())
        })
        .collect::<Result<HashSet<_>>>()?;
    ensure!(
        members == HashSet::from(["manifest.json".into(), "units".into()]),
        "evidence package must contain exactly manifest.json and units/"
    );
    let manifest_validator = contract_validator(PACKAGE_MANIFEST_SCHEMA)?;
    let manifest: EvidencePackageManifest = read_validated_json(
        &root.join("manifest.json"),
        &manifest_validator,
        "evidence-package manifest",
    )?;
    let units_directory = open_private_bound_directory(&root.join("units"))?;
    let actual_paths = fs::read_dir(units_directory.path())?
        .map(|entry| {
            let entry = entry?;
            ensure!(
                !entry.file_type()?.is_symlink() && entry.metadata()?.is_file(),
                "evidence-package units directory contains a non-file: {}",
                entry.path().display()
            );
            let name = entry.file_name().into_string().map_err(|name| {
                anyhow::anyhow!(
                    "evidence-unit filename must be UTF-8: {}",
                    name.to_string_lossy()
                )
            })?;
            Ok(format!("units/{name}"))
        })
        .collect::<Result<HashSet<_>>>()?;
    let expected_paths = manifest
        .units
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<HashSet<_>>();
    ensure!(
        actual_paths == expected_paths,
        "evidence-package unit files do not exactly match the manifest"
    );
    let unit_validator = contract_validator(EVIDENCE_UNIT_SCHEMA)?;
    let mut loaded = Vec::new();
    let mut ids = HashSet::new();
    for entry in &manifest.units {
        ensure!(ids.insert(entry.unit_id.clone()), "duplicate manifest unit");
        let path = safe_join(root, &entry.path)?;
        let unit: EvidenceUnit = read_validated_json(&path, &unit_validator, "evidence unit")?;
        ensure!(
            unit.schema_version == manifest.schema_version,
            "evidence-unit schema version does not match its package manifest: {}",
            entry.unit_id
        );
        validate_unit(entry, &unit)?;
        loaded.push(unit);
    }
    Ok(loaded)
}

fn validate_unit(entry: &EvidencePackageEntry, unit: &EvidenceUnit) -> Result<()> {
    ensure!(
        unit.unit_id == entry.unit_id
            && unit.source_type == entry.source_type
            && unit.unit_sha256 == entry.unit_sha256,
        "evidence-package manifest binding mismatch for {}",
        entry.unit_id
    );
    ensure!(
        digest(&unit.canonical_bytes()?) == unit.unit_sha256,
        "evidence-unit checksum mismatch for {}",
        unit.unit_id
    );
    let expected_sources = source_paths(&unit.source_type, &unit.source_locator)?;
    ensure!(
        unit.sources
            .iter()
            .take(expected_sources.len())
            .map(|source| source.path.as_str())
            .eq(expected_sources.iter().map(String::as_str)),
        "source receipts do not match the locator for {}",
        unit.unit_id
    );
    let mut span_ids = HashSet::new();
    for span in &unit.spans {
        ensure!(
            span_ids.insert(span.id.as_str()),
            "duplicate span ID in {}",
            unit.unit_id
        );
        ensure!(
            digest(span.text.as_bytes()) == span.text_sha256,
            "span checksum mismatch in {}",
            unit.unit_id
        );
    }
    ensure!(
        unit.source_type == "conversation-email" || unit.attachments.is_empty(),
        "non-email unit contains attachments: {}",
        unit.unit_id
    );
    let span_by_id = unit
        .spans
        .iter()
        .map(|span| (span.id.as_str(), span))
        .collect::<HashMap<_, _>>();
    let mut attachment_ids = HashSet::new();
    let mut artifact_paths = HashSet::new();
    let mut expected_artifacts = Vec::new();
    for attachment in &unit.attachments {
        ensure!(
            attachment_ids.insert(attachment.id.as_str()),
            "duplicate attachment ID in {}",
            unit.unit_id
        );
        let parent = span_by_id
            .get(attachment.span_id.as_str())
            .with_context(|| format!("invalid attachment span reference in {}", unit.unit_id))?;
        ensure!(
            attachment
                .locator
                .strip_prefix(&parent.locator)
                .is_some_and(valid_mime_part_suffix),
            "attachment locator does not extend its parent message locator in {}",
            unit.unit_id
        );
        if let Some(source) = &attachment.source {
            validate_content_addressed_source(source)?;
            if artifact_paths.insert(source.path.as_str()) {
                expected_artifacts.push(source);
            } else {
                ensure!(
                    expected_artifacts.contains(&source),
                    "attachment source receipts disagree in {}",
                    unit.unit_id
                );
            }
        }
    }
    ensure!(
        unit.sources
            .iter()
            .skip(expected_sources.len())
            .eq(expected_artifacts.into_iter()),
        "artifact sources do not match first attachment occurrence order in {}",
        unit.unit_id
    );
    Ok(())
}

fn valid_mime_part_suffix(suffix: &str) -> bool {
    suffix.strip_prefix(";part=").is_some_and(|path| {
        !path.is_empty()
            && path
                .split('.')
                .all(|component| component.parse::<u64>().is_ok_and(|value| value > 0))
    })
}

fn validate_content_addressed_source(source: &SourceFile) -> Result<()> {
    safe_join(Path::new("."), &source.path)?;
    let path = Path::new(&source.path);
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("artifact source has no UTF-8 digest filename")?;
    let prefix = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .context("artifact source has no digest-prefix directory")?;
    ensure!(
        filename == source.sha256
            && source.sha256.get(..2) == Some(prefix)
            && source.sha256.len() == 64
            && source
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "artifact source path does not match its SHA-256 receipt: {}",
        source.path
    );
    Ok(())
}

pub(super) fn source_paths(source_type: &str, locator: &Value) -> Result<Vec<String>> {
    let paths = match source_type {
        "canonical-markdown"
        | "conversation-chatgpt"
        | "conversation-email"
        | "conversation-table" => vec![locator_str(locator, "file")?.to_owned()],
        "docling-json" => {
            let document = locator_str(locator, "file")?;
            let original = locator_str(locator, "original_file")?;
            ensure!(
                document != original,
                "Docling JSON and original source must be different files"
            );
            vec![document.to_owned(), original.to_owned()]
        }
        "execution-history" => locator_strings(locator, "files")?
            .into_iter()
            .map(str::to_owned)
            .collect(),
        value => bail!("unsupported evidence source type {value}"),
    };
    Ok(paths)
}
