//! The launchable process set of one compiled robot.
//!
//! There is exactly one derivation of "what runs", and it is a pure function of
//! the compiled robot the manifest carries: the mandatory `brain`, every
//! `services` entry, and every `components` entry that declares a `driver:`
//! block. Staging uses it to decide which executables land in `bin/`, the
//! local launcher to decide which children to spawn, and `install` to decide
//! which systemd units to write. None of them may disagree, so none of them
//! derives it a second time.
//!
//! What each process is *configured* with is not here. Configuration is
//! checked against the schema its own binary embeds, from the authored
//! document the operator edits (`crate::validation`), and read at runtime by
//! the process itself out of the bundle; a third copy passing through this
//! launch set would only be a fourth thing to keep in agreement.
//!
//! The *expected presence* set the supervisor publishes is the same set,
//! derived the same way from the same manifest: a component without a driver
//! block launches no process, so expecting one would hold the graph
//! permanently incomplete. The supervisor computes it from the manifest it
//! opened; a client reads it off the snapshot rather than recomputing it here.

use phoxal::model::Robot;

/// Where in the manifest a runtime's identity came from.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RuntimeRole {
    /// The one mandatory root brain, staged as `bin/brain`.
    Brain,
    /// A `services` entry - official or authored, indistinguishable here
    /// because the manifest no longer records which is which.
    Service,
    /// A `components` entry that declares a `driver:` block.
    Driver,
}

impl RuntimeRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Brain => "brain",
            Self::Service => "service",
            Self::Driver => "driver",
        }
    }
}

impl std::fmt::Display for RuntimeRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The reserved identity of the one mandatory root brain.
pub const BRAIN_ID: &str = "brain";

/// One process a compiled robot launches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RobotRuntime {
    /// The launch identity, passed as `--participant-id`.
    pub participant_id: String,
    /// The `bin/` entry to execute. A launcher derives it from the id it is
    /// launching: `brain`, the service id, or - for a driver - the component
    /// *type*, because one driver binary serves every instance of a type.
    pub binary: String,
    pub role: RuntimeRole,
}

/// Every process the compiled robot launches, ordered by participant id.
///
/// The ordering matches the supervisor's snapshot ordering, so a merged view
/// of "expected" and "this session's children" never has to re-sort either
/// side.
#[must_use]
pub fn robot_runtimes(robot: &Robot) -> Vec<RobotRuntime> {
    let mut runtimes = vec![RobotRuntime {
        participant_id: BRAIN_ID.to_string(),
        binary: BRAIN_ID.to_string(),
        role: RuntimeRole::Brain,
    }];
    for (id, _) in robot.services() {
        runtimes.push(RobotRuntime {
            participant_id: id.as_str().to_string(),
            binary: phoxal_cli_catalog::bundle_binary_name(id.as_str()),
            role: RuntimeRole::Service,
        });
    }
    for component in robot
        .components()
        .filter(|component| component.instance().driver().is_some())
    {
        runtimes.push(RobotRuntime {
            participant_id: component.id().as_str().to_string(),
            binary: phoxal_cli_catalog::bundle_binary_name(
                component.instance().component_type().as_str(),
            ),
            role: RuntimeRole::Driver,
        });
    }
    runtimes.sort_by(|left, right| left.participant_id.cmp(&right.participant_id));
    runtimes
}

/// Every distinct `bin/` entry the robot launches, in the order a staging pass
/// should produce them. One driver binary serves every instance of its type, so
/// this is shorter than [`robot_runtimes`] whenever a type is mounted twice.
#[must_use]
pub fn robot_binaries(robot: &Robot) -> std::collections::BTreeSet<String> {
    robot_runtimes(robot)
        .into_iter()
        .map(|runtime| runtime.binary)
        .collect()
}

/// Read one installed or staged bundle's launchable process set.
///
/// This is how a launcher - the CLI locally, `install` when it writes systemd
/// units - learns what to start. It reads the same document the supervisor and
/// every participant read, so no third party has to be told what the robot is.
///
/// # Errors
///
/// When `bundle_root` does not hold a readable `manifest.json`.
pub fn bundle_runtimes(bundle_root: &std::path::Path) -> anyhow::Result<Vec<RobotRuntime>> {
    use anyhow::Context;

    let bundle = phoxal::bundle::RuntimeBundle::open(bundle_root).with_context(|| {
        format!(
            "failed to read the bundle manifest at {}",
            bundle_root.display()
        )
    })?;
    Ok(robot_runtimes(bundle.robot()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal::model::builder::{ComponentBuilder, RobotBuilder};
    use phoxal::model::connection::{Connection, Serial};

    /// A rover with two instances of one driven type and one driverless mount.
    fn rover() -> Robot {
        let driven = |port: &'static str| {
            move |mounted: ComponentBuilder| {
                mounted.driver(
                    Connection::Serial(Serial {
                        port: port.to_owned(),
                        baud: 115_200,
                    }),
                    Some(serde_json::json!({ "reduction": 20 })),
                )
            }
        };
        RobotBuilder::new("rover")
            .service("mission", Some(serde_json::json!({ "speed": 1 })))
            .service("drive", None)
            .component_type("ddsm115", |wheel| wheel.motor("motor", "wheel_joint"))
            .component_type("caster", |caster| caster.motor("motor", "caster_joint"))
            .component_with("left_drive", "ddsm115", driven("/dev/ttyUSB0"))
            .component_with("right_drive", "ddsm115", driven("/dev/ttyUSB1"))
            .component("free_wheel", "caster")
            .build()
            .expect("a valid canonical robot")
    }

    /// The launchable set is the brain, every service, and exactly the
    /// components that declare a driver - a mounted component without one has
    /// no process to start.
    #[test]
    fn the_launchable_set_is_brain_services_and_driven_components() {
        let runtimes = robot_runtimes(&rover());
        let ids = runtimes
            .iter()
            .map(|runtime| runtime.participant_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec!["brain", "drive", "left_drive", "mission", "right_drive"]
        );
        assert!(
            !ids.contains(&"free_wheel"),
            "a driverless mount launches nothing"
        );
    }

    /// A driver runs from its component *type*, so two instances of one type
    /// share a single `bin/` entry; everything else runs from its own id.
    #[test]
    fn a_driver_runs_from_its_component_type_and_a_service_from_its_id() {
        let runtimes = robot_runtimes(&rover());
        let binary = |id: &str| {
            runtimes
                .iter()
                .find(|runtime| runtime.participant_id == id)
                .map(|runtime| runtime.binary.clone())
                .expect("the fixture declares it")
        };
        assert_eq!(binary("brain"), "brain");
        assert_eq!(binary("drive"), "drive");
        assert_eq!(binary("mission"), "mission");
        assert_eq!(binary("left_drive"), "ddsm115");
        assert_eq!(binary("right_drive"), "ddsm115");
        assert_eq!(
            robot_binaries(&rover()).into_iter().collect::<Vec<_>>(),
            vec!["brain", "ddsm115", "drive", "mission"]
        );
    }
}
