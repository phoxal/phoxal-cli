//! Report responsibilities for run.

use super::{DriverDecision, DriversMode, RunOptions};
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::project::layout::DriverSelection;
use std::collections::BTreeSet;

#[derive(Debug)]
pub(crate) struct DriverPolicy {
    mode: DriversMode,
    subset: BTreeSet<String>,
}

impl DriverPolicy {
    pub(crate) fn drivers_off_for_sim() -> Self {
        Self {
            mode: DriversMode::Off,
            subset: BTreeSet::new(),
        }
    }

    /// Build the driver policy from the run options, validating a `--driver`
    /// subset against `available` - the full set of driven component-instance
    /// ids from the robot model. This must validate against every driven
    /// instance, not the constructed plan: the plan already has excluded drivers
    /// removed (#936), so an unknown id would otherwise be reported against an
    /// empty or narrowed list.
    pub(crate) fn from_options(options: &RunOptions, available: &BTreeSet<String>) -> Result<Self> {
        let subset = options
            .drivers_subset
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let unknown = subset.difference(available).cloned().collect::<Vec<_>>();
        if !unknown.is_empty() {
            let available = available.iter().cloned().collect::<Vec<_>>().join(", ");
            bail!(
                "unknown driver id(s): {}; available drivers: {}",
                unknown.join(", "),
                if available.is_empty() {
                    "<none>".to_string()
                } else {
                    available
                }
            );
        }
        Ok(Self {
            mode: options.drivers,
            subset,
        })
    }

    /// The core plan-construction selection this policy maps onto: `run`/`start`
    /// feed it into [`RuntimeLayout::construct_plan`](phoxal_cli_core::project::layout::RuntimeLayout::construct_plan)
    /// and [`stage_complete_bin_store`](super::participants::stage_complete_bin_store) so an
    /// excluded driver is never required, resolved, inspected, staged, or planned.
    pub(crate) fn selection(&self) -> DriverSelection {
        match self.mode {
            DriversMode::Off => DriverSelection::None,
            DriversMode::On if self.subset.is_empty() => DriverSelection::All,
            DriversMode::On => DriverSelection::Only(self.subset.clone()),
        }
    }

    /// The driven component instances this policy excludes from the plan, each
    /// with the operator-facing reason. The excluded drivers are never plan
    /// participants (the plan constructor drops them, #936), so this is the only
    /// place their absence is explained; [`report_excluded_drivers`] surfaces it
    /// as a session-level informational summary (#936, finding 8).
    pub(crate) fn excluded_drivers(&self, driven: &BTreeSet<String>) -> Vec<(String, String)> {
        driven
            .iter()
            .filter_map(|id| match self.decision(id) {
                DriverDecision::Degraded(reason) => Some((id.clone(), reason)),
                DriverDecision::Launch => None,
            })
            .collect()
    }

    pub(crate) fn decision(&self, id: &str) -> DriverDecision {
        match self.mode {
            DriversMode::Off => DriverDecision::Degraded("drivers off".to_string()),
            DriversMode::On if !self.subset.is_empty() && !self.subset.contains(id) => {
                DriverDecision::Degraded("not selected by --driver".to_string())
            }
            DriversMode::On => DriverDecision::Launch,
        }
    }
}

/// Emit a session-level informational summary of the drivers the policy excludes
/// from the plan, so an operator understands why hardware rows are absent (#936,
/// finding 8). The excluded drivers are deliberately NOT plan participants; this
/// is a one-line advisory into the session diagnostics stream, nothing more. No
/// output when the policy launches every driver.
pub(crate) fn report_excluded_drivers(
    policy: &DriverPolicy,
    driven: &BTreeSet<String>,
    ui: &dyn crate::Reporter,
) {
    let excluded = policy.excluded_drivers(driven);
    if excluded.is_empty() {
        return;
    }
    let list = excluded
        .iter()
        .map(|(id, reason)| format!("{id} ({reason})"))
        .collect::<Vec<_>>()
        .join(", ");
    ui.info(format!("drivers excluded by policy, not launched: {list}"));
}

/// Surface workspace runtime crates that are present but not declared in
/// robot.yaml (#950): legal drift, not built or launched. One advisory line
/// naming each crate and the map that would declare it, so authors notice a
/// service or tool they forgot to declare. No output when there is no drift.
pub(crate) fn report_undeclared_runtimes(
    undeclared: &[phoxal_cli_core::project::resolver::UndeclaredRuntime],
    ui: &dyn crate::Reporter,
) {
    if undeclared.is_empty() {
        return;
    }
    let list = undeclared
        .iter()
        .map(|runtime| {
            format!(
                "{family}/{name} (declare it under `{map}:` in robot.yaml to run it)",
                family = runtime.family,
                name = runtime.name,
                map = runtime.family
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    ui.warn(format!("undeclared workspace runtimes, not built: {list}"));
}

/// The full set of driven component-instance ids from a compiled robot model.
/// A `--driver` subset is validated against this - not the constructed plan,
/// which already drops the drivers the policy excludes (#936).
pub(crate) fn driven_instances(
    robot: &phoxal::model::source::robot::v0::Manifest,
) -> BTreeSet<String> {
    robot
        .robot
        .components
        .iter()
        .filter(|(_, component)| component.driver.is_some())
        .map(|(instance, _)| instance.clone())
        .collect()
}
