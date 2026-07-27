//! `cargo install` materialization of official runtimes (organization#951 WS4).
//!
//! Cargo owns download, integrity, and compilation of every official service,
//! tool, the infrastructure router, and every component driver package -
//! against the static registry `sparse+https://phoxal.github.io/registry/`.
//! There is no vendored artifact store and no suite/tarball download anymore:
//! `cargo install` resolves, fetches, verifies, and builds each package, and
//! Cargo's own build-directory locking covers its own cache.
//!
//! Every invocation is pinned to the *exact* locked framework train
//! (`<package>@<train>`, never a bare name) and passes `--locked` so the
//! packaged lockfile - not a fresh re-resolution of transitives - is what
//! gets built. `--no-track` is deliberately never passed: it does not remove
//! `.crates.toml`/`.crates2.json` (both are created empty either way) and it
//! disables Cargo's protection against concurrent invocations, so the
//! archiving step excludes dotfiles instead of trading that away.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use phoxal_cli_core::project::catalog::{REGISTRY_NAME, cargo_package_name, registry_config_arg};

/// One official package to materialize: its catalog identity
/// (`phoxal/service-drive`) and the exact framework train it is pinned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializeSpec {
    pub catalog_package: String,
    pub train: String,
}

impl MaterializeSpec {
    #[must_use]
    pub fn new(catalog_package: impl Into<String>, train: impl Into<String>) -> Self {
        Self {
            catalog_package: catalog_package.into(),
            train: train.into(),
        }
    }

    /// The Cargo package name this catalog identity is published under, which
    /// is also the exact binary name every official package's `[[bin]]`
    /// target carries - the catalog id's kind prefix and `official_binary_name`'s
    /// kind prefix are the identical string, so the two projections always
    /// agree.
    #[must_use]
    pub fn cargo_package_name(&self) -> String {
        cargo_package_name(&self.catalog_package)
    }
}

/// `cargo install <package>@<train> --registry phoxal --locked --root <root>`,
/// the most standard invocation the task's own acceptance criteria call for.
/// Binaries are always harvested from `<root>/bin/<name>` afterward, never
/// from the `executable` path a `--message-format json` run would report:
/// `cargo install` MOVES the built binary into `<root>/bin`, so that path is
/// stale by the time it would be read.
pub fn cargo_install(root: &Path, spec: &MaterializeSpec, offline: bool) -> Result<PathBuf> {
    let package = spec.cargo_package_name();
    let command = build_install_command(root, &package, &spec.train, offline);
    run_cargo_install(command, &package)?;
    harvest_binary(root, &package)
}

/// The exact argv `cargo_install` runs, split out so the command shape is
/// independently testable without a network or a real `cargo` binary.
fn build_install_command(root: &Path, package: &str, train: &str, offline: bool) -> Command {
    let mut command = Command::new("cargo");
    command
        .arg("install")
        .arg(format!("{package}@{train}"))
        .arg("--registry")
        .arg(REGISTRY_NAME)
        .arg("--locked")
        .arg("--root")
        .arg(root)
        .arg("--config")
        .arg(registry_config_arg());
    if offline {
        command.arg("--offline");
    }
    command
}

fn run_cargo_install(mut command: Command, package: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("failed to run `cargo install` for {package}"))?;
    ensure!(
        output.status.success(),
        "cargo install {package} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Harvest the binary `cargo install` moved into `<root>/bin/<package>`.
fn harvest_binary(root: &Path, package: &str) -> Result<PathBuf> {
    let binary = root.join("bin").join(package);
    ensure!(
        binary.is_file(),
        "cargo install {package} reported success but {} is missing; the package's binary target \
         must be named exactly {package}",
        binary.display()
    );
    Ok(binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn cargo_package_name_matches_official_binary_name_for_every_kind() {
        use phoxal_cli_core::project::catalog::ArtifactKind;
        use phoxal_cli_core::project::resolver::official_binary_name;

        let cases = [
            ("phoxal/service-drive", ArtifactKind::Service, "drive"),
            ("phoxal/tool-bus", ArtifactKind::Tool, "bus"),
            (
                "phoxal/infrastructure-router",
                ArtifactKind::Infrastructure,
                "router",
            ),
            (
                "phoxal/component-ddsm115",
                ArtifactKind::ComponentDriver,
                "ddsm115",
            ),
        ];
        for (catalog_id, kind, short) in cases {
            let spec = MaterializeSpec::new(catalog_id, "0.42.0");
            assert_eq!(spec.cargo_package_name(), official_binary_name(kind, short));
        }
    }

    #[test]
    fn install_command_pins_the_exact_version_and_carries_every_required_flag() {
        let spec = MaterializeSpec::new("phoxal/service-drive", "0.42.0");
        let command = build_install_command(
            Path::new("/tmp/bundle"),
            "phoxal-service-drive",
            "0.42.0",
            false,
        );
        let argv = args(&command);
        assert_eq!(argv[0], "install");
        assert_eq!(argv[1], "phoxal-service-drive@0.42.0");
        assert!(argv.contains(&"--locked".to_string()));
        assert!(!argv.contains(&"--no-track".to_string()));
        let registry_index = argv
            .iter()
            .position(|arg| arg == "--config")
            .map(|index| argv[index + 1].clone())
            .expect("--config is present");
        assert_eq!(
            registry_index,
            "registries.phoxal.index=\"sparse+https://phoxal.github.io/registry/\""
        );
        let root_index = argv
            .iter()
            .position(|arg| arg == "--root")
            .map(|index| argv[index + 1].clone())
            .expect("--root is present");
        assert_eq!(root_index, "/tmp/bundle");
        let _ = spec;
    }

    #[test]
    fn offline_appends_the_offline_flag() {
        let command = build_install_command(
            Path::new("/tmp/bundle"),
            "phoxal-service-drive",
            "0.42.0",
            true,
        );
        assert!(args(&command).contains(&"--offline".to_string()));
    }

    #[test]
    fn a_bare_package_name_is_never_installed_without_a_pinned_version() {
        // Regression for the empirically verified failure mode: a bare name
        // installs the newest train the moment one exists.
        let command = build_install_command(
            Path::new("/tmp/bundle"),
            "phoxal-service-drive",
            "0.42.0",
            false,
        );
        let argv = args(&command);
        assert!(
            argv[1].contains('@'),
            "the install spec must pin a version: {argv:?}"
        );
    }

    #[test]
    fn harvest_fails_precisely_when_cargo_install_reports_success_but_bin_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let error = harvest_binary(root.path(), "phoxal-service-drive").unwrap_err();
        assert!(error.to_string().contains("bin/phoxal-service-drive"));
    }

    #[test]
    fn harvest_reads_the_root_bin_path_never_a_stale_message_format_json_path() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("bin")).unwrap();
        std::fs::write(root.path().join("bin/phoxal-service-drive"), b"binary").unwrap();
        let binary = harvest_binary(root.path(), "phoxal-service-drive").unwrap();
        assert_eq!(binary, root.path().join("bin/phoxal-service-drive"));
    }
}
