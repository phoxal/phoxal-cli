//! `cargo install` materialization of official runtimes.
//!
//! Cargo owns download, integrity, and compilation of every official service
//! and every component driver package, against the static registry
//! `sparse+https://phoxal.github.io/registry/`.
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
//! host, exactly like the selected-source batch builder does for
//! workspace-owned crates - the two mechanisms carry the identical
//! cross-compilation caveat (a missing cross toolchain or a native
//! dependency that cannot cross-link fails here too). The container builder
//! avoids this caveat entirely for the always-present official set by
//! running these same commands *inside* the target-native container instead
//! of cross-compiling them from the host - see `commands::build::container`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use phoxal_cli_catalog::{REGISTRY_NAME, cargo_package_name, registry_config_arg};

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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
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

/// Install a compatible exact-version registry group with one Cargo process.
/// The caller groups by target/profile/target-directory; this function rejects
/// an accidental mixed group rather than silently changing a package's build
/// semantics. It verifies every final `bin/` entry before returning.
pub fn cargo_install_batch(
    root: &Path,
    specs: &[MaterializeSpec],
    offline: bool,
    reporter: &dyn crate::Reporter,
) -> Result<Vec<PathBuf>> {
    ensure!(
        !specs.is_empty(),
        "cannot materialize an empty registry batch"
    );
    let first = &specs[0];
    ensure!(
        specs.iter().all(|spec| {
            spec.target == first.target
                && spec.profile == first.profile
                && spec.target_dir == first.target_dir
        }),
        "registry batch mixes target, profile, or target-directory settings"
    );
    let mut packages = BTreeMap::new();
    for spec in specs {
        let package = spec.cargo_package_name();
        if let Some(previous) = packages.insert(package.clone(), spec.train.clone()) {
            ensure!(
                previous == spec.train,
                "registry batch selects {package} at conflicting trains {previous} and {}",
                spec.train
            );
        }
    }
    let args = build_install_args(
        packages
            .iter()
            .map(|(package, train)| (package.as_str(), train.as_str())),
        first.target.as_deref(),
        first.profile,
        offline,
    );
    let mut command = Command::new("cargo");
    command.arg("install").args(&args);
    if let Some(target_dir) = &first.target_dir {
        command.arg("--target-dir").arg(target_dir);
    }
    command.arg("--root").arg(root);
    let operands = packages
        .iter()
        .map(|(package, train)| format!("{package}@{train}"))
        .collect::<Vec<_>>()
        .join(", ");
    crate::progress::run_phase(
        reporter,
        crate::progress_phase::PhaseId::new("materialize-registry-batch"),
        format!("Materializing registry batch ({} packages)", packages.len()),
        || run_cargo_install(&mut command, &operands, reporter),
    )?;
    packages
        .keys()
        .map(|package| harvest_binary(root, package))
        .collect()
}

/// The exact `cargo install` arguments after `install` and before `--root
/// <root>` (appended separately since `root` is a `Path`, not a `str`) - the
/// part shared between a host [`std::process::Command`] and a shell command
/// line embedded in a container/remote invocation. Cross-compiles with
/// `--target <triple>` only when `target` is set AND differs from the host -
/// an explicit host-triple target is the plain native build, matching
/// the selected-source batch builder's identical check.
#[must_use]
pub fn build_install_args<'a>(
    packages: impl IntoIterator<Item = (&'a str, &'a str)>,
    target: Option<&str>,
    profile: MaterializeProfile,
    offline: bool,
) -> Vec<String> {
    let mut args = packages
        .into_iter()
        .map(|(package, train)| format!("{package}@{train}"))
        .collect::<Vec<_>>();
    args.extend([
        "--registry".to_string(),
        REGISTRY_NAME.to_string(),
        "--locked".to_string(),
        "--config".to_string(),
        registry_config_arg(),
    ]);
    let cross = target.filter(|triple| *triple != crate::source::host_target_triple());
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

fn run_cargo_install(
    command: &mut Command,
    package: &str,
    reporter: &dyn crate::Reporter,
) -> Result<()> {
    let status = crate::build::shell::command_status_captured(command, reporter)
        .with_context(|| format!("failed to run `cargo install` for {package}"))?;
    ensure!(
        status.success(),
        "cargo install {package} failed with status {status}"
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
            std::iter::once((package.as_str(), spec.train.as_str())),
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
        use crate::source::resolver::official_binary_name;
        use phoxal_cli_catalog::ArtifactKind;

        let cases = [
            ("phoxal/service-drive", ArtifactKind::Service, "drive"),
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
    fn install_args_batch_packages_at_independent_exact_versions() {
        let args = build_install_args(
            [
                ("phoxal-service-drive", "0.42.3"),
                ("phoxal-component-ddsm115", "0.41.7"),
            ],
            None,
            MaterializeProfile::Release,
            false,
        );
        assert_eq!(
            &args[..2],
            [
                "phoxal-service-drive@0.42.3",
                "phoxal-component-ddsm115@0.41.7",
            ]
        );
        assert_eq!(args.iter().filter(|arg| *arg == "--registry").count(), 1);
        assert_eq!(args.iter().filter(|arg| *arg == "--locked").count(), 1);
        assert_eq!(args.iter().filter(|arg| *arg == "--config").count(), 1);
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
        let foreign = if crate::source::host_target_triple() == "aarch64-unknown-linux-gnu" {
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
        let host = crate::source::host_target_triple();
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
