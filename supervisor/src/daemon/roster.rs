//! The selected process set, keyed both ways.
//!
//! The internal store is keyed by [`phoxal_cli_core::runtime::ProcessKey`] - a
//! scope plus a string - because that is what the process machinery has always
//! spawned and observed against. The wire is keyed by
//! [`phoxal_supervisor_api::ProcessKey`], a closed enum whose variant *is* the
//! participant kind.
//!
//! Neither is converted into the other on the fly. The roster is built once,
//! from the same [`RuntimeRequirements`] that decided what runs at all, and it
//! is the only place the two spellings meet: the projection reads it to publish
//! a snapshot, and the command server reads it to resolve a restart target.
//! Nothing else in the daemon may map a key.

use std::collections::BTreeMap;

use phoxal_cli_core::project::requirements::{
    RequiredParticipant, RequiredParticipantKind, RuntimeRequirements,
};
use phoxal_cli_core::runtime::{ProcessKey as CoreProcessKey, RobotKey};
use phoxal_supervisor_api::{
    ComponentBinding, Name, ProcessKey as WireProcessKey, StartupRequirement,
};

/// One selected process, in both key spellings plus the static facts a snapshot
/// row carries that the store never learns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RosterEntry {
    pub(crate) core: CoreProcessKey,
    pub(crate) wire: WireProcessKey,
    /// Present exactly for a driver, the one component-bound kind.
    pub(crate) component: Option<ComponentBinding>,
    pub(crate) startup: StartupRequirement,
}

/// The complete selected process set, in the requirement set's deterministic
/// order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Roster {
    entries: Vec<RosterEntry>,
    by_wire: BTreeMap<WireProcessKey, usize>,
}

impl Roster {
    /// Build the roster from the derived requirements of one finalized bundle.
    pub(crate) fn from_requirements(robot: &RobotKey, requirements: &RuntimeRequirements) -> Self {
        let entries: Vec<_> = requirements
            .participants
            .iter()
            .map(|required| RosterEntry {
                core: CoreProcessKey::robot(robot.clone(), &required.participant_id),
                wire: wire_key(required),
                component: component_binding(required),
                // Every required participant is startup-required: the catalog
                // owns that policy and no authored document may weaken it.
                startup: StartupRequirement::Required,
            })
            .collect();
        let by_wire = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.wire.clone(), index))
            .collect();
        Self { entries, by_wire }
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &RosterEntry> {
        self.entries.iter()
    }

    /// Resolve a wire key a client sent to the process the store knows.
    ///
    /// `None` is exactly the `UnknownProcess` rejection: a key that names no
    /// selected process.
    pub(crate) fn resolve(&self, wire: &WireProcessKey) -> Option<&RosterEntry> {
        self.by_wire.get(wire).map(|index| &self.entries[*index])
    }

    /// The world-clock owner, when this execution has one. Its absence from the
    /// snapshot is what the `clock: simulated` readiness diagnostic names.
    pub(crate) fn world_clock(&self) -> Option<&RosterEntry> {
        self.entries
            .iter()
            .find(|entry| matches!(entry.wire, WireProcessKey::Simulator { .. }))
    }
}

fn wire_key(required: &RequiredParticipant) -> WireProcessKey {
    match required.kind {
        RequiredParticipantKind::Brain => WireProcessKey::Brain,
        RequiredParticipantKind::OfficialService | RequiredParticipantKind::UserService => {
            WireProcessKey::Service {
                id: Name::new(&required.participant_id),
            }
        }
        RequiredParticipantKind::ComponentDriver => WireProcessKey::Driver {
            instance: Name::new(
                required
                    .component_instance
                    .as_deref()
                    .unwrap_or(&required.participant_id),
            ),
        },
        // One simulator owns simulated time for the whole robot, so it is
        // identified by participant id rather than by a component instance.
        RequiredParticipantKind::WorldClock => WireProcessKey::Simulator {
            id: Name::new(&required.participant_id),
        },
    }
}

fn component_binding(required: &RequiredParticipant) -> Option<ComponentBinding> {
    if required.kind != RequiredParticipantKind::ComponentDriver {
        return None;
    }
    required
        .component_instance
        .as_deref()
        .map(|instance| ComponentBinding {
            instance: Name::new(instance),
            // A component participant's artifact identity IS its component
            // type: one binary serves every instance of that type.
            component_type: Name::new(&required.artifact_id),
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn required(
        participant_id: &str,
        artifact_id: &str,
        kind: RequiredParticipantKind,
        component_instance: Option<&str>,
    ) -> RequiredParticipant {
        RequiredParticipant {
            participant_id: participant_id.to_string(),
            artifact_id: artifact_id.to_string(),
            binary_name: artifact_id.to_string(),
            kind,
            component_instance: component_instance.map(ToString::to_string),
            config: None,
        }
    }

    pub(crate) fn roster() -> Roster {
        Roster::from_requirements(
            &RobotKey::new("demo", "rover"),
            &RuntimeRequirements {
                participants: vec![
                    required("brain", "brain", RequiredParticipantKind::Brain, None),
                    required(
                        "drive",
                        "drive",
                        RequiredParticipantKind::OfficialService,
                        None,
                    ),
                    required(
                        "left",
                        "ddsm115",
                        RequiredParticipantKind::ComponentDriver,
                        Some("left"),
                    ),
                    required(
                        "webots",
                        "webots",
                        RequiredParticipantKind::WorldClock,
                        None,
                    ),
                ],
            },
        )
    }

    #[test]
    fn every_requirement_kind_maps_to_its_wire_key_and_binding() {
        let roster = roster();
        let wire: Vec<_> = roster
            .entries()
            .map(|entry| entry.wire.to_string())
            .collect();
        assert_eq!(
            wire,
            ["brain", "service:drive", "driver:left", "simulator:webots"]
        );

        let driver = roster
            .resolve(&WireProcessKey::Driver {
                instance: Name::new("left"),
            })
            .expect("the driver resolves");
        assert_eq!(
            driver.component,
            Some(ComponentBinding {
                instance: Name::new("left"),
                component_type: Name::new("ddsm115"),
            }),
            "a driver row names the frozen component type it resolved to"
        );
        assert_eq!(driver.core.to_string(), "demo/rover::left");

        // Only a driver is component-bound: the world clock drives the whole
        // robot, so binding it to one instance would be a lie.
        assert_eq!(roster.world_clock().expect("world clock").component, None);
    }

    #[test]
    fn a_key_naming_no_selected_process_does_not_resolve() {
        assert!(
            roster()
                .resolve(&WireProcessKey::Service {
                    id: Name::new("absent"),
                })
                .is_none()
        );
    }
}
