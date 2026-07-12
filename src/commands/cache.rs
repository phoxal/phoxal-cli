use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
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
    #[command(about = "Clean selected project-local .phoxal state.")]
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
    #[arg(long)]
    pub artifacts: bool,
    #[arg(long)]
    pub build: bool,
    #[arg(long)]
    pub git: bool,
    #[arg(long)]
    pub webots: bool,
    #[arg(long, conflicts_with_all = ["artifacts", "build", "git", "webots"])]
    pub all: bool,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    pub message_format: MessageFormat,
}

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
}

impl Clean {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let state = app.project.root().join(".phoxal");
        let scopes = self.scopes()?;
        let dry_run = self.dry_run;
        let summary = tokio::task::spawn_blocking(move || {
            let _artifact_lock = scopes
                .contains(&"artifacts")
                .then(|| {
                    if dry_run {
                        crate::native_artifacts::ArtifactStoreLock::shared()
                    } else {
                        crate::native_artifacts::ArtifactStoreLock::exclusive("cache clean")
                    }
                })
                .transpose()?;
            clean(&state, &scopes, dry_run)
        })
        .await
        .context("cache clean worker failed")??;
        crate::commands::print_message(
            &summary,
            || {
                if summary.entries.is_empty() {
                    println!("selected project state is already clean");
                } else {
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
                    println!(
                        "{} {} total{}",
                        if summary.dry_run {
                            "would free"
                        } else {
                            "freed"
                        },
                        format_bytes(summary.total_bytes),
                        if summary.dry_run { " (dry run)" } else { "" }
                    );
                }
                Ok(())
            },
            self.message_format,
        )
    }

    fn scopes(&self) -> Result<Vec<&'static str>> {
        let mut scopes = Vec::new();
        if self.all || self.artifacts {
            scopes.push("artifacts");
        }
        if self.all || self.build {
            scopes.push("build");
        }
        if self.all || self.git {
            scopes.push("git");
        }
        if self.all || self.webots {
            scopes.push("webots");
        }
        ensure!(
            !scopes.is_empty(),
            "choose a clean scope: --artifacts, --build, --git, --webots, or --all"
        );
        Ok(scopes)
    }
}

fn clean(state: &Path, scopes: &[&str], dry_run: bool) -> Result<CleanSummary> {
    let mut entries = Vec::new();
    let mut total_bytes = 0;
    for scope in scopes {
        let path = state.join(scope);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
            }
        }
        let bytes = dir_size(&path)?;
        if !dry_run {
            if *scope == "artifacts" {
                for child in fs::read_dir(&path)? {
                    let child = child?;
                    if child.file_name() != ".lock" {
                        remove_path(&child.path())?;
                    }
                }
            } else {
                remove_path(&path)?;
            }
        }
        total_bytes += bytes;
        entries.push(CleanedEntry { path, bytes });
    }
    Ok(CleanSummary {
        dry_run,
        entries,
        total_bytes,
    })
}

fn dir_size(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0;
    for entry in fs::read_dir(path)? {
        total += dir_size(&entry?.path())?;
    }
    Ok(total)
}

fn remove_path(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}
