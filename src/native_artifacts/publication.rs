//! Prepared-scope publication and rollback guards.

use super::PreparedScope;
use anyhow::Context;
use anyhow::Result;
use std::fs;
use std::path::PathBuf;

pub(crate) struct PublishedScope {
    final_root: PathBuf,
    backup_root: Option<PathBuf>,
}

pub(crate) struct ScopePublication {
    scopes: Vec<PublishedScope>,
    committed: bool,
}

impl ScopePublication {
    pub(crate) fn publish(prepared: Vec<PreparedScope>) -> Result<Self> {
        let mut publication = Self {
            scopes: Vec::new(),
            committed: false,
        };
        for mut prepared_scope in prepared {
            let candidate = prepared_scope
                .candidate_root
                .take()
                .context("prepared artifact scope lost its candidate")?;
            let parent = prepared_scope
                .final_root
                .parent()
                .context("artifact scope has no parent")?;
            let backup_root = if prepared_scope.final_root.exists() {
                let reserved = tempfile::Builder::new()
                    .prefix(".scope-rollback-")
                    .tempdir_in(parent)?
                    .keep();
                fs::remove_dir_all(&reserved)?;
                fs::rename(&prepared_scope.final_root, &reserved)?;
                Some(reserved)
            } else {
                None
            };
            if let Err(error) = fs::rename(&candidate, &prepared_scope.final_root) {
                if let Some(backup) = &backup_root {
                    let _ = fs::rename(backup, &prepared_scope.final_root);
                }
                let _ = fs::remove_dir_all(candidate);
                return Err(error).with_context(|| {
                    format!(
                        "failed to publish prepared artifact scope {}",
                        prepared_scope.final_root.display()
                    )
                });
            }
            publication.scopes.push(PublishedScope {
                final_root: std::mem::take(&mut prepared_scope.final_root),
                backup_root,
            });
        }
        Ok(publication)
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        self.committed = true;
        for scope in &mut self.scopes {
            if let Some(backup) = scope.backup_root.take() {
                fs::remove_dir_all(&backup).with_context(|| {
                    format!("failed to remove artifact rollback {}", backup.display())
                })?;
            }
        }
        Ok(())
    }
}

impl Drop for ScopePublication {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for scope in self.scopes.iter_mut().rev() {
            let _ = fs::remove_dir_all(&scope.final_root);
            if let Some(backup) = scope.backup_root.take() {
                let _ = fs::rename(backup, &scope.final_root);
            }
        }
    }
}
