//! Build responsibilities for run.

use super::profile::Profile;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::project::resolver::BundlePlan;
use phoxal_manifest::source::robot::v0::ConnectionConfig;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use toml::Value as TomlValue;

/// The one preparation-owned view of source binaries Cargo must produce.
///
/// It deliberately contains neither a target-directory guess nor a second
/// source-discovery path. Cargo reports the executable it actually selected in
/// its JSON stream; checking and staging consume that exact path afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceArtifacts {
    by_participant: BTreeMap<String, PathBuf>,
}

impl SourceArtifacts {
    pub(crate) fn binary(&self, participant: &SourceParticipant) -> Result<&Path> {
        self.binary_named(&participant.name)
    }

    pub(crate) fn binary_named(&self, name: &str) -> Result<&Path> {
        self.by_participant
            .get(name)
            .map(PathBuf::as_path)
            .ok_or_else(|| anyhow!("source build plan did not contain participant `{name}`"))
    }

    #[cfg(test)]
    pub(crate) fn for_test(name: impl Into<String>, binary: PathBuf) -> Self {
        Self {
            by_participant: [(name.into(), binary)].into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SourceArtifactKey {
    package_id: String,
    binary: String,
}

#[derive(Debug, Clone)]
struct PlannedSourceArtifact {
    participant: String,
    package_selector: String,
    key: SourceArtifactKey,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SourceBuildGroupKey {
    workspace_root: PathBuf,
    target: Option<String>,
    profile: Profile,
    offline: bool,
}

#[derive(Debug, Clone)]
struct SourceBuildGroup {
    key: SourceBuildGroupKey,
    artifacts: Vec<PlannedSourceArtifact>,
}

/// Compile each compatible selected source group exactly once and retain the
/// compiler-reported executable for every logical participant.  A package may
/// back several component instances; that is one Cargo artifact and several
/// map entries pointing to the same bytes.
pub(crate) fn build_selected_source_artifacts(
    participants: &[SourceParticipant],
    target: Option<&str>,
    profile: Profile,
    prebuilt_target_dir: Option<&Path>,
    offline: bool,
    reporter: &dyn crate::Reporter,
) -> Result<SourceArtifacts> {
    if participants.is_empty() {
        return Ok(SourceArtifacts {
            by_participant: BTreeMap::new(),
        });
    }
    // Container builds have already compiled the selected packages. Do not
    // invoke host Cargo metadata merely to rediscover their output names: a
    // pure manifest projection preserves the selected-binary rule while
    // keeping this path valid on hosts without the target toolchain/cache.
    if let Some(target_dir) = prebuilt_target_dir {
        let mut by_participant = BTreeMap::new();
        for participant in participants {
            let binary_name =
                source_binary_name_from_manifest(&participant.crate_dir, &participant.name)?;
            let binary = locate_prebuilt_binary(&binary_name, target_dir, target, profile)?;
            if by_participant
                .insert(participant.name.clone(), binary)
                .is_some()
            {
                bail!(
                    "source build plan contains duplicate participant `{}`",
                    participant.name
                );
            }
        }
        return Ok(SourceArtifacts { by_participant });
    }
    let cross = target
        .filter(|triple| *triple != crate::resolve::project::host_target_triple())
        .map(str::to_string);
    let mut metadata = Vec::<WorkspaceMetadata>::new();
    let mut grouped = BTreeMap::<SourceBuildGroupKey, SourceBuildGroup>::new();
    for participant in participants {
        let crate_dir = participant.crate_dir.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize source crate {}",
                participant.crate_dir.display()
            )
        })?;
        let workspace = if let Some(workspace) = metadata
            .iter()
            .find(|workspace| workspace.packages.contains_key(&crate_dir))
        {
            workspace
        } else {
            metadata.push(load_workspace_metadata(&crate_dir, offline)?);
            metadata.last().expect("inserted workspace metadata")
        };
        let package = workspace.package_for(&crate_dir)?;
        let binary = package.binary_for(&participant.name)?;
        let group_key = SourceBuildGroupKey {
            workspace_root: workspace.workspace_root.clone(),
            target: cross.clone(),
            profile,
            offline,
        };
        grouped
            .entry(group_key.clone())
            .or_insert_with(|| SourceBuildGroup {
                key: group_key,
                artifacts: Vec::new(),
            })
            .artifacts
            .push(PlannedSourceArtifact {
                participant: participant.name.clone(),
                package_selector: package.selector.clone(),
                key: SourceArtifactKey {
                    package_id: package.package_id.clone(),
                    binary,
                },
            });
    }

    let total_groups = grouped.len();
    let total_artifacts = grouped
        .values()
        .flat_map(|group| group.artifacts.iter().map(|artifact| artifact.key.clone()))
        .collect::<BTreeSet<_>>()
        .len();
    if let Some(triple) = cross.as_deref() {
        ensure_cross_toolchain(triple)?;
    }
    reporter.report(crate::PreparationEvent::SourceBuildPlanned {
        groups: total_groups,
        artifacts: total_artifacts,
    });
    let mut completed = 0;
    let mut by_participant = BTreeMap::new();
    for (index, mut group) in grouped.into_values().enumerate() {
        group
            .artifacts
            .sort_by(|left, right| left.participant.cmp(&right.participant));
        let group_artifacts = group
            .artifacts
            .iter()
            .map(|artifact| artifact.key.clone())
            .collect::<BTreeSet<_>>()
            .len();
        reporter.report(crate::PreparationEvent::SourceBuildGroupStarted {
            current: index + 1,
            total: total_groups,
            artifacts: group_artifacts,
        });
        let artifacts = match build_source_group(&group, completed, total_artifacts, reporter) {
            Ok(artifacts) => artifacts,
            Err(error) => {
                reporter.report(crate::PreparationEvent::SourceBuildGroupFinished {
                    current: index + 1,
                    total: total_groups,
                    success: false,
                });
                return Err(error);
            }
        };
        completed += group_artifacts;
        reporter.report(crate::PreparationEvent::SourceBuildGroupFinished {
            current: index + 1,
            total: total_groups,
            success: true,
        });
        for planned in &group.artifacts {
            let binary = artifacts.get(&planned.key).ok_or_else(|| {
                anyhow!(
                    "cargo build did not report selected executable `{}` for participant `{}`",
                    planned.key.binary,
                    planned.participant
                )
            })?;
            if by_participant
                .insert(planned.participant.clone(), binary.clone())
                .is_some()
            {
                bail!(
                    "source build plan contains duplicate participant `{}`",
                    planned.participant
                );
            }
        }
    }
    Ok(SourceArtifacts { by_participant })
}

fn source_binary_name_from_manifest(crate_dir: &Path, preferred: &str) -> Result<String> {
    let manifest_path = crate_dir.join("Cargo.toml");
    let contents = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest = toml::from_str::<TomlValue>(&contents)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let package_name = manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(TomlValue::as_str)
        .ok_or_else(|| anyhow!("{} does not declare package.name", manifest_path.display()))?;
    let declared_bins = manifest
        .get("bin")
        .and_then(TomlValue::as_array)
        .cloned()
        .unwrap_or_default();
    let mut binaries = declared_bins
        .iter()
        .filter_map(|bin| bin.get("name")?.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let claimed_paths = declared_bins
        .iter()
        .filter_map(|bin| bin.get("path")?.as_str().map(PathBuf::from))
        .collect::<BTreeSet<_>>();
    let auto_bins_enabled = manifest
        .get("package")
        .and_then(|package| package.get("autobins"))
        .and_then(TomlValue::as_bool)
        .unwrap_or(true);
    if auto_bins_enabled {
        binaries.extend(auto_discovered_binary_names(
            crate_dir,
            package_name,
            &claimed_paths,
        )?);
    }
    binaries.sort();
    binaries.dedup();
    if binaries.iter().any(|name| name == preferred) {
        return Ok(preferred.to_owned());
    }
    if binaries.len() == 1 {
        return Ok(binaries[0].clone());
    }
    bail!(
        "{} has {} binary targets ({}) but none named `{preferred}`",
        manifest_path.display(),
        binaries.len(),
        binaries.join(", ")
    )
}

/// Cargo adds `src/main.rs`, `src/bin/*.rs`, and `src/bin/*/main.rs` to its
/// target list unless `package.autobins = false`. This is deliberately shared
/// only by the prebuilt projection; the normal path obtains the authoritative
/// list from `cargo metadata` instead of maintaining a second general resolver.
fn auto_discovered_binary_names(
    crate_dir: &Path,
    package_name: &str,
    claimed_paths: &BTreeSet<PathBuf>,
) -> Result<Vec<String>> {
    let mut binaries = Vec::new();
    if crate_dir.join("src/main.rs").is_file() && !claimed_paths.contains(Path::new("src/main.rs"))
    {
        binaries.push(package_name.to_owned());
    }
    let bin_dir = crate_dir.join("src/bin");
    if !bin_dir.is_dir() {
        return Ok(binaries);
    }
    for entry in
        fs::read_dir(&bin_dir).with_context(|| format!("failed to read {}", bin_dir.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {} entry", bin_dir.display()))?;
        let path = entry.path();
        let relative_path = Path::new("src/bin").join(entry.file_name());
        if path.is_file()
            && path.extension().is_some_and(|extension| extension == "rs")
            && !claimed_paths.contains(&relative_path)
            && let Some(name) = path.file_stem().and_then(|name| name.to_str())
        {
            binaries.push(name.to_owned());
        } else if path.is_dir()
            && path.join("main.rs").is_file()
            && !claimed_paths.contains(&relative_path.join("main.rs"))
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
        {
            binaries.push(name.to_owned());
        }
    }
    Ok(binaries)
}

#[derive(Debug)]
struct SourcePackageMetadata {
    package_id: String,
    selector: String,
    binaries: Vec<String>,
}

#[derive(Debug)]
struct WorkspaceMetadata {
    workspace_root: PathBuf,
    packages: BTreeMap<PathBuf, SourcePackageMetadata>,
}

impl WorkspaceMetadata {
    fn package_for(&self, crate_dir: &Path) -> Result<&SourcePackageMetadata> {
        self.packages.get(crate_dir).ok_or_else(|| {
            anyhow!(
                "locked Cargo metadata for {} did not contain package {}",
                self.workspace_root.display(),
                crate_dir.display()
            )
        })
    }
}

impl SourcePackageMetadata {
    fn binary_for(&self, preferred: &str) -> Result<String> {
        // `binaries` stores Cargo's target declarations rather than parsing
        // Cargo.toml again after metadata has already resolved the package.
        if self.binaries.iter().any(|binary| binary == preferred) {
            return Ok(preferred.to_string());
        }
        if self.binaries.len() == 1 {
            return Ok(self.binaries[0].clone());
        }
        bail!(
            "selected package {} has {} binary targets ({}) but none named `{preferred}`",
            self.package_id,
            self.binaries.len(),
            self.binaries.join(", ")
        )
    }
}

fn load_workspace_metadata(crate_dir: &Path, offline: bool) -> Result<WorkspaceMetadata> {
    let mut args = vec!["metadata", "--format-version", "1", "--no-deps", "--locked"];
    if offline {
        args.push("--offline");
    }
    let output = crate::build::shell::run_stdout("cargo", args, Some(crate_dir), None)
        .with_context(|| {
            format!(
                "failed to read locked Cargo metadata in {}",
                crate_dir.display()
            )
        })?;
    let metadata: Value = serde_json::from_str(&output).context("cargo metadata was not JSON")?;
    parse_workspace_metadata(&metadata)
}

fn parse_workspace_metadata(metadata: &Value) -> Result<WorkspaceMetadata> {
    let workspace_root = metadata
        .get("workspace_root")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cargo metadata did not include workspace_root"))?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("cargo metadata did not include packages"))?
        .iter()
        .filter_map(|package| {
            let manifest = package.get("manifest_path")?.as_str()?;
            let crate_dir = Path::new(manifest).parent()?.to_path_buf();
            let package_id = package.get("id")?.as_str()?.to_string();
            let selector = format!(
                "{}@{}",
                package.get("name")?.as_str()?,
                package.get("version")?.as_str()?
            );
            let binaries = package
                .get("targets")?
                .as_array()?
                .iter()
                .filter_map(|target| {
                    target
                        .get("kind")?
                        .as_array()?
                        .iter()
                        .any(|kind| kind.as_str() == Some("bin"))
                        .then(|| target.get("name")?.as_str().map(str::to_string))?
                })
                .collect();
            Some((
                crate_dir,
                SourcePackageMetadata {
                    package_id,
                    selector,
                    binaries,
                },
            ))
        })
        .collect();
    Ok(WorkspaceMetadata {
        workspace_root,
        packages,
    })
}

fn build_source_group(
    group: &SourceBuildGroup,
    completed_before: usize,
    total_artifacts: usize,
    reporter: &dyn crate::Reporter,
) -> Result<BTreeMap<SourceArtifactKey, PathBuf>> {
    let mut package_selectors = BTreeSet::new();
    let group_artifact_count = group
        .artifacts
        .iter()
        .map(|artifact| artifact.key.clone())
        .collect::<BTreeSet<_>>()
        .len();
    for artifact in &group.artifacts {
        package_selectors.insert(artifact.package_selector.clone());
    }
    reporter.info(format!(
        "building source workspace {} for {}: {group_artifact_count} selected binaries",
        group.key.workspace_root.display(),
        group.key.target.as_deref().unwrap_or("host"),
    ));
    crate::progress::run_phase(
        reporter,
        crate::PhaseId::new("build"),
        format!("Cargo batch · {group_artifact_count} selected artifacts"),
        || {
            let mut command = Command::new("cargo");
            command
                .args(source_build_args(group, &package_selectors))
                .current_dir(&group.key.workspace_root);
            run_cargo_json_build(
                &mut command,
                group,
                completed_before,
                total_artifacts,
                reporter,
            )
        },
    )
}

fn source_build_args(
    group: &SourceBuildGroup,
    package_selectors: &BTreeSet<String>,
) -> Vec<String> {
    let mut args = vec![
        "build".to_string(),
        "--locked".to_string(),
        "--message-format=json-render-diagnostics".to_string(),
    ];
    for package in package_selectors {
        args.push("--package".to_string());
        args.push(package.clone());
    }
    // `--bins` is intentionally scoped by the exact package list above; it
    // never widens to workspace members the resolved graph did not choose.
    args.push("--bins".to_string());
    if let Some(target) = &group.key.target {
        args.push("--target".to_string());
        args.push(target.clone());
    }
    if matches!(group.key.profile, Profile::Release) {
        args.push("--release".to_string());
    }
    if group.key.offline {
        args.push("--offline".to_string());
    }
    args
}

fn run_cargo_json_build(
    command: &mut Command,
    group: &SourceBuildGroup,
    completed_before: usize,
    total_artifacts: usize,
    reporter: &dyn crate::Reporter,
) -> Result<BTreeMap<SourceArtifactKey, PathBuf>> {
    let mut collector = ArtifactCollector::new(&group.artifacts);
    let status = crate::build::shell::command_status_streaming_stdout(command, reporter, |line| {
        for diagnostic in cargo_rendered_diagnostics(line) {
            reporter.report(crate::PreparationEvent::CommandLine(diagnostic));
        }
        if collector.collect_line(line)? {
            reporter.report(crate::PreparationEvent::SourceBuildArtifactCompleted {
                completed: completed_before + collector.found.len(),
                total: total_artifacts,
            });
        }
        Ok(())
    })?;
    anyhow::ensure!(
        status.success(),
        "selected-source cargo build failed in {} for {} with status {status}; ensure the workspace Cargo.lock is current, then run `cargo update` and commit it",
        group.key.workspace_root.display(),
        package_selectors(&group.artifacts).join(", "),
    );
    collector.finish()
}

fn package_selectors(artifacts: &[PlannedSourceArtifact]) -> Vec<String> {
    artifacts
        .iter()
        .map(|artifact| artifact.package_selector.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn cargo_rendered_diagnostics(line: &str) -> Vec<String> {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|message| {
            message
                .get("message")
                .and_then(|message| message.get("rendered"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .into_iter()
        .flat_map(|rendered| {
            rendered
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

struct ArtifactCollector {
    expected: BTreeSet<SourceArtifactKey>,
    found: BTreeMap<SourceArtifactKey, PathBuf>,
}

impl ArtifactCollector {
    fn new(planned: &[PlannedSourceArtifact]) -> Self {
        Self {
            expected: planned
                .iter()
                .map(|artifact| artifact.key.clone())
                .collect(),
            found: BTreeMap::new(),
        }
    }

    fn collect_line(&mut self, line: &str) -> Result<bool> {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return Ok(false);
        };
        let Some((key, executable)) = selected_artifact(&message, &self.expected)? else {
            return Ok(false);
        };
        if let Some(previous) = self.found.insert(key.clone(), executable.clone()) {
            bail!(
                "cargo reported duplicate executable artifacts for selected binary `{}`: {} and {}",
                key.binary,
                previous.display(),
                executable.display()
            );
        }
        Ok(true)
    }

    fn finish(self) -> Result<BTreeMap<SourceArtifactKey, PathBuf>> {
        let missing = self
            .expected
            .difference(&self.found.keys().cloned().collect())
            .map(|key| key.binary.clone())
            .collect::<Vec<_>>();
        anyhow::ensure!(
            missing.is_empty(),
            "cargo build did not report executable artifacts for selected binaries: {}",
            missing.join(", ")
        );
        Ok(self.found)
    }
}

fn selected_artifact(
    message: &Value,
    expected: &BTreeSet<SourceArtifactKey>,
) -> Result<Option<(SourceArtifactKey, PathBuf)>> {
    if message.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
        return Ok(None);
    }
    let Some(package_id) = message.get("package_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(target) = message.get("target") else {
        return Ok(None);
    };
    let is_bin = target
        .get("kind")
        .and_then(Value::as_array)
        .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("bin")));
    let Some(binary) = target.get("name").and_then(Value::as_str) else {
        return Ok(None);
    };
    let key = SourceArtifactKey {
        package_id: package_id.to_string(),
        binary: binary.to_string(),
    };
    if !is_bin || !expected.contains(&key) {
        return Ok(None);
    }
    let executable = message
        .get("executable")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow!("cargo reported selected binary `{binary}` without an executable path")
        })?;
    Ok(Some((key, executable)))
}

#[cfg(test)]
fn parse_source_artifacts(
    stdout: &str,
    planned: &[PlannedSourceArtifact],
) -> Result<BTreeMap<SourceArtifactKey, PathBuf>> {
    let mut collector = ArtifactCollector::new(planned);
    for line in stdout.lines() {
        let _ = collector.collect_line(line)?;
    }
    collector.finish()
}

/// Resolve a user/driver crate binary the container builder already compiled
/// into `target_dir` (the project's persistent Cargo target directory,
/// bind-mounted into the container at `/phoxal/src/target`), for the same
/// `target` a host build would have used. No cargo runs here - the binary must
/// already exist, so a missing one is a precise error naming it.
pub(super) fn locate_prebuilt_binary(
    binary_name: &str,
    target_dir: &Path,
    target: Option<&str>,
    profile: Profile,
) -> Result<PathBuf> {
    // The container always compiles with an explicit `--target <triple>`, so
    // its output is always under `target/<triple>/<profile>` - including when the
    // triple equals the CLI host triple (a Linux host building its own arch in
    // a container). Never collapse to the implicit `target/debug` here (#936).
    let path = profile_binary_path(target_dir, target, profile, binary_name);
    if !path.is_file() {
        bail!(
            "container build did not produce the selected binary `{binary_name}` (expected {}); \
             the in-container `cargo build --target` may have failed",
            path.display()
        );
    }
    Ok(path)
}

/// The build output path for `binary_name` under `target_dir` for `profile`,
/// in the `<triple>/<profile>/` subtree when cross-compiling and plain
/// `<profile>/` otherwise.
fn profile_binary_path(
    target_dir: &Path,
    cross: Option<&str>,
    profile: Profile,
    binary_name: &str,
) -> PathBuf {
    let dir = match cross {
        Some(triple) => target_dir.join(triple).join(profile.dir_name()),
        None => target_dir.join(profile.dir_name()),
    };
    dir.join(binary_name_with_suffix(binary_name))
}

/// Fail with an actionable error when the cross target's standard library is not
/// installed, naming the exact `rustup target add` command. The CLI never
/// installs toolchains (#936); this only turns an opaque later cargo failure
/// into a precise instruction. If `rustup` is not on PATH the check is skipped -
/// a non-rustup toolchain may still have the target - and cargo reports any real
/// gap itself.
fn ensure_cross_toolchain(triple: &str) -> Result<()> {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    let Ok(output) = output else {
        // No rustup on PATH: let the build proceed and cargo surface any gap.
        return Ok(());
    };
    if !output.status.success() {
        return Ok(());
    }
    let installed = String::from_utf8_lossy(&output.stdout);
    if installed.lines().any(|line| line.trim() == triple) {
        return Ok(());
    }
    bail!(
        "the Rust standard library for target `{triple}` is not installed; \
         install it with `rustup target add {triple}` (the CLI never installs toolchains), \
         or use `--builder container` to compile inside a toolchain image"
    )
}

pub(crate) fn cargo_target_dir(crate_dir: &Path, offline: bool) -> Result<PathBuf> {
    // `--locked` keeps metadata reads on the committed `Cargo.lock` too, so
    // resolving the target directory never triggers a lock rewrite or a
    // registry resolve. `--no-deps` already means this call has no real need
    // to reach the network, but `--offline` is threaded through anyway
    // (organization#951 WS4 review, round 2) so it holds on principle for
    // every Cargo invocation this CLI makes, not just the ones that happen
    // to need it today.
    let mut args = vec!["metadata", "--format-version", "1", "--no-deps", "--locked"];
    if offline {
        args.push("--offline");
    }
    let output = crate::build::shell::run_stdout("cargo", args, Some(crate_dir), None)?;
    let json: Value = serde_json::from_str(&output).context("cargo metadata was not JSON")?;
    json.get("target_directory")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cargo metadata did not include target_directory"))
}

pub(crate) fn binary_name_with_suffix(binary_name: &str) -> String {
    if cfg!(windows) {
        format!("{binary_name}.exe")
    } else {
        binary_name.to_string()
    }
}

pub(crate) fn device_missing_note(resolved: &BundlePlan, participant_id: &str) -> Option<String> {
    let component = resolved
        .source_manifest
        .robot
        .components
        .get(participant_id)?;
    let driver = component.driver.as_ref()?;
    let missing = missing_device_path(&driver.connection)?;
    Some(format!(
        "DeviceMissing: {missing} for driver {participant_id}"
    ))
}

pub(crate) fn missing_device_path(connection: &ConnectionConfig) -> Option<String> {
    match connection {
        ConnectionConfig::Serial { port, .. } | ConnectionConfig::Uart { port, .. } => {
            (!Path::new(port).exists()).then(|| port.clone())
        }
        ConnectionConfig::Can { bus, .. } => {
            let path = PathBuf::from(format!("/sys/class/net/can{bus}"));
            (!path.exists()).then(|| path.display().to_string())
        }
        ConnectionConfig::I2c { bus, .. } => {
            let path = PathBuf::from(format!("/dev/i2c-{bus}"));
            (!path.exists()).then(|| path.display().to_string())
        }
        ConnectionConfig::Spi { bus, chip_select } => {
            let path = PathBuf::from(format!("/dev/spidev{bus}.{chip_select}"));
            (!path.exists()).then(|| path.display().to_string())
        }
        ConnectionConfig::Gpio { chip, .. } => {
            let path = if chip.starts_with('/') {
                PathBuf::from(chip)
            } else {
                PathBuf::from("/dev").join(chip)
            };
            (!path.exists()).then(|| path.display().to_string())
        }
        ConnectionConfig::Usb {
            vendor_id,
            product_id,
        } => usb_missing(*vendor_id, *product_id),
    }
}

pub(crate) fn usb_missing(vendor_id: Option<u16>, product_id: Option<u16>) -> Option<String> {
    let (Some(vendor_id), Some(product_id)) = (vendor_id, product_id) else {
        return None;
    };
    let devices = Path::new("/sys/bus/usb/devices");
    let entries = fs::read_dir(devices).ok()?;
    let wanted_vendor = format!("{vendor_id:04x}");
    let wanted_product = format!("{product_id:04x}");
    for entry in entries.flatten() {
        let path = entry.path();
        let vendor = fs::read_to_string(path.join("idVendor")).unwrap_or_default();
        let product = fs::read_to_string(path.join("idProduct")).unwrap_or_default();
        if vendor.trim().eq_ignore_ascii_case(&wanted_vendor)
            && product.trim().eq_ignore_ascii_case(&wanted_product)
        {
            return None;
        }
    }
    Some(format!("usb {wanted_vendor}:{wanted_product}"))
}

#[cfg(test)]
mod prebuilt_tests {
    use super::*;

    /// The container always compiles with an explicit `--target`, so the
    /// prebuilt lookup must read `target/<triple>/release` even when the triple
    /// equals the CLI host triple - a Linux host container-building its own
    /// arch previously collapsed to `target/debug` and missed the binary
    /// (#936, round-2 finding 2).
    #[test]
    fn prebuilt_lookup_uses_the_explicit_target_dir_for_the_host_triple() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let host = crate::resolve::project::host_target_triple();
        let target_dir = dir.path().join("target");
        let release = target_dir.join(&host).join("release");
        std::fs::create_dir_all(&release)?;
        std::fs::write(release.join(binary_name_with_suffix("svc")), b"bin")?;

        let found = locate_prebuilt_binary("svc", &target_dir, Some(&host), Profile::Release)?;
        assert_eq!(found, release.join(binary_name_with_suffix("svc")));

        // The implicit `target/release` location must NOT satisfy the lookup.
        std::fs::remove_file(release.join(binary_name_with_suffix("svc")))?;
        let plain = target_dir.join("release");
        std::fs::create_dir_all(&plain)?;
        std::fs::write(plain.join(binary_name_with_suffix("svc")), b"bin")?;
        assert!(
            locate_prebuilt_binary("svc", &target_dir, Some(&host), Profile::Release,).is_err(),
            "the prebuilt lookup must never collapse to the implicit target/release"
        );
        Ok(())
    }

    #[test]
    fn prebuilt_artifacts_need_no_cargo_metadata_or_lockfile() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let crate_dir = dir.path().join("svc");
        std::fs::create_dir_all(&crate_dir)?;
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"svc\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        std::fs::create_dir_all(crate_dir.join("src"))?;
        std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
        // Deliberately no Cargo.lock: this cannot satisfy the production
        // metadata command's `--locked` contract, but authoritative container
        // output must still be staged without starting host Cargo.
        let target = dir.path().join("target");
        let binary = target.join("debug").join(binary_name_with_suffix("svc"));
        std::fs::create_dir_all(binary.parent().expect("binary parent"))?;
        std::fs::write(&binary, b"prebuilt")?;
        let participants = [SourceParticipant::user_service("svc", crate_dir)];

        let artifacts = build_selected_source_artifacts(
            &participants,
            None,
            Profile::Debug,
            Some(&target),
            true,
            &crate::SilentReporter,
        )?;
        assert_eq!(artifacts.binary_named("svc")?, binary);
        Ok(())
    }

    #[test]
    fn prebuilt_manifest_projection_honors_auto_discovered_preferred_bin() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let crate_dir = dir.path().join("svc");
        std::fs::create_dir_all(crate_dir.join("src/bin"))?;
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"svc\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
        std::fs::write(crate_dir.join("src/bin/worker.rs"), "fn main() {}")?;
        let target = dir.path().join("target");
        let worker = target.join("debug").join(binary_name_with_suffix("worker"));
        std::fs::create_dir_all(worker.parent().expect("binary parent"))?;
        std::fs::write(&worker, b"prebuilt")?;
        let participants = [SourceParticipant::user_service("worker", crate_dir)];

        let artifacts = build_selected_source_artifacts(
            &participants,
            None,
            Profile::Debug,
            Some(&target),
            true,
            &crate::SilentReporter,
        )?;
        assert_eq!(artifacts.binary_named("worker")?, worker);
        Ok(())
    }

    #[test]
    fn prebuilt_manifest_projection_honors_directory_auto_bin() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let crate_dir = dir.path().join("svc");
        std::fs::create_dir_all(crate_dir.join("src/bin/worker"))?;
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"svc\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        std::fs::write(crate_dir.join("src/bin/worker/main.rs"), "fn main() {}")?;
        let target = dir.path().join("target");
        let worker = target.join("debug").join(binary_name_with_suffix("worker"));
        std::fs::create_dir_all(worker.parent().expect("binary parent"))?;
        std::fs::write(&worker, b"prebuilt")?;
        let participants = [SourceParticipant::user_service("worker", crate_dir)];

        let artifacts = build_selected_source_artifacts(
            &participants,
            None,
            Profile::Debug,
            Some(&target),
            true,
            &crate::SilentReporter,
        )?;
        assert_eq!(artifacts.binary_named("worker")?, worker);
        Ok(())
    }

    #[test]
    fn prebuilt_manifest_projection_rejects_an_ambiguous_auto_bin_selection() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let crate_dir = dir.path().join("svc");
        std::fs::create_dir_all(crate_dir.join("src/bin"))?;
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"svc\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
        std::fs::write(crate_dir.join("src/bin/worker.rs"), "fn main() {}")?;

        let error = source_binary_name_from_manifest(&crate_dir, "missing")
            .expect_err("an absent preferred name must not guess among auto bins");
        assert!(format!("{error:#}").contains("2 binary targets (svc, worker)"));
        Ok(())
    }

    #[test]
    fn prebuilt_manifest_projection_does_not_double_count_an_explicit_bin_path() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let crate_dir = dir.path().join("mission");
        std::fs::create_dir_all(crate_dir.join("src"))?;
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"mission\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"mission-runtime\"\npath = \"src/main.rs\"\n",
        )?;
        std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
        let target = dir.path().join("target");
        let binary = target
            .join("debug")
            .join(binary_name_with_suffix("mission-runtime"));
        std::fs::create_dir_all(binary.parent().expect("binary parent"))?;
        std::fs::write(&binary, b"prebuilt")?;
        let participants = [SourceParticipant::user_service("mission", crate_dir)];

        let artifacts = build_selected_source_artifacts(
            &participants,
            None,
            Profile::Debug,
            Some(&target),
            true,
            &crate::SilentReporter,
        )?;
        assert_eq!(artifacts.binary_named("mission")?, binary);
        Ok(())
    }

    fn planned(participant: &str, package: &str, binary: &str) -> PlannedSourceArtifact {
        PlannedSourceArtifact {
            participant: participant.to_string(),
            package_selector: "fixture@0.1.0".to_string(),
            key: SourceArtifactKey {
                package_id: package.to_string(),
                binary: binary.to_string(),
            },
        }
    }

    #[test]
    fn cargo_json_maps_only_selected_binary_artifacts_including_fresh_hits() -> Result<()> {
        let stream = concat!(
            r#"{"reason":"compiler-artifact","package_id":"path+file:///robot#mission@0.1.0","target":{"name":"mission","kind":["bin"]},"executable":"/target/debug/mission","fresh":true}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"path+file:///robot#mission@0.1.0","target":{"name":"mission","kind":["lib"]},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"dep","target":{"name":"dependency","kind":["bin"]},"executable":"/target/debug/dependency"}"#
        );
        let artifacts = parse_source_artifacts(
            stream,
            &[planned(
                "mission",
                "path+file:///robot#mission@0.1.0",
                "mission",
            )],
        )?;
        assert_eq!(
            artifacts
                .get(&SourceArtifactKey {
                    package_id: "path+file:///robot#mission@0.1.0".to_string(),
                    binary: "mission".to_string(),
                })
                .unwrap(),
            Path::new("/target/debug/mission")
        );
        Ok(())
    }

    #[test]
    fn cargo_json_rejects_missing_or_duplicate_selected_binaries() {
        let planned = [planned("mission", "pkg", "mission")];
        let missing = parse_source_artifacts("", &planned).unwrap_err();
        assert!(format!("{missing:#}").contains("mission"));
        let duplicate = concat!(
            r#"{"reason":"compiler-artifact","package_id":"pkg","target":{"name":"mission","kind":["bin"]},"executable":"/one"}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"pkg","target":{"name":"mission","kind":["bin"]},"executable":"/two"}"#
        );
        assert!(
            format!(
                "{:#}",
                parse_source_artifacts(duplicate, &planned).unwrap_err()
            )
            .contains("duplicate executable")
        );
    }

    #[test]
    fn metadata_projection_and_command_select_exact_packages_without_workspace_widening()
    -> Result<()> {
        let metadata = serde_json::json!({
            "workspace_root": "/robot",
            "packages": [
                {
                    "id": "path+file:///robot#alpha@0.1.0",
                    "name": "alpha",
                    "version": "0.1.0",
                    "manifest_path": "/robot/alpha/Cargo.toml",
                    "targets": [{"name": "alpha", "kind": ["bin"]}]
                },
                {
                    "id": "path+file:///robot#beta@0.2.0",
                    "name": "beta",
                    "version": "0.2.0",
                    "manifest_path": "/robot/beta/Cargo.toml",
                    "targets": [{"name": "beta-runtime", "kind": ["bin"]}]
                }
            ]
        });
        let workspace = parse_workspace_metadata(&metadata)?;
        assert_eq!(workspace.workspace_root, Path::new("/robot"));
        let alpha = workspace.package_for(Path::new("/robot/alpha"))?;
        let beta = workspace.package_for(Path::new("/robot/beta"))?;
        assert_eq!(alpha.binary_for("alpha")?, "alpha");
        assert_eq!(beta.binary_for("beta")?, "beta-runtime");

        let group = SourceBuildGroup {
            key: SourceBuildGroupKey {
                workspace_root: workspace.workspace_root.clone(),
                target: Some("aarch64-unknown-linux-gnu".to_string()),
                profile: Profile::Release,
                offline: true,
            },
            artifacts: vec![],
        };
        let packages = [alpha.selector.clone(), beta.selector.clone()]
            .into_iter()
            .collect();
        let args = source_build_args(&group, &packages);
        assert_eq!(args.iter().filter(|arg| *arg == "--package").count(), 2);
        assert!(args.contains(&"alpha@0.1.0".to_string()));
        assert!(args.contains(&"beta@0.2.0".to_string()));
        assert!(args.contains(&"--bins".to_string()));
        assert!(args.contains(&"--target".to_string()));
        assert!(args.contains(&"--release".to_string()));
        assert!(args.contains(&"--offline".to_string()));
        assert!(!args.contains(&"--workspace".to_string()));
        Ok(())
    }
}
