use std::fs;
use std::path::Path;
use std::process::{Child, Command};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default)]
pub struct SpawnedProcesses {
    children: Vec<(String, Child)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessState {
    pub pid: u32,
    pub label: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StateFile {
    pub processes: Vec<ProcessState>,
}

impl SpawnedProcesses {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn(&mut self, label: impl Into<String>, command: &mut Command) -> Result<()> {
        let label = label.into();
        let child = command
            .spawn()
            .with_context(|| format!("failed to spawn {label}"))?;
        self.children.push((label, child));
        Ok(())
    }

    #[must_use]
    pub fn state(&self) -> StateFile {
        StateFile {
            processes: self
                .children
                .iter()
                .map(|(label, child)| ProcessState {
                    pid: child.id(),
                    label: label.clone(),
                })
                .collect(),
        }
    }

    pub fn write_state(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(path, serde_yaml::to_string(&self.state())?)
            .with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn kill_reverse(&mut self) {
        for (_, child) in self.children.iter_mut().rev() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for SpawnedProcesses {
    fn drop(&mut self) {
        self.kill_reverse();
    }
}
