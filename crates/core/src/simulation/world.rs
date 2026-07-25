//! Project-local Webots world resolution for simulation plans.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// Resolve `--world <name>` as either `<project>/worlds/<name>.wbt` or the
/// supplied path. Relative paths are anchored at the robot project; absolute
/// paths and explicit `..` traversal are accepted by design so a source
/// project can use an authored world outside its own directory. There is no
/// implicit host-wide `~/.phoxal/worlds/` fallback.
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
        "failed to resolve world '{world_arg}'; tried:\n{}\n{}\nplace a .wbt file at one of these paths",
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
    fn resolves_an_explicit_world_outside_the_project() -> anyhow::Result<()> {
        let parent = tempfile::tempdir()?;
        let project = parent.path().join("robot");
        fs::create_dir_all(&project)?;
        let world = parent.path().join("shared.wbt");
        fs::write(&world, "#VRML_SIM R2025a utf8\n")?;
        assert_eq!(
            resolve_world(&project, "../shared.wbt")?.canonicalize()?,
            world.canonicalize()?
        );
        assert_eq!(
            resolve_world(&project, world.to_str().expect("utf8 path"))?,
            world
        );
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
