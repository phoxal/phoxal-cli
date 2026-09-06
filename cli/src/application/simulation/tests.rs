use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex};

use super::connect::{
    ConnectedSimulationEnding, await_connected_simulation_ending, connect_verified,
    connected_simulation_ending_description, ensure_bootstrap_matches_registration,
    ensure_compatible_train, ensure_ready_and_paused,
};
use super::*;
use phoxal::identity::{ExecutionId, ProducerId, RobotId};
use phoxal::model::identity::WorldId;
use phoxal::model::world::{WorldDigest, WorldInstanceId, WorldProgress, WorldProvenance};
use phoxal::supervisor::api::simulation::SimulationEndReason;
use phoxal::world::api::session::diagnostics::WorldSessionDiagnostics;
use phoxal::world::{WorldSessionHandler, WorldSessionOperation, WorldSessionServer};
use phoxal_cli_host::world::{
    ProcessIdentity, ProcessInspector, REGISTRATION_SCHEMA, RegisteredWorld,
    SystemProcessInspector, TERMINAL_SUMMARY_SCHEMA, TerminalCleanup, TerminalFailure,
    TerminalRetention,
};
use serde::Serialize;
use tokio::sync::broadcast;

const WORKFLOW_INSTANCE: &str = "3234567890abcdef1234567890abcdef";

struct WorkflowWorld {
    bootstrap: WorldSessionBootstrap,
    state: Mutex<WorldSessionState>,
    states: Mutex<broadcast::Sender<WorldSessionState>>,
    diagnostics: Mutex<WorldSessionDiagnostics>,
    diagnostic_updates: Mutex<broadcast::Sender<WorldSessionDiagnostics>>,
    paths: WorldPaths,
}

impl WorkflowWorld {
    fn new(paths: WorldPaths) -> Self {
        let instance = WorldInstanceId::parse(WORKFLOW_INSTANCE).unwrap();
        let world = WorldId::new("warehouse").unwrap();
        let digest = WorldDigest::parse(&"c".repeat(64)).unwrap();
        let bootstrap = WorldSessionBootstrap {
            instance,
            framework: FrameworkVersion::CURRENT,
            world: world.clone(),
            digest,
        };
        let state = WorldSessionState {
            revision: 1,
            instance,
            provenance: WorldProvenance {
                world,
                digest,
                random_seed: 17,
                framework: FrameworkVersion::CURRENT,
                adapter: "workflow-test".to_owned(),
                adapter_version: "1".to_owned(),
                simulator_version: "fake".to_owned(),
                platform: "test".to_owned(),
                time_step_ns: 10_000_000,
            },
            lifecycle: WorldLifecycle::Ready {
                motion: WorldMotion::Paused,
            },
            progress: WorldProgress::at(3, 10_000_000).unwrap(),
            members: Vec::new(),
        };
        let (states, _) = broadcast::channel(8);
        let (diagnostic_updates, _) = broadcast::channel(8);
        Self {
            bootstrap,
            state: Mutex::new(state),
            states: Mutex::new(states),
            diagnostics: Mutex::new(WorldSessionDiagnostics {
                revision: 2,
                pacing: None,
                last_transition_age_ns: Some(1),
            }),
            diagnostic_updates: Mutex::new(diagnostic_updates),
            paths,
        }
    }

    fn rotate_state_stream(&self) {
        let (replacement, _) = broadcast::channel(8);
        *self.states.lock().unwrap() = replacement;
    }

    fn rotate_diagnostics_stream(&self) {
        let (replacement, _) = broadcast::channel(8);
        *self.diagnostic_updates.lock().unwrap() = replacement;
    }

    fn stop(&self) -> Result<WorldSessionState, String> {
        let mut state = self.state.lock().unwrap();
        state.revision += 1;
        state.lifecycle = WorldLifecycle::Stopping;
        let stopped = state.clone();
        let _ = self.states.lock().unwrap().send(stopped.clone());
        let root = self.paths.evidence_path(WORKFLOW_INSTANCE);
        let summary = WorldTerminalSummary {
            schema: TERMINAL_SUMMARY_SCHEMA.to_owned(),
            instance: state.instance,
            provenance: state.provenance.clone(),
            outcome: TerminalOutcome::Stopped {
                reason: SimulationEndReason::WorldStopped,
            },
            progress: state.progress,
            members: state.members.clone(),
            member_evidence: Vec::new(),
            failing: TerminalFailure {
                process: None,
                producer: None,
            },
            evidence: vec!["host.log".to_owned(), "webots.log".to_owned()],
            cleanup: TerminalCleanup {
                complete: true,
                detail: None,
            },
            retention: TerminalRetention {
                log_byte_limit: 1_024,
                truncated: Vec::new(),
            },
            ended_at_unix_ms: 123_456,
        };
        write_owner_json(&root.join("summary.json"), &summary)
            .map_err(|error| error.to_string())?;
        fs::remove_file(self.paths.registration_path(WORKFLOW_INSTANCE))
            .map_err(|error| error.to_string())?;
        fs::remove_file(
            self.paths
                .registry()
                .join(format!("{WORKFLOW_INSTANCE}.lease")),
        )
        .map_err(|error| error.to_string())?;
        Ok(stopped)
    }
}

impl WorldSessionHandler for WorkflowWorld {
    fn bootstrap(&self) -> WorldSessionBootstrap {
        self.bootstrap.clone()
    }

    fn state(&self) -> WorldSessionState {
        self.state.lock().unwrap().clone()
    }

    fn subscribe_state(&self) -> broadcast::Receiver<WorldSessionState> {
        self.states.lock().unwrap().subscribe()
    }

    fn diagnostics(&self) -> WorldSessionDiagnostics {
        *self.diagnostics.lock().unwrap()
    }

    fn subscribe_diagnostics(&self) -> broadcast::Receiver<WorldSessionDiagnostics> {
        self.diagnostic_updates.lock().unwrap().subscribe()
    }

    fn control(&self, request: WorldControl) -> WorldSessionOperation<'_, WorldSessionState> {
        Box::pin(async move {
            match request {
                WorldControl::Stop => self.stop(),
                WorldControl::Pause | WorldControl::Resume => Ok(self.state()),
            }
        })
    }

    fn attach(
        &self,
        _execution: ExecutionId,
        _supervisor_endpoint: String,
        _spawn: Option<SpawnId>,
    ) -> WorldSessionOperation<'_, WorldSessionState> {
        Box::pin(async move { Ok(self.state()) })
    }
}

#[cfg(unix)]
fn write_owner_json(path: &Path, value: &impl Serialize) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    Ok(file)
}

#[cfg(unix)]
fn write_owner_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(file)
}

#[cfg(unix)]
fn create_owner_directory(path: &Path) {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new().mode(0o700).create(path).unwrap();
}

#[cfg(unix)]
fn write_live_registration(paths: &WorldPaths, endpoint: &str) -> (LocalWorldRegistration, File) {
    use std::os::fd::AsRawFd;

    let inspector = SystemProcessInspector;
    let pid = std::process::id();
    let process = ProcessIdentity {
        pid,
        started_at_unix_s: inspector.started_at_unix_s(pid).unwrap(),
    };
    let instance = WorldInstanceId::parse(WORKFLOW_INSTANCE).unwrap();
    let registration = LocalWorldRegistration {
        schema: REGISTRATION_SCHEMA.to_owned(),
        instance,
        endpoint: endpoint.to_owned(),
        process,
        framework: FrameworkVersion::CURRENT,
        world: RegisteredWorld {
            id: WorldId::new("warehouse").unwrap(),
            digest: WorldDigest::parse(&"c".repeat(64)).unwrap(),
        },
        lease: format!("{WORKFLOW_INSTANCE}.lease"),
    };
    let lease = write_owner_bytes(
        &paths.registry().join(format!("{WORKFLOW_INSTANCE}.lease")),
        b"",
    )
    .unwrap();
    // SAFETY: `lease` owns a valid descriptor for the fixture's lifetime.
    assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX) }, 0);
    write_owner_json(&paths.registration_path(WORKFLOW_INSTANCE), &registration).unwrap();
    (registration, lease)
}

#[cfg(unix)]
async fn workflow_fixture() -> (
    tempfile::TempDir,
    Stores,
    Arc<WorkflowWorld>,
    WorldSessionServer,
    LocalWorldRegistration,
    File,
) {
    let temporary = tempfile::tempdir().unwrap();
    let paths = WorldPaths::create(
        temporary.path().join("registry"),
        temporary.path().join("evidence"),
    )
    .unwrap();
    let evidence = paths.evidence_path(WORKFLOW_INSTANCE);
    create_owner_directory(&evidence);
    write_owner_bytes(&evidence.join("host.log"), b"host retained\n").unwrap();
    write_owner_bytes(&evidence.join("webots.log"), b"webots retained\n").unwrap();
    let handler = Arc::new(WorkflowWorld::new(paths.clone()));
    let server = WorldSessionServer::bind(Arc::clone(&handler))
        .await
        .unwrap();
    let (registration, lease) = write_live_registration(&paths, server.endpoint());
    (
        temporary,
        Stores::at(paths),
        handler,
        server,
        registration,
        lease,
    )
}

fn registration() -> LocalWorldRegistration {
    let instance =
        WorldInstanceId::parse("1234567890abcdef1234567890abcdef").expect("world instance");
    LocalWorldRegistration {
        schema: REGISTRATION_SCHEMA.to_owned(),
        instance,
        endpoint: "tcp://127.0.0.1:12345".to_owned(),
        process: ProcessIdentity {
            pid: 42,
            started_at_unix_s: 100,
        },
        framework: FrameworkVersion::new(0, 68, 2),
        world: RegisteredWorld {
            id: WorldId::new("warehouse").expect("world id"),
            digest: WorldDigest::parse(&"a".repeat(64)).expect("world digest"),
        },
        lease: format!("{instance}.lease"),
    }
}

fn bootstrap(registration: &LocalWorldRegistration) -> WorldSessionBootstrap {
    WorldSessionBootstrap {
        instance: registration.instance,
        framework: registration.framework,
        world: registration.world.id.clone(),
        digest: registration.world.digest,
    }
}

fn state(lifecycle: WorldLifecycle) -> WorldSessionState {
    WorldSessionState {
        revision: 1,
        instance: WorldInstanceId::parse("1234567890abcdef1234567890abcdef").unwrap(),
        provenance: WorldProvenance {
            world: WorldId::new("warehouse").unwrap(),
            digest: WorldDigest::parse(&"a".repeat(64)).unwrap(),
            random_seed: 7,
            framework: FrameworkVersion::new(0, 68, 2),
            adapter: "webots".to_owned(),
            adapter_version: "R2025a".to_owned(),
            simulator_version: "R2025a".to_owned(),
            platform: "test".to_owned(),
            time_step_ns: 10_000_000,
        },
        lifecycle,
        progress: WorldProgress::at(0, 10_000_000).unwrap(),
        members: Vec::new(),
    }
}

#[test]
fn live_status_is_a_complete_pure_projection_of_the_returned_state() {
    let rendered = format_live_status(&state(WorldLifecycle::Ready {
        motion: WorldMotion::Paused,
    }));
    for expected in [
        "instance:  1234567890abcdef1234567890abcdef",
        "world:     warehouse",
        "lifecycle: ready/paused",
        "step:      0",
        "members:   0",
    ] {
        assert!(rendered.contains(expected), "{rendered}");
    }
}

fn member_ending(
    reason: WorldMemberEndReason,
    cleanup: WorldMemberCleanup,
) -> ConnectedSimulationEnding {
    ConnectedSimulationEnding::Member(WorldMemberEvidence {
        schema: phoxal_cli_host::world::MEMBER_TERMINAL_SCHEMA.to_owned(),
        terminal: phoxal::world::api::session::WorldMemberTerminal {
            execution: ExecutionId::parse("1234567890abcdef1234567890abcdef").unwrap(),
            robot: RobotId::new("rover").unwrap(),
            controller: ProducerId::parse("2234567890abcdef1234567890abcdef").unwrap(),
            spawn: SpawnId::new("loading-bay").unwrap(),
            reason,
            last_progress: WorldProgress::at(4, 10_000_000).unwrap(),
            cleanup,
            evidence_paths: Vec::new(),
        },
    })
}

#[test]
fn same_line_patches_pass_preflight_in_both_directions() {
    assert!(
        ensure_compatible_train(
            FrameworkVersion::new(0, 68, 2),
            FrameworkVersion::new(0, 68, 0)
        )
        .is_ok()
    );
    assert!(
        ensure_compatible_train(
            FrameworkVersion::new(1, 4, 0),
            FrameworkVersion::new(1, 9, 7)
        )
        .is_ok()
    );
}

#[test]
fn adjacent_line_is_refused_with_both_versions_and_required_line() {
    let error = ensure_compatible_train(
        FrameworkVersion::new(0, 68, 2),
        FrameworkVersion::new(0, 69, 0),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("0.68.2"), "{error}");
    assert!(error.contains("0.69.0"), "{error}");
    assert!(error.contains("0.68.x"), "{error}");
    assert!(error.contains("before any build or launch"), "{error}");
}

#[test]
fn live_endpoint_bootstrap_must_match_every_locator_identity() {
    let registration = registration();
    assert!(
        ensure_bootstrap_matches_registration(&bootstrap(&registration), &registration).is_ok()
    );

    let mut wrong_instance = bootstrap(&registration);
    wrong_instance.instance = WorldInstanceId::parse("2234567890abcdef1234567890abcdef").unwrap();
    let error = ensure_bootstrap_matches_registration(&wrong_instance, &registration)
        .unwrap_err()
        .to_string();
    assert!(error.contains("mismatched locator"), "{error}");

    let mut wrong_provenance = bootstrap(&registration);
    wrong_provenance.digest = WorldDigest::parse(&"b".repeat(64)).unwrap();
    let error = ensure_bootstrap_matches_registration(&wrong_provenance, &registration)
        .unwrap_err()
        .to_string();
    assert!(error.contains("registered digest"), "{error}");
}

#[test]
fn launch_commit_requires_authoritative_ready_and_paused_state() {
    assert!(
        ensure_ready_and_paused(&state(WorldLifecycle::Ready {
            motion: WorldMotion::Paused,
        }))
        .is_ok()
    );
    for lifecycle in [
        WorldLifecycle::Starting,
        WorldLifecycle::Ready {
            motion: WorldMotion::Running,
        },
        WorldLifecycle::Stopping,
    ] {
        let error = ensure_ready_and_paused(&state(lifecycle))
            .unwrap_err()
            .to_string();
        assert!(error.contains("authoritative lifecycle"), "{error}");
    }
}

#[test]
fn typed_member_outcomes_distinguish_clean_stop_from_world_failure() {
    let (stopped, failed) = connected_simulation_ending_description(&member_ending(
        WorldMemberEndReason::Stopped,
        WorldMemberCleanup::Complete,
    ));
    assert!(!failed, "{stopped}");
    assert!(stopped.contains("Stopped"), "{stopped}");

    let (fault, failed) = connected_simulation_ending_description(&member_ending(
        WorldMemberEndReason::ControllerFault,
        WorldMemberCleanup::Incomplete {
            detail: "native controller survived".to_owned(),
        },
    ));
    assert!(failed, "{fault}");
    assert!(fault.contains("ControllerFault"), "{fault}");
    assert!(fault.contains("native controller survived"), "{fault}");
}

#[cfg(unix)]
#[tokio::test]
async fn world_stop_member_evidence_waits_for_world_terminal_summary() {
    let (_temporary, stores, handler, server, registration, _lease) = workflow_fixture().await;
    let client = connect_verified(&registration).await.unwrap();
    let ConnectedSimulationEnding::Member(member) =
        member_ending(WorldMemberEndReason::Stopped, WorldMemberCleanup::Complete)
    else {
        unreachable!();
    };
    let members = handler
        .paths
        .evidence_path(WORKFLOW_INSTANCE)
        .join("members");
    create_owner_directory(&members);
    write_owner_json(
        &members.join(format!("{}.json", member.terminal.execution)),
        &member,
    )
    .unwrap();
    handler.state.lock().unwrap().lifecycle = WorldLifecycle::Stopping;
    let ending = await_connected_simulation_ending(
        &stores,
        &registration,
        &client,
        member.terminal.execution,
    );
    tokio::pin!(ending);
    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut ending)
            .await
            .is_err(),
        "member evidence during world stop must not be reported as an independently live world"
    );
    handler.stop().unwrap();
    assert!(matches!(
        ending.await.unwrap(),
        ConnectedSimulationEnding::World(_)
    ));
    server.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn member_only_stop_resolves_while_world_is_ready() {
    let (_temporary, stores, handler, server, registration, _lease) = workflow_fixture().await;
    let client = connect_verified(&registration).await.unwrap();
    let ConnectedSimulationEnding::Member(member) =
        member_ending(WorldMemberEndReason::Stopped, WorldMemberCleanup::Complete)
    else {
        unreachable!();
    };
    let members = handler
        .paths
        .evidence_path(WORKFLOW_INSTANCE)
        .join("members");
    create_owner_directory(&members);
    write_owner_json(
        &members.join(format!("{}.json", member.terminal.execution)),
        &member,
    )
    .unwrap();
    let ending = tokio::time::timeout(
        Duration::from_secs(3),
        await_connected_simulation_ending(
            &stores,
            &registration,
            &client,
            member.terminal.execution,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(ending, ConnectedSimulationEnding::Member(_)));
    server.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn command_workflow_reads_live_and_terminal_state_logs_and_list_scope() {
    let (_temporary, stores, _handler, server, registration, _lease) = workflow_fixture().await;

    let StatusReport::Live(live) = load_status(&stores, WORKFLOW_INSTANCE).await.unwrap() else {
        panic!("a held registration must resolve as live");
    };
    assert_eq!(live.instance, registration.instance);
    assert_eq!(
        live.lifecycle,
        WorldLifecycle::Ready {
            motion: WorldMotion::Paused
        }
    );
    assert_eq!(
        load_logs(&stores, WORKFLOW_INSTANCE).await.unwrap(),
        vec![
            ("host.log".to_owned(), b"host retained\n".to_vec()),
            ("webots.log".to_owned(), b"webots retained\n".to_vec()),
        ]
    );

    let live_only = load_list(&stores, false).await.unwrap();
    assert_eq!(live_only.live.len(), 1);
    assert!(live_only.terminal.is_empty());
    let live_and_terminal = load_list(&stores, true).await.unwrap();
    assert_eq!(live_and_terminal.live.len(), 1);
    assert!(live_and_terminal.terminal.is_empty());

    let stopped = stop_world(registration, &stores).await.unwrap();
    assert_eq!(stopped.outcome.reason(), SimulationEndReason::WorldStopped);
    let StatusReport::Terminal { summary, members } =
        load_status(&stores, WORKFLOW_INSTANCE).await.unwrap()
    else {
        panic!("a stopped world must resolve from retained evidence");
    };
    assert_eq!(*summary, stopped);
    assert!(members.is_empty());
    assert_eq!(
        load_logs(&stores, WORKFLOW_INSTANCE).await.unwrap(),
        vec![
            ("host.log".to_owned(), b"host retained\n".to_vec()),
            ("webots.log".to_owned(), b"webots retained\n".to_vec()),
        ]
    );

    let live_only = load_list(&stores, false).await.unwrap();
    assert!(live_only.live.is_empty());
    assert!(live_only.terminal.is_empty());
    let all = load_list(&stores, true).await.unwrap();
    assert!(all.live.is_empty());
    assert_eq!(all.terminal, vec![stopped]);

    server.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn monitor_feeds_reconnect_state_and_diagnostics_after_stream_loss() {
    let (_temporary, _stores, handler, server, registration, _lease) = workflow_fixture().await;
    let client = connect_verified(&registration).await.unwrap();

    let states = client.state_subscription().await.unwrap();
    let (state_tx, mut state_rx) = tokio::sync::mpsc::channel(4);
    let mut state_tasks = tokio::task::JoinSet::new();
    spawn_state_feed(
        &mut state_tasks,
        client.clone(),
        states,
        registration,
        state_tx,
    );
    handler.rotate_state_stream();
    let state = tokio::time::timeout(Duration::from_secs(3), state_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let phoxal_cli_ui::WorldInput::State(state) = state else {
        panic!("state loss must reconnect with a fresh authoritative snapshot");
    };
    assert_eq!(state.instance.to_string(), WORKFLOW_INSTANCE);
    state_tasks.shutdown().await;

    let diagnostics = client.diagnostics_subscription().await.unwrap();
    let (diagnostics_tx, mut diagnostics_rx) = tokio::sync::mpsc::channel(4);
    let mut diagnostics_tasks = tokio::task::JoinSet::new();
    spawn_diagnostics_feed(&mut diagnostics_tasks, client, diagnostics, diagnostics_tx);
    handler.rotate_diagnostics_stream();
    let diagnostics = tokio::time::timeout(Duration::from_secs(3), diagnostics_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let phoxal_cli_ui::WorldInput::Diagnostics(diagnostics) = diagnostics else {
        panic!("diagnostics loss must reconnect with a fresh bounded snapshot");
    };
    assert_eq!(diagnostics.revision, 2);
    diagnostics_tasks.shutdown().await;

    server.close().await.unwrap();
}
