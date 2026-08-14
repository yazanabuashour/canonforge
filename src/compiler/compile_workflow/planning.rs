use std::collections::{HashMap, HashSet, hash_map::Entry};

use anyhow::{Context, Result, bail, ensure};

use super::super::{AssignedUnit, PlannedUnit, SourcePlan, SourceRole, SourceUse};
use crate::compiler::package::source_paths;

pub(super) fn compile_plan(
    units: Vec<AssignedUnit>,
) -> Result<(Vec<PlannedUnit>, Vec<SourcePlan>)> {
    let mut planned_units = Vec::with_capacity(units.len());
    let mut source_plans = Vec::new();
    let mut source_indices = HashMap::new();
    let mut unit_ids = HashSet::new();
    for (unit_index, unit) in units.into_iter().enumerate() {
        ensure!(
            unit_ids.insert(unit.unit_id.clone()),
            "duplicate assigned unit {}",
            unit.unit_id
        );
        let paths = source_paths(&unit.source_type, &unit.locator)?;
        let roles = source_roles(&unit.source_type, paths.len())?;
        ensure!(
            paths.len() == roles.len(),
            "source role count does not match source paths for {}",
            unit.unit_id
        );
        let source_count = paths.len();
        for (source_index, (path, role)) in paths.into_iter().zip(roles).enumerate() {
            let plan_index = match source_indices.entry(path.clone()) {
                Entry::Occupied(entry) => *entry.get(),
                Entry::Vacant(entry) => {
                    let index = source_plans.len();
                    entry.insert(index);
                    source_plans.push(SourcePlan {
                        path: path.clone(),
                        parsers: Vec::new(),
                        uses: Vec::new(),
                    });
                    index
                }
            };
            let plan = source_plans
                .get_mut(plan_index)
                .context("source plan index is outside the compile plan")?;
            if role != SourceRole::ReceiptOnly && !plan.parsers.contains(&role) {
                plan.parsers.push(role);
            }
            plan.uses.push(SourceUse {
                unit_index,
                source_index,
                role,
            });
        }
        planned_units.push(PlannedUnit {
            unit: Some(unit),
            receipts: (0..source_count).map(|_| None).collect(),
            raw_spans: (0..source_count).map(|_| None).collect(),
            raw_attachments: (0..source_count).map(|_| None).collect(),
            execution_headers: (0..source_count).map(|_| None).collect(),
            identities: HashSet::new(),
            remaining_sources: source_count,
        });
    }
    Ok((planned_units, source_plans))
}

fn source_roles(source_type: &str, source_count: usize) -> Result<Vec<SourceRole>> {
    let role = match source_type {
        "canonical-markdown" => SourceRole::Markdown,
        "conversation-chatgpt" => SourceRole::ChatGpt,
        "conversation-email" => SourceRole::Email,
        "conversation-table" => SourceRole::ConversationTable,
        "execution-history" => SourceRole::Execution,
        "docling-json" => {
            ensure!(
                source_count == 2,
                "Docling assignment must have two sources"
            );
            return Ok(vec![SourceRole::Docling, SourceRole::ReceiptOnly]);
        }
        value => bail!("unsupported evidence source type {value}"),
    };
    Ok(vec![role; source_count])
}
