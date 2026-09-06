use std::collections::BTreeMap;
use std::fs::FileTimes;
use std::io::Write;
use std::sync::Mutex;

use phoxal::identity::{ExecutionId, ProducerId, RobotId};
use phoxal::model::identity::{SpawnId, WorldId};
use phoxal::model::world::{WorldDigest, WorldProgress, WorldProvenance};
use phoxal::version::FrameworkVersion;
use phoxal::world::api::session::state::WorldSessionState;
use phoxal::world::api::session::{
    WorldLifecycle, WorldMemberCleanup, WorldMemberEndReason, WorldMemberTerminal, WorldMotion,
};

use super::*;

const INSTANCE: &str = "123456789abcdef0123456789abcdef0";

#[derive(Default)]
struct Inspector(BTreeMap<u32, u64>);

impl ProcessInspector for Inspector {
    fn started_at_unix_s(&self, pid: u32) -> Option<u64> {
        self.0.get(&pid).copied()
    }
}

struct NativeControl {
    state: Mutex<NativeControlState>,
    on_first_wait: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

struct NativeControlState {
    observed: Option<ObservedNativeProcess>,
    alive: bool,
    stop_on_terminate: bool,
    waits: Vec<Duration>,
    signals: Vec<NativeSignal>,
}

impl NativeControl {
    fn exited() -> Self {
        Self {
            state: Mutex::new(NativeControlState {
                observed: None,
                alive: false,
                stop_on_terminate: false,
                waits: Vec::new(),
                signals: Vec::new(),
            }),
            on_first_wait: Mutex::new(None),
        }
    }

    fn live(observed: ObservedNativeProcess, stop_on_terminate: bool) -> Self {
        Self {
            state: Mutex::new(NativeControlState {
                observed: Some(observed),
                alive: true,
                stop_on_terminate,
                waits: Vec::new(),
                signals: Vec::new(),
            }),
            on_first_wait: Mutex::new(None),
        }
    }

    fn on_first_wait(self, callback: impl FnOnce() + Send + 'static) -> Self {
        *self.on_first_wait.lock().unwrap() = Some(Box::new(callback));
        self
    }
}

impl NativeProcessControl for NativeControl {
    fn observe(&self, _pid: u32) -> Result<Option<ObservedNativeProcess>> {
        Ok(self.state.lock().unwrap().observed.clone())
    }

    fn process_group_alive(&self, _process_group: u32) -> Result<bool> {
        Ok(self.state.lock().unwrap().alive)
    }

    fn signal_process_group(&self, _process_group: u32, signal: NativeSignal) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.signals.push(signal);
        if signal == NativeSignal::Kill
            || (signal == NativeSignal::Terminate && state.stop_on_terminate)
        {
            state.alive = false;
        }
        Ok(())
    }

    fn wait(&self, duration: Duration) {
        self.state.lock().unwrap().waits.push(duration);
        if let Some(callback) = self.on_first_wait.lock().unwrap().take() {
            callback();
        }
    }
}

fn paths() -> (tempfile::TempDir, WorldPaths) {
    let temporary = tempfile::tempdir().unwrap();
    let paths = WorldPaths::create(
        temporary.path().join("run"),
        temporary.path().join("evidence"),
    )
    .unwrap();
    (temporary, paths)
}

fn summary(instance: &str, ended_at_unix_ms: u64) -> WorldTerminalSummary {
    WorldTerminalSummary {
        schema: TERMINAL_SUMMARY_SCHEMA.to_string(),
        instance: WorldInstanceId::parse(instance).unwrap(),
        provenance: WorldProvenance {
            world: WorldId::new("warehouse").unwrap(),
            digest: WorldDigest::parse(&"b".repeat(64)).unwrap(),
            random_seed: 7,
            framework: FrameworkVersion::new(0, 68, 2),
            adapter: "webots".to_owned(),
            adapter_version: "0.68.2".to_owned(),
            simulator_version: "R2025a".to_owned(),
            platform: "test".to_owned(),
            time_step_ns: 12_000_000,
        },
        outcome: TerminalOutcome::Stopped {
            reason: SimulationEndReason::WorldStopped,
        },
        progress: WorldProgress::at(1, 12_000_000).unwrap(),
        members: Vec::new(),
        member_evidence: Vec::new(),
        failing: TerminalFailure {
            process: None,
            producer: None,
        },
        evidence: Vec::new(),
        cleanup: TerminalCleanup {
            complete: true,
            detail: None,
        },
        retention: TerminalRetention {
            log_byte_limit: 16,
            truncated: Vec::new(),
        },
        ended_at_unix_ms,
    }
}

#[cfg(unix)]
fn write_owner_json(path: &Path, value: &impl Serialize) -> File {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(&serde_json::to_vec(value).unwrap()).unwrap();
    file
}

fn registration() -> LocalWorldRegistration {
    LocalWorldRegistration {
        schema: REGISTRATION_SCHEMA.to_string(),
        instance: WorldInstanceId::parse(INSTANCE).unwrap(),
        endpoint: "tcp/127.0.0.1:7447".to_string(),
        process: ProcessIdentity {
            pid: 42,
            started_at_unix_s: 100,
        },
        framework: FrameworkVersion::new(0, 68, 2),
        world: RegisteredWorld {
            id: WorldId::new("warehouse").unwrap(),
            digest: WorldDigest::parse(&"a".repeat(64)).unwrap(),
        },
        lease: format!("{INSTANCE}.lease"),
    }
}

#[cfg(unix)]
fn write_registration(paths: &WorldPaths) -> File {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let lease_path = paths.registry().join(format!("{INSTANCE}.lease"));
    let lease = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lease_path)
        .unwrap();
    // SAFETY: `lease` owns a valid descriptor.
    assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX) }, 0);

    let registration = registration();
    let path = paths.registration_path(INSTANCE);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(&serde_json::to_vec(&registration).unwrap())
        .unwrap();
    lease
}

fn checkpoint() -> WorldCheckpoint {
    let registration = registration();
    WorldCheckpoint {
        schema: WORLD_CHECKPOINT_SCHEMA.to_owned(),
        process: registration.process,
        native_process: Some(NativeProcessIdentity {
            process: ProcessIdentity {
                pid: 52,
                started_at_unix_s: 200,
            },
            executable: std::env::current_exe().unwrap().canonicalize().unwrap(),
            process_group: Some(52),
        }),
        state: WorldSessionState {
            revision: 3,
            instance: registration.instance,
            provenance: WorldProvenance {
                world: registration.world.id,
                digest: registration.world.digest,
                random_seed: 7,
                framework: registration.framework,
                adapter: "webots".to_owned(),
                adapter_version: "0.68.2".to_owned(),
                simulator_version: "R2025a".to_owned(),
                platform: "test".to_owned(),
                time_step_ns: 12_000_000,
            },
            lifecycle: WorldLifecycle::Ready {
                motion: WorldMotion::Running,
            },
            progress: WorldProgress::at(4, 12_000_000).unwrap(),
            members: Vec::new(),
        },
        updated_at_unix_ms: 200_001,
    }
}

#[cfg(unix)]
fn recovery_fixture() -> (tempfile::TempDir, WorldPaths) {
    let (temporary, paths) = paths();
    drop(write_registration(&paths));
    let root = paths.evidence_path(INSTANCE);
    secure_directory(&root).unwrap();
    secure_directory(&root.join("members")).unwrap();
    write_owner_json(&root.join("checkpoint.json"), &checkpoint());
    (temporary, paths)
}

#[cfg(unix)]
#[test]
fn exact_lookup_requires_matching_process_birth_and_a_held_lease() {
    let (_temporary, paths) = paths();
    let _lease = write_registration(&paths);
    let registry = WorldRegistry::new(paths, Inspector(BTreeMap::from([(42, 100)])));
    assert_eq!(
        registry.resolve(INSTANCE).unwrap().instance.to_string(),
        INSTANCE
    );
    assert!(registry.resolve(&INSTANCE[..31]).is_err());
}

#[cfg(unix)]
#[test]
fn a_reused_pid_is_never_accepted_as_the_original_host() {
    let (_temporary, paths) = paths();
    let lease = write_registration(&paths);
    drop(lease);
    let registry = WorldRegistry::new(paths.clone(), Inspector(BTreeMap::from([(42, 101)])));
    assert!(registry.resolve(INSTANCE).is_err());
    assert!(paths.registration_path(INSTANCE).exists());
    assert!(paths.registry().join(format!("{INSTANCE}.lease")).exists());
}

#[cfg(unix)]
#[test]
fn stale_host_recovery_publishes_typed_host_lost_summary_after_native_exit() {
    let (_temporary, paths) = recovery_fixture();
    let root = paths.evidence_path(INSTANCE);
    let execution = ExecutionId::parse("1234567890abcdef1234567890abcdef").unwrap();
    let record = WorldMemberEvidence {
        schema: MEMBER_TERMINAL_SCHEMA.to_owned(),
        terminal: WorldMemberTerminal {
            execution,
            robot: RobotId::new("rover").unwrap(),
            controller: ProducerId::parse("2234567890abcdef1234567890abcdef").unwrap(),
            spawn: SpawnId::new("loading-bay").unwrap(),
            reason: WorldMemberEndReason::ControllerFault,
            last_progress: WorldProgress::at(3, 12_000_000).unwrap(),
            cleanup: WorldMemberCleanup::Incomplete {
                detail: "host disappeared".to_owned(),
            },
            evidence_paths: Vec::new(),
        },
    };
    write_owner_json(
        &root.join("members").join(format!("{execution}.json")),
        &record,
    );
    write_owner_json(&root.join("host.log"), &"host evidence");
    let registry = WorldRegistry::new(paths.clone(), Inspector::default());
    let evidence = WorldEvidence::new(paths.clone());
    let control = NativeControl::exited();

    let recovered = registry
        .recover_host_loss(&evidence, INSTANCE, &control)
        .unwrap()
        .unwrap();

    assert_eq!(recovered.outcome.reason(), SimulationEndReason::HostLost);
    assert_eq!(recovered.progress, checkpoint().state.progress);
    assert_eq!(
        recovered.member_evidence,
        vec![WorldMemberEvidenceIndex {
            execution,
            path: format!("members/{execution}.json"),
        }]
    );
    assert_eq!(recovered.evidence, vec!["host.log"]);
    assert!(!recovered.cleanup.complete);
    assert_eq!(
        control.state.lock().unwrap().waits.first(),
        Some(&NATIVE_EXIT_GRACE)
    );
    assert!(control.state.lock().unwrap().signals.is_empty());
    assert!(!paths.registration_path(INSTANCE).exists());
    assert!(!paths.registry().join(format!("{INSTANCE}.lease")).exists());
    assert_eq!(evidence.read_summary(INSTANCE).unwrap(), Some(recovered));
}

#[cfg(unix)]
#[test]
fn native_pid_reuse_refuses_every_group_signal_and_retains_recovery_inputs() {
    let (_temporary, paths) = recovery_fixture();
    let expected = checkpoint().native_process.unwrap();
    let control = NativeControl::live(
        ObservedNativeProcess {
            process: ProcessIdentity {
                started_at_unix_s: expected.process.started_at_unix_s + 1,
                ..expected.process
            },
            executable: expected.executable,
            process_group: expected.process_group.unwrap(),
        },
        false,
    );
    let registry = WorldRegistry::new(paths.clone(), Inspector::default());
    let evidence = WorldEvidence::new(paths.clone());

    let error = registry
        .recover_host_loss(&evidence, INSTANCE, &control)
        .unwrap_err()
        .to_string();

    assert!(error.contains("reused"), "{error}");
    assert!(control.state.lock().unwrap().signals.is_empty());
    assert!(paths.registration_path(INSTANCE).exists());
    assert!(evidence.read_summary(INSTANCE).unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn exact_native_identity_is_revalidated_before_bounded_group_termination() {
    let (_temporary, paths) = recovery_fixture();
    let expected = checkpoint().native_process.unwrap();
    let control = NativeControl::live(
        ObservedNativeProcess {
            process: expected.process,
            executable: expected.executable,
            process_group: expected.process_group.unwrap(),
        },
        true,
    );
    let registry = WorldRegistry::new(paths.clone(), Inspector::default());
    let evidence = WorldEvidence::new(paths);

    registry
        .recover_host_loss(&evidence, INSTANCE, &control)
        .unwrap()
        .unwrap();

    assert_eq!(
        control.state.lock().unwrap().signals,
        vec![NativeSignal::Terminate]
    );
}

#[cfg(unix)]
#[test]
fn malformed_mismatched_and_stale_checkpoints_never_finalize() {
    let (_temporary, paths) = recovery_fixture();
    let checkpoint_path = paths.evidence_path(INSTANCE).join("checkpoint.json");
    fs::remove_file(&checkpoint_path).unwrap();
    write_owner_json(&checkpoint_path, &serde_json::json!({"schema": "broken"}));
    let registry = WorldRegistry::new(paths.clone(), Inspector::default());
    let evidence = WorldEvidence::new(paths.clone());
    assert!(
        registry
            .recover_host_loss(&evidence, INSTANCE, &NativeControl::exited())
            .is_err()
    );
    assert!(paths.registration_path(INSTANCE).exists());

    fs::remove_file(&checkpoint_path).unwrap();
    let mut mismatched = checkpoint();
    mismatched.process.started_at_unix_s += 1;
    write_owner_json(&checkpoint_path, &mismatched);
    assert!(
        registry
            .recover_host_loss(&evidence, INSTANCE, &NativeControl::exited())
            .unwrap_err()
            .to_string()
            .contains("process identity")
    );

    fs::remove_file(&checkpoint_path).unwrap();
    let mut stale = checkpoint();
    stale.updated_at_unix_ms = 99_999;
    write_owner_json(&checkpoint_path, &stale);
    assert!(
        registry
            .recover_host_loss(&evidence, INSTANCE, &NativeControl::exited())
            .unwrap_err()
            .to_string()
            .contains("predates")
    );
    assert!(evidence.read_summary(INSTANCE).unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn real_summary_wins_before_or_during_recovery() {
    let (_temporary, paths) = recovery_fixture();
    let expected = summary(INSTANCE, 71);
    write_owner_json(
        &paths.evidence_path(INSTANCE).join("summary.json"),
        &expected,
    );
    let registry = WorldRegistry::new(paths.clone(), Inspector::default());
    let evidence = WorldEvidence::new(paths.clone());
    let recovered = registry
        .recover_host_loss(&evidence, INSTANCE, &NativeControl::exited())
        .unwrap()
        .unwrap();
    assert_eq!(recovered, expected);
    assert!(!paths.registration_path(INSTANCE).exists());

    let (_temporary, paths) = recovery_fixture();
    let expected = summary(INSTANCE, 72);
    let summary_path = paths.evidence_path(INSTANCE).join("summary.json");
    let late = expected.clone();
    let control = NativeControl::exited().on_first_wait(move || {
        write_owner_json(&summary_path, &late);
    });
    let registry = WorldRegistry::new(paths.clone(), Inspector::default());
    let evidence = WorldEvidence::new(paths);
    let recovered = registry
        .recover_host_loss(&evidence, INSTANCE, &control)
        .unwrap()
        .unwrap();
    assert_eq!(recovered, expected);
}

#[cfg(unix)]
#[test]
fn atomic_recovery_publish_never_overwrites_an_existing_summary() {
    let (_temporary, paths) = paths();
    let root = paths.evidence_path(INSTANCE);
    secure_directory(&root).unwrap();
    let authoritative = summary(INSTANCE, 91);
    write_owner_json(&root.join("summary.json"), &authoritative);
    let mut recovery = summary(INSTANCE, 92);
    recovery.outcome = TerminalOutcome::Failed {
        reason: SimulationEndReason::HostLost,
        detail: "synthetic".to_owned(),
    };
    let evidence = WorldEvidence::new(paths);

    let winner = evidence
        .publish_recovered_summary(INSTANCE, &recovery)
        .unwrap();

    assert_eq!(winner, authoritative);
    assert_eq!(
        evidence.read_summary(INSTANCE).unwrap(),
        Some(authoritative)
    );
}

#[cfg(unix)]
#[test]
fn repeated_host_loss_recovery_is_idempotent() {
    let (_temporary, paths) = recovery_fixture();
    let registry = WorldRegistry::new(paths.clone(), Inspector::default());
    let evidence = WorldEvidence::new(paths);
    let first = registry
        .recover_host_loss(&evidence, INSTANCE, &NativeControl::exited())
        .unwrap()
        .unwrap();
    let second = registry
        .recover_host_loss(&evidence, INSTANCE, &NativeControl::exited())
        .unwrap()
        .unwrap();
    assert_eq!(second, first);
}

#[test]
fn retention_removes_only_oldest_complete_terminal_sessions() {
    let (_temporary, paths) = paths();
    let evidence = WorldEvidence::new(paths.clone());
    let first = INSTANCE;
    let second = "223456789abcdef0123456789abcdef0";
    for (instance, ended) in [(first, 1), (second, 2)] {
        let directory = paths.evidence_path(instance);
        secure_directory(&directory).unwrap();
        let summary = summary(instance, ended);
        #[cfg(unix)]
        {
            let file = write_owner_json(&directory.join("summary.json"), &summary);
            let modified = if ended == 1 { 200 } else { 100 };
            file.set_times(
                FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(modified)),
            )
            .unwrap();
        }
    }
    let report = evidence.prune(1, &BTreeSet::new()).unwrap();
    assert_eq!(report.removed, vec![first]);
    assert!(!paths.evidence_path(first).exists());
    assert!(paths.evidence_path(second).exists());
}

#[cfg(unix)]
#[test]
fn retention_removes_only_stale_exact_bootstrap_log_residue() {
    let (_temporary, paths) = paths();
    let evidence = WorldEvidence::new(paths.clone());
    let stale = paths.evidence().join(".starting-Ab12Cd.host.log");
    let current = paths.evidence().join(".starting-Ef34Gh.host.log");
    let lookalike = paths.evidence().join(".starting-not-a-temp.host.log");
    let stale_file = write_owner_json(&stale, &"old");
    write_owner_json(&current, &"current");
    write_owner_json(&lookalike, &"keep");
    stale_file
        .set_times(
            FileTimes::new()
                .set_modified(SystemTime::now() - STALE_BOOTSTRAP_LOG_AGE - Duration::from_secs(1)),
        )
        .unwrap();

    let report = evidence.prune(1, &BTreeSet::new()).unwrap();

    assert_eq!(report.bootstrap_logs_removed, vec![stale.clone()]);
    assert!(!stale.exists());
    assert!(current.exists());
    assert!(lookalike.exists());
}

#[test]
fn terminal_summary_round_trips_typed_failure_identity() {
    let mut summary = summary(INSTANCE, 7);
    summary.outcome = TerminalOutcome::Failed {
        reason: SimulationEndReason::ControllerLost,
        detail: "robot controller exited".to_owned(),
    };
    summary.failing = TerminalFailure {
        process: Some(ProcessIdentity {
            pid: 42,
            started_at_unix_s: 100,
        }),
        producer: Some(ProducerId::parse("2234567890abcdef1234567890abcdef").unwrap()),
    };

    let decoded: WorldTerminalSummary =
        serde_json::from_slice(&serde_json::to_vec(&summary).unwrap()).unwrap();

    assert_eq!(decoded, summary);
    assert_eq!(
        decoded.outcome.reason(),
        SimulationEndReason::ControllerLost
    );
    assert_eq!(decoded.outcome.detail(), Some("robot controller exited"));
}

#[cfg(unix)]
#[test]
fn member_terminal_evidence_is_read_and_validated() {
    let (_temporary, paths) = paths();
    let mut summary = summary(INSTANCE, 7);
    let members = paths.evidence_path(INSTANCE).join("members");
    secure_directory(&members).unwrap();
    let execution = ExecutionId::parse("1234567890abcdef1234567890abcdef").unwrap();
    let record = WorldMemberEvidence {
        schema: MEMBER_TERMINAL_SCHEMA.to_owned(),
        terminal: WorldMemberTerminal {
            execution,
            robot: RobotId::new("rover").unwrap(),
            controller: ProducerId::parse("2234567890abcdef1234567890abcdef").unwrap(),
            spawn: SpawnId::new("loading-bay").unwrap(),
            reason: WorldMemberEndReason::Stopped,
            last_progress: WorldProgress::at(4, 12_000_000).unwrap(),
            cleanup: WorldMemberCleanup::Complete,
            evidence_paths: vec!["members/controller.log".to_owned()],
        },
    };
    summary.member_evidence = vec![WorldMemberEvidenceIndex {
        execution,
        path: format!("members/{execution}.json"),
    }];
    write_owner_json(&members.join(format!("{execution}.json")), &record);

    let evidence = WorldEvidence::new(paths);
    assert_eq!(
        evidence.read_member_terminal(INSTANCE, execution).unwrap(),
        Some(record.clone())
    );
    let read = evidence.read_member_evidence(&summary).unwrap();

    assert_eq!(read, vec![record]);
}
