use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Serialize;

use crate::AppContext;
use crate::commands::MessageFormat;
use crate::native_artifacts::{ArtifactProgressReporter, NativeArtifactDescriptor};
use crate::output_mode::OutputMode;
use crate::resolver::{ResolveOptions, discover_robot_yaml, load_robot_with_extras, resolve};

#[derive(Debug, Args)]
pub struct Update {
    #[arg(
        long,
        help = "Plan and report the update without downloading or changing active versions."
    )]
    pub dry_run: bool,
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageUpdate {
    pub package: String,
    pub target: Option<String>,
    pub old: Option<String>,
    pub new: String,
    pub bytes: u64,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateSummary {
    pub dry_run: bool,
    pub channel: String,
    pub snapshot: String,
    pub destination: PathBuf,
    pub package_count: usize,
    pub download_bytes: u64,
    pub free_disk_bytes: Option<u64>,
    pub updates: Vec<PackageUpdate>,
    pub pins_skipped: Vec<String>,
    pub coherence: &'static str,
    pub config: &'static str,
}

impl Update {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let project_root = app.project.root().to_path_buf();
        let catalog_source = app.catalog_source.clone();
        let dry_run = self.dry_run;
        let ui = app.ui;
        let output_mode = ui.mode();
        let summary = tokio::task::spawn_blocking(move || {
            update(&project_root, catalog_source, dry_run, &ui)
        })
        .await
        .context("update worker failed")??;
        crate::commands::print_message(
            &summary,
            || print_human(&summary, output_mode),
            self.message_format,
        )
    }
}

fn update(
    project_start: &Path,
    catalog_source: Option<String>,
    dry_run: bool,
    ui: &crate::Ui,
) -> Result<UpdateSummary> {
    let robot_path = discover_robot_yaml(project_start)?;
    let project_root = robot_path.parent().context("robot.yaml has no parent")?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let channel = crate::catalog::selection_channel(loaded.robot.artifacts.channel);
    let robot_source = loaded.extras.catalog_source.as_ref().map(|source| {
        if source.is_absolute() {
            source.clone()
        } else {
            project_root.join(source)
        }
    });
    let catalog = crate::catalog::load_pinned_catalog(
        crate::catalog::CatalogLoadOptions {
            cli_source: catalog_source,
            robot_source,
            offline: false,
        },
        channel,
        ui.mode(),
    )?
    .context("update requires a reachable artifact catalog; --offline cannot update")?;
    let resolved = resolve(
        &loaded.robot,
        project_root,
        Some(&catalog),
        ResolveOptions {
            refresh_channel_head: true,
            resolve_source_commits: false,
            resolve_component_asset_commits: false,
            ..ResolveOptions::default()
        },
    )?;

    let destination = project_root.join(".phoxal/artifacts");
    let mut descriptors = crate::native_artifacts::descriptors(&resolved)?;
    include_existing_target_scopes(&mut descriptors, &catalog)?;
    let updates = descriptors
        .iter()
        .map(|descriptor| PackageUpdate {
            package: descriptor.package_id.clone(),
            target: descriptor.target.clone(),
            old: crate::native_artifacts::active_version(descriptor)
                .ok()
                .flatten(),
            new: descriptor.version.clone(),
            bytes: descriptor.size,
            pinned: loaded
                .robot
                .artifacts
                .pins
                .contains_key(&descriptor.package_id),
        })
        .collect::<Vec<_>>();
    let download_bytes = updates
        .iter()
        .filter(|update| update.old.as_deref() != Some(update.new.as_str()))
        .map(|update| update.bytes)
        .sum();
    let free_disk_bytes = free_disk_bytes(project_root).ok();
    if let Some(free) = free_disk_bytes
        && download_bytes > free
    {
        bail!(
            "artifact update needs {} but only {} are free at {}",
            crate::human::bytes(download_bytes),
            crate::human::bytes(free),
            destination.display()
        );
    }

    let pins_skipped = loaded
        .robot
        .artifacts
        .pins
        .keys()
        .cloned()
        .collect::<Vec<_>>();

    if !dry_run {
        let _lock = crate::native_artifacts::ArtifactStoreLock::exclusive("update")?;
        fs::create_dir_all(&destination)?;
        let progress = UpdateProgress::new(ui.mode(), &updates);
        crate::native_artifacts::prepare_and_activate_descriptors_with_progress(
            &descriptors,
            &progress,
        )?;
    }

    Ok(UpdateSummary {
        dry_run,
        channel: channel.to_string(),
        snapshot: catalog.build.tag,
        destination,
        package_count: descriptors.len(),
        download_bytes,
        free_disk_bytes,
        updates,
        pins_skipped,
        coherence: "deferred to W6",
        config: "deferred to W7",
    })
}

fn include_existing_target_scopes(
    descriptors: &mut Vec<crate::native_artifacts::NativeArtifactDescriptor>,
    catalog: &crate::catalog::Catalog,
) -> Result<()> {
    let current = descriptors.clone();
    for descriptor in current {
        if descriptor.target.is_none() {
            continue;
        }
        let artifact = catalog
            .artifacts
            .iter()
            .find(|artifact| {
                artifact.package == descriptor.package_id && artifact.version == descriptor.version
            })
            .with_context(|| {
                format!(
                    "resolved {} {} is absent from snapshot {}",
                    descriptor.package_id, descriptor.version, catalog.build.tag
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
                    "snapshot {} has no {} blob for retained target {target}; switch channel or update phoxal-cli",
                    catalog.build.tag, descriptor.package_id
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

fn print_human(summary: &UpdateSummary, output_mode: OutputMode) -> Result<()> {
    for line in human_lines(summary, output_mode) {
        println!("{line}");
    }
    Ok(())
}

fn human_lines(summary: &UpdateSummary, output_mode: OutputMode) -> Vec<String> {
    if output_mode == OutputMode::Rich && !summary.dry_run {
        return if summary.updates.iter().any(|update| !update.pinned) {
            Vec::new()
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
    if lines.is_empty() {
        lines.push("no updates available".to_string());
    }
    lines
}

fn update_result(update: &PackageUpdate) -> String {
    format!(
        "{} ({})",
        update_label(update),
        crate::human::bytes(update.bytes)
    )
}

fn update_label(update: &PackageUpdate) -> String {
    let version = match update.old.as_deref() {
        Some(old) if old != update.new => format!("{old} -> {}", update.new),
        Some(_) => update.new.clone(),
        None => format!("missing -> {}", update.new),
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
    fn new(mode: OutputMode, updates: &[PackageUpdate]) -> Self {
        Self {
            rows: crate::progress::Rows::new(mode),
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
        self.rows.bytes(update_label(update), descriptor.size)
    }

    fn complete(&self, descriptor: &NativeArtifactDescriptor, row: Option<crate::progress::Row>) {
        let Some(update) = self.update(descriptor) else {
            if let Some(row) = row {
                row.clear();
            }
            return;
        };
        if update.pinned {
            if let Some(row) = row {
                row.clear();
            }
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
    use crate::catalog::{
        SelectionChannel, fixture_blob_for_tests, fixture_catalog_for_tests,
        fixture_service_entry_for_tests,
    };
    use crate::host_paths::test_support::ScratchPhoxalHome;
    use crate::native_artifacts::NativeArtifactDescriptor;

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

        let mut entry = fixture_service_entry_for_tests(
            "drive",
            "1.2.3",
            SelectionChannel::Stable,
            host,
            true,
            Vec::new(),
        );
        entry.artifact.targets.insert(
            robot.to_string(),
            fixture_blob_for_tests("https://example.invalid/robot", &"b".repeat(64), 42),
        );
        let catalog = fixture_catalog_for_tests(vec![entry]);
        let mut descriptors = vec![NativeArtifactDescriptor {
            package_id: package.to_string(),
            kind: crate::catalog::ArtifactKind::Service,
            name: "drive".to_string(),
            version: "1.2.3".to_string(),
            url: "https://example.invalid/host".to_string(),
            sha256: "a".repeat(64),
            size: 21,
            binary_name: "phoxal-service-drive".to_string(),
            target: Some(host.to_string()),
        }];

        include_existing_target_scopes(&mut descriptors, &catalog)?;

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
            channel: "stable".to_string(),
            snapshot: "build-ignored".to_string(),
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
                },
                PackageUpdate {
                    package: "phoxal/component-passive_caster".to_string(),
                    target: None,
                    old: Some("0.1.0".to_string()),
                    new: "0.1.0".to_string(),
                    bytes: 123,
                    pinned: true,
                },
                PackageUpdate {
                    package: "phoxal/service-drive".to_string(),
                    target: Some("aarch64-apple-darwin".to_string()),
                    old: Some("0.19.8".to_string()),
                    new: "0.19.8".to_string(),
                    bytes: 7_356_930,
                    pinned: false,
                },
            ],
            pins_skipped: vec!["phoxal/component-passive_caster".to_string()],
            coherence: "deferred to W6",
            config: "deferred to W7",
        };

        assert_eq!(
            human_lines(&summary, OutputMode::Plain),
            vec![
                "\u{2713} phoxal/simulator-webots-controller [aarch64-apple-darwin] 0.1.7 -> 0.1.9 (7.1 MiB)"
                    .to_string(),
                "\u{2713} phoxal/service-drive [aarch64-apple-darwin] 0.19.8 (7.0 MiB)"
                    .to_string(),
            ]
        );
        assert!(human_lines(&summary, OutputMode::Rich).is_empty());

        let mut all_pinned = summary;
        all_pinned.updates.retain(|update| update.pinned);
        assert_eq!(
            human_lines(&all_pinned, OutputMode::Rich),
            vec!["no updates available".to_string()]
        );
    }
}
