use anyhow::Result;

use super::{EvidenceUnit, EvidenceUnitCore, EvidenceUnitCoreV1};

impl EvidenceUnit {
    pub(super) fn canonical_bytes(&self) -> Result<Vec<u8>> {
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
