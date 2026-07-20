//! Source-artifact selection and cross-build input analysis.

#[cfg(not(test))]
use super::deploy_command;
use super::{
    SourceBuildArtifact, TargetTriples, cross_build_source_binary, ensure_zigbuild_toolchain,
    payload_bin, sha256_file,
};
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
#[cfg(not(test))]
use anyhow::bail;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::check::source::SourceParticipantKind;
use phoxal_cli_core::project::catalog::ArtifactKind;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::resolver::ResolvedComponentSource;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::tooling::hash_tree;
use phoxal_cli_core::project::tooling::make_executable;
use phoxal_cli_core::project::tooling::resolve_project_path;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn stage_source_artifacts(
    project_root: &Path,
    root: &Path,
    resolved: &ResolvedRobot,
    source_participants: &[SourceParticipant],
    plan: &LaunchPlan,
    target: &TargetTriples,
    ui: &crate::Ui,
) -> Result<BTreeMap<String, SourceBuildArtifact>> {
    let needed = needed_source_artifact_ids(plan, source_participants);
    let mut artifacts = BTreeMap::new();

    for participant in source_participants {
        let artifact_id = source_artifact_id(participant);
        if !needed.contains(&artifact_id) {
            continue;
        }
        if artifacts.contains_key(&artifact_id) {
            continue;
        }
        let artifact = build_source_artifact(
            project_root,
            root,
            resolved,
            participant,
            &artifact_id,
            target,
            ui,
        )?;
        artifacts.insert(artifact_id, artifact);
    }

    Ok(artifacts)
}

pub(crate) fn needed_source_artifact_ids(
    plan: &LaunchPlan,
    source_participants: &[SourceParticipant],
) -> BTreeSet<String> {
    let source_by_participant = source_participants
        .iter()
        .map(|participant| (participant.name.as_str(), participant))
        .collect::<BTreeMap<_, _>>();
    plan.robots
        .iter()
        .flat_map(|robot| &robot.participants)
        .filter_map(|participant| {
            source_by_participant.get(participant.launch.participant_id.as_str())
        })
        .map(|participant| source_artifact_id(participant))
        .collect()
}

pub(crate) fn source_artifact_id(participant: &SourceParticipant) -> String {
    match participant.kind {
        SourceParticipantKind::UserService => participant.expected_artifact_id.clone(),
        SourceParticipantKind::OfficialService => {
            format!("service-{}", participant.expected_artifact_id)
        }
        SourceParticipantKind::ComponentDriver => {
            format!("driver-{}", participant.expected_artifact_id)
        }
        SourceParticipantKind::Tool => participant.name.clone(),
        SourceParticipantKind::Simulator => {
            format!("simulator-{}", participant.expected_artifact_id)
        }
    }
}

pub(crate) fn build_source_artifact(
    project_root: &Path,
    root: &Path,
    resolved: &ResolvedRobot,
    participant: &SourceParticipant,
    artifact_id: &str,
    target: &TargetTriples,
    ui: &crate::Ui,
) -> Result<SourceBuildArtifact> {
    if let Some(native_dep) = native_sysroot_dependency(&participant.crate_dir)? {
        return Err(cross_build_unsupported_error(
            participant.kind_label(),
            &participant.name,
            &native_dep,
        ));
    }
    ensure_rust_target(&target.local_triple, ui)?;
    let toolchain = ensure_zigbuild_toolchain(ui)?;
    let actual_binary = cross_build_source_binary(
        &participant.crate_dir,
        artifact_id,
        &target.local_triple,
        &toolchain,
        ui,
    )
    .with_context(|| {
        format!(
            "failed to cross-build {} {} for {}",
            participant.kind_label(),
            participant.name,
            target.local_triple
        )
    })?;
    let dest = payload_bin(root).join(artifact_id);
    fs::copy(&actual_binary, &dest).with_context(|| {
        format!(
            "failed to stage source binary {} to {}",
            actual_binary.display(),
            dest.display()
        )
    })?;
    make_executable(&dest)?;
    let sha256 = sha256_file(&dest)?;
    let source = source_record(project_root, resolved, participant)?;
    Ok(SourceBuildArtifact {
        artifact_id: artifact_id.to_string(),
        kind: source_kind(participant.kind),
        source,
        sha256,
        payload_path: dest,
    })
}

pub(crate) fn source_kind(kind: SourceParticipantKind) -> ArtifactKind {
    use phoxal_cli_core::session::ParticipantKind;
    match kind.shared_kind() {
        ParticipantKind::Service => ArtifactKind::Service,
        ParticipantKind::Driver => ArtifactKind::ComponentDriver,
        ParticipantKind::Tool => ArtifactKind::Tool,
        ParticipantKind::Simulator => ArtifactKind::Simulator,
    }
}

pub(crate) fn source_record(
    project_root: &Path,
    resolved: &ResolvedRobot,
    participant: &SourceParticipant,
) -> Result<Value> {
    if participant.kind == SourceParticipantKind::ComponentDriver
        && let Some(component) = resolved
            .components
            .iter()
            .find(|component| component.instance == participant.name)
        && let Some(driver) = &component.driver
    {
        return match &driver.source {
            ResolvedComponentSource::Git { git, rev, .. } => {
                Ok(serde_json::json!({ "git": git, "rev": rev }))
            }
            ResolvedComponentSource::Path { path } => {
                let full = resolve_project_path(project_root, path);
                Ok(serde_json::json!({
                    "path": path.display().to_string(),
                    "tree": format!("sha256:{}", hash_tree(&full)?)
                }))
            }
            ResolvedComponentSource::Catalog => {
                Ok(serde_json::json!({ "package": driver.package }))
            }
        };
    }

    let display_path = path_relative_to(project_root, &participant.crate_dir);
    Ok(serde_json::json!({
        "path": display_path.display().to_string(),
        "tree": format!("sha256:{}", hash_tree(&participant.crate_dir)?)
    }))
}

pub(crate) fn path_relative_to(root: &Path, path: &Path) -> PathBuf {
    pathdiff::diff_paths(path, root).unwrap_or_else(|| path.to_path_buf())
}

pub(crate) fn native_sysroot_dependency(crate_dir: &Path) -> Result<Option<String>> {
    let manifest_path = crate_dir.join("Cargo.toml");
    let contents = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest = toml::from_str::<toml::Value>(&contents)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let mut names = Vec::new();
    collect_dependency_names(&manifest, &mut names);
    names.sort();
    names.dedup();
    Ok(names.into_iter().find(|name| {
        name == "opencv"
            || name == "libudev"
            || name == "v4l"
            || name == "v4l2"
            || name == "libusb"
            || name == "realsense-rust"
    }))
}

pub(crate) fn ensure_no_native_c_source_dependencies(
    participants: &[SourceParticipant],
) -> Result<()> {
    for participant in participants {
        if let Some(native_dep) = native_sysroot_dependency(&participant.crate_dir)? {
            return Err(cross_build_unsupported_error(
                participant.kind_label(),
                &participant.name,
                &native_dep,
            ));
        }
    }
    Ok(())
}

pub(crate) fn cross_build_unsupported_error(
    kind: &str,
    name: &str,
    native_dep: &str,
) -> anyhow::Error {
    anyhow!(
        "CrossBuildUnsupported: {kind} {name} depends on native sysroot crate '{native_dep}', which cargo-zigbuild cannot make portable by itself. Fix: provide the target-native headers/libs in a pinned sysroot, publish a CI-built native artifact, or remove/feature-gate the dependency."
    )
}

pub(crate) fn collect_dependency_names(value: &toml::Value, names: &mut Vec<String>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, value) in table {
        if matches!(
            key.as_str(),
            "dependencies" | "dev-dependencies" | "build-dependencies"
        ) && let Some(deps) = value.as_table()
        {
            names.extend(deps.keys().cloned());
        }
        collect_dependency_names(value, names);
    }
}

#[cfg(not(test))]
pub(crate) fn ensure_rust_target(target: &str, ui: &crate::Ui) -> Result<()> {
    let output = deploy_command("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .context("CrossBuildUnsupported: rustup is required to manage deploy cross targets")?;
    if !output.status.success() {
        bail!(
            "CrossBuildUnsupported: rustup is required to manage deploy cross targets and `rustup target list --installed` failed with status {}.",
            output.status
        );
    }
    let installed = String::from_utf8(output.stdout)
        .context("CrossBuildUnsupported: rustup wrote non-UTF8 stdout")?;
    if installed.lines().any(|line| line.trim() == target) {
        return Ok(());
    }
    ui.info(format!("provisioning Rust target {target} with rustup"));
    let status = deploy_command("rustup")
        .args(["target", "add", target])
        .status()
        .context("failed to start rustup target add")?;
    if status.success() {
        return Ok(());
    }
    bail!(
        "CrossBuildTargetMissing: rustup could not install target {target} (status {status}). Fix: run `rustup target add {target}` with network access, then rerun `phoxal-cli deploy`."
    )
}

#[cfg(test)]
pub(crate) fn ensure_rust_target(_target: &str, _ui: &crate::Ui) -> Result<()> {
    Ok(())
}
