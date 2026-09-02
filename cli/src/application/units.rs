//! The systemd units an installed robot runs as.
//!
//! On a device the launcher is systemd, not this CLI, so what `phoxal run`
//! spawns as children exists here as units instead: one for the supervisor, one
//! for every runtime the installed `manifest.json` declares, and one target
//! that owns the set.
//!
//! The runtime units are `BindsTo=` and `PartOf=` the supervisor's, which is
//! the whole design in two directives: the supervisor runs the router every
//! runtime talks through, so a runtime without it has nothing to connect to.
//! `BindsTo` stops a runtime when the supervisor goes away for any reason;
//! `PartOf` makes `systemctl stop phoxal-supervisor` stop the runtimes with it.
//! `After` orders the start so a runtime is not racing a router that does not
//! exist yet.
//!
//! Everything here is a pure function of the manifest and the fixed installed
//! paths. That is deliberate: systemd cannot run on the machine this is
//! developed on, so the unit text is proven by reading it rather than by
//! starting it.

use anyhow::{Context, Result};
use phoxal::identity::ParticipantId;
use phoxal::participant::launch::LaunchCommand;
use phoxal_cli_project::RobotRuntime;

/// The first line every unit this CLI writes carries, and the only ownership
/// claim it makes: a unit without it is somebody else's and is never rewritten
/// or removed.
pub(crate) const UNIT_MARKER: &str = "# Managed by phoxal";

/// The target that owns the whole robot.
pub(crate) const TARGET_UNIT: &str = "phoxal.target";

/// The unit name one runtime runs as.
#[must_use]
pub(crate) fn runtime_unit_name(participant: &str) -> String {
    format!("phoxal-{participant}.service")
}

/// The supervisor unit.
///
/// `ExecStart` is the active release's own supervisor on that release's own
/// bundle, and nothing else. Both paths resolve through `/var/phoxal` at spawn,
/// so an install or a rollback switches the supervisor and the bundle together
/// by moving one symlink. `Type=notify` because the supervisor owns READY - it
/// notifies once its router is up, which is exactly when a runtime has
/// something to connect to.
#[must_use]
pub(crate) fn supervisor_unit() -> String {
    format!(
        r#"{UNIT_MARKER}
[Unit]
Description=Phoxal robot supervisor
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
NotifyAccess=main
{hardening}{run_directory}ExecStart={active}/{supervisor} {active}/{bundle}

[Install]
WantedBy=multi-user.target
"#,
        hardening = hardening(),
        run_directory = run_directory(),
        supervisor = phoxal_cli_project::SUPERVISOR_FILE,
        bundle = phoxal_cli_project::BUNDLE_DIR,
        active = phoxal_cli_project::ACTIVE_RUNTIME_ROOT,
    )
}

/// One runtime's unit.
///
/// The argv is the process contract and nothing more: the participant id and
/// the endpoint the supervisor bound. It is rendered by the
/// framework's own launch encoder, so the flags systemd will pass on a device
/// are the flags the participant's parser accepts, rather than a second
/// spelling of them kept in this repository. Time mode is supervisor state and
/// never appears in a participant unit.
///
/// # Errors
///
/// When the manifest names a runtime whose id is not a launch identity.
pub(crate) fn runtime_unit(runtime: &RobotRuntime) -> Result<String> {
    let argv = LaunchCommand::for_rendezvous(
        participant_id(runtime)?,
        format!(
            "unixsock-stream/{volatile}/supervisor.sock",
            volatile = phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
        ),
    )
    .argv()
    .join(" ");
    Ok(format!(
        r#"{UNIT_MARKER}
[Unit]
Description=Phoxal {role} {participant}
After={supervisor_unit}
BindsTo={supervisor_unit}
PartOf={supervisor_unit}

[Service]
Type=simple
{hardening}ExecStart={active}/{bundle}/bin/{binary} {argv}

[Install]
WantedBy={target}
"#,
        role = runtime.role,
        participant = runtime.participant_id,
        binary = runtime.binary,
        supervisor_unit = phoxal_cli_host::paths::SYSTEMD_UNIT,
        hardening = hardening(),
        active = phoxal_cli_project::ACTIVE_RUNTIME_ROOT,
        bundle = phoxal_cli_project::BUNDLE_DIR,
        target = TARGET_UNIT,
    ))
}

/// One runtime's launch identity, as the framework spells it.
///
/// The manifest a bundle carries was written by the compiler out of validated
/// model ids, so this parse describes a corrupt bundle rather than an operator
/// mistake.
fn participant_id(runtime: &RobotRuntime) -> Result<ParticipantId> {
    ParticipantId::new(runtime.participant_id.clone()).with_context(|| {
        format!(
            "the installed manifest names a runtime '{}' whose id is not a launch identity",
            runtime.participant_id
        )
    })
}

/// The target that names the whole robot, so `service start`/`stop`/`status`
/// address the set rather than a list an operator has to keep in their head.
#[must_use]
pub(crate) fn target_unit(runtimes: &[RobotRuntime]) -> String {
    let wants = runtimes
        .iter()
        .map(|runtime| format!("Wants={}\n", runtime_unit_name(&runtime.participant_id)))
        .collect::<String>();
    format!(
        r#"{UNIT_MARKER}
[Unit]
Description=Phoxal robot
Requires={supervisor_unit}
After={supervisor_unit}
{wants}
[Install]
WantedBy=multi-user.target
"#,
        supervisor_unit = phoxal_cli_host::paths::SYSTEMD_UNIT,
    )
}

/// The service settings every managed unit shares.
///
/// One string rather than one per unit: a robot whose supervisor and runtimes
/// were hardened differently, or wrote to different roots, would be a robot
/// whose failure modes depend on which half you are looking at.
fn hardening() -> String {
    format!(
        r"User=phoxal
Group=phoxal
WorkingDirectory={active}
Restart=on-failure
RestartSec=2s
TimeoutStartSec=300s
TimeoutStopSec=300s
KillMode=control-group
UMask=0007
ProtectSystem=strict
ProtectHome=true
ReadWritePaths={state} {volatile}
",
        active = phoxal_cli_project::ACTIVE_RUNTIME_ROOT,
        state = phoxal_cli_project::INSTALLED_STATE_ROOT,
        volatile = phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
    )
}

/// The run directory, declared by the supervisor's unit and by nothing else.
///
/// systemd deletes a `RuntimeDirectory=` when the unit that declares it stops,
/// and every unit that declares the same one shares that fate. With this on the
/// runtime units too, one restarting runtime deleted `/run/phoxal` out from
/// under a healthy supervisor, taking the socket every other runtime was
/// connected through with it - a robot that crash-looped itself, observed on a
/// device. The supervisor owns the directory because it owns the socket inside
/// it; the runtimes only reach in, which `ReadWritePaths` above already grants.
fn run_directory() -> String {
    "RuntimeDirectory=phoxal\nRuntimeDirectoryMode=2775\n".to_string()
}

/// Every unit an installed bundle needs, as `(file name, contents)`.
///
/// The supervisor's own unit is deliberately not here: `service install`
/// writes it once, and it does not change when the robot does.
///
/// # Errors
///
/// When the manifest names a runtime whose id is not a launch identity.
pub(crate) fn bundle_units(runtimes: &[RobotRuntime]) -> Result<Vec<(String, String)>> {
    let mut units = runtimes
        .iter()
        .map(|runtime| {
            Ok((
                runtime_unit_name(&runtime.participant_id),
                runtime_unit(runtime)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    units.push((TARGET_UNIT.to_string(), target_unit(runtimes)));
    Ok(units)
}

/// Whether `name` is a unit file this CLI generates per bundle, so a robot that
/// no longer declares a runtime has its stale unit removed rather than left
/// behind for systemd to keep restarting.
#[must_use]
pub(crate) fn is_generated_unit(name: &str) -> bool {
    name == TARGET_UNIT
        || (name.starts_with("phoxal-")
            && name.ends_with(".service")
            && name != phoxal_cli_host::paths::SYSTEMD_UNIT)
}

#[cfg(test)]
mod tests {
    use phoxal_cli_project::RuntimeRole;

    use super::*;

    /// A rebooted device must come back as a robot, not as a lone router: the
    /// target is what pulls every runtime up, so it declares the install
    /// section that lets `systemctl enable` make that true.
    #[test]
    fn the_target_can_be_enabled_so_a_reboot_brings_the_runtimes_back() {
        let units = bundle_units(&[runtime("brain", "brain", RuntimeRole::Brain)])
            .expect("the fixture ids are launch identities");
        let (_, target) = units
            .iter()
            .find(|(name, _)| name == TARGET_UNIT)
            .expect("the robot target is generated");
        assert!(target.contains("[Install]\nWantedBy=multi-user.target"));
        assert!(target.contains("Wants=phoxal-brain.service"));
    }

    /// systemd removes a `RuntimeDirectory=` when the declaring unit stops, and
    /// every unit declaring the same one shares that removal. Only the
    /// supervisor may declare `/run/phoxal`: it owns the socket inside it, and
    /// a runtime that declared it too would delete the whole robot's transport
    /// every time it restarted.
    #[test]
    fn only_the_supervisor_owns_the_run_directory() {
        assert!(supervisor_unit().contains("RuntimeDirectory=phoxal"));
        for (name, contents) in bundle_units(&[
            runtime("brain", "brain", RuntimeRole::Brain),
            runtime("left_drive", "ddsm115", RuntimeRole::Driver),
        ])
        .expect("the fixture ids are launch identities")
        {
            assert!(
                !contents.contains("RuntimeDirectory="),
                "{name} must not declare the supervisor's run directory"
            );
            if name != TARGET_UNIT {
                assert!(
                    contents.contains("ReadWritePaths=")
                        && contents.contains(phoxal_cli_project::INSTALLED_VOLATILE_ROOT),
                    "{name} still needs write access to the run directory"
                );
            }
        }
    }

    fn runtime(participant: &str, binary: &str, role: RuntimeRole) -> RobotRuntime {
        RobotRuntime {
            participant_id: participant.to_string(),
            binary: binary.to_string(),
            role,
        }
    }

    fn rover() -> Vec<RobotRuntime> {
        vec![
            runtime("brain", "brain", RuntimeRole::Brain),
            runtime("drive", "drive", RuntimeRole::Service),
            runtime("left_drive", "ddsm115", RuntimeRole::Driver),
            runtime("right_drive", "ddsm115", RuntimeRole::Driver),
        ]
    }

    /// The supervisor unit runs the active release's own supervisor on that
    /// release's own bundle, and nothing else. Both halves resolve through
    /// `/var/phoxal` at spawn, so an activation or rollback moves them
    /// together. The interactive client is never the supervisor: it builds, and
    /// a durable systemd service must never acquire Cargo or a terminal.
    #[test]
    fn the_supervisor_unit_runs_the_active_releases_own_supervisor_on_its_own_bundle() {
        let unit = supervisor_unit();
        assert_eq!(unit.matches("ExecStart=").count(), 1);
        assert!(unit.contains("ExecStart=/var/phoxal/phoxal-supervisor /var/phoxal/bundle"));
        let exec_start = unit
            .lines()
            .find(|line| line.starts_with("ExecStart="))
            .expect("the unit has an ExecStart");
        assert!(
            !exec_start.contains("/phoxal "),
            "the interactive client must never be run as the supervisor: {exec_start}"
        );
        for absent in [
            " start ",
            " run ",
            "--drivers",
            "--driver",
            "--participant-id",
        ] {
            assert!(
                !exec_start.contains(absent),
                "the supervisor takes a bundle root and nothing else: {exec_start}"
            );
        }
        assert!(unit.contains("Type=notify"));
        assert!(unit.contains("NotifyAccess=main"));
        assert!(unit.contains("User=phoxal\nGroup=phoxal"));
    }

    /// A runtime's argv is the whole process contract: its id and the endpoint.
    #[test]
    fn a_runtime_unit_carries_exactly_the_process_contract() {
        let unit = runtime_unit(&runtime("left_drive", "ddsm115", RuntimeRole::Driver))
            .expect("the fixture id is a launch identity");
        assert!(
            unit.contains(
                "ExecStart=/var/phoxal/bundle/bin/ddsm115 --participant-id left_drive \
                 --connect \
                 unixsock-stream//run/phoxal/supervisor.sock"
            ),
            "{unit}"
        );
        for absent in [
            "--simulation",
            "--bundle-root",
            "--execution-id",
            "--execution-origin",
            "--shutdown-grace-ms",
        ] {
            assert!(!unit.contains(absent), "{absent} must not appear: {unit}");
        }
        assert!(unit.contains("Restart=on-failure"), "{unit}");
        assert!(unit.contains("RestartSec=2s"), "{unit}");
    }

    /// A runtime has nothing to talk to without the supervisor's router, and
    /// that is stated three ways because systemd needs all three: ordered
    /// after it, stopped when it goes away, and stopped when it is stopped.
    #[test]
    fn every_runtime_unit_lives_and_dies_with_the_supervisor() {
        for runtime in rover() {
            let unit = runtime_unit(&runtime).expect("the fixture ids are launch identities");
            for required in [
                "After=phoxal-supervisor.service",
                "BindsTo=phoxal-supervisor.service",
                "PartOf=phoxal-supervisor.service",
            ] {
                assert!(unit.contains(required), "{required} missing: {unit}");
            }
        }
    }

    /// One driver binary serves every instance of its type, so two instances
    /// are two units running the same executable under different ids.
    #[test]
    fn two_instances_of_one_driver_type_are_two_units_of_one_binary() {
        let left = runtime_unit(&runtime("left_drive", "ddsm115", RuntimeRole::Driver))
            .expect("the fixture id is a launch identity");
        let right = runtime_unit(&runtime("right_drive", "ddsm115", RuntimeRole::Driver))
            .expect("the fixture id is a launch identity");
        assert!(
            left.contains("bin/ddsm115 --participant-id left_drive"),
            "{left}"
        );
        assert!(
            right.contains("bin/ddsm115 --participant-id right_drive"),
            "{right}"
        );
        assert_eq!(runtime_unit_name("left_drive"), "phoxal-left_drive.service");
    }

    /// The generated set is one unit per runtime plus the target that owns
    /// them, and every one of it claims this CLI's ownership on its first line.
    #[test]
    fn the_generated_set_is_one_unit_per_runtime_plus_the_target() {
        let units = bundle_units(&rover()).expect("the fixture ids are launch identities");
        let names = units
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "phoxal-brain.service",
                "phoxal-drive.service",
                "phoxal-left_drive.service",
                "phoxal-right_drive.service",
                "phoxal.target",
            ]
        );
        for (name, contents) in &units {
            assert_eq!(
                contents.lines().next(),
                Some(UNIT_MARKER),
                "{name} must claim ownership on its first line"
            );
        }
        let (_, target) = units.last().expect("the target is generated");
        for runtime in rover() {
            assert!(
                target.contains(&format!(
                    "Wants={}",
                    runtime_unit_name(&runtime.participant_id)
                )),
                "{target}"
            );
        }
        assert!(
            target.contains("Requires=phoxal-supervisor.service"),
            "{target}"
        );
    }

    /// The per-bundle units are regenerated on every install, so exactly they
    /// are the ones a stale-unit sweep may remove - and the supervisor's own
    /// unit, which `service install` owns, never is.
    #[test]
    fn only_per_bundle_units_are_swept_and_never_the_supervisors_own() {
        assert!(is_generated_unit("phoxal-brain.service"));
        assert!(is_generated_unit("phoxal-left_drive.service"));
        assert!(is_generated_unit("phoxal.target"));
        assert!(!is_generated_unit("phoxal-supervisor.service"));
        assert!(!is_generated_unit("sshd.service"));
    }
}
