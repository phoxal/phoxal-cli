use crate::{RuntimeAuthority, RuntimeTarget};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::paths::runtime::RuntimePaths;

pub fn resolve(explicit: Option<&Path>, fallback: &Path) -> Result<RuntimeTarget> {
    let selected = explicit.unwrap_or(fallback);
    let preserve_installed_identity =
        selected == Path::new(crate::paths::runtime::ACTIVE_RUNTIME_ROOT);
    let selected = if preserve_installed_identity {
        selected.to_path_buf()
    } else {
        absolute(selected.to_path_buf())?
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", selected.display()))?
    };
    // The stable installed selector is also used by stop/status/rollback when
    // no active release is currently selected. Keep its identity resolvable
    // without requiring robot.yaml; commands that need a runnable layout
    // validate the pinned release before execution.
    let (root, requested_entry) = if preserve_installed_identity {
        (selected, None)
    } else if selected.is_file() {
        (
            selected
                .parent()
                .context("entry file has no parent project")?
                .to_path_buf(),
            Some(selected),
        )
    } else {
        let entry = crate::source::resolver::discover_robot_yaml(&selected)
            .with_context(|| format!("failed to find robot.yaml from {}", selected.display()))?;
        (
            entry
                .parent()
                .context("robot.yaml has no parent project")?
                .canonicalize()?,
            None,
        )
    };
    let paths = RuntimePaths::for_root(&root);
    let logical_root = paths.ownership_root().to_path_buf();
    let authority = if crate::paths::runtime::is_installed_root(&root) {
        RuntimeAuthority::SystemdUnit {
            unit: crate::paths::runtime::SYSTEMD_UNIT.to_string(),
        }
    } else {
        RuntimeAuthority::DetachedSession
    };
    // The supervisor's Zenoh listen endpoint IS the supervisor socket: one stable
    // path locates the current execution, and the per-boot identity is learned
    // from the router behind it. Checked here so a too-deep project refuses
    // before any build instead of failing at the supervisor's bind.
    let supervisor_socket = paths.checked_supervisor_socket()?;
    Ok(RuntimeTarget {
        logical_root,
        requested_entry,
        build_lock: paths.build_lock(),
        zenoh_endpoint: format!("unixsock-stream/{}", supervisor_socket.display()),
        supervisor_socket,
        authority,
    })
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()
        .context("failed to determine the current directory")?
        .join(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_project_uses_detached_project_local_authority() -> Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("robot.yaml"), "schema: phoxal/robot/v1\n")?;
        let target = resolve(Some(root.path()), Path::new("/unused"))?;
        let root = root.path().canonicalize()?;
        assert_eq!(target.logical_root, root);
        assert_eq!(target.build_lock, root.join(".phoxal/run/build.lock"));
        assert_eq!(
            target.supervisor_socket,
            root.join(".phoxal/run/supervisor.sock")
        );
        assert_eq!(target.authority, RuntimeAuthority::DetachedSession);
        Ok(())
    }

    #[test]
    fn installed_root_uses_stable_systemd_authority() -> Result<()> {
        let target = resolve(Some(Path::new("/var/phoxal")), Path::new("/unused"))?;
        assert_eq!(
            target.authority,
            RuntimeAuthority::SystemdUnit {
                unit: "phoxal-supervisor.service".to_string()
            }
        );
        assert_eq!(target.build_lock, Path::new("/run/phoxal/build.lock"));
        assert_eq!(
            target.supervisor_socket,
            Path::new("/run/phoxal/supervisor.sock")
        );
        Ok(())
    }
}
