//! `cargo install` materialization of exact registry packages.
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
//!
//! The Cargo invocation and argument construction here are neutral across
//! registry packages. On the host, that shared machinery does not make the
//! supervisor a participant: its `Release`/`ReleaseRoot` specification runs as
//! a separate batch from `BundleBin` participants whenever profile or
//! destination differs. The completed supervisor is copied beside `bundle/`
//! only during release finalization.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use phoxal_cli_catalog::{REGISTRY_NAME, registry_config_arg};

/// The framework-owned execution supervisor package and binary.
pub(crate) const SUPERVISOR_PACKAGE: &str = "phoxal-supervisor";

/// One Cargo package to materialize: its published package/binary name, the
/// exact framework train it is pinned to, and
/// the target triple to build for (`None` builds for the host running the
/// command - inside a container, that IS the target, so no cross flag is
/// needed there either).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializeSpec {
    pub cargo_package: String,
    pub train: String,
    pub target: Option<String>,
    pub profile: MaterializeProfile,
    pub target_dir: Option<PathBuf>,
    pub destination: MaterializationDestination,
}

/// The owner boundary a registry package materializes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializationDestination {
    /// A participant binary referenced by the frozen runtime document.
    BundleBin,
    /// The framework supervisor beside `bundle/`, never inside it.
    ReleaseRoot,
}

impl MaterializationDestination {
    #[must_use]
    pub const fn enters_runtime_document(self) -> bool {
        matches!(self, Self::BundleBin)
    }
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
    pub fn new(cargo_package: impl Into<String>, train: impl Into<String>) -> Self {
        Self {
            cargo_package: cargo_package.into(),
            train: train.into(),
            target: None,
            profile: MaterializeProfile::Release,
            target_dir: None,
            destination: MaterializationDestination::BundleBin,
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

    /// The package's explicit Cargo identity, also required to be its binary
    /// target name so `cargo install --root` has one deterministic harvest path.
    #[must_use]
    pub fn cargo_package_name(&self) -> &str {
        &self.cargo_package
    }
}

/// The non-participant framework supervisor specification.
///
/// It is deliberately constructed outside the participant catalog and always
/// uses Cargo's release profile, even when an interactive run builds its
/// participant binaries in debug mode. It shares the neutral Cargo
/// materializer, but its release profile and release-root destination keep it
/// in a separate host batch from participant bundle entries.
#[must_use]
pub(crate) fn supervisor_spec(
    train: impl Into<String>,
    target: Option<String>,
    target_dir: Option<PathBuf>,
) -> MaterializeSpec {
    let mut spec = MaterializeSpec::new(SUPERVISOR_PACKAGE, train)
        .with_target(target)
        .with_profile(MaterializeProfile::Release);
    spec.destination = MaterializationDestination::ReleaseRoot;
    if let Some(target_dir) = target_dir {
        spec = spec.with_target_dir(target_dir);
    }
    spec
}

/// Install a compatible exact-version registry group with one Cargo process.
/// The caller groups by target/profile/target-directory and invokes this
/// neutral mechanism separately for distinct destinations. In particular, the
/// host supervisor's `ReleaseRoot` batch is not folded into a participant
/// `BundleBin` batch. This function rejects an accidental mixed build setting
/// rather than silently changing a package's build semantics, and verifies
/// every final `bin/` entry before returning.
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
        let package = spec.cargo_package_name().to_string();
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

/// One exact-train supervisor materialized outside the participant bundle.
pub(crate) struct MaterializedSupervisor {
    path: PathBuf,
    _install_root: Option<tempfile::TempDir>,
}

impl MaterializedSupervisor {
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Materialize the non-catalog framework supervisor, consuming a target-native
/// container batch when supplied and otherwise running the shared Cargo
/// materializer as its own release-profile, release-destination batch in a
/// dedicated temporary install root. Container absence is a hard error, never
/// a host fallback.
pub(crate) fn materialize_supervisor(
    train: &str,
    target: Option<&str>,
    offline: bool,
    officials_source: Option<&Path>,
    target_dir: Option<PathBuf>,
    reporter: &dyn crate::Reporter,
) -> Result<MaterializedSupervisor> {
    if let Some(source_root) = officials_source {
        let path = source_root.join("bin").join(SUPERVISOR_PACKAGE);
        ensure!(
            path.is_file(),
            "the target-native registry batch did not materialize {SUPERVISOR_PACKAGE}@{train} \
             (expected {}); this is a hard error, not a fallback to a host build",
            path.display()
        );
        return Ok(MaterializedSupervisor {
            path,
            _install_root: None,
        });
    }

    let install_root = tempfile::Builder::new()
        .prefix("phoxal-supervisor-install-")
        .tempdir()
        .context("failed to create the supervisor materialization root")?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cargo")));
    let offline_diagnostic = offline.then(|| {
        offline_supervisor_diagnostic(train, cargo_home.as_deref(), target_dir.as_deref())
    });
    let spec = supervisor_spec(train, target.map(str::to_string), target_dir);
    ensure!(
        !spec.destination.enters_runtime_document(),
        "the framework supervisor must materialize at the release root, never as a runtime participant"
    );
    let mut installed = cargo_install_batch(
        install_root.path(),
        std::slice::from_ref(&spec),
        offline,
        reporter,
    )
    .with_context(|| {
        if let Some(diagnostic) = &offline_diagnostic {
            diagnostic.clone()
        } else {
            format!("could not materialize {SUPERVISOR_PACKAGE}@{train}")
        }
    })?;
    let path = installed
        .pop()
        .context("the supervisor registry batch returned no binary")?;
    Ok(MaterializedSupervisor {
        path,
        _install_root: Some(install_root),
    })
}

fn offline_supervisor_diagnostic(
    train: &str,
    cargo_home: Option<&Path>,
    target_dir: Option<&Path>,
) -> String {
    let registry = cargo_home.map(|home| home.join("registry")).map_or_else(
        || "<unresolved CARGO_HOME>/registry".to_string(),
        |path| path.display().to_string(),
    );
    let build = target_dir.map_or_else(
        || "<Cargo target directory>".to_string(),
        |path| path.display().to_string(),
    );
    format!(
        "could not materialize {SUPERVISOR_PACKAGE}@{train} while offline; the required \
         package/source must already be in Cargo registry cache {registry} and its build \
         artifacts in target cache {build}; retry once without --offline to warm that exact \
         framework train, then repeat with --offline"
    )
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
            std::iter::once((package, spec.train.as_str())),
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
            let spec =
                MaterializeSpec::new(phoxal_cli_catalog::cargo_package_name(catalog_id), "0.42.0");
            assert_eq!(spec.cargo_package_name(), official_binary_name(kind, short));
        }
    }

    #[test]
    fn install_command_pins_the_exact_version_and_carries_every_required_flag() {
        let spec = MaterializeSpec::new("phoxal-service-drive", "0.42.0");
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
        let spec = MaterializeSpec::new("phoxal-service-drive", "0.42.0");
        let command = command_for(&spec, Path::new("/tmp/bundle"), true);
        assert!(args(&command).contains(&"--offline".to_string()));
    }

    #[test]
    fn debug_profile_and_shared_target_directory_are_explicit() {
        let spec = MaterializeSpec::new("phoxal-service-drive", "0.42.0")
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
        let spec = MaterializeSpec::new("phoxal-service-drive", "0.42.0");
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
        let spec = MaterializeSpec::new("phoxal-service-drive", "0.42.0")
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
        let spec = MaterializeSpec::new("phoxal-service-drive", "0.42.0").with_target(Some(host));
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

    #[test]
    fn supervisor_is_an_explicit_non_catalog_release_package() {
        let spec = supervisor_spec(
            "0.60.1",
            Some("aarch64-unknown-linux-gnu".to_string()),
            Some(PathBuf::from("/workspace/target")),
        );
        assert_eq!(spec.cargo_package_name(), SUPERVISOR_PACKAGE);
        assert_eq!(spec.train, "0.60.1");
        assert_eq!(spec.profile, MaterializeProfile::Release);
        assert_eq!(spec.destination, MaterializationDestination::ReleaseRoot);
        assert!(!spec.destination.enters_runtime_document());
        assert_eq!(spec.target.as_deref(), Some("aarch64-unknown-linux-gnu"));
        assert_eq!(spec.target_dir, Some(PathBuf::from("/workspace/target")));
        let catalog = phoxal_cli_catalog::Catalog::official();
        assert!(
            catalog
                .native()
                .chain(catalog.simulation())
                .all(|official| {
                    phoxal_cli_catalog::cargo_package_name(official.package) != SUPERVISOR_PACKAGE
                }),
            "the supervisor must never become a participant catalog entry"
        );
    }

    #[test]
    fn offline_supervisor_diagnostic_names_exact_package_caches_and_warmup() {
        let diagnostic = offline_supervisor_diagnostic(
            "0.60.1",
            Some(Path::new("/cache/cargo")),
            Some(Path::new("/cache/target")),
        );
        for expected in [
            "phoxal-supervisor@0.60.1",
            "/cache/cargo/registry",
            "/cache/target",
            "without --offline",
            "then repeat with --offline",
        ] {
            assert!(diagnostic.contains(expected), "{diagnostic}");
        }
    }
}
