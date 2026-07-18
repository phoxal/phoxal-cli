use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::resolver::ResolvedRobot;

const ROBOT_DIR: &str = "robot";
const PREVIOUS_ROBOT_DIR: &str = ".robot.previous";
const BEHAVIORS_DIR: &str = "behaviors";
const MESHES_DIR: &str = "meshes";

#[must_use]
pub fn path(project_root: &Path) -> PathBuf {
    project_root.join(".phoxal/run").join(ROBOT_DIR)
}

/// Build and publish the complete robot root consumed by a live `run` or
/// `simulation run` session. The caller owns the project run lock for the
/// whole operation, so no participant can observe the brief exchange between
/// the previous complete root and the newly validated candidate.
pub fn publish(project_root: &Path, resolved: &ResolvedRobot) -> Result<PathBuf> {
    let run_dir = project_root.join(".phoxal/run");
    fs::create_dir_all(&run_dir).with_context(|| {
        format!(
            "failed to create runtime state directory {}",
            run_dir.display()
        )
    })?;
    let candidate = tempfile::Builder::new()
        .prefix(".robot-candidate-")
        .tempdir_in(&run_dir)
        .with_context(|| {
            format!(
                "failed to create runtime robot-root candidate in {}",
                run_dir.display()
            )
        })?;

    stage_candidate(project_root, candidate.path(), resolved)?;
    validate_candidate(candidate.path(), resolved)?;

    let target = path(project_root);
    let previous = run_dir.join(PREVIOUS_ROBOT_DIR);
    remove_if_present(&previous)?;
    let candidate = candidate.keep();
    let had_previous = fs::symlink_metadata(&target).is_ok();
    if had_previous {
        fs::rename(&target, &previous).with_context(|| {
            format!(
                "failed to move previous runtime robot root {} aside",
                target.display()
            )
        })?;
    }

    if let Err(error) = fs::rename(&candidate, &target) {
        if had_previous {
            let _ = fs::rename(&previous, &target);
        }
        let _ = remove_if_present(&candidate);
        return Err(error).with_context(|| {
            format!(
                "failed to atomically publish runtime robot root {}",
                target.display()
            )
        });
    }
    remove_if_present(&previous)?;
    Ok(target)
}

fn stage_candidate(project_root: &Path, candidate: &Path, resolved: &ResolvedRobot) -> Result<()> {
    phoxal::model::robot::Robot::V0(resolved.robot.clone())
        .write_to_dir(candidate)
        .context("failed to write resolved runtime robot.yaml")?;

    let structure = &resolved.robot.robot.structure;
    ensure_safe_relative_path(structure, "robot structure")?;
    copy_file_preserving_path(project_root, candidate, structure, "robot structure")?;
    if let Some(structure_parent) = structure.parent() {
        let mesh_path = structure_parent.join(MESHES_DIR);
        copy_optional_dir_preserving_path(project_root, candidate, &mesh_path)?;
    }
    copy_optional_dir_preserving_path(project_root, candidate, Path::new(BEHAVIORS_DIR))?;

    crate::native_artifacts::stage_component_bundles_into_robot_root(
        project_root,
        candidate,
        resolved,
    )
    .context("failed to stage component assets into the runtime robot root")
}

fn ensure_safe_relative_path(path: &Path, label: &str) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "{label} must be a non-empty relative path without '.' or '..': {}",
        path.display()
    );
    Ok(())
}

fn validate_candidate(candidate: &Path, resolved: &ResolvedRobot) -> Result<()> {
    // Resolution already ran the model's semantic validation against the
    // catalog's platform names. Reparse the serialized candidate here to
    // prove the on-disk manifest is complete and strict without losing that
    // owner-specific validation context.
    let staged = phoxal::model::robot::Robot::parse_from_dir(candidate)
        .context("published runtime robot.yaml failed strict parsing")?;
    ensure!(
        staged.as_v0() == &resolved.robot,
        "published runtime robot.yaml differs from the resolved manifest"
    );
    ensure!(
        candidate.join(&resolved.robot.robot.structure).is_file(),
        "published runtime root is missing robot structure {}",
        resolved.robot.robot.structure.display()
    );

    for component in &resolved.components {
        if component.assets.is_some() {
            let component_file = candidate
                .join("components")
                .join(&component.source_name)
                .join("component.yaml");
            ensure!(
                component_file.is_file(),
                "published runtime root is missing component metadata {}",
                component_file.display()
            );
        }
    }
    Ok(())
}

fn copy_file_preserving_path(
    project_root: &Path,
    candidate: &Path,
    relative: &Path,
    label: &str,
) -> Result<()> {
    let source = project_root.join(relative);
    let dest = candidate.join(relative);
    let parent = dest
        .parent()
        .context("runtime robot-root destination has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::copy(&source, &dest).with_context(|| {
        format!(
            "failed to stage {label} {} to {}",
            source.display(),
            dest.display()
        )
    })?;
    Ok(())
}

fn copy_optional_dir_preserving_path(
    project_root: &Path,
    candidate: &Path,
    relative: &Path,
) -> Result<()> {
    let source = project_root.join(relative);
    if !source.is_dir() {
        return Ok(());
    }
    copy_dir_recursive(&source, &candidate.join(relative))
}

fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &dest_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "failed to stage runtime asset {} to {}",
                    source_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("failed to remove stale runtime state {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ArtifactKind;
    use crate::host_paths::test_support::ScratchPhoxalHome;
    use crate::resolver::host_target_triple;
    use crate::resolver::{ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource};

    fn resolved_robot() -> Result<ResolvedRobot> {
        let yaml = r#"schema: robot/v0
robot:
  id: robot_v1
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: model/structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#;
        Ok(ResolvedRobot {
            robot: phoxal::model::robot::v0::Robot::parse_from_string(yaml)?,
            channel: crate::catalog::SelectionChannel::Stable,
            target: host_target_triple(),
            catalog_snapshot: None,
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    #[test]
    fn publishes_complete_resolved_root_and_replaces_previous_generation() -> Result<()> {
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model/meshes"))?;
        fs::create_dir_all(project.path().join(BEHAVIORS_DIR))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        fs::write(project.path().join("model/meshes/chassis.dae"), "mesh")?;
        fs::write(
            project.path().join("behaviors/default.yaml"),
            "behavior: []",
        )?;

        let resolved = resolved_robot()?;
        let published = publish(project.path(), &resolved)?;
        assert_eq!(published, path(project.path()));
        assert!(published.join("robot.yaml").is_file());
        assert_eq!(
            fs::read_to_string(published.join("model/structure.urdf"))?,
            "<robot/>"
        );
        assert!(published.join("model/meshes/chassis.dae").is_file());
        assert!(published.join("behaviors/default.yaml").is_file());

        fs::write(published.join("stale"), "old")?;
        publish(project.path(), &resolved)?;
        assert!(!published.join("stale").exists());
        assert!(!project.path().join(".phoxal/run/.robot.previous").exists());
        Ok(())
    }

    #[test]
    fn failed_candidate_preserves_previous_published_root() -> Result<()> {
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "first")?;
        let resolved = resolved_robot()?;
        let published = publish(project.path(), &resolved)?;

        fs::remove_file(project.path().join("model/structure.urdf"))?;
        assert!(publish(project.path(), &resolved).is_err());
        assert_eq!(
            fs::read_to_string(published.join("model/structure.urdf"))?,
            "first"
        );
        Ok(())
    }

    #[test]
    fn robot_structure_cannot_escape_the_runtime_root() -> Result<()> {
        let project = tempfile::tempdir()?;
        for structure in [
            PathBuf::from("../outside.urdf"),
            PathBuf::from("/tmp/outside.urdf"),
        ] {
            let mut resolved = resolved_robot()?;
            resolved.robot.robot.structure = structure.clone();
            let error = publish(project.path(), &resolved).unwrap_err().to_string();
            assert!(error.contains("robot structure must be a non-empty relative path"));
            assert!(!path(project.path()).exists());

            let run_dir = project.path().join(".phoxal/run");
            let candidates = fs::read_dir(&run_dir)?
                .filter_map(std::result::Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".robot-candidate-")
                })
                .count();
            assert_eq!(candidates, 0, "failed candidates must clean themselves up");
        }
        Ok(())
    }

    #[test]
    fn publishes_component_bundle_without_mutating_its_source_tree() -> Result<()> {
        // Component staging takes the process-wide artifact-store lock. Join
        // the same serialized scratch-root scope as native-artifact tests so a
        // parallel test cannot temporarily point this test at its locked store.
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "robot")?;
        let source = project.path().join("components/ddsm115");
        fs::create_dir_all(source.join("meshes"))?;
        fs::write(source.join("component.yaml"), "schema: component/v0\n")?;
        fs::write(source.join("simulation.yaml"), "device: motor\n")?;
        fs::write(source.join("meshes/wheel.dae"), "wheel")?;
        let source_before = [
            fs::read(source.join("component.yaml"))?,
            fs::read(source.join("simulation.yaml"))?,
            fs::read(source.join("meshes/wheel.dae"))?,
        ];

        let mut resolved = resolved_robot()?;
        resolved.components.push(ResolvedComponent {
            instance: "left_drive".to_string(),
            source_name: "ddsm115".to_string(),
            assets: Some(ResolvedComponentPackage {
                package: "phoxal/component-ddsm115".to_string(),
                kind: ArtifactKind::ComponentAssets,
                source: ResolvedComponentSource::Path {
                    path: PathBuf::from("components/ddsm115"),
                },
                path_override: None,
                catalog_runtime: None,
            }),
            driver: None,
            has_driver: false,
        });

        let published = publish(project.path(), &resolved)?;

        let staged = published.join("components/ddsm115");
        assert!(staged.join("component.yaml").is_file());
        assert!(staged.join("simulation.yaml").is_file());
        assert!(staged.join("meshes/wheel.dae").is_file());
        assert_eq!(
            source_before,
            [
                fs::read(source.join("component.yaml"))?,
                fs::read(source.join("simulation.yaml"))?,
                fs::read(source.join("meshes/wheel.dae"))?,
            ],
            "publishing must leave source component assets unchanged"
        );
        Ok(())
    }
}
