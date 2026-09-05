//! Owner-only local discovery and retained evidence for world sessions.
//!
//! The adapter host owns each registration's contents and lease. The CLI owns
//! the per-user roots, exact-ID lookup, process-birth and lease validation,
//! stale cleanup, terminal evidence reads, and bounded terminal-session
//! retention. Mutable world state never appears here; callers obtain it from
//! the framework-owned typed world-session API at the registered endpoint.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use phoxal::identity::ExecutionId;
use phoxal::model::world::WorldInstanceId;
use phoxal::supervisor::api::simulation::SimulationEndReason;
pub use phoxal::world::api::session::document::{
    LOCAL_WORLD_REGISTRATION_SCHEMA as REGISTRATION_SCHEMA, LocalWorldRegistration,
    NativeProcessIdentity, ProcessIdentity, RegisteredWorld, TerminalCleanup, TerminalFailure,
    TerminalOutcome, TerminalRetention, WORLD_CHECKPOINT_SCHEMA,
    WORLD_MEMBER_TERMINAL_SCHEMA as MEMBER_TERMINAL_SCHEMA,
    WORLD_TERMINAL_SUMMARY_SCHEMA as TERMINAL_SUMMARY_SCHEMA, WorldCheckpoint,
    WorldMemberEvidence as TerminalMemberEvidence, WorldMemberEvidenceIndex as MemberEvidence,
    WorldTerminalSummary as TerminalWorldSummary,
};
use serde::Serialize;
use sysinfo::{Pid, ProcessesToUpdate, System};

pub const REGISTRY_DIR_ENV: &str = "PHOXAL_SIMULATION_REGISTRY_DIR";
pub const EVIDENCE_DIR_ENV: &str = "PHOXAL_SIMULATION_EVIDENCE_DIR";
pub const LOG_BYTE_LIMIT_ENV: &str = "PHOXAL_SIMULATION_LOG_BYTE_LIMIT";
pub const DEFAULT_TERMINAL_SESSION_LIMIT: usize = 50;
pub const DEFAULT_LOG_BYTE_LIMIT: u64 = 16 * 1024 * 1024;
const STALE_BOOTSTRAP_LOG_AGE: Duration = Duration::from_secs(10 * 60);
const NATIVE_EXIT_GRACE: Duration = Duration::from_secs(3);
const NATIVE_TERM_BUDGET: Duration = Duration::from_secs(20);
const NATIVE_KILL_BUDGET: Duration = Duration::from_secs(1);
const NATIVE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The two CLI-owned per-user roots shared with a locally launched host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorldPaths {
    registry: PathBuf,
    evidence: PathBuf,
}

impl WorldPaths {
    /// Resolve and secure the platform's per-user runtime and data roots.
    pub fn discover() -> Result<Self> {
        let runtime = dirs::runtime_dir().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("phoxal-{}", effective_user_id()))
        });
        let data = dirs::data_local_dir().context("the host has no per-user data directory")?;
        Self::create(
            runtime.join("phoxal").join("simulation"),
            data.join("phoxal").join("simulation"),
        )
    }

    /// Build paths at explicit roots, primarily for deterministic tests.
    pub fn create(registry: PathBuf, evidence: PathBuf) -> Result<Self> {
        secure_directory(&registry)?;
        secure_directory(&evidence)?;
        Ok(Self { registry, evidence })
    }

    #[must_use]
    pub fn registry(&self) -> &Path {
        &self.registry
    }

    #[must_use]
    pub fn evidence(&self) -> &Path {
        &self.evidence
    }

    #[must_use]
    pub fn registration_path(&self, instance: &str) -> PathBuf {
        self.registry.join(format!("{instance}.json"))
    }

    #[must_use]
    pub fn evidence_path(&self, instance: &str) -> PathBuf {
        self.evidence.join(instance)
    }
}

trait ValidateLocalWorldRegistration {
    fn validate(&self, expected_instance: &str) -> Result<()>;
}

impl ValidateLocalWorldRegistration for LocalWorldRegistration {
    fn validate(&self, expected_instance: &str) -> Result<()> {
        let expected = parse_instance_id(expected_instance)?;
        self.validate_structure(expected)?;
        ensure!(
            self.lease == format!("{}.lease", self.instance),
            "world registration lease must be the instance-relative basename"
        );
        Ok(())
    }
}

trait ValidateWorldCheckpoint {
    fn validate(&self, registration: &LocalWorldRegistration) -> Result<()>;
}

impl ValidateWorldCheckpoint for WorldCheckpoint {
    fn validate(&self, registration: &LocalWorldRegistration) -> Result<()> {
        self.validate_structure(registration)?;
        if let Some(native) = &self.native_process {
            ensure!(
                native.executable.is_absolute()
                    && native.executable.components().all(|component| matches!(
                        component,
                        Component::Prefix(_) | Component::RootDir | Component::Normal(_)
                    )),
                "native executable must be a canonical absolute path"
            );
            #[cfg(unix)]
            ensure!(
                native.process_group == Some(native.process.pid),
                "native Unix process group must equal its direct Webots PID"
            );
        }
        Ok(())
    }
}

/// Supplies process start time so PID-reuse behavior is deterministic in tests.
pub trait ProcessInspector {
    fn started_at_unix_s(&self, pid: u32) -> Option<u64>;
}

/// One fresh process-table observation used immediately before a native
/// process-group signal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedNativeProcess {
    pub process: ProcessIdentity,
    pub executable: PathBuf,
    pub process_group: u32,
}

/// Narrow process control boundary for deterministic orphan-recovery tests.
pub trait NativeProcessControl {
    fn observe(&self, pid: u32) -> Result<Option<ObservedNativeProcess>>;
    fn process_group_alive(&self, process_group: u32) -> Result<bool>;
    fn signal_process_group(&self, process_group: u32, signal: NativeSignal) -> Result<()>;
    fn wait(&self, duration: Duration);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeSignal {
    Terminate,
    Kill,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProcessInspector;

impl ProcessInspector for SystemProcessInspector {
    fn started_at_unix_s(&self, pid: u32) -> Option<u64> {
        let pid = Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        system.process(pid).map(sysinfo::Process::start_time)
    }
}

impl NativeProcessControl for SystemProcessInspector {
    fn observe(&self, pid: u32) -> Result<Option<ObservedNativeProcess>> {
        let process_id = Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::Some(&[process_id]), true);
        let Some(process) = system.process(process_id) else {
            return Ok(None);
        };
        let Some(executable) = process.exe() else {
            bail!("process {pid} has no observable executable path");
        };
        let native_pid = libc::pid_t::try_from(pid).context("native PID does not fit pid_t")?;
        // SAFETY: `getpgid` takes a scalar PID and does not dereference memory.
        let process_group = unsafe { libc::getpgid(native_pid) };
        if process_group < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).context("failed to inspect the native process group");
        }
        Ok(Some(ObservedNativeProcess {
            process: ProcessIdentity {
                pid,
                started_at_unix_s: process.start_time(),
            },
            executable: executable.to_path_buf(),
            process_group: u32::try_from(process_group)
                .context("native process group is negative")?,
        }))
    }

    fn process_group_alive(&self, process_group: u32) -> Result<bool> {
        probe_process_group(process_group)
    }

    fn signal_process_group(&self, process_group: u32, signal: NativeSignal) -> Result<()> {
        signal_owned_process_group(process_group, signal)
    }

    fn wait(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Exact-ID reader and stale-cleaner for live registrations.
pub struct WorldRegistry<I = SystemProcessInspector> {
    paths: WorldPaths,
    inspector: I,
}

enum RegistrationProbe {
    Missing,
    Live(LocalWorldRegistration),
    Stale(StaleRegistration),
}

struct StaleRegistration {
    registration: LocalWorldRegistration,
    registration_file: File,
    lease_file: File,
}

impl WorldRegistry<SystemProcessInspector> {
    pub fn discover() -> Result<Self> {
        Ok(Self::new(WorldPaths::discover()?, SystemProcessInspector))
    }
}

impl<I: ProcessInspector> WorldRegistry<I> {
    #[must_use]
    pub const fn new(paths: WorldPaths, inspector: I) -> Self {
        Self { paths, inspector }
    }

    #[must_use]
    pub const fn paths(&self) -> &WorldPaths {
        &self.paths
    }

    /// Resolve exactly one complete instance ID and validate both liveness
    /// witnesses. An unlocked lease is reported as stale but retained for
    /// evidence-aware recovery. A locked lease paired with the wrong process
    /// birth is inconsistent and is never silently removed or trusted.
    pub fn resolve(&self, instance: &str) -> Result<LocalWorldRegistration> {
        validate_instance_id(instance)?;
        self.find(instance)?.with_context(|| {
            format!(
                "no live world instance `{instance}` is registered; `phoxal simulation list` shows live instances"
            )
        })
    }

    /// Resolve a complete instance ID when it is live, returning `None` for a
    /// missing or ordinary stale registration.
    pub fn find(&self, instance: &str) -> Result<Option<LocalWorldRegistration>> {
        validate_instance_id(instance)?;
        self.read_live(instance)
    }

    /// Return every valid live registration in full-ID order. Stale crash
    /// residue is retained so an evidence-aware lookup can finalize it.
    pub fn list(&self) -> Result<Vec<LocalWorldRegistration>> {
        let mut instances = Vec::new();
        for instance in self.registration_instances()? {
            if let Some(registration) = self.read_live(&instance)? {
                instances.push(registration);
            }
        }
        instances.sort_by_key(|registration| registration.instance.to_string());
        Ok(instances)
    }

    /// Return every syntactically valid instance named by a registration,
    /// including stale entries. This performs no lifecycle recovery.
    pub fn registration_instances(&self) -> Result<Vec<String>> {
        let mut instances = Vec::new();
        for entry in fs::read_dir(self.paths.registry()).with_context(|| {
            format!(
                "failed to read world registry {}",
                self.paths.registry().display()
            )
        })? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(instance) = name.strip_suffix(".json") else {
                continue;
            };
            validate_instance_id(instance)
                .with_context(|| format!("invalid world registration filename `{name}`"))?;
            instances.push(instance.to_owned());
        }
        instances.sort();
        instances.dedup();
        Ok(instances)
    }

    fn read_live(&self, instance: &str) -> Result<Option<LocalWorldRegistration>> {
        match self.probe(instance)? {
            RegistrationProbe::Missing | RegistrationProbe::Stale(_) => Ok(None),
            RegistrationProbe::Live(registration) => Ok(Some(registration)),
        }
    }

    fn probe(&self, instance: &str) -> Result<RegistrationProbe> {
        let path = self.paths.registration_path(instance);
        let Some((registration_file, document)) = open_and_read_owner_file_if_present(&path)?
        else {
            return Ok(RegistrationProbe::Missing);
        };
        let registration: LocalWorldRegistration = serde_json::from_slice(&document)
            .with_context(|| format!("failed to parse world registration {}", path.display()))?;
        registration.validate(instance)?;

        let lease_path = self.paths.registry().join(&registration.lease);
        let lease = open_owner_file(&lease_path, true)
            .with_context(|| format!("failed to open world lease {}", lease_path.display()))?;
        let acquired = try_lock_lease(&lease)?;
        let observed_birth = self.inspector.started_at_unix_s(registration.process.pid);
        let process_matches = observed_birth == Some(registration.process.started_at_unix_s);

        if !acquired && process_matches {
            return Ok(RegistrationProbe::Live(registration));
        }
        if !acquired {
            bail!(
                "world registration `{instance}` has a live lease but PID {} has birth {:?}, expected {}; refusing to trust or remove it",
                registration.process.pid,
                observed_birth,
                registration.process.started_at_unix_s
            );
        }
        if process_matches {
            bail!(
                "world registration `{instance}` has an unlocked lease while its exact host process {} is still live; refusing premature recovery",
                registration.process.pid
            );
        }
        Ok(RegistrationProbe::Stale(StaleRegistration {
            registration,
            registration_file,
            lease_file: lease,
        }))
    }

    /// Finalize one host-loss orphan from its last durable checkpoint. A live
    /// registration is never changed. The stale lease remains exclusively
    /// held until a complete terminal summary exists and the exact stale
    /// registration files have been removed.
    pub fn recover_host_loss<C: NativeProcessControl>(
        &self,
        evidence: &WorldEvidence,
        instance: &str,
        control: &C,
    ) -> Result<Option<TerminalWorldSummary>> {
        validate_instance_id(instance)?;
        let stale = match self.probe(instance)? {
            RegistrationProbe::Missing => return evidence.read_summary(instance),
            RegistrationProbe::Live(_) => return Ok(None),
            RegistrationProbe::Stale(stale) => stale,
        };

        if let Some(summary) = evidence.read_summary(instance)? {
            stale.remove_exact(&self.paths)?;
            return Ok(Some(summary));
        }

        let checkpoint = evidence
            .read_checkpoint(instance)?
            .with_context(|| format!("stale world {instance} has no durable checkpoint"))?;
        checkpoint.validate(&stale.registration)?;
        let native = checkpoint.native_process.as_ref().with_context(|| {
            format!(
                "stale world {instance} was registered without durable native process ownership"
            )
        })?;
        converge_native_process_group(native, control)?;

        // A normal adapter summary can win while recovery waits for native
        // convergence. It is authoritative and must never be overwritten.
        if let Some(summary) = evidence.read_summary(instance)? {
            stale.remove_exact(&self.paths)?;
            return Ok(Some(summary));
        }

        let member_evidence = evidence.discover_member_evidence(&checkpoint)?;
        let (retained_logs, truncated) = evidence.recovery_logs(instance)?;
        let summary = TerminalWorldSummary {
            schema: TERMINAL_SUMMARY_SCHEMA.to_owned(),
            instance: checkpoint.state.instance,
            provenance: checkpoint.state.provenance,
            outcome: TerminalOutcome::Failed {
                reason: SimulationEndReason::HostLost,
                detail: format!(
                    "world host process {} born {} exited without terminal evidence",
                    stale.registration.process.pid,
                    stale.registration.process.started_at_unix_s
                ),
            },
            progress: checkpoint.state.progress,
            members: checkpoint.state.members,
            member_evidence,
            failing: TerminalFailure {
                process: Some(stale.registration.process),
                producer: None,
            },
            evidence: retained_logs,
            cleanup: TerminalCleanup {
                complete: false,
                detail: Some(
                    "the exact orphaned native process group converged, but abrupt host loss prevented authoritative member cleanup"
                        .to_owned(),
                ),
            },
            retention: TerminalRetention {
                log_byte_limit: DEFAULT_LOG_BYTE_LIMIT,
                truncated,
            },
            ended_at_unix_ms: unix_ms()?,
        };
        let summary = evidence.publish_recovered_summary(instance, &summary)?;
        stale.remove_exact(&self.paths)?;
        Ok(Some(summary))
    }
}

trait ValidateTerminalMemberEvidence {
    fn validate(&self, expected_execution: ExecutionId) -> Result<()>;
}

impl ValidateTerminalMemberEvidence for TerminalMemberEvidence {
    fn validate(&self, expected_execution: ExecutionId) -> Result<()> {
        self.validate_structure(expected_execution)?;
        for evidence in &self.terminal.evidence_paths {
            validate_relative_evidence_path(evidence)?;
        }
        Ok(())
    }
}

trait ValidateTerminalWorldSummary {
    fn validate(&self, expected_instance: &str) -> Result<()>;
}

impl ValidateTerminalWorldSummary for TerminalWorldSummary {
    fn validate(&self, expected_instance: &str) -> Result<()> {
        let expected = parse_instance_id(expected_instance)?;
        self.validate_structure(expected)?;
        for member in &self.member_evidence {
            validate_relative_evidence_path(&member.path)?;
        }
        for evidence in &self.evidence {
            validate_relative_evidence_path(evidence)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PruneReport {
    pub removed: Vec<String>,
    pub bootstrap_logs_removed: Vec<PathBuf>,
    pub incomplete: Vec<PathBuf>,
}

/// Typed terminal evidence reads and count-bounded retention.
#[derive(Clone, Debug)]
pub struct WorldEvidence {
    paths: WorldPaths,
}

impl WorldEvidence {
    #[must_use]
    pub const fn new(paths: WorldPaths) -> Self {
        Self { paths }
    }

    pub fn read_summary(&self, instance: &str) -> Result<Option<TerminalWorldSummary>> {
        validate_instance_id(instance)?;
        let path = self.paths.evidence_path(instance).join("summary.json");
        let Some(document) = read_owner_file_if_present(&path)? else {
            return Ok(None);
        };
        let summary: TerminalWorldSummary =
            serde_json::from_slice(&document).with_context(|| {
                format!("failed to parse terminal world summary {}", path.display())
            })?;
        summary.validate(instance)?;
        Ok(Some(summary))
    }

    pub fn read_checkpoint(&self, instance: &str) -> Result<Option<WorldCheckpoint>> {
        validate_instance_id(instance)?;
        let path = self.paths.evidence_path(instance).join("checkpoint.json");
        let Some(document) = read_owner_file_if_present(&path)? else {
            return Ok(None);
        };
        let checkpoint: WorldCheckpoint = serde_json::from_slice(&document)
            .with_context(|| format!("failed to parse world checkpoint {}", path.display()))?;
        Ok(Some(checkpoint))
    }

    pub fn read_member_evidence(
        &self,
        summary: &TerminalWorldSummary,
    ) -> Result<Vec<TerminalMemberEvidence>> {
        let instance = summary.instance.to_string();
        summary.validate(&instance)?;
        let root = self.paths.evidence_path(&instance);
        let mut members = Vec::new();
        for indexed in &summary.member_evidence {
            let path = root.join(&indexed.path);
            let document = read_owner_file(&path)?;
            let member: TerminalMemberEvidence = serde_json::from_slice(&document)
                .with_context(|| format!("failed to parse member evidence {}", path.display()))?;
            member.validate(indexed.execution)?;
            member
                .terminal
                .last_progress
                .validate(summary.provenance.time_step_ns)
                .with_context(|| {
                    format!(
                        "member {} progress disagrees with retained provenance",
                        member.terminal.execution
                    )
                })?;
            members.push(member);
        }
        members.sort_by_key(|member| member.terminal.execution.to_string());
        Ok(members)
    }

    /// Read one typed member-terminal record while its world may remain live.
    pub fn read_member_terminal(
        &self,
        instance: &str,
        execution: ExecutionId,
    ) -> Result<Option<TerminalMemberEvidence>> {
        validate_instance_id(instance)?;
        let path = self
            .paths
            .evidence_path(instance)
            .join("members")
            .join(format!("{execution}.json"));
        let Some(document) = read_owner_file_if_present(&path)? else {
            return Ok(None);
        };
        let member: TerminalMemberEvidence = serde_json::from_slice(&document)
            .with_context(|| format!("failed to parse member evidence {}", path.display()))?;
        member.validate(execution)?;
        Ok(Some(member))
    }

    fn discover_member_evidence(
        &self,
        checkpoint: &WorldCheckpoint,
    ) -> Result<Vec<MemberEvidence>> {
        let instance = checkpoint.state.instance.to_string();
        let directory = self.paths.evidence_path(&instance).join("members");
        validate_owner_directory(&directory)?;
        let mut members = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if stem.ends_with(".actuation") {
                continue;
            }
            let execution = ExecutionId::parse(stem)
                .with_context(|| format!("invalid member evidence filename `{name}`"))?;
            let path = entry.path();
            let document = read_owner_file(&path)?;
            let record: TerminalMemberEvidence = serde_json::from_slice(&document)
                .with_context(|| format!("failed to parse member evidence {}", path.display()))?;
            record.validate(execution)?;
            record
                .terminal
                .last_progress
                .validate(checkpoint.state.provenance.time_step_ns)
                .with_context(|| {
                    format!(
                        "member {} progress disagrees with checkpoint provenance",
                        record.terminal.execution
                    )
                })?;
            members.push(MemberEvidence {
                execution,
                path: format!("members/{execution}.json"),
            });
        }
        members.sort_by_key(|member| member.execution.to_string());
        Ok(members)
    }

    fn recovery_logs(&self, instance: &str) -> Result<(Vec<String>, Vec<String>)> {
        validate_instance_id(instance)?;
        let root = self.paths.evidence_path(instance);
        validate_owner_directory(&root)?;
        let per_log_limit = (DEFAULT_LOG_BYTE_LIMIT / 2).max(1);
        let mut retained = Vec::new();
        let mut truncated = Vec::new();
        for name in ["host.log", "webots.log"] {
            let path = root.join(name);
            let Some((file, _)) = open_and_read_owner_file_if_present(&path)? else {
                continue;
            };
            if file.metadata()?.len() >= per_log_limit {
                truncated.push(name.to_owned());
            }
            retained.push(name.to_owned());
        }
        Ok((retained, truncated))
    }

    fn publish_recovered_summary(
        &self,
        instance: &str,
        summary: &TerminalWorldSummary,
    ) -> Result<TerminalWorldSummary> {
        summary.validate(instance)?;
        let root = self.paths.evidence_path(instance);
        validate_owner_directory(&root)?;
        let path = root.join("summary.json");
        match atomic_owner_json_if_absent(&path, summary)? {
            AtomicPublish::Published => Ok(summary.clone()),
            AtomicPublish::AlreadyExists => self
                .read_summary(instance)?
                .context("terminal summary appeared during recovery but could not be read"),
        }
    }

    pub fn list_summaries(&self) -> Result<Vec<TerminalWorldSummary>> {
        let mut summaries = Vec::new();
        for directory in fs::read_dir(self.paths.evidence())? {
            let directory = directory?;
            let Some(instance) = directory.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_instance_id(&instance).is_err() {
                continue;
            }
            if let Some(summary) = self.read_summary(&instance)? {
                summaries.push(summary);
            }
        }
        summaries.sort_by(|left, right| {
            left.ended_at_unix_ms
                .cmp(&right.ended_at_unix_ms)
                .then_with(|| left.instance.to_string().cmp(&right.instance.to_string()))
        });
        Ok(summaries)
    }

    pub fn read_logs(&self, summary: &TerminalWorldSummary) -> Result<Vec<(String, Vec<u8>)>> {
        let instance = summary.instance.to_string();
        summary.validate(&instance)?;
        let root = self.paths.evidence_path(&instance);
        let mut logs = Vec::new();
        for relative in &summary.evidence {
            validate_relative_evidence_path(relative)?;
            let path = root.join(relative);
            let document = read_owner_file(&path)?;
            logs.push((relative.clone(), document));
        }
        Ok(logs)
    }

    /// Read conventional retained files for a live session whose terminal
    /// summary has not been written yet.
    pub fn read_live_logs(&self, instance: &str) -> Result<Vec<(String, Vec<u8>)>> {
        validate_instance_id(instance)?;
        let root = self.paths.evidence_path(instance);
        let mut logs = Vec::new();
        for name in ["host.log", "webots.log"] {
            let path = root.join(name);
            if let Some(document) = read_owner_file_if_present(&path)? {
                logs.push((name.to_string(), document));
            }
        }
        Ok(logs)
    }

    /// Keep at most `limit` complete terminal sessions. Live instances and
    /// incomplete evidence directories are never candidates.
    pub fn prune(&self, limit: usize, live_instances: &BTreeSet<String>) -> Result<PruneReport> {
        let mut candidates = Vec::new();
        let mut report = PruneReport::default();
        for directory in fs::read_dir(self.paths.evidence())? {
            let directory = directory?;
            let path = directory.path();
            let Some(instance) = directory.file_name().to_str().map(str::to_owned) else {
                report.incomplete.push(path);
                continue;
            };
            if validate_instance_id(&instance).is_err() {
                if stale_bootstrap_log(&directory, SystemTime::now())? {
                    fs::remove_file(&path).with_context(|| {
                        format!("failed to remove stale bootstrap log {}", path.display())
                    })?;
                    report.bootstrap_logs_removed.push(path);
                }
                continue;
            }
            if live_instances.contains(&instance) {
                continue;
            }
            match self.read_summary(&instance) {
                Ok(Some(summary)) => {
                    candidates.push((summary.ended_at_unix_ms, instance, path));
                }
                Ok(None) | Err(_) => report.incomplete.push(path),
            }
        }
        candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        let remove_count = candidates.len().saturating_sub(limit);
        for (_, instance, path) in candidates.into_iter().take(remove_count) {
            ensure!(
                path.parent() == Some(self.paths.evidence()),
                "refusing to prune evidence outside {}",
                self.paths.evidence().display()
            );
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to prune terminal evidence {}", path.display()))?;
            report.removed.push(instance);
        }
        Ok(report)
    }
}

fn stale_bootstrap_log(entry: &fs::DirEntry, now: SystemTime) -> Result<bool> {
    let name = entry.file_name();
    let Some(name) = name.to_str() else {
        return Ok(false);
    };
    let Some(random) = name
        .strip_prefix(".starting-")
        .and_then(|name| name.strip_suffix(".host.log"))
    else {
        return Ok(false);
    };
    if random.len() != 6 || !random.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Ok(false);
    }
    if !entry.file_type()?.is_file() {
        return Ok(false);
    }
    let metadata = entry.metadata()?;
    Ok(metadata.modified().ok().is_some_and(|modified| {
        now.duration_since(modified)
            .is_ok_and(|age| age >= STALE_BOOTSTRAP_LOG_AGE)
    }))
}

impl StaleRegistration {
    fn remove_exact(self, paths: &WorldPaths) -> Result<()> {
        let registration_path = paths.registration_path(&self.registration.instance.to_string());
        let lease_path = paths.registry().join(&self.registration.lease);
        remove_exact_open_file(&registration_path, &self.registration_file)?;
        remove_exact_open_file(&lease_path, &self.lease_file)?;
        Ok(())
    }
}

fn converge_native_process_group<C: NativeProcessControl>(
    expected: &NativeProcessIdentity,
    control: &C,
) -> Result<()> {
    let process_group = expected
        .process_group
        .context("native checkpoint has no Unix process-group ownership")?;
    ensure!(
        process_group == expected.process.pid,
        "native checkpoint process group does not equal its direct process PID"
    );

    control.wait(NATIVE_EXIT_GRACE);
    if !control.process_group_alive(process_group)? {
        return Ok(());
    }

    for (signal, budget) in [
        (NativeSignal::Terminate, NATIVE_TERM_BUDGET),
        (NativeSignal::Kill, NATIVE_KILL_BUDGET),
    ] {
        validate_native_process_before_signal(expected, control)?;
        control.signal_process_group(process_group, signal)?;
        let mut remaining = budget;
        while !remaining.is_zero() {
            if !control.process_group_alive(process_group)? {
                return Ok(());
            }
            let interval = NATIVE_POLL_INTERVAL.min(remaining);
            control.wait(interval);
            remaining = remaining.saturating_sub(interval);
        }
        if !control.process_group_alive(process_group)? {
            return Ok(());
        }
    }
    bail!("orphaned native process group {process_group} remained alive after SIGKILL")
}

fn validate_native_process_before_signal<C: NativeProcessControl>(
    expected: &NativeProcessIdentity,
    control: &C,
) -> Result<()> {
    let canonical_executable = expected.executable.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize checkpointed native executable {}",
            expected.executable.display()
        )
    })?;
    ensure!(
        canonical_executable == expected.executable,
        "checkpointed native executable {} is not canonical; refusing to signal its process group",
        expected.executable.display()
    );
    let observed = control.observe(expected.process.pid)?.with_context(|| {
        format!(
            "native process group {} is live but its direct process {} is absent; refusing an unvalidated group signal",
            expected.process_group.unwrap_or_default(),
            expected.process.pid
        )
    })?;
    ensure!(
        observed.process == expected.process,
        "native PID {} was reused or its birth identity changed; refusing to signal process group {}",
        expected.process.pid,
        observed.process_group
    );
    ensure!(
        observed.executable == canonical_executable,
        "native PID {} executable is {}, expected {}; refusing to signal an ambiguous process group",
        expected.process.pid,
        observed.executable.display(),
        expected.executable.display()
    );
    ensure!(
        Some(observed.process_group) == expected.process_group,
        "native PID {} belongs to process group {}, expected {}; refusing to signal it",
        expected.process.pid,
        observed.process_group,
        expected.process_group.unwrap_or_default()
    );
    Ok(())
}

#[cfg(unix)]
fn probe_process_group(process_group: u32) -> Result<bool> {
    let process_group = libc::pid_t::try_from(process_group)
        .context("native process-group ID does not fit pid_t")?;
    ensure!(
        process_group > 0,
        "native process-group ID must be positive"
    );
    // SAFETY: `kill` takes no pointer. A negative PID and signal zero probe
    // the exact process group without delivering a signal.
    if unsafe { libc::kill(-process_group, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).context("failed to inspect the native process group"),
    }
}

#[cfg(not(unix))]
fn probe_process_group(_process_group: u32) -> Result<bool> {
    bail!("local world orphan recovery requires Unix process-group semantics")
}

#[cfg(unix)]
fn signal_owned_process_group(process_group: u32, signal: NativeSignal) -> Result<()> {
    let process_group = libc::pid_t::try_from(process_group)
        .context("native process-group ID does not fit pid_t")?;
    ensure!(
        process_group > 0,
        "native process-group ID must be positive"
    );
    let signal = match signal {
        NativeSignal::Terminate => libc::SIGTERM,
        NativeSignal::Kill => libc::SIGKILL,
    };
    // SAFETY: `kill` takes no pointer. The negative PID targets only the
    // checkpoint-owned process group that was just revalidated.
    if unsafe { libc::kill(-process_group, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("failed to signal the orphaned native process group")
}

#[cfg(not(unix))]
fn signal_owned_process_group(_process_group: u32, _signal: NativeSignal) -> Result<()> {
    bail!("local world orphan recovery requires Unix process-group semantics")
}

pub fn parse_instance_id(instance: &str) -> Result<WorldInstanceId> {
    WorldInstanceId::parse(instance).map_err(Into::into)
}

pub fn validate_instance_id(instance: &str) -> Result<()> {
    parse_instance_id(instance).map(|_| ())
}

fn validate_relative_evidence_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(!path.as_os_str().is_empty(), "evidence path is empty");
    ensure!(!path.is_absolute(), "evidence path must be relative");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "evidence path `{value}` escapes its session directory"
    );
    Ok(())
}

fn unix_ms() -> Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .context("system clock predates the Unix epoch")?;
    u64::try_from(elapsed.as_millis()).context("Unix timestamp overflows u64 milliseconds")
}

enum AtomicPublish {
    Published,
    AlreadyExists,
}

fn atomic_owner_json_if_absent(path: &Path, value: &impl Serialize) -> Result<AtomicPublish> {
    let parent = path
        .parent()
        .context("terminal summary path has no parent")?;
    validate_owner_directory(parent)?;
    let temporary = parent.join(format!(
        ".summary-recovery-{}-{}.tmp",
        std::process::id(),
        unix_ms()?
    ));
    let mut created = false;
    let result = (|| -> Result<AtomicPublish> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary).with_context(|| {
            format!("failed to create recovery summary {}", temporary.display())
        })?;
        created = true;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                File::open(parent)?.sync_all()?;
                Ok(AtomicPublish::Published)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(AtomicPublish::AlreadyExists)
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to publish recovered terminal summary {}",
                    path.display()
                )
            }),
        }
    })();
    if created {
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to remove recovery summary temporary {}",
                        temporary.display()
                    )
                });
            }
        }
    }
    result
}

fn read_owner_file(path: &Path) -> Result<Vec<u8>> {
    let mut file = open_owner_file(path, false)
        .with_context(|| format!("failed to open owner-only file {}", path.display()))?;
    let mut document = Vec::new();
    file.read_to_end(&mut document)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(document)
}

fn open_and_read_owner_file_if_present(path: &Path) -> Result<Option<(File, Vec<u8>)>> {
    match open_owner_file(path, false) {
        Ok(mut file) => {
            let mut document = Vec::new();
            file.read_to_end(&mut document)
                .with_context(|| format!("failed to read {}", path.display()))?;
            Ok(Some((file, document)))
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to open owner-only file {}", path.display()))
        }
    }
}

fn read_owner_file_if_present(path: &Path) -> Result<Option<Vec<u8>>> {
    match open_owner_file(path, false) {
        Ok(mut file) => {
            let mut document = Vec::new();
            file.read_to_end(&mut document)
                .with_context(|| format!("failed to read {}", path.display()))?;
            Ok(Some(document))
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to open owner-only file {}", path.display()))
        }
    }
}

#[cfg(unix)]
fn open_owner_file(path: &Path, writable: bool) -> Result<File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    ensure!(
        metadata.uid() == effective_user_id(),
        "{} is not owned by the current user",
        path.display()
    );
    ensure!(
        metadata.mode() & 0o777 == 0o600,
        "{} must have mode 0600, found {:04o}",
        path.display(),
        metadata.mode() & 0o777
    );
    let _ = file.as_raw_fd();
    Ok(file)
}

#[cfg(not(unix))]
fn open_owner_file(_path: &Path, _writable: bool) -> Result<File> {
    bail!("local world sessions require Unix owner and lease semantics")
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fs::create_dir_all(path)
        .with_context(|| format!("failed to create owner-only directory {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_dir(),
        "{} is not a directory",
        path.display()
    );
    ensure!(
        metadata.uid() == effective_user_id(),
        "{} is not owned by the current user",
        path.display()
    );
    if metadata.mode() & 0o777 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn validate_owner_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect owner-only directory {}", path.display()))?;
    ensure!(
        metadata.file_type().is_dir(),
        "{} is not an owner-only directory",
        path.display()
    );
    ensure!(
        metadata.uid() == effective_user_id(),
        "{} is not owned by the current user",
        path.display()
    );
    ensure!(
        metadata.mode() & 0o777 == 0o700,
        "{} must have mode 0700, found {:04o}",
        path.display(),
        metadata.mode() & 0o777
    );
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<()> {
    bail!("local world sessions require Unix owner and lease semantics")
}

#[cfg(not(unix))]
fn validate_owner_directory(_path: &Path) -> Result<()> {
    bail!("local world sessions require Unix owner and lease semantics")
}

#[cfg(unix)]
fn remove_exact_open_file(path: &Path, open: &File) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let open_metadata = open.metadata()?;
    let path_metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "stale world file {} disappeared before exact cleanup",
            path.display()
        )
    })?;
    ensure!(
        path_metadata.file_type().is_file()
            && path_metadata.dev() == open_metadata.dev()
            && path_metadata.ino() == open_metadata.ino(),
        "stale world file {} was replaced during recovery; refusing to remove it",
        path.display()
    );
    fs::remove_file(path)
        .with_context(|| format!("failed to remove stale world file {}", path.display()))
}

#[cfg(not(unix))]
fn remove_exact_open_file(_path: &Path, _open: &File) -> Result<()> {
    bail!("local world sessions require Unix owner and lease semantics")
}

#[cfg(unix)]
fn try_lock_lease(file: &File) -> Result<bool> {
    use std::os::fd::AsRawFd;

    // SAFETY: `file` owns a valid open descriptor for this call's duration.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
    {
        return Ok(false);
    }
    Err(error).context("failed to inspect the world host lease")
}

#[cfg(not(unix))]
fn try_lock_lease(_file: &File) -> Result<bool> {
    bail!("local world sessions require Unix owner and lease semantics")
}

#[cfg(unix)]
fn effective_user_id() -> u32 {
    // SAFETY: `geteuid` has no preconditions and does not mutate memory.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
const fn effective_user_id() -> u32 {
    0
}

#[cfg(test)]
mod tests {
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

    fn summary(instance: &str, ended_at_unix_ms: u64) -> TerminalWorldSummary {
        TerminalWorldSummary {
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
        let record = TerminalMemberEvidence {
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
            vec![MemberEvidence {
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
                FileTimes::new().set_modified(
                    SystemTime::now() - STALE_BOOTSTRAP_LOG_AGE - Duration::from_secs(1),
                ),
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

        let decoded: TerminalWorldSummary =
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
        let record = TerminalMemberEvidence {
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
        summary.member_evidence = vec![MemberEvidence {
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
}
