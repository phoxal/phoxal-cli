use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Serialize;

use crate::AppContext;
use crate::commands::MessageFormat;
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
    pub target: String,
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
    pub retained_versions: usize,
    pub pruned_versions: usize,
    pub coherence: &'static str,
    pub config: &'static str,
}

impl Update {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let project_root = app.project.root().to_path_buf();
        let catalog_source = app.catalog_source.clone();
        let dry_run = self.dry_run;
        let ui = app.ui;
        let summary = tokio::task::spawn_blocking(move || {
            update(&project_root, catalog_source, dry_run, &ui)
        })
        .await
        .context("update worker failed")??;
        crate::commands::print_message(&summary, || print_human(&summary), self.message_format)
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

    let destination = project_root.join(".phoxal/binaries");
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
            "artifact update needs {download_bytes} bytes but only {free} bytes are free at {}",
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

    let (retained_versions, pruned_versions) = if dry_run {
        crate::native_artifacts::preview_prune_inactive_versions(&descriptors)?
    } else {
        let _lock = crate::native_artifacts::ArtifactStoreLock::exclusive("update")?;
        fs::create_dir_all(&destination)?;
        ui.info(format!(
            "downloading {} package target(s), {} bytes, into {}",
            descriptors.len(),
            download_bytes,
            destination.display()
        ));
        crate::native_artifacts::prepare_and_activate_descriptors(&descriptors, Some(ui))?;
        crate::native_artifacts::prune_inactive_versions(&descriptors)?
    };

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
        retained_versions,
        pruned_versions,
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
        if descriptor.target == crate::catalog::TARGET_INDEPENDENT_SCOPE {
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
                existing.package_id == descriptor.package_id && existing.target == target
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
            retained_target.target = target;
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

fn print_human(summary: &UpdateSummary) -> Result<()> {
    println!(
        "update {} channel at {} ({} package target(s), {} bytes)",
        summary.channel, summary.snapshot, summary.package_count, summary.download_bytes
    );
    println!("destination: {}", summary.destination.display());
    if let Some(free) = summary.free_disk_bytes {
        println!("free disk: {free} bytes");
    }
    let mut changed = false;
    for update in &summary.updates {
        if update.pinned {
            continue;
        }
        if update.old.as_deref() != Some(update.new.as_str()) {
            changed = true;
            println!(
                "  {} [{}]: {} -> {} ({} bytes)",
                update.package,
                update.target,
                update.old.as_deref().unwrap_or("missing"),
                update.new,
                update.bytes
            );
        }
    }
    for package in &summary.pins_skipped {
        println!("  {package}: explicit pin skipped by channel update");
    }
    if !changed {
        println!("no updates available");
    }
    println!(
        "retained {} version(s), pruned {}{}",
        summary.retained_versions,
        summary.pruned_versions,
        if summary.dry_run { " (dry run)" } else { "" }
    );
    Ok(())
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
        fs::create_dir_all(crate::native_artifacts::artifact_target_dir_for(
            package, robot,
        )?)?;

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
            target: host.to_string(),
        }];

        include_existing_target_scopes(&mut descriptors, &catalog)?;

        let retained = descriptors
            .iter()
            .find(|descriptor| descriptor.target == robot)
            .expect("existing robot target is included in the update");
        assert_eq!(retained.url, "https://example.invalid/robot");
        assert_eq!(retained.size, 42);
        Ok(())
    }
}
