use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// Resolve `--world <name>` against the robot project only: either
/// `<project>/worlds/<name>.wbt`, or `world_arg` itself as a project-relative
/// path. There is no host-wide `~/.phoxal/worlds/` fallback - a world file is
/// project content, not a shared host resource.
pub fn resolve_world(project_root: &Path, world_arg: &str) -> Result<PathBuf> {
    let candidates = [
        project_root.join("worlds").join(format!("{world_arg}.wbt")),
        project_root.join(world_arg),
    ];
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    bail!(
        "failed to resolve world '{world_arg}'; tried:\n{}\n{}\nplace a .wbt file at one of these project-relative paths",
        candidates[0].display(),
        candidates[1].display()
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn resolves_project_world_by_bare_name() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        let world = project.path().join("worlds/default.wbt");
        fs::create_dir_all(world.parent().expect("world parent"))?;
        fs::write(&world, "#VRML_SIM R2025a utf8\n")?;

        assert_eq!(resolve_world(project.path(), "default")?, world);
        Ok(())
    }

    #[test]
    fn resolves_project_path_as_given() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        let world = project.path().join("worlds/foo.wbt");
        fs::create_dir_all(world.parent().expect("world parent"))?;
        fs::write(&world, "#VRML_SIM R2025a utf8\n")?;

        assert_eq!(resolve_world(project.path(), "worlds/foo.wbt")?, world);
        Ok(())
    }

    #[test]
    fn missing_world_is_a_clear_error_with_no_host_wide_fallback() {
        let project = tempfile::tempdir().expect("tempdir");
        let error = resolve_world(project.path(), "missing").expect_err("missing world errors");
        let message = error.to_string();
        assert!(message.contains("missing"), "{message}");
        assert!(
            !message.contains(".phoxal"),
            "there is no host-wide worlds fallback to mention: {message}"
        );
    }
}
