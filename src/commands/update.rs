use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Serialize;

use crate::AppContext;
use crate::native_artifacts::ArtifactProgressReporter;
use crate::resolver::resolve;
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use phoxal_cli_core::project::resolver::{ResolveOptions, discover_robot_yaml, load_robot};

#[derive(Debug, Args)]
pub struct Update {
    #[arg(
        long,
        help = "Plan and report the update without downloading or changing active versions."
    )]
    pub dry_run: bool,
    #[arg(
        long = "target",
        value_name = "TRIPLE",
        help = "Additionally vendor the official per-target blobs for this Rust target triple, so \
                `phoxal build --target <TRIPLE>` can stage offline. Repeatable. Already-vendored \
                triples are always kept fresh without this flag."
    )]
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageUpdate {
    pub package: String,
    pub target: Option<String>,
    pub old: Option<String>,
    pub new: String,
    pub bytes: u64,
    pub pinned: bool,
    pub refresh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateSummary {
    pub dry_run: bool,
    pub train: String,
    pub destination: PathBuf,
    pub package_count: usize,
    pub download_bytes: u64,
    pub free_disk_bytes: Option<u64>,
    pub updates: Vec<PackageUpdate>,
    pub prune_versions: BTreeMap<String, Vec<String>>,
    pub pins_skipped: Vec<String>,
}

impl Update {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let project_root = app.project.root().to_path_buf();
        let _project_lock = crate::supervisor::ProjectLock::acquire(
            crate::supervisor::ProjectLockIdentity::resolve(
                &project_root,
                crate::supervisor::ProjectOperation::Update,
            ),
        )?;
        let suite_source = app.suite_source.clone();
        let dry_run = self.dry_run;
        let targets = self.targets.clone();
        let ui = app.ui;
        let interactive = ui.interactive();
        let summary = tokio::task::spawn_blocking(move || {
            update(&project_root, suite_source, dry_run, &targets, &ui)
        })
        .await
        .context("update worker failed")??;
        print_human(&summary, interactive)
    }
}

fn update(
    project_start: &Path,
    suite_source: Option<String>,
    dry_run: bool,
    targets: &[String],
    ui: &crate::Ui,
) -> Result<UpdateSummary> {
    let robot_path = discover_robot_yaml(project_start)?;
    let project_root = robot_path.parent().context("robot.yaml has no parent")?;
    let robot = load_robot(&robot_path)?;
    let locked = phoxal_cli_core::project::train::resolve_locked_train(project_root)?;
    if locked.is_published() && !phoxal_cli_core::project::suite::offline_from_env() {
        match phoxal_cli_core::project::train::inspect_registry_train(&locked.version) {
            Ok(phoxal_cli_core::project::train::RegistryStatus::Yanked) => {
                eprintln!(
                    "warning: locked framework train {} is yanked on crates.io; preserving this existing lock is supported, but Cargo will not select it for a new update",
                    locked.version
                );
            }
            Ok(phoxal_cli_core::project::train::RegistryStatus::Available) => {}
            Err(error) => {
                eprintln!(
                    "warning: could not inspect framework train {} on crates.io: {error:#}",
                    locked.version
                );
            }
        }
    }
    update_locked_train(
        project_root,
        &robot,
        suite_source,
        dry_run,
        targets,
        ui,
        &locked.version,
    )
}

#[allow(clippy::too_many_arguments)]
fn update_locked_train(
    project_root: &Path,
    robot: &phoxal::model::robot::RobotV0,
    suite_source: Option<String>,
    dry_run: bool,
    targets: &[String],
    ui: &crate::Ui,
    locked_version: &str,
) -> Result<UpdateSummary> {
    let suite = phoxal_cli_core::project::suite::load_suite(
        phoxal_cli_core::project::suite::SuiteLoadOptions {
            cli_source: suite_source,
            offline: false,
        },
        locked_version,
    )?
    .context("update requires a reachable artifact suite; --offline cannot update")?;
    let resolved = resolve(
        robot,
        project_root,
        Some(&suite),
        ResolveOptions {
            ..ResolveOptions::default()
        },
    )?;

    let destination = project_root.join(".phoxal/artifacts");
    let mut descriptors = crate::native_artifacts::descriptors(&resolved)?;
    include_existing_target_scopes(&mut descriptors, &suite)?;
    include_requested_target_scopes(&mut descriptors, &suite, targets)?;
    let updates = descriptors
        .iter()
        .map(|descriptor| {
            let old = crate::native_artifacts::active_version_read_only(descriptor)
                .ok()
                .flatten();
            PackageUpdate {
                package: descriptor.package_id.clone(),
                target: descriptor.target.clone(),
                refresh: old.as_deref() == Some(descriptor.version.as_str())
                    && !crate::native_artifacts::descriptor_is_current(descriptor),
                old,
                new: descriptor.version.clone(),
                bytes: descriptor.size,
                pinned: false,
            }
        })
        .collect::<Vec<_>>();
    let download_bytes = updates
        .iter()
        .zip(&descriptors)
        .filter(|(_, descriptor)| !crate::native_artifacts::descriptor_is_current(descriptor))
        .map(|(update, _)| update.bytes)
        .sum();
    let mut prune_versions = BTreeMap::new();
    for descriptor in &descriptors {
        let versions = crate::native_artifacts::inactive_versions(descriptor)?;
        if !versions.is_empty() {
            prune_versions.insert(descriptor.package_id.clone(), versions);
        }
    }
    let free_disk_bytes = free_disk_bytes(project_root).ok();
    if let Some(free) = free_disk_bytes
        && download_bytes > free
    {
        bail!(
            "artifact update needs {} but only {} are free at {}",
            phoxal_cli_core::session::human::bytes(download_bytes),
            phoxal_cli_core::session::human::bytes(free),
            destination.display()
        );
    }

    let pins_skipped = Vec::new();

    if !dry_run {
        let _lock = crate::native_artifacts::ArtifactStoreLock::exclusive("update")?;
        fs::create_dir_all(&destination)?;
        let progress = UpdateProgress::new(ui.interactive(), &updates);
        crate::native_artifacts::prepare_and_activate_descriptors_with_progress(
            &descriptors,
            &progress,
        )?;
        // Persist the fetched suite into the vendored store so `run`/`start`/
        // `build` resolve the locked train fully offline (#936, finding
        // 1). `phoxal update` is the only path that fetches and vendors it.
        crate::native_artifacts::persist_suite(&suite)?;
    }

    Ok(UpdateSummary {
        dry_run,
        train: suite.version.clone(),
        destination,
        package_count: descriptors.len(),
        download_bytes,
        free_disk_bytes,
        updates,
        prune_versions,
        pins_skipped,
    })
}

/// Vendor the official per-target blobs for each explicitly requested triple
/// (`update --target <TRIPLE>`), so a later `phoxal build --target <TRIPLE>`
/// stages entirely from the vendored store with no network (#936). A triple the
/// suite has no blob for fails naming the package, so a typo'd or unsupported
/// target is caught here rather than at build time.
fn include_requested_target_scopes(
    descriptors: &mut Vec<phoxal_cli_core::artifacts::NativeArtifactDescriptor>,
    suite: &phoxal_cli_core::project::suite::Suite,
    targets: &[String],
) -> Result<()> {
    if targets.is_empty() {
        return Ok(());
    }
    let current = descriptors.clone();
    for descriptor in current {
        if descriptor.target.is_none() {
            continue;
        }
        // Native runtime layouts exclude simulator binaries, and the Webots
        // simulators only exist for hosts Webots itself supports - a requested
        // device target never needs them.
        if descriptor.kind == phoxal_cli_core::project::suite::ArtifactKind::Simulator {
            continue;
        }
        let artifact = suite
            .artifacts
            .iter()
            .find(|artifact| artifact.id == descriptor.package_id)
            .with_context(|| {
                format!(
                    "resolved {} is absent from framework train {} suite",
                    descriptor.package_id, suite.version
                )
            })?;
        for target in targets {
            if descriptors.iter().any(|existing| {
                existing.package_id == descriptor.package_id
                    && existing.target.as_deref() == Some(target.as_str())
            }) {
                continue;
            }
            let blob = artifact.targets.get(target.as_str()).with_context(|| {
                format!(
                    "framework train {} has no {} blob for requested target {target}; \
                     supported targets: {}",
                    suite.version,
                    descriptor.package_id,
                    artifact
                        .targets
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            let mut requested = descriptor.clone();
            requested.target = Some(target.clone());
            requested.url = blob.url.clone();
            requested.sha256 = blob.sha256.clone();
            requested.size = blob.size;
            descriptors.push(requested);
        }
    }
    descriptors.sort_by(|left, right| {
        (&left.package_id, &left.target).cmp(&(&right.package_id, &right.target))
    });
    descriptors
        .dedup_by(|left, right| left.package_id == right.package_id && left.target == right.target);
    Ok(())
}

fn include_existing_target_scopes(
    descriptors: &mut Vec<phoxal_cli_core::artifacts::NativeArtifactDescriptor>,
    suite: &phoxal_cli_core::project::suite::Suite,
) -> Result<()> {
    let current = descriptors.clone();
    for descriptor in current {
        if descriptor.target.is_none() {
            continue;
        }
        let artifact = suite
            .artifacts
            .iter()
            .find(|artifact| {
                artifact.id == descriptor.package_id && suite.version == descriptor.version
            })
            .with_context(|| {
                format!(
                    "resolved {} {} is absent from framework train {} suite",
                    descriptor.package_id, descriptor.version, suite.version
                )
            })?;
        for target in crate::native_artifacts::existing_target_scopes(&descriptor.package_id)? {
            if descriptors.iter().any(|existing| {
                existing.package_id == descriptor.package_id
                    && existing.target.as_deref() == Some(target.as_str())
            }) {
                continue;
            }
            let blob = artifact.targets.get(&target).with_context(|| {
                format!(
                    "framework train {} has no {} blob for retained target {target}; update phoxal or choose a supported target",
                    suite.version, descriptor.package_id
                )
            })?;
            let mut retained_target = descriptor.clone();
            retained_target.target = Some(target);
            retained_target.url = blob.url.clone();
            retained_target.sha256 = blob.sha256.clone();
            retained_target.size = blob.size;
            descriptors.push(retained_target);
        }
    }
    descriptors.sort_by(|left, right| {
        (&left.package_id, &left.target).cmp(&(&right.package_id, &right.target))
    });
    Ok(())
}

fn print_human(summary: &UpdateSummary, interactive: bool) -> Result<()> {
    for line in human_lines(summary, interactive) {
        println!("{line}");
    }
    Ok(())
}

fn human_lines(summary: &UpdateSummary, interactive: bool) -> Vec<String> {
    if interactive && !summary.dry_run {
        return if summary.updates.iter().any(|update| !update.pinned) {
            summary
                .prune_versions
                .iter()
                .map(|(package, versions)| format!("pruned {package}: {}", versions.join(", ")))
                .collect()
        } else {
            vec!["no updates available".to_string()]
        };
    }
    let marker = if summary.dry_run {
        "\u{2022}"
    } else {
        "\u{2713}"
    };
    let mut lines = Vec::new();
    for update in &summary.updates {
        if update.pinned {
            continue;
        }
        lines.push(format!("{marker} {}", update_result(update)));
    }
    for (package, versions) in &summary.prune_versions {
        lines.push(format!("{marker} prune {package}: {}", versions.join(", ")));
    }
    if lines.is_empty() {
        lines.push("no updates available".to_string());
    }
    lines
}

fn update_result(update: &PackageUpdate) -> String {
    format!(
        "{} ({})",
        update_label(update),
        phoxal_cli_core::session::human::bytes(update.bytes)
    )
}

fn update_label(update: &PackageUpdate) -> String {
    let version = if update.refresh {
        format!("refresh {} (suite digest changed)", update.new)
    } else {
        match update.old.as_deref() {
            Some(old) if old != update.new => format!("{old} -> {}", update.new),
            Some(_) => update.new.clone(),
            None => format!("missing -> {}", update.new),
        }
    };
    format!(
        "{} [{}] {version}",
        update.package,
        update.target.as_deref().unwrap_or("assets")
    )
}

struct UpdateProgress {
    rows: crate::progress::Rows,
    updates: BTreeMap<(String, Option<String>), PackageUpdate>,
}

impl UpdateProgress {
    fn new(interactive: bool, updates: &[PackageUpdate]) -> Self {
        Self {
            rows: crate::progress::Rows::new(interactive),
            updates: updates
                .iter()
                .cloned()
                .map(|update| ((update.package.clone(), update.target.clone()), update))
                .collect(),
        }
    }

    fn update<'a>(&'a self, descriptor: &NativeArtifactDescriptor) -> Option<&'a PackageUpdate> {
        self.updates
            .get(&(descriptor.package_id.clone(), descriptor.target.clone()))
    }
}

impl ArtifactProgressReporter for UpdateProgress {
    fn begin(&self, descriptor: &NativeArtifactDescriptor) -> crate::progress::Row {
        let Some(update) = self.update(descriptor) else {
            return crate::progress::Row::Silent;
        };
        if update.pinned {
            return crate::progress::Row::Silent;
        }
        self.rows.begin(update_label(update))
    }

    fn complete(&self, descriptor: &NativeArtifactDescriptor, row: Option<crate::progress::Row>) {
        let Some(update) = self.update(descriptor) else {
            return;
        };
        if update.pinned {
            return;
        }
        let message = update_result(update);
        if let Some(row) = row {
            row.finish(message);
        } else {
            self.rows.completed(message);
        }
    }

    fn failed(
        &self,
        descriptor: &NativeArtifactDescriptor,
        row: crate::progress::Row,
        error: &anyhow::Error,
    ) {
        let label = self.update(descriptor).map_or_else(
            || {
                format!(
                    "{} [{}] {}",
                    descriptor.package_id,
                    descriptor.target.as_deref().unwrap_or("assets"),
                    descriptor.version
                )
            },
            update_result,
        );
        row.abandon(format!("{label}: {error:#}"));
    }
}

#[cfg(unix)]
// `statvfs` field widths vary by platform (`f_bavail` is u32 on macOS, u64 on
// Linux), so the `u64::from` below is a real widening on some targets and a
// no-op on others; allow the lint rather than pick a cast that only moves it.
#[allow(clippy::useless_conversion)]
fn free_disk_bytes(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is a valid NUL-terminated C string and stats points to writable storage.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: statvfs returned success and initialized stats.
    let stats = unsafe { stats.assume_init() };
    Ok(u64::from(stats.f_bavail).saturating_mul(stats.f_frsize))
}

#[cfg(not(unix))]
fn free_disk_bytes(_path: &Path) -> Result<u64> {
    bail!("free-disk reporting is unavailable on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_paths::test_support::ScratchPhoxalHome;
    use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
    use phoxal_cli_core::project::suite::{
        fixture_blob_for_tests, fixture_service_entry_for_tests, fixture_suite_for_tests,
    };

    #[test]
    fn dry_run_does_not_create_project_state_in_a_clean_project() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let root = crate::host_paths::project_root()?;
        fs::write(
            root.join("robot.yaml"),
            r#"schema: robot/v0
robot:
  id: robot_v1
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components: {}
"#,
        )?;
        fs::write(root.join("structure.urdf"), "<robot/>")?;
        fs::create_dir_all(root.join("src"))?;
        fs::create_dir_all(root.join("train/phoxal/src"))?;
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\nphoxal = { path = \"train/phoxal\" }\n",
        )?;
        fs::write(root.join("src/lib.rs"), "")?;
        fs::write(
            root.join("train/phoxal/Cargo.toml"),
            "[package]\nname = \"phoxal\"\nversion = \"0.36.0\"\nedition = \"2024\"\n",
        )?;
        fs::write(root.join("train/phoxal/src/lib.rs"), "")?;
        anyhow::ensure!(
            std::process::Command::new("cargo")
                .arg("generate-lockfile")
                .current_dir(&root)
                .status()?
                .success(),
            "failed to generate fixture Cargo.lock"
        );
        let suite_path = root.join("suite.json");
        fs::write(
            &suite_path,
            serde_json::to_vec(&fixture_suite_for_tests(Vec::new()))?,
        )?;

        let robot = load_robot(&root.join("robot.yaml"))?;
        let summary = update_locked_train(
            &root,
            &robot,
            Some(suite_path.display().to_string()),
            true,
            &[],
            &crate::Ui::from_env(),
            "0.36.0",
        )?;

        assert!(summary.dry_run);
        assert!(
            !root.join(".phoxal").exists(),
            "dry-run must not create the project state directory"
        );
        Ok(())
    }

    #[test]
    fn update_vendors_requested_target_scopes_and_skips_simulators() -> Result<()> {
        let host = "aarch64-apple-darwin";
        let device = "aarch64-unknown-linux-gnu";

        let mut service = fixture_service_entry_for_tests("drive", "1.2.3", host, true, Vec::new());
        service.artifact.targets.insert(
            device.to_string(),
            fixture_blob_for_tests("https://example.invalid/device", &"b".repeat(64), 42),
        );
        let suite = fixture_suite_for_tests(vec![service]);
        let mut descriptors = vec![
            NativeArtifactDescriptor {
                package_id: "phoxal/service-drive".to_string(),
                kind: phoxal_cli_core::project::suite::ArtifactKind::Service,
                name: "drive".to_string(),
                version: "1.2.3".to_string(),
                url: "https://example.invalid/host".to_string(),
                sha256: "a".repeat(64),
                size: 21,
                binary_name: "phoxal-service-drive".to_string(),
                target: Some(host.to_string()),
            },
            // A simulator artifact must be skipped: native layouts exclude
            // simulators and Webots does not exist for device targets.
            NativeArtifactDescriptor {
                package_id: "phoxal/simulator-webots-controller".to_string(),
                kind: phoxal_cli_core::project::suite::ArtifactKind::Simulator,
                name: "webots-controller".to_string(),
                version: "1.2.3".to_string(),
                url: "https://example.invalid/sim".to_string(),
                sha256: "c".repeat(64),
                size: 7,
                binary_name: "phoxal-simulator-webots-controller".to_string(),
                target: Some(host.to_string()),
            },
        ];

        include_requested_target_scopes(&mut descriptors, &suite, &[device.to_string()])?;

        assert!(
            descriptors.iter().any(|descriptor| {
                descriptor.package_id == "phoxal/service-drive"
                    && descriptor.target.as_deref() == Some(device)
                    && descriptor.url == "https://example.invalid/device"
            }),
            "the requested device blob must be vendored"
        );
        assert!(
            !descriptors.iter().any(|descriptor| {
                descriptor.package_id == "phoxal/simulator-webots-controller"
                    && descriptor.target.as_deref() == Some(device)
            }),
            "simulator artifacts are never vendored for a requested device target"
        );

        // An unsupported triple fails naming the package and supported targets.
        let error = include_requested_target_scopes(
            &mut descriptors,
            &suite,
            &["riscv64gc-unknown-linux-gnu".to_string()],
        )
        .expect_err("an unsupported requested target must fail");
        assert!(format!("{error:#}").contains("phoxal/service-drive"));
        Ok(())
    }

    #[test]
    fn update_includes_existing_target_scopes() -> Result<()> {
        let _root = ScratchPhoxalHome::new()?;
        let host = "aarch64-apple-darwin";
        let robot = "aarch64-unknown-linux-musl";
        let package = "phoxal/service-drive";
        let package_dir = crate::host_paths::artifacts_dir()?
            .join("phoxal")
            .join("service-drive");
        fs::create_dir_all(package_dir.join("versions/1.1.0/targets").join(robot))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink("versions/1.1.0", package_dir.join("active"))?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir("versions/1.1.0", package_dir.join("active"))?;

        let mut entry = fixture_service_entry_for_tests("drive", "1.2.3", host, true, Vec::new());
        entry.artifact.targets.insert(
            robot.to_string(),
            fixture_blob_for_tests("https://example.invalid/robot", &"b".repeat(64), 42),
        );
        let suite = fixture_suite_for_tests(vec![entry]);
        let mut descriptors = vec![NativeArtifactDescriptor {
            package_id: package.to_string(),
            kind: phoxal_cli_core::project::suite::ArtifactKind::Service,
            name: "drive".to_string(),
            version: "1.2.3".to_string(),
            url: "https://example.invalid/host".to_string(),
            sha256: "a".repeat(64),
            size: 21,
            binary_name: "phoxal-service-drive".to_string(),
            target: Some(host.to_string()),
        }];

        include_existing_target_scopes(&mut descriptors, &suite)?;

        let retained = descriptors
            .iter()
            .find(|descriptor| descriptor.target.as_deref() == Some(robot))
            .expect("existing robot target is included in the update");
        assert_eq!(retained.url, "https://example.invalid/robot");
        assert_eq!(retained.size, 42);
        Ok(())
    }

    #[test]
    fn human_update_rows_replace_the_footer_and_hide_pins() {
        let summary = UpdateSummary {
            dry_run: false,
            train: "0.36.0".to_string(),
            destination: PathBuf::from("/ignored"),
            package_count: 3,
            download_bytes: 7_461_785,
            free_disk_bytes: Some(99),
            updates: vec![
                PackageUpdate {
                    package: "phoxal/simulator-webots-controller".to_string(),
                    target: Some("aarch64-apple-darwin".to_string()),
                    old: Some("0.1.7".to_string()),
                    new: "0.1.9".to_string(),
                    bytes: 7_461_785,
                    pinned: false,
                    refresh: false,
                },
                PackageUpdate {
                    package: "phoxal/component-passive_caster".to_string(),
                    target: None,
                    old: Some("0.1.0".to_string()),
                    new: "0.1.0".to_string(),
                    bytes: 123,
                    pinned: true,
                    refresh: false,
                },
                PackageUpdate {
                    package: "phoxal/service-drive".to_string(),
                    target: Some("aarch64-apple-darwin".to_string()),
                    old: Some("0.19.8".to_string()),
                    new: "0.19.8".to_string(),
                    bytes: 7_356_930,
                    pinned: false,
                    refresh: false,
                },
            ],
            prune_versions: BTreeMap::new(),
            pins_skipped: vec!["phoxal/component-passive_caster".to_string()],
        };

        assert_eq!(
            human_lines(&summary, false),
            vec![
                "\u{2713} phoxal/simulator-webots-controller [aarch64-apple-darwin] 0.1.7 -> 0.1.9 (7.1 MiB)"
                    .to_string(),
                "\u{2713} phoxal/service-drive [aarch64-apple-darwin] 0.19.8 (7.0 MiB)"
                    .to_string(),
            ]
        );
        assert!(human_lines(&summary, true).is_empty());

        let mut refresh = summary.clone();
        refresh.updates = vec![PackageUpdate {
            package: "phoxal/service-drive".to_string(),
            target: Some("aarch64-apple-darwin".to_string()),
            old: Some("0.19.8".to_string()),
            new: "0.19.8".to_string(),
            bytes: 7_356_930,
            pinned: false,
            refresh: true,
        }];
        refresh.prune_versions.insert(
            "phoxal/service-drive".to_string(),
            vec!["0.19.7".to_string()],
        );
        assert_eq!(
            human_lines(&refresh, false),
            vec![
                "✓ phoxal/service-drive [aarch64-apple-darwin] refresh 0.19.8 (suite digest changed) (7.0 MiB)".to_string(),
                "✓ prune phoxal/service-drive: 0.19.7".to_string(),
            ]
        );

        let mut all_pinned = summary;
        all_pinned.updates.retain(|update| update.pinned);
        assert_eq!(
            human_lines(&all_pinned, true),
            vec!["no updates available".to_string()]
        );
    }
}
