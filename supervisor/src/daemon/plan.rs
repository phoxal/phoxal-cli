//! Translate persisted runtime records into host process specifications.

use std::time::Duration;

use crate::model::{
    ParticipantKind, ParticipantSpec, RestartPolicy, RuntimeFailurePolicy, StartupRequirement,
};
use phoxal_bundle::RuntimeBundle;
use phoxal_runtime_contract::identity::ExecutionId;
use phoxal_runtime_contract::metadata::ParticipantKind as RuntimeKind;
use phoxal_runtime_contract::origin::ExecutionOrigin;

const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 2_000;

pub(crate) fn participant_specs(
    bundle: &RuntimeBundle,
    execution: ExecutionId,
    origin: Option<ExecutionOrigin>,
    connect_endpoint: &str,
) -> anyhow::Result<Vec<ParticipantSpec>> {
    bundle
        .participants()
        .iter()
        .map(|participant| -> anyhow::Result<_> {
            let artifact = bundle
                .artifacts()
                .get(participant.artifact())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "validated runtime participant {} has no artifact {}",
                        participant.id(),
                        participant.artifact()
                    )
                })?;
            let id = participant.id().as_str().to_string();
            let mut args = vec![
                "--execution-id".to_string(),
                execution.to_string(),
                "--participant-id".to_string(),
                id.clone(),
                "--bundle-root".to_string(),
                bundle.root().display().to_string(),
                "--connect".to_string(),
                connect_endpoint.to_string(),
                "--shutdown-grace-ms".to_string(),
                DEFAULT_SHUTDOWN_GRACE_MS.to_string(),
            ];
            if let Some(origin) = origin {
                args.extend(["--execution-origin".to_string(), origin.encode()]);
            }
            Ok(ParticipantSpec {
                spawn: artifact.contract().kind != RuntimeKind::Simulator,
                key: participant.id().clone().into(),
                kind: kind(artifact.contract().kind),
                executable: bundle.root().join(artifact.path().as_str()),
                args,
                shutdown_grace: Duration::from_millis(DEFAULT_SHUTDOWN_GRACE_MS),
                startup_requirement: StartupRequirement::Required,
                runtime_failure: RuntimeFailurePolicy::StopProject,
                restart_policy: RestartPolicy::default(),
            })
        })
        .collect()
}

const fn kind(kind: RuntimeKind) -> ParticipantKind {
    match kind {
        RuntimeKind::Brain => ParticipantKind::Brain,
        RuntimeKind::Service => ParticipantKind::Service,
        RuntimeKind::Driver => ParticipantKind::Driver,
        RuntimeKind::Simulator => ParticipantKind::Simulator,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use phoxal_bundle::{
        AssetIndex, BinaryReference, BinarySource, BundlePath, BundleWriter, ParticipantClock,
        Runtime, RuntimeDocument, RuntimeParticipant,
    };
    use phoxal_model::{Clock, RobotBuilder};
    use phoxal_runtime_contract::identity::{ParticipantArtifactId, ParticipantId};
    use phoxal_runtime_contract::metadata::{
        ParticipantContract, ParticipantKind, ParticipantSchemas,
    };
    use phoxal_runtime_contract::version::{BusAbi, LaunchAbi, RobotApiVersion, RuntimeSchema};

    use super::*;

    #[test]
    fn strict_launch_uses_argv_and_no_environment_compatibility_path() {
        let (_root, bundle) = bundle(Clock::Real, ParticipantKind::Brain);
        let execution = ExecutionId::mint();
        let origin = ExecutionOrigin::try_mint().expect("host boot clock");
        let specs = participant_specs(
            &bundle,
            execution,
            Some(origin),
            "unixsock-stream/test.sock",
        )
        .expect("the verified bundle has a process plan");
        assert_eq!(specs.len(), 1);
        assert_eq!(
            specs[0].args,
            vec![
                "--execution-id".to_string(),
                execution.to_string(),
                "--participant-id".to_string(),
                "brain".to_string(),
                "--bundle-root".to_string(),
                bundle.root().display().to_string(),
                "--connect".to_string(),
                "unixsock-stream/test.sock".to_string(),
                "--shutdown-grace-ms".to_string(),
                "2000".to_string(),
                "--execution-origin".to_string(),
                origin.encode(),
            ]
        );
        assert!(specs[0].spawn);
    }

    #[test]
    fn a_simulator_uses_the_same_strict_argv_without_an_origin_or_spawn() {
        let (_root, bundle) = bundle(Clock::Simulated, ParticipantKind::Simulator);
        let specs = participant_specs(
            &bundle,
            ExecutionId::mint(),
            None,
            "unixsock-stream/test.sock",
        )
        .expect("the verified bundle has a process plan");
        let simulator = specs
            .iter()
            .find(|spec| spec.key.participant().as_str() == "webots")
            .expect("simulator spec");
        assert!(!simulator.spawn);
        assert!(!simulator.args.iter().any(|arg| arg == "--execution-origin"));
        assert_eq!(
            simulator
                .args
                .iter()
                .filter(|arg| arg.starts_with("--"))
                .count(),
            5
        );
    }

    fn bundle(clock: Clock, kind: ParticipantKind) -> (tempfile::TempDir, RuntimeBundle) {
        let root = tempfile::tempdir().expect("temporary bundle parent");
        let source = BinarySource::open(std::env::current_exe().expect("test executable"))
            .expect("test executable source");
        let artifact_id = ParticipantArtifactId::new(match kind {
            ParticipantKind::Brain => "brain",
            ParticipantKind::Simulator => "webots",
            ParticipantKind::Service | ParticipantKind::Driver => "participant",
        })
        .expect("artifact id");
        let participant_id = ParticipantId::new(artifact_id.as_str()).expect("participant id");
        let binary_path = BundlePath::new(format!("bin/{artifact_id}")).expect("binary path");
        let reference = BinaryReference::from_source(
            binary_path.clone(),
            ParticipantContract {
                id: artifact_id.clone(),
                kind,
                api: RobotApiVersion::new(0, 1),
                schemas: ParticipantSchemas {
                    bus: BusAbi::V0,
                    launch: LaunchAbi::V0,
                    runtime: RuntimeSchema::V0,
                },
                requirement: None,
                config_schema: serde_json::json!({"type": "null"}),
            },
            &source,
        )
        .expect("binary reference");
        let participant = RuntimeParticipant::new(
            participant_id,
            artifact_id.clone(),
            None,
            None,
            if clock == Clock::Real {
                ParticipantClock::Real
            } else {
                ParticipantClock::Simulation
            },
        );
        let mut artifacts = BTreeMap::from([(artifact_id.clone(), reference)]);
        let mut participants = vec![participant];
        let mut binaries = BTreeMap::from([(binary_path, source)]);
        if kind == ParticipantKind::Simulator {
            let brain_id = ParticipantArtifactId::new("brain").expect("brain artifact id");
            let brain_path = BundlePath::new("bin/brain").expect("brain path");
            let brain_source =
                BinarySource::open(std::env::current_exe().expect("test executable"))
                    .expect("brain executable source");
            let brain = BinaryReference::from_source(
                brain_path.clone(),
                ParticipantContract {
                    id: brain_id.clone(),
                    kind: ParticipantKind::Brain,
                    api: RobotApiVersion::new(0, 1),
                    schemas: ParticipantSchemas {
                        bus: BusAbi::V0,
                        launch: LaunchAbi::V0,
                        runtime: RuntimeSchema::V0,
                    },
                    requirement: None,
                    config_schema: serde_json::json!({"type": "null"}),
                },
                &brain_source,
            )
            .expect("brain reference");
            artifacts.insert(brain_id.clone(), brain);
            participants.push(RuntimeParticipant::new(
                ParticipantId::new("brain").expect("brain participant"),
                brain_id,
                None,
                None,
                ParticipantClock::Simulation,
            ));
            binaries.insert(brain_path, brain_source);
        }
        let runtime = Runtime::new(
            RobotBuilder::new("rover")
                .clock(clock)
                .build()
                .expect("robot"),
            artifacts,
            participants,
            AssetIndex::from_bytes(&BTreeMap::new()).expect("empty asset index"),
            None,
        )
        .expect("runtime");
        let bundle = BundleWriter::write(
            root.path().join("bundle"),
            &RuntimeDocument::new(runtime),
            &BTreeMap::new(),
            &binaries,
        )
        .expect("verified bundle");
        (root, bundle)
    }
}
