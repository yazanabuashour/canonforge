use std::{collections::HashSet, fs, os::unix::fs::DirBuilderExt};

use anyhow::{Context, Result, ensure};

use super::{
    super::{
        AssignedUnit, EVIDENCE_SCHEMA_VERSION, EVIDENCE_UNIT_SCHEMA, EvidencePackageEntry,
        EvidencePackageManifest, EvidenceUnit,
        json_support::{contract_validator, digest, validate_contract_value, write_staging_json},
    },
    validate_unit,
};
use crate::protected_fs::{PrivateDirectory, sync_directory};

pub(in crate::compiler) struct PackageWriter {
    staging: PrivateDirectory,
    validator: jsonschema::Validator,
    slots: Vec<UnitSlot>,
}

struct UnitSlot {
    unit_id: String,
    source_type: String,
    entry: Option<EvidencePackageEntry>,
}

impl PackageWriter {
    pub(in crate::compiler) fn new(
        staging: PrivateDirectory,
        assignments: &[AssignedUnit],
    ) -> Result<Self> {
        ensure!(
            !assignments.is_empty(),
            "evidence package requires assigned units"
        );
        let mut ids = HashSet::new();
        for unit in assignments {
            ensure!(
                ids.insert(&unit.unit_id),
                "duplicate assigned unit {}",
                unit.unit_id
            );
        }
        let validator = contract_validator(EVIDENCE_UNIT_SCHEMA)?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(staging.path().join("units"))?;
        Ok(Self {
            staging,
            validator,
            slots: assignments
                .iter()
                .map(|unit| UnitSlot {
                    unit_id: unit.unit_id.clone(),
                    source_type: unit.source_type.clone(),
                    entry: None,
                })
                .collect(),
        })
    }

    pub(in crate::compiler) fn write(
        &mut self,
        index: usize,
        mut unit: EvidenceUnit,
    ) -> Result<()> {
        let slot = self
            .slots
            .get_mut(index)
            .context("manifest entry index is outside the compile plan")?;
        ensure!(slot.entry.is_none(), "unit was compiled twice");
        ensure!(
            unit.unit_id == slot.unit_id && unit.source_type == slot.source_type,
            "compiled unit does not match its assignment slot"
        );
        unit.unit_sha256 = digest(&unit.canonical_bytes()?);
        validate_contract_value(
            &serde_json::to_value(&unit)?,
            &self.validator,
            "evidence unit",
        )?;
        let entry = EvidencePackageEntry {
            unit_id: unit.unit_id.clone(),
            source_type: unit.source_type.clone(),
            unit_sha256: unit.unit_sha256.clone(),
            path: format!("units/{}.json", digest(unit.unit_id.as_bytes())),
        };
        validate_unit(&entry, &unit)?;
        write_staging_json(&self.staging.path().join(&entry.path), &unit)?;
        slot.entry = Some(entry);
        Ok(())
    }

    pub(in crate::compiler) fn finish(self) -> Result<()> {
        let units = self
            .slots
            .into_iter()
            .map(|slot| slot.entry.context("assigned unit was not compiled"))
            .collect::<Result<Vec<_>>>()?;
        write_staging_json(
            &self.staging.path().join("manifest.json"),
            &EvidencePackageManifest {
                schema_version: EVIDENCE_SCHEMA_VERSION,
                units,
            },
        )?;
        sync_directory(&self.staging.path().join("units"))?;
        self.staging.finish()
    }
}
