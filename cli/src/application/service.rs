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
    // Prove ownership of both transition paths before creating users,
    // changing directories, or retiring the legacy unit. A foreign current
    // unit must leave an otherwise valid legacy installation untouched.
    validate_managed_unit(Path::new(UNIT_PATH), "overwrite")?;
    validate_managed_unit(
        Path::new(phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT_PATH),
        "retire",
    )?;
    ensure_service_group()?;
    ensure_service_user()?;
    ensure_runtime_paths()?;
    install_unit_files_with(
        Path::new(UNIT_PATH),
        Path::new(phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT_PATH),
        disable_managed_unit,
    )?;
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
    let path = Path::new(UNIT_PATH);
    let legacy_path = Path::new(phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT_PATH);
    if uninstall_unit_files_with(path, legacy_path, disable_managed_unit)? {
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

fn install_unit_files_with(
    current_path: &Path,
    legacy_path: &Path,
    mut disable_now: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    retire_managed_unit_with(
        legacy_path,
        phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT,
        "retire",
        &mut disable_now,
    )?;
    write_managed_unit(current_path)
}

fn uninstall_unit_files_with(
    current_path: &Path,
    legacy_path: &Path,
    mut disable_now: impl FnMut(&str) -> Result<()>,
) -> Result<bool> {
    // Validate both ownership claims before stopping or removing either unit.
    // A foreign legacy unit must therefore leave the current managed unit
    // untouched, just as a foreign current unit leaves the legacy one intact.
    let current_exists = validate_managed_unit(current_path, "remove")?;
    let legacy_exists = validate_managed_unit(legacy_path, "retire")?;
    if current_exists {
        retire_managed_unit_with(
            current_path,
            phoxal_cli_host::paths::SYSTEMD_UNIT,
            "remove",
            &mut disable_now,
        )?;
    }
    if legacy_exists {
        retire_managed_unit_with(
            legacy_path,
            phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT,
            "retire",
            &mut disable_now,
        )?;
    }
    Ok(current_exists || legacy_exists)
}

/// Stop and retire only an exact unit this CLI manages. A missing unit is the
/// sole benign no-op; any failure to stop an existing managed unit leaves its
/// file in place so systemd cannot later restart a process from removed policy.
fn retire_managed_unit_with(
    path: &Path,
    unit: &str,
    action: &str,
    disable_now: &mut impl FnMut(&str) -> Result<()>,
) -> Result<bool> {
    if !validate_managed_unit(path, action)? {
        return Ok(false);
    }
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

    #[test]
    fn failed_current_stop_leaves_current_and_legacy_unit_files_untouched() -> Result<()> {
        let root = tempfile::tempdir()?;
        let current = root.path().join(phoxal_cli_host::paths::SYSTEMD_UNIT);
        let legacy = root
            .path()
            .join(phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT);
        write_managed_test_unit(&current)?;
        write_managed_test_unit(&legacy)?;

        let error = uninstall_unit_files_with(&current, &legacy, |unit| {
            anyhow::bail!("injected stop failure for {unit}")
        })
        .expect_err("the injected current-unit stop must fail uninstall");

        assert!(error.to_string().contains("failed to disable and stop"));
        assert!(current.exists());
        assert!(legacy.exists());
        Ok(())
    }

    #[test]
    fn failed_legacy_stop_blocks_legacy_removal_and_new_unit_installation() -> Result<()> {
        let root = tempfile::tempdir()?;
        let current = root.path().join(phoxal_cli_host::paths::SYSTEMD_UNIT);
        let legacy = root
            .path()
            .join(phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT);
        write_managed_test_unit(&legacy)?;

        let error = install_unit_files_with(&current, &legacy, |unit| {
            anyhow::bail!("injected stop failure for {unit}")
        })
        .expect_err("the injected legacy-unit stop must fail install");

        assert!(error.to_string().contains("failed to disable and stop"));
        assert!(legacy.exists());
        assert!(!current.exists());
        Ok(())
    }

    #[test]
    fn uninstall_stops_each_exact_managed_unit_before_removing_its_file() -> Result<()> {
        let root = tempfile::tempdir()?;
        let current = root.path().join(phoxal_cli_host::paths::SYSTEMD_UNIT);
        let legacy = root
            .path()
            .join(phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT);
        write_managed_test_unit(&current)?;
        write_managed_test_unit(&legacy)?;
        let mut stopped = Vec::new();

        let changed = uninstall_unit_files_with(&current, &legacy, |unit| {
            match unit {
                phoxal_cli_host::paths::SYSTEMD_UNIT => {
                    assert!(current.exists());
                    assert!(legacy.exists());
                }
                phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT => {
                    assert!(!current.exists());
                    assert!(legacy.exists());
                }
                other => panic!("unexpected unit {other}"),
            }
            stopped.push(unit.to_owned());
            Ok(())
        })?;

        assert!(changed);
        assert_eq!(
            stopped,
            [
                phoxal_cli_host::paths::SYSTEMD_UNIT,
                phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT,
            ]
        );
        assert!(!current.exists());
        assert!(!legacy.exists());
        Ok(())
    }

    #[test]
    fn install_stops_and_removes_exact_legacy_unit_before_writing_current() -> Result<()> {
        let root = tempfile::tempdir()?;
        let current = root.path().join(phoxal_cli_host::paths::SYSTEMD_UNIT);
        let legacy = root
            .path()
            .join(phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT);
        write_managed_test_unit(&legacy)?;

        install_unit_files_with(&current, &legacy, |unit| {
            assert_eq!(unit, phoxal_cli_host::paths::LEGACY_SYSTEMD_UNIT);
            assert!(legacy.exists());
            assert!(!current.exists());
            Ok(())
        })?;

        assert!(!legacy.exists());
        assert_eq!(std::fs::read_to_string(&current)?, unit_contents());
        Ok(())
    }
}
