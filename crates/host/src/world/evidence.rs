//! Typed terminal evidence reads and bounded retention.

use super::*;

trait ValidateWorldMemberEvidence {
    fn validate(&self, expected_execution: ExecutionId) -> Result<()>;
}

impl ValidateWorldMemberEvidence for WorldMemberEvidence {
    fn validate(&self, expected_execution: ExecutionId) -> Result<()> {
        self.validate_structure(expected_execution)?;
        for evidence in &self.terminal.evidence_paths {
            validate_relative_evidence_path(evidence)?;
        }
        Ok(())
    }
}

trait ValidateWorldTerminalSummary {
    fn validate(&self, expected_instance: &str) -> Result<()>;
}

impl ValidateWorldTerminalSummary for WorldTerminalSummary {
    fn validate(&self, expected_instance: &str) -> Result<()> {
        let expected = parse_instance_id(expected_instance)?;
        self.validate_structure(expected)?;
        for member in &self.member_evidence {
            validate_relative_evidence_path(&member.path)?;
        }
        for evidence in &self.evidence {
            validate_relative_evidence_path(evidence)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PruneReport {
    pub removed: Vec<String>,
    pub bootstrap_logs_removed: Vec<PathBuf>,
    pub incomplete: Vec<PathBuf>,
}

/// Typed terminal evidence reads and count-bounded retention.
#[derive(Clone, Debug)]
pub struct WorldEvidence {
    paths: WorldPaths,
}

impl WorldEvidence {
    #[must_use]
    pub const fn new(paths: WorldPaths) -> Self {
        Self { paths }
    }

    pub fn read_summary(&self, instance: &str) -> Result<Option<WorldTerminalSummary>> {
        validate_instance_id(instance)?;
        let path = self.paths.evidence_path(instance).join("summary.json");
        let Some(document) = read_owner_file_if_present(&path)? else {
            return Ok(None);
        };
        let summary: WorldTerminalSummary =
            serde_json::from_slice(&document).with_context(|| {
                format!("failed to parse terminal world summary {}", path.display())
            })?;
        summary.validate(instance)?;
        Ok(Some(summary))
    }

    pub fn read_checkpoint(&self, instance: &str) -> Result<Option<WorldCheckpoint>> {
        validate_instance_id(instance)?;
        let path = self.paths.evidence_path(instance).join("checkpoint.json");
        let Some(document) = read_owner_file_if_present(&path)? else {
            return Ok(None);
        };
        let checkpoint: WorldCheckpoint = serde_json::from_slice(&document)
            .with_context(|| format!("failed to parse world checkpoint {}", path.display()))?;
        Ok(Some(checkpoint))
    }

    pub fn read_member_evidence(
        &self,
        summary: &WorldTerminalSummary,
    ) -> Result<Vec<WorldMemberEvidence>> {
        let instance = summary.instance.to_string();
        summary.validate(&instance)?;
        let root = self.paths.evidence_path(&instance);
        let mut members = Vec::new();
        for indexed in &summary.member_evidence {
            let path = root.join(&indexed.path);
            let document = read_owner_file(&path)?;
            let member: WorldMemberEvidence = serde_json::from_slice(&document)
                .with_context(|| format!("failed to parse member evidence {}", path.display()))?;
            member.validate(indexed.execution)?;
            member
                .terminal
                .last_progress
                .validate(summary.provenance.time_step_ns)
                .with_context(|| {
                    format!(
                        "member {} progress disagrees with retained provenance",
                        member.terminal.execution
                    )
                })?;
            members.push(member);
        }
        members.sort_by_key(|member| member.terminal.execution.to_string());
        Ok(members)
    }

    /// Read one typed member-terminal record while its world may remain live.
    pub fn read_member_terminal(
        &self,
        instance: &str,
        execution: ExecutionId,
    ) -> Result<Option<WorldMemberEvidence>> {
        validate_instance_id(instance)?;
        let path = self
            .paths
            .evidence_path(instance)
            .join("members")
            .join(format!("{execution}.json"));
        let Some(document) = read_owner_file_if_present(&path)? else {
            return Ok(None);
        };
        let member: WorldMemberEvidence = serde_json::from_slice(&document)
            .with_context(|| format!("failed to parse member evidence {}", path.display()))?;
        member.validate(execution)?;
        Ok(Some(member))
    }

    pub(super) fn discover_member_evidence(
        &self,
        checkpoint: &WorldCheckpoint,
    ) -> Result<Vec<WorldMemberEvidenceIndex>> {
        let instance = checkpoint.state.instance.to_string();
        let directory = self.paths.evidence_path(&instance).join("members");
        validate_owner_directory(&directory)?;
        let mut members = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if stem.ends_with(".actuation") {
                continue;
            }
            let execution = ExecutionId::parse(stem)
                .with_context(|| format!("invalid member evidence filename `{name}`"))?;
            let path = entry.path();
            let document = read_owner_file(&path)?;
            let record: WorldMemberEvidence = serde_json::from_slice(&document)
                .with_context(|| format!("failed to parse member evidence {}", path.display()))?;
            record.validate(execution)?;
            record
                .terminal
                .last_progress
                .validate(checkpoint.state.provenance.time_step_ns)
                .with_context(|| {
                    format!(
                        "member {} progress disagrees with checkpoint provenance",
                        record.terminal.execution
                    )
                })?;
            members.push(WorldMemberEvidenceIndex {
                execution,
                path: format!("members/{execution}.json"),
            });
        }
        members.sort_by_key(|member| member.execution.to_string());
        Ok(members)
    }

    pub(super) fn recovery_logs(&self, instance: &str) -> Result<(Vec<String>, Vec<String>)> {
        validate_instance_id(instance)?;
        let root = self.paths.evidence_path(instance);
        validate_owner_directory(&root)?;
        let per_log_limit = (DEFAULT_LOG_BYTE_LIMIT / 2).max(1);
        let mut retained = Vec::new();
        let mut truncated = Vec::new();
        for name in ["host.log", "webots.log"] {
            let path = root.join(name);
            let Some((file, _)) = open_and_read_owner_file_if_present(&path)? else {
                continue;
            };
            if file.metadata()?.len() >= per_log_limit {
                truncated.push(name.to_owned());
            }
            retained.push(name.to_owned());
        }
        Ok((retained, truncated))
    }

    pub(super) fn publish_recovered_summary(
        &self,
        instance: &str,
        summary: &WorldTerminalSummary,
    ) -> Result<WorldTerminalSummary> {
        summary.validate(instance)?;
        let root = self.paths.evidence_path(instance);
        validate_owner_directory(&root)?;
        let path = root.join("summary.json");
        match atomic_owner_json_if_absent(&path, summary)? {
            AtomicPublish::Published => Ok(summary.clone()),
            AtomicPublish::AlreadyExists => self
                .read_summary(instance)?
                .context("terminal summary appeared during recovery but could not be read"),
        }
    }

    pub fn list_summaries(&self) -> Result<Vec<WorldTerminalSummary>> {
        let mut summaries = Vec::new();
        for directory in fs::read_dir(self.paths.evidence())? {
            let directory = directory?;
            let Some(instance) = directory.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_instance_id(&instance).is_err() {
                continue;
            }
            if let Some(summary) = self.read_summary(&instance)? {
                summaries.push(summary);
            }
        }
        summaries.sort_by(|left, right| {
            left.ended_at_unix_ms
                .cmp(&right.ended_at_unix_ms)
                .then_with(|| left.instance.to_string().cmp(&right.instance.to_string()))
        });
        Ok(summaries)
    }

    pub fn read_logs(&self, summary: &WorldTerminalSummary) -> Result<Vec<(String, Vec<u8>)>> {
        let instance = summary.instance.to_string();
        summary.validate(&instance)?;
        let root = self.paths.evidence_path(&instance);
        let mut logs = Vec::new();
        for relative in &summary.evidence {
            validate_relative_evidence_path(relative)?;
            let path = root.join(relative);
            let document = read_owner_file(&path)?;
            logs.push((relative.clone(), document));
        }
        Ok(logs)
    }

    /// Read conventional retained files for a live session whose terminal
    /// summary has not been written yet.
    pub fn read_live_logs(&self, instance: &str) -> Result<Vec<(String, Vec<u8>)>> {
        validate_instance_id(instance)?;
        let root = self.paths.evidence_path(instance);
        let mut logs = Vec::new();
        for name in ["host.log", "webots.log"] {
            let path = root.join(name);
            if let Some(document) = read_owner_file_if_present(&path)? {
                logs.push((name.to_string(), document));
            }
        }
        Ok(logs)
    }

    /// Keep at most `limit` complete terminal sessions. Live instances and
    /// incomplete evidence directories are never candidates.
    pub fn prune(&self, limit: usize, live_instances: &BTreeSet<String>) -> Result<PruneReport> {
        let mut candidates = Vec::new();
        let mut report = PruneReport::default();
        for directory in fs::read_dir(self.paths.evidence())? {
            let directory = directory?;
            let path = directory.path();
            let Some(instance) = directory.file_name().to_str().map(str::to_owned) else {
                report.incomplete.push(path);
                continue;
            };
            if validate_instance_id(&instance).is_err() {
                if stale_bootstrap_log(&directory, SystemTime::now())? {
                    fs::remove_file(&path).with_context(|| {
                        format!("failed to remove stale bootstrap log {}", path.display())
                    })?;
                    report.bootstrap_logs_removed.push(path);
                }
                continue;
            }
            if live_instances.contains(&instance) {
                continue;
            }
            match self.read_summary(&instance) {
                Ok(Some(summary)) => {
                    candidates.push((summary.ended_at_unix_ms, instance, path));
                }
                Ok(None) | Err(_) => report.incomplete.push(path),
            }
        }
        candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        let remove_count = candidates.len().saturating_sub(limit);
        for (_, instance, path) in candidates.into_iter().take(remove_count) {
            ensure!(
                path.parent() == Some(self.paths.evidence()),
                "refusing to prune evidence outside {}",
                self.paths.evidence().display()
            );
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to prune terminal evidence {}", path.display()))?;
            report.removed.push(instance);
        }
        Ok(report)
    }
}

fn stale_bootstrap_log(entry: &fs::DirEntry, now: SystemTime) -> Result<bool> {
    let name = entry.file_name();
    let Some(name) = name.to_str() else {
        return Ok(false);
    };
    let Some(random) = name
        .strip_prefix(".starting-")
        .and_then(|name| name.strip_suffix(".host.log"))
    else {
        return Ok(false);
    };
    if random.len() != 6 || !random.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Ok(false);
    }
    if !entry.file_type()?.is_file() {
        return Ok(false);
    }
    let metadata = entry.metadata()?;
    Ok(metadata.modified().ok().is_some_and(|modified| {
        now.duration_since(modified)
            .is_ok_and(|age| age >= STALE_BOOTSTRAP_LOG_AGE)
    }))
}
