use std::collections::{HashMap, hash_map::Entry};

use anyhow::{Context, Result};

use super::{
    super::{AssignedUnit, SourceRole, source_inputs::source_inputs},
    unit::PlannedUnit,
};

pub(super) struct SourceUse {
    pub unit_index: usize,
    pub source_index: usize,
    pub role: SourceRole,
}

pub(super) struct SourcePlan {
    pub path: String,
    pub parsers: Vec<SourceRole>,
    pub uses: Vec<SourceUse>,
}

pub(super) fn compile_plan(
    units: Vec<AssignedUnit>,
) -> Result<(Vec<PlannedUnit>, Vec<SourcePlan>)> {
    let mut planned_units = Vec::with_capacity(units.len());
    let mut source_plans = Vec::new();
    let mut source_indices = HashMap::new();
    for (unit_index, unit) in units.into_iter().enumerate() {
        let inputs = source_inputs(&unit.source_type, &unit.locator)?;
        let roles = inputs.iter().map(|input| input.role).collect();
        for (source_index, input) in inputs.into_iter().enumerate() {
            let path = input.path;
            let role = input.role;
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
        planned_units.push(PlannedUnit::new(unit, roles));
    }
    Ok((planned_units, source_plans))
}
