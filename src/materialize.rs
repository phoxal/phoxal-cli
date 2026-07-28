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
//!
//! `target` cross-compiles with `--target <triple>` when it differs from the
//! host, exactly like [`crate::run::build_source_binary`] does for
//! workspace-owned crates - the two mechanisms carry the identical
//! cross-compilation caveat (a missing cross toolchain or a native
//! dependency that cannot cross-link fails here too). The container builder
//! avoids this caveat entirely for the always-present official set by
//! running these same commands *inside* the target-native container instead
//! of cross-compiling them from the host - see `commands::build::container`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use phoxal_cli_core::project::catalog::{REGISTRY_NAME, cargo_package_name, registry_config_arg};

/// One official package to materialize: its catalog identity
/// (`phoxal/service-drive`), the exact framework train it is pinned to, and
/// the target triple to build for (`None` builds for the host running the
/// command - inside a container, that IS the target, so no cross flag is
/// needed there either).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializeSpec {
    pub catalog_package: String,
    pub train: String,
    pub target: Option<String>,
    pub profile: MaterializeProfile,
    pub target_dir: Option<PathBuf>,
}

/// Cargo's two install profiles. Interactive staging uses debug builds so
/// source and registry participants share one development profile; deployable
/// bundles use Cargo's default release profile throughout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MaterializeProfile {
    Debug,
    #[default]
    Release,
}

impl MaterializeSpec {
    #[must_use]
    pub fn new(catalog_package: impl Into<String>, train: impl Into<String>) -> Self {
        Self {
            catalog_package: catalog_package.into(),
            train: train.into(),
            target: None,
            profile: MaterializeProfile::Release,
            target_dir: None,
        }
    }

    /// Materialize for an explicit target triple. A triple equal to the
    /// host's own is treated identically to `None` - see
    /// [`build_install_args`].
    #[must_use]
    pub fn with_target(mut self, target: Option<String>) -> Self {
        self.target = target;
        self
    }

    #[must_use]
    pub fn with_profile(mut self, profile: MaterializeProfile) -> Self {
        self.profile = profile;
        self
    }

    /// Share Cargo's project target directory with source builds. The install
    /// root still owns only the final runtime binary; compiler intermediates
    /// stay in Cargo's standard cache.
    #[must_use]
    pub fn with_target_dir(mut self, target_dir: PathBuf) -> Self {
        self.target_dir = Some(target_dir);
        self
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
    let args = build_install_args(
        &package,
        &spec.train,
        spec.target.as_deref(),
        spec.profile,
        offline,
    );
    let mut command = Command::new("cargo");
    command.arg("install").args(&args);
    if let Some(target_dir) = &spec.target_dir {
        command.arg("--target-dir").arg(target_dir);
    }
    command.arg("--root").arg(root);
    run_cargo_install(command, &package)?;
    harvest_binary(root, &package)
}

/// The exact `cargo install` arguments after `install` and before `--root
/// <root>` (appended separately since `root` is a `Path`, not a `str`) - the
/// part shared between a host [`std::process::Command`] and a shell command
/// line embedded in a container/remote invocation. Cross-compiles with
/// `--target <triple>` only when `target` is set AND differs from the host -
/// an explicit host-triple target is the plain native build, matching
/// [`crate::run::build_source_binary`]'s identical check.
#[must_use]
pub fn build_install_args(
    package: &str,
    train: &str,
    target: Option<&str>,
    profile: MaterializeProfile,
    offline: bool,
) -> Vec<String> {
    let mut args = vec![
        format!("{package}@{train}"),
        "--registry".to_string(),
        REGISTRY_NAME.to_string(),
        "--locked".to_string(),
        "--config".to_string(),
        registry_config_arg(),
    ];
    let cross = target.filter(|triple| *triple != phoxal_cli_core::project::host_target_triple());
    if let Some(triple) = cross {
        args.push("--target".to_string());
        args.push(triple.to_string());
    }
    if profile == MaterializeProfile::Debug {
        args.push("--debug".to_string());
    }
    if offline {
        args.push("--offline".to_string());
    }
    args
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
/// `cargo install --target <triple>` still normalizes its final output to
/// `<root>/bin/` - `--target` only changes the internal build directory
/// shape (`target/<triple>/release/`), never the install destination.
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

    fn command_for(spec: &MaterializeSpec, root: &Path, offline: bool) -> Command {
        let package = spec.cargo_package_name();
        let install_args = build_install_args(
            &package,
            &spec.train,
            spec.target.as_deref(),
            spec.profile,
            offline,
        );
        let mut command = Command::new("cargo");
        command.arg("install").args(&install_args);
        if let Some(target_dir) = &spec.target_dir {
            command.arg("--target-dir").arg(target_dir);
        }
        command.arg("--root").arg(root);
        command
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
        let command = command_for(&spec, Path::new("/tmp/bundle"), false);
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
    }

    #[test]
    fn offline_appends_the_offline_flag() {
        let spec = MaterializeSpec::new("phoxal/service-drive", "0.42.0");
        let command = command_for(&spec, Path::new("/tmp/bundle"), true);
        assert!(args(&command).contains(&"--offline".to_string()));
    }

    #[test]
    fn debug_profile_and_shared_target_directory_are_explicit() {
        let spec = MaterializeSpec::new("phoxal/service-drive", "0.42.0")
            .with_profile(MaterializeProfile::Debug)
            .with_target_dir(PathBuf::from("/workspace/target"));
        let argv = args(&command_for(&spec, Path::new("/tmp/bundle"), false));
        assert!(argv.contains(&"--debug".to_string()));
        let target_dir = argv
            .iter()
            .position(|arg| arg == "--target-dir")
            .map(|index| argv[index + 1].clone())
            .expect("--target-dir is present");
        assert_eq!(target_dir, "/workspace/target");
    }

    #[test]
    fn a_bare_package_name_is_never_installed_without_a_pinned_version() {
        // Regression for the empirically verified failure mode: a bare name
        // installs the newest train the moment one exists.
        let spec = MaterializeSpec::new("phoxal/service-drive", "0.42.0");
        let command = command_for(&spec, Path::new("/tmp/bundle"), false);
        let argv = args(&command);
        assert!(
            argv[1].contains('@'),
            "the install spec must pin a version: {argv:?}"
        );
    }

    #[test]
    fn a_foreign_target_appends_the_cross_flag() {
        let foreign =
            if phoxal_cli_core::project::host_target_triple() == "aarch64-unknown-linux-gnu" {
                "x86_64-unknown-linux-gnu"
            } else {
                "aarch64-unknown-linux-gnu"
            };
        let spec = MaterializeSpec::new("phoxal/service-drive", "0.42.0")
            .with_target(Some(foreign.to_string()));
        let command = command_for(&spec, Path::new("/tmp/bundle"), false);
        let argv = args(&command);
        let target_index = argv
            .iter()
            .position(|arg| arg == "--target")
            .map(|index| argv[index + 1].clone())
            .expect("--target is present for a foreign triple");
        assert_eq!(target_index, foreign);
    }

    #[test]
    fn a_target_matching_the_host_omits_the_cross_flag() {
        let host = phoxal_cli_core::project::host_target_triple();
        let spec = MaterializeSpec::new("phoxal/service-drive", "0.42.0").with_target(Some(host));
        let command = command_for(&spec, Path::new("/tmp/bundle"), false);
        assert!(!args(&command).contains(&"--target".to_string()));
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
