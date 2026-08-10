//! Driver intent selected while compiling authored project source.
//!
//! Clock and driver selection are manifest data, not runtime options: the
//! finalized document alone answers "what runs and on which clock", so nothing
//! downstream of finalization carries a mode flag, a driver flag, or an
//! execution-options type.

use std::collections::BTreeSet;

use phoxal_manifest::source::robot::v0::Clock;

/// Which component drivers the run intent keeps.
///
/// The selection is applied once, at finalization, by stripping the excluded
/// `driver:` blocks out of the finalized document. An excluded driver is
/// therefore never derived, never staged into `bin/`, and never inspected -
/// exclusion is enforced by the one requirement derivation reading a document
/// that no longer mentions it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum DriverSelection {
    /// Every driven component instance keeps its `driver:` block.
    #[default]
    All,
    /// Only these component-instance ids keep theirs.
    Only(BTreeSet<String>),
    /// No component instance keeps one.
    None,
}

impl DriverSelection {
    /// Whether the component instance `instance` keeps its `driver:` block.
    #[must_use]
    pub fn includes_instance(&self, instance: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(selected) => selected.contains(instance),
            Self::None => false,
        }
    }
}

/// The complete run intent finalization resolves into manifest data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunIntent {
    pub clock: Clock,
    pub drivers: DriverSelection,
}

impl Default for RunIntent {
    fn default() -> Self {
        Self::real(DriverSelection::All)
    }
}

impl RunIntent {
    /// A real-hardware run with the given driver selection.
    #[must_use]
    pub fn real(drivers: DriverSelection) -> Self {
        Self {
            clock: Clock::Real,
            drivers,
        }
    }

    /// A simulation run. Simulated time and a physical component driver are
    /// never meaningful together, so every driver block is stripped.
    #[must_use]
    pub fn simulated() -> Self {
        Self {
            clock: Clock::Simulated,
            drivers: DriverSelection::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_simulation_intent_strips_every_driver() {
        let intent = RunIntent::simulated();
        assert_eq!(intent.clock, Clock::Simulated);
        assert!(!intent.drivers.includes_instance("left_drive"));
    }

    #[test]
    fn a_subset_keeps_only_the_named_instances() {
        let drivers = DriverSelection::Only(["left_drive".to_string()].into_iter().collect());
        assert!(drivers.includes_instance("left_drive"));
        assert!(!drivers.includes_instance("right_drive"));
    }
}
