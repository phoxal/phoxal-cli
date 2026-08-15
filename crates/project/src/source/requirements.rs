//! Build-time derivation of the persisted participant set.
//!
//! [`derive_runtime_requirements`] is a pure function of a finalized `phoxal/robot/v0`
//! document, the frozen simulation membership of its component types, and the
//! CLI-internal catalog. It reads no Cargo metadata, no registry, no source
//! directory outside the bundle, no descriptor, and no persisted plan, and it
//! takes no execution options: the finalized manifest already carries the run
//! intent as data (`clock:` plus the `driver:` blocks finalization kept).
//!
//! `phoxal` uses it to decide what to build and stage; `phoxal-supervisor` uses
//! it to decide what to validate and launch. There is deliberately no second
//! derivation for either side to disagree with.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use phoxal_cli_catalog::{ArtifactKind, Catalog};
use phoxal_manifest::source::robot::v0::{Clock, Manifest, RESERVED_BRAIN_ID};

/// The component types whose frozen definition carries a simulation.
///
/// Simulator selection is a property of the frozen component definitions, not
/// of the robot document, so it is a separate input. The compiled robot is the
/// one truth about a frozen definition: an authored `simulation.yaml` is
/// absorbed into the model at compile time and never staged as a bundle asset.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SimulationMembership(BTreeSet<String>);

impl SimulationMembership {
    /// The membership the compiled robot's frozen component definitions carry.
    #[must_use]
    pub fn from_compiled_robot(robot: &phoxal_model::Robot, types: &BTreeSet<&str>) -> Self {
        Self(
            types
                .iter()
                .filter(|component_type| {
                    robot
                        .simulation_for_component_type(component_type)
                        .is_some()
                })
                .map(|component_type| (*component_type).to_string())
                .collect(),
        )
    }

    #[must_use]
    pub fn contains(&self, component_type: &str) -> bool {
        self.0.contains(component_type)
    }
}

impl FromIterator<String> for SimulationMembership {
    fn from_iter<I: IntoIterator<Item = String>>(types: I) -> Self {
        Self(types.into_iter().collect())
    }
}

/// The role a required participant plays. It selects the canonical staged
/// binary name, the board classification, and the config side channel - it is
/// never a launch policy: every required participant is startup-required and
/// stops the project on terminal failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredParticipantKind {
    /// The one mandatory project-owned root brain.
    Brain,
    /// A catalog-owned native service. Always runs, takes no configuration.
    OfficialService,
    /// A `services:` entry, configured by its author.
    UserService,
    /// A component driver, required exactly when its instance declares a
    /// `driver:` block.
    ComponentDriver,
    /// The catalog-owned simulator that owns simulated time and drives every
    /// simulated component of the robot. Required only under
    /// `clock: simulated`.
    WorldClock,
}

/// One participant process a finalized robot definition requires.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequiredParticipant {
    /// The launch and board identity: `brain`, the official short name, the
    /// user-service name, or - for a component participant - the instance id.
    pub participant_id: String,
    /// The identity every process sharing one binary declares: the official
    /// short name, the user-service name, or the component type.
    pub artifact_id: String,
    /// The canonical file name under `bin/` this participant resolves to.
    pub binary_name: String,
    pub kind: RequiredParticipantKind,
    /// The component instance this participant is bound to, for drivers and
    /// simulators.
    pub component_instance: Option<String>,
    /// The authored configuration this participant is launched with. Officials,
    /// the brain, drivers, and simulators have no configuration side channel.
    pub config: Option<serde_json::Value>,
}

/// Everything a finalized robot definition requires, in a deterministic order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeRequirements {
    pub participants: Vec<RequiredParticipant>,
}

impl RuntimeRequirements {
    /// Every distinct `bin/` file the requirement set resolves, each paired
    /// with the artifact identity its binary must declare. One binary serves
    /// every instance of a component type, so this is what binary validation
    /// iterates - never the participant list.
    #[must_use]
    pub fn selected_binaries(&self) -> BTreeMap<&str, &RequiredParticipant> {
        let mut selected = BTreeMap::new();
        for participant in &self.participants {
            selected
                .entry(participant.binary_name.as_str())
                .or_insert(participant);
        }
        selected
    }
}

/// Derive the complete participant set a finalized robot definition requires.
///
/// The rules, in the order they are applied:
///
/// - the mandatory project brain, exactly once, whatever else the document says;
/// - every catalog-owned native official service;
/// - every authored `services:` entry;
/// - a component driver **if and only if** the instance declares a `driver:`
///   block;
/// - under `clock: simulated` only, the world-clock owner - the one simulator
///   participant, which drives every simulated component of the robot. Each
///   such component's frozen definition must carry a `simulation.yaml`, and a
///   component that does not is named here rather than failing opaquely inside
///   the simulator.
///
/// Behavior never appears: the subsystem is gone and the brain replaced it.
pub fn derive_runtime_requirements(
    robot: &Manifest,
    simulated: &SimulationMembership,
    catalog: &Catalog,
) -> Result<RuntimeRequirements> {
    let mut participants = vec![RequiredParticipant {
        participant_id: RESERVED_BRAIN_ID.to_string(),
        artifact_id: RESERVED_BRAIN_ID.to_string(),
        binary_name: RESERVED_BRAIN_ID.to_string(),
        kind: RequiredParticipantKind::Brain,
        component_instance: None,
        config: None,
    }];

    for official in catalog.native() {
        if official.kind != ArtifactKind::Service {
            continue;
        }
        participants.push(RequiredParticipant {
            participant_id: official.short_name().to_string(),
            artifact_id: official.short_name().to_string(),
            binary_name: official.staged_binary_name(),
            kind: RequiredParticipantKind::OfficialService,
            component_instance: None,
            config: None,
        });
    }

    for (id, service) in &robot.services {
        if id == RESERVED_BRAIN_ID {
            bail!("services.brain is reserved for the mandatory root brain");
        }
        if catalog.is_official_service(id) {
            bail!(
                "robot.yaml declares services.{id}, but '{id}' is an official service; official \
                 runtimes are catalog-owned, always run, and take no robot.yaml declaration"
            );
        }
        participants.push(RequiredParticipant {
            participant_id: id.clone(),
            artifact_id: id.clone(),
            binary_name: id.clone(),
            kind: RequiredParticipantKind::UserService,
            component_instance: None,
            config: service.config.clone(),
        });
    }

    let simulated_clock = robot.clock == Clock::Simulated;
    for (instance, component) in &robot.robot.components {
        if component.driver.is_some() {
            // A physical driver stepping on simulated time is never meaningful.
            // Finalization already rejects it; deriving one here would hide a
            // hand-edited bundle from that check.
            if simulated_clock {
                bail!(
                    "component instance '{instance}' still declares a `driver:` block under \
                     `clock: simulated`; a simulated robot has no physical component drivers"
                );
            }
            participants.push(RequiredParticipant {
                participant_id: instance.clone(),
                artifact_id: component.component.clone(),
                binary_name: ArtifactKind::ComponentDriver.staged_binary_name(&component.component),
                kind: RequiredParticipantKind::ComponentDriver,
                component_instance: Some(instance.clone()),
                // Driver wiring is compiler-side data, not participant runtime
                // config: it is resolved into the launch record per instance.
                config: None,
            });
        }
        if simulated_clock && !simulated.contains(&component.component) {
            bail!(
                "component instance '{instance}' has type '{component_type}', whose frozen \
                 definition carries no simulation; author a simulation.yaml beside that \
                 component and rebuild, because a `clock: simulated` robot cannot simulate it",
                component_type = component.component
            );
        }
    }

    if simulated_clock {
        let world_clock = catalog.world_clock_owner();
        participants.push(RequiredParticipant {
            participant_id: world_clock.short_name().to_string(),
            artifact_id: world_clock.short_name().to_string(),
            binary_name: world_clock.staged_binary_name(),
            kind: RequiredParticipantKind::WorldClock,
            component_instance: None,
            config: None,
        });
    }

    participants.sort_by(|left, right| {
        left.participant_id
            .cmp(&right.participant_id)
            .then_with(|| left.artifact_id.cmp(&right.artifact_id))
            .then_with(|| left.binary_name.cmp(&right.binary_name))
    });
    Ok(RuntimeRequirements { participants })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(yaml: &str) -> Manifest {
        let phoxal_manifest::source::robot::Manifest::V0(manifest) =
            phoxal_manifest::source::robot::Manifest::parse(yaml).expect("fixture parses");
        manifest
    }

    const DRIVEN: &str = r#"schema: phoxal/robot/v0
robot:
  id: testbot
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: [left_drive.motor]
    encoders: []
  components:
    left_drive:
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
    right_drive:
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB1
          baud: 115200
    caster:
      component: caster
      mount_link: base
services:
  mission:
    config:
      speed: 1
"#;

    fn derive(yaml: &str, simulated: &[&str]) -> Result<RuntimeRequirements> {
        derive_runtime_requirements(
            &manifest(yaml),
            &simulated
                .iter()
                .map(|value| (*value).to_string())
                .collect::<SimulationMembership>(),
            &Catalog::official(),
        )
    }

    #[test]
    fn the_brain_is_required_exactly_once_and_behavior_never_appears() -> Result<()> {
        let requirements = derive(DRIVEN, &[])?;
        let brains = requirements
            .participants
            .iter()
            .filter(|participant| participant.kind == RequiredParticipantKind::Brain)
            .collect::<Vec<_>>();
        assert_eq!(brains.len(), 1, "{brains:?}");
        assert_eq!(brains[0].binary_name, "brain");
        assert_eq!(brains[0].config, None);
        assert!(
            !requirements
                .participants
                .iter()
                .any(|participant| participant.participant_id.contains("behavior")),
            "behavior is gone from the participant vocabulary"
        );
        Ok(())
    }

    #[test]
    fn a_driver_is_required_if_and_only_if_the_instance_declares_one() -> Result<()> {
        let requirements = derive(DRIVEN, &[])?;
        let drivers = requirements
            .participants
            .iter()
            .filter(|participant| participant.kind == RequiredParticipantKind::ComponentDriver)
            .map(|participant| participant.participant_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(drivers, vec!["left_drive", "right_drive"]);
        // Two instances, one shared binary.
        assert_eq!(
            requirements
                .selected_binaries()
                .keys()
                .filter(|name| name.starts_with("phoxal-component-"))
                .count(),
            1
        );
        // The driverless `caster` mount contributes nothing.
        assert!(
            !requirements
                .participants
                .iter()
                .any(|participant| participant.component_instance.as_deref() == Some("caster"))
        );
        Ok(())
    }

    #[test]
    fn officials_and_authored_services_are_distinguished() -> Result<()> {
        let requirements = derive(DRIVEN, &[])?;
        let drive = requirements
            .participants
            .iter()
            .find(|participant| participant.participant_id == "drive")
            .expect("official `drive` is required");
        assert_eq!(drive.kind, RequiredParticipantKind::OfficialService);
        assert_eq!(drive.binary_name, "phoxal-service-drive");
        assert_eq!(drive.config, None);

        let mission = requirements
            .participants
            .iter()
            .find(|participant| participant.participant_id == "mission")
            .expect("user service `mission` is required");
        assert_eq!(mission.kind, RequiredParticipantKind::UserService);
        assert_eq!(mission.binary_name, "mission");
        assert_eq!(mission.config, Some(serde_json::json!({"speed": 1})));
        Ok(())
    }

    #[test]
    fn simulators_are_required_only_under_a_simulated_clock() -> Result<()> {
        let real = derive(DRIVEN, &["ddsm115"])?;
        assert!(
            !real
                .participants
                .iter()
                .any(|participant| participant.kind == RequiredParticipantKind::WorldClock),
            "a real-clock robot requires no simulator"
        );

        // The simulation shape finalization produces: `clock: simulated` with
        // every driver block stripped.
        let simulated = DRIVEN
            .replace(
                "schema: phoxal/robot/v0\n",
                "schema: phoxal/robot/v0\nclock: simulated\n",
            )
            .lines()
            .filter(|line| {
                !line.contains("driver:")
                    && !line.contains("connection:")
                    && !line.contains("type: serial")
                    && !line.contains("port: /dev/tty")
                    && !line.contains("baud:")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let requirements = derive(&simulated, &["ddsm115", "caster"])?;
        let simulators = requirements
            .participants
            .iter()
            .filter(|participant| participant.kind == RequiredParticipantKind::WorldClock)
            .map(|participant| participant.binary_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(simulators, vec!["phoxal-simulator-webots-controller"]);

        // A simulated robot whose component carries no frozen simulation
        // definition is named before anything is built.
        let error = derive(&simulated, &["ddsm115"])
            .expect_err("a component with no simulation.yaml cannot be simulated");
        assert!(error.to_string().contains("caster"), "{error}");
        assert!(
            !requirements
                .participants
                .iter()
                .any(|participant| participant.kind == RequiredParticipantKind::ComponentDriver),
            "a simulated robot requires no physical driver"
        );
        Ok(())
    }

    #[test]
    fn a_simulated_clock_with_a_remaining_driver_block_is_rejected() {
        let simulated = DRIVEN.replace(
            "schema: phoxal/robot/v0\n",
            "schema: phoxal/robot/v0\nclock: simulated\n",
        );
        let error = derive(&simulated, &[])
            .expect_err("`clock: simulated` plus a driver block must be rejected");
        assert!(error.to_string().contains("clock: simulated"), "{error}");
    }

    #[test]
    fn an_official_identity_in_the_services_map_is_named_before_any_build() {
        let yaml = DRIVEN.replace("  mission:\n    config:\n      speed: 1\n", "  drive: {}\n");
        let error =
            derive(&yaml, &[]).expect_err("an official identity in `services:` must be rejected");
        assert!(error.to_string().contains("official service"), "{error}");
    }
}
