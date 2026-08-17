//! Installing, removing, and inspecting `phoxal-supervisor.service`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::context::AppContext;

use super::units::{UNIT_MARKER, supervisor_unit};

const UNIT_PATH: &str = phoxal_cli_host::paths::SYSTEMD_UNIT_PATH;

fn unit_contents() -> String {
    supervisor_unit()
}

pub(crate) async fn install(app: &AppContext) -> Result<()> {
    require_root()?;
    require_systemd()?;
    // Prove ownership of the unit before creating users or changing
    // directories: a foreign unit at that path ends the install having touched
    // nothing.
    validate_managed_unit(Path::new(UNIT_PATH), "overwrite")?;
    ensure_service_group()?;
    ensure_service_user()?;
    ensure_runtime_paths()?;
    write_managed_unit(Path::new(UNIT_PATH))?;
    run_status("systemctl", &["daemon-reload"])?;
    run_status(
        "systemctl",
        &["enable", phoxal_cli_host::paths::SYSTEMD_UNIT],
    )?;
    app.ui
        .info("installed phoxal-supervisor.service; install a build.phoxal before starting it");
    Ok(())
}

pub(crate) async fn uninstall(app: &AppContext) -> Result<()> {
    require_root()?;
    require_systemd()?;
    if uninstall_unit_file_with(Path::new(UNIT_PATH), disable_managed_unit)? {
        run_status("systemctl", &["daemon-reload"])?;
    }
    app.ui.info(
        "removed phoxal-supervisor.service; releases, state, and the phoxal user were preserved",
    );
    Ok(())
}

/// The whole robot's state, not one unit's.
///
/// A robot is a supervisor plus one unit per runtime, and an operator asking
/// "is it running" is asking about the set. The target is what names that set,
/// so it is what is inspected - and it is only there once a bundle has been
/// installed, which is why the supervisor's own unit is inspected either way.
pub(crate) async fn status(_app: &AppContext) -> Result<()> {
    require_systemd()?;
    let mut units = vec![phoxal_cli_host::paths::SYSTEMD_UNIT];
    if Path::new(phoxal_cli_host::paths::SYSTEMD_UNIT_ROOT)
        .join(super::units::TARGET_UNIT)
        .is_file()
    {
        units.push(super::units::TARGET_UNIT);
    }
    let mut args = vec!["status", "--no-pager", "--full"];
    args.extend(units);
    run_status("systemctl", &args)
}

fn require_root() -> Result<()> {
    anyhow::ensure!(
        unsafe { libc::geteuid() } == 0,
        "`phoxal service install` and `uninstall` require root"
    );
    Ok(())
}

fn require_systemd() -> Result<()> {
    anyhow::ensure!(
        Path::new(phoxal_cli_project::SYSTEMD_ACTIVE_ROOT).is_dir(),
        "systemd is not the active service manager on this host"
    );
    Ok(())
}

/// The unit runs as its own `phoxal` user, so the user's own group has to exist
/// before `useradd --gid` can name it.
fn ensure_service_group() -> Result<()> {
    if Command::new("getent")
        .args(["group", "phoxal"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return Ok(());
    }
    run_status("groupadd", &["--system", "phoxal"])
}

fn ensure_service_user() -> Result<()> {
    if Command::new("id")
        .args(["-u", "phoxal"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return run_status("usermod", &["--gid", "phoxal", "phoxal"]);
    }
    run_status(
        "useradd",
        &[
            "--system",
            "--gid",
            "phoxal",
            "--home-dir",
            phoxal_cli_project::INSTALL_ROOT,
            "--shell",
            "/usr/sbin/nologin",
            "phoxal",
        ],
    )
}

fn ensure_runtime_paths() -> Result<()> {
    use std::fs::OpenOptions;
    for path in [
        phoxal_cli_project::RELEASES_ROOT,
        phoxal_cli_project::INSTALLED_STATE_ROOT,
        phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
    ] {
        std::fs::create_dir_all(path)?;
    }
    // The unit runs as `phoxal`, so the two roots it writes have to belong to it.
    for path in [
        phoxal_cli_project::INSTALLED_STATE_ROOT,
        phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
    ] {
        run_status("chown", &["phoxal:phoxal", path])?;
    }
    let lock = Path::new(phoxal_cli_project::INSTALLED_STATE_ROOT).join("project.lock");
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)?
        .sync_all()?;
    run_status("chown", &["phoxal:phoxal", lock.to_string_lossy().as_ref()])
}

fn write_managed_unit(path: &Path) -> Result<()> {
    if path.exists() {
        let current = std::fs::read_to_string(path)?;
        anyhow::ensure!(
            has_exact_managed_marker(&current),
            "refusing to overwrite foreign unit {}",
            path.display()
        );
        if current == unit_contents() {
            return Ok(());
        }
    }
    let candidate = PathBuf::from(format!(
        "{}.candidate-{}",
        path.display(),
        std::process::id()
    ));
    std::fs::write(&candidate, unit_contents())?;
    std::fs::File::open(&candidate)?.sync_all()?;
    std::fs::rename(&candidate, path)?;
    sync_parent(path)
}

/// Stop and remove the unit only if this CLI wrote it. A missing unit is the
/// sole benign no-op; a failure to stop an existing managed unit leaves its
/// file in place so systemd cannot later restart a process from removed policy.
fn uninstall_unit_file_with(
    path: &Path,
    mut disable_now: impl FnMut(&str) -> Result<()>,
) -> Result<bool> {
    if !validate_managed_unit(path, "remove")? {
        return Ok(false);
    }
    let unit = phoxal_cli_host::paths::SYSTEMD_UNIT;
    disable_now(unit).with_context(|| format!("failed to disable and stop {unit}"))?;
    std::fs::remove_file(path)?;
    sync_parent(path)?;
    Ok(true)
}

fn disable_managed_unit(unit: &str) -> Result<()> {
    run_status("systemctl", &["disable", "--now", unit])
}

fn validate_managed_unit(path: &Path, action: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let contents = std::fs::read_to_string(path)?;
    anyhow::ensure!(
        has_exact_managed_marker(&contents),
        "refusing to {action} foreign unit {}",
        path.display()
    );
    Ok(true)
}

fn has_exact_managed_marker(contents: &str) -> bool {
    contents.lines().next() == Some(UNIT_MARKER)
}

fn sync_parent(path: &Path) -> Result<()> {
    std::fs::File::open(path.parent().context("path has no parent")?)?.sync_all()?;
    Ok(())
}

fn run_status(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program).args(args).status()?;
    anyhow::ensure!(
        status.success(),
        "{} {} failed with {status}",
        program,
        args.join(" ")
    );
    Ok(())
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    fn write_managed_test_unit(path: &Path) -> Result<()> {
        std::fs::write(path, format!("{UNIT_MARKER}\n[Unit]\n"))?;
        Ok(())
    }

    #[test]
    fn managed_marker_must_be_the_exact_first_line() {
        assert!(has_exact_managed_marker(UNIT_MARKER));
        assert!(has_exact_managed_marker(&format!(
            "{UNIT_MARKER}\n[Unit]\nDescription=Phoxal"
        )));

        for foreign in [
            "# Managed by phoxal-old\n[Unit]",
            "# Managed by phoxal with additions\n[Unit]",
            "\n# Managed by phoxal\n[Unit]",
            "[Unit]\n# Managed by phoxal",
            "",
        ] {
            assert!(
                !has_exact_managed_marker(foreign),
                "foreign first line was accepted: {foreign:?}"
            );
        }
    }

    /// A unit that could not be stopped keeps its file: systemd must never be
    /// left able to restart a process from policy this CLI already removed.
    #[test]
    fn a_failed_stop_leaves_the_unit_file_in_place() -> Result<()> {
        let root = tempfile::tempdir()?;
        let unit = root.path().join(phoxal_cli_host::paths::SYSTEMD_UNIT);
        write_managed_test_unit(&unit)?;

        let error = uninstall_unit_file_with(&unit, |unit| {
            anyhow::bail!("injected stop failure for {unit}")
        })
        .expect_err("the injected stop must fail uninstall");

        assert!(error.to_string().contains("failed to disable and stop"));
        assert!(unit.exists());
        Ok(())
    }

    #[test]
    fn uninstall_stops_the_managed_unit_before_removing_its_file() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join(phoxal_cli_host::paths::SYSTEMD_UNIT);
        write_managed_test_unit(&path)?;
        let mut stopped = Vec::new();

        let changed = uninstall_unit_file_with(&path, |unit| {
            assert_eq!(unit, phoxal_cli_host::paths::SYSTEMD_UNIT);
            assert!(path.exists(), "the file outlives the stop that precedes it");
            stopped.push(unit.to_owned());
            Ok(())
        })?;

        assert!(changed);
        assert_eq!(stopped, [phoxal_cli_host::paths::SYSTEMD_UNIT]);
        assert!(!path.exists());
        Ok(())
    }

    /// A unit file this CLI did not write is never stopped, removed, or
    /// overwritten - whoever wrote it owns it.
    #[test]
    fn a_foreign_unit_is_left_entirely_alone() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join(phoxal_cli_host::paths::SYSTEMD_UNIT);
        std::fs::write(&path, "# Managed by somebody else\n[Unit]\n")?;

        let error = uninstall_unit_file_with(&path, |unit| {
            panic!("a foreign unit must never be stopped: {unit}")
        })
        .expect_err("a foreign unit is refused");

        assert!(
            error
                .to_string()
                .contains("refusing to remove foreign unit")
        );
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "# Managed by somebody else\n[Unit]\n"
        );
        Ok(())
    }

    /// Writing the unit is idempotent and produces exactly the contents the
    /// generator renders.
    #[test]
    fn writing_the_managed_unit_produces_the_generated_contents() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join(phoxal_cli_host::paths::SYSTEMD_UNIT);

        write_managed_unit(&path)?;
        assert_eq!(std::fs::read_to_string(&path)?, unit_contents());
        write_managed_unit(&path)?;
        assert_eq!(std::fs::read_to_string(&path)?, unit_contents());
        Ok(())
    }
}
