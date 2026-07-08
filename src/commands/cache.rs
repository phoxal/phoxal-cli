use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::AppContext;
use crate::commands::MessageFormat;

#[derive(Debug, Args)]
pub struct CacheCmd {
    #[command(subcommand)]
    pub command: CacheSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum CacheSubcommand {
    #[command(about = "Clear the reusable native-artifact/git/deploy caches under cache/.")]
    Clean(Clean),
}

impl CacheCmd {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            CacheSubcommand::Clean(command) => command.run(app).await,
        }
    }
}

#[derive(Debug, Args)]
pub struct Clean {
    #[arg(
        long,
        help = "Report what would be removed and how much space would be freed, without deleting anything."
    )]
    pub dry_run: bool,
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the clean summary."
    )]
    pub message_format: MessageFormat,
}

/// One cache entry `clean` clears - either a reusable subdir of `cache/`
/// (`artifacts/`, `git-artifacts/`, `deploy/`) or a legacy path from an older
/// `phoxal-cli` version that must not linger (`emit-apis/`, `components/`,
/// `catalog/`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanedEntry {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanSummary {
    pub dry_run: bool,
    pub entries: Vec<CleanedEntry>,
    pub total_bytes: u64,
    pub local_manifest_entries_pruned: usize,
}

impl Clean {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let dry_run = self.dry_run;
        let summary = tokio::task::spawn_blocking(move || run(dry_run))
            .await
            .context("cache clean worker failed")??;
        let _ = app;
        crate::commands::print_message(
            &summary,
            || {
                if summary.entries.is_empty() {
                    println!("cache is already clean; nothing to remove");
                    return Ok(());
                }
                let verb = if summary.dry_run {
                    "would remove"
                } else {
                    "removed"
                };
                for entry in &summary.entries {
                    println!(
                        "{verb} {} ({})",
                        entry.path.display(),
                        format_bytes(entry.bytes)
                    );
                }
                let total_verb = if summary.dry_run {
                    "would free"
                } else {
                    "freed"
                };
                println!(
                    "{total_verb} {} total{}",
                    format_bytes(summary.total_bytes),
                    if summary.dry_run { " (dry run)" } else { "" }
                );
                if summary.local_manifest_entries_pruned > 0 {
                    println!(
                        "{} the local download manifest of {} entr{}",
                        if summary.dry_run {
                            "would prune"
                        } else {
                            "pruned"
                        },
                        summary.local_manifest_entries_pruned,
                        if summary.local_manifest_entries_pruned == 1 {
                            "y"
                        } else {
                            "ies"
                        }
                    );
                }
                Ok(())
            },
            self.message_format,
        )
    }
}

/// The reusable caches `clean` clears, plus legacy paths from older
/// `phoxal-cli` versions that must never linger (docs: no `cache/components/`,
/// `cache/catalog/`, `cache/emit-apis/`, per-package artifact dirs, or
/// `_assets` mirror). `run/` is never touched - it holds supervisor lock/state
/// and per-play simulation staging, not reusable cache content.
fn candidate_paths() -> Result<Vec<PathBuf>> {
    let cache_dir = crate::host_paths::cache_dir()?;
    Ok(vec![
        crate::host_paths::artifacts_dir()?,
        crate::host_paths::git_artifacts_dir()?,
        crate::host_paths::deploy_dir()?,
        // Legacy paths from before this cache restructure - clean sweeps them
        // out if an old install left them behind.
        cache_dir.join("emit-apis"),
        cache_dir.join("components"),
        cache_dir.join("catalog"),
    ])
}

fn run(dry_run: bool) -> Result<CleanSummary> {
    let cache_root = crate::host_paths::cache_dir()?;
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;

    for path in candidate_paths()? {
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
            }
        }
        assert_within_cache(&cache_root, &path)?;
        let bytes =
            dir_size(&path).with_context(|| format!("failed to measure {}", path.display()))?;
        if !dry_run {
            remove_cache_path(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        total_bytes += bytes;
        entries.push(CleanedEntry { path, bytes });
    }

    // A real clean always empties `cache/artifacts/` entirely, so every
    // current local-manifest entry would lose its tarball and get dropped;
    // dry-run previews that count without writing anything.
    let local_manifest_entries_pruned = if dry_run {
        crate::catalog::local_manifest_entry_count()?
    } else {
        crate::catalog::prune_local_manifest()?
    };

    Ok(CleanSummary {
        dry_run,
        entries,
        total_bytes,
        local_manifest_entries_pruned,
    })
}

/// Defensive guard: every path `clean` touches must be a child of
/// `~/.phoxal/cache` (never `run/`, never anything outside `PHOXAL_HOME`).
fn assert_within_cache(cache_root: &Path, path: &Path) -> Result<()> {
    let cache_root = cache_root
        .canonicalize()
        .with_context(|| format!("failed to resolve cache root {}", cache_root.display()))?;
    let parent = path
        .parent()
        .context("cache clean candidate did not have a parent directory")?
        .canonicalize()
        .with_context(|| format!("failed to resolve parent of {}", path.display()))?;
    anyhow::ensure!(
        parent == cache_root,
        "refusing to clean {} - it is not directly under the cache root {}",
        path.display(),
        cache_root.display()
    );
    Ok(())
}

fn dir_size(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(&entry_path)?;
        if metadata.is_dir() {
            total += dir_size(&entry_path)?;
        } else {
            total += metadata.len();
        }
    }
    Ok(total)
}

fn remove_cache_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_paths::test_support::ScratchPhoxalHome;

    #[test]
    fn clean_removes_reusable_caches_but_leaves_run_alone() -> Result<()> {
        let _guard = ScratchPhoxalHome::new()?;
        let artifacts_dir = crate::host_paths::artifacts_dir()?;
        let git_dir = crate::host_paths::git_artifacts_dir()?;
        let deploy_dir = crate::host_paths::deploy_dir()?;
        let run_dir = crate::host_paths::run_dir()?;
        fs::create_dir_all(&artifacts_dir)?;
        fs::write(
            artifacts_dir.join("phoxal-service-drive-v0.1.0.tar.zst"),
            b"tarball",
        )?;
        fs::create_dir_all(git_dir.join("abc123"))?;
        fs::write(git_dir.join("abc123/marker"), b"x")?;
        fs::create_dir_all(&deploy_dir)?;
        fs::write(deploy_dir.join("marker"), b"x")?;
        fs::create_dir_all(&run_dir)?;
        fs::write(run_dir.join("supervisor.state"), b"keep me")?;

        let summary = run(false)?;

        assert!(!summary.dry_run);
        assert!(!artifacts_dir.exists());
        assert!(!git_dir.exists());
        assert!(!deploy_dir.exists());
        assert!(
            run_dir.join("supervisor.state").is_file(),
            "run/ must be untouched"
        );
        assert!(summary.total_bytes > 0);
        assert_eq!(summary.entries.len(), 3);
        Ok(())
    }

    #[test]
    fn dry_run_reports_without_deleting() -> Result<()> {
        let _guard = ScratchPhoxalHome::new()?;
        let artifacts_dir = crate::host_paths::artifacts_dir()?;
        fs::create_dir_all(&artifacts_dir)?;
        fs::write(artifacts_dir.join("tarball.tar.zst"), b"12345")?;

        let summary = run(true)?;

        assert!(summary.dry_run);
        assert!(artifacts_dir.exists(), "dry run must not delete anything");
        assert_eq!(summary.total_bytes, 5);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn clean_removes_cache_symlink_without_following_it() -> Result<()> {
        let _guard = ScratchPhoxalHome::new()?;
        let outside = tempfile::tempdir()?;
        let outside_marker = outside.path().join("keep");
        fs::write(&outside_marker, b"outside")?;
        let artifacts_dir = crate::host_paths::artifacts_dir()?;
        fs::create_dir_all(artifacts_dir.parent().expect("artifacts parent"))?;
        std::os::unix::fs::symlink(outside.path(), &artifacts_dir)?;

        let summary = run(false)?;

        assert!(
            !artifacts_dir.exists(),
            "the cache symlink itself is removed"
        );
        assert!(
            outside_marker.is_file(),
            "cache clean must not follow a symlink outside the cache"
        );
        assert_eq!(summary.entries.len(), 1);
        Ok(())
    }

    #[test]
    fn clean_prunes_local_manifest_entries_whose_tarballs_are_gone() -> Result<()> {
        let _guard = ScratchPhoxalHome::new()?;
        let artifacts_dir = crate::host_paths::artifacts_dir()?;
        fs::create_dir_all(&artifacts_dir)?;
        let tarball_name = "phoxal-service-drive-v0.1.0-aarch64-unknown-linux-gnu.tar.zst";
        fs::write(artifacts_dir.join(tarball_name), b"tarball")?;
        crate::catalog::upsert_local_manifest_entry(crate::catalog::LocalManifestUpsert {
            kind: crate::catalog::ArtifactKind::Service,
            package: "phoxal/service-drive".to_string(),
            version: "0.1.0".to_string(),
            generation: "y2026_1".to_string(),
            contracts: Vec::new(),
            config_schema: None,
            bus_abi: Some("phoxal-bus/v0".to_string()),
            changed_contracts: Vec::new(),
            channel: crate::catalog::Channel::Stable,
            target: "aarch64-unknown-linux-gnu".to_string(),
            tarball: tarball_name.to_string(),
            sha256: "0".repeat(64),
        })?;
        let before = crate::catalog::load_local_manifest()?;
        assert_eq!(before.services.len(), 1);

        let summary = run(false)?;
        assert_eq!(summary.local_manifest_entries_pruned, 1);

        let after = crate::catalog::load_local_manifest()?;
        assert!(after.services.is_empty());
        Ok(())
    }

    #[test]
    fn format_bytes_reads_in_human_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
    }
}
