//! Evidence-aware recovery of stale world hosts and native process groups.

use super::*;

impl<I: ProcessInspector> WorldRegistry<I> {
    /// Finalize one host-loss orphan from its last durable checkpoint. A live
    /// registration is never changed. The stale lease remains exclusively
    /// held until a complete terminal summary exists and the exact stale
    /// registration files have been removed.
    pub fn recover_host_loss<C: NativeProcessControl>(
        &self,
        evidence: &WorldEvidence,
        instance: &str,
        control: &C,
    ) -> Result<Option<WorldTerminalSummary>> {
        validate_instance_id(instance)?;
        let stale = match self.probe(instance)? {
            RegistrationProbe::Missing => return evidence.read_summary(instance),
            RegistrationProbe::Live(_) => return Ok(None),
            RegistrationProbe::Stale(stale) => stale,
        };

        if let Some(summary) = evidence.read_summary(instance)? {
            stale.remove_exact(&self.paths)?;
            return Ok(Some(summary));
        }

        let checkpoint = evidence
            .read_checkpoint(instance)?
            .with_context(|| format!("stale world {instance} has no durable checkpoint"))?;
        checkpoint.validate(&stale.registration)?;
        let native = checkpoint.native_process.as_ref().with_context(|| {
            format!(
                "stale world {instance} was registered without durable native process ownership"
            )
        })?;
        converge_native_process_group(native, control)?;

        // A normal adapter summary can win while recovery waits for native
        // convergence. It is authoritative and must never be overwritten.
        if let Some(summary) = evidence.read_summary(instance)? {
            stale.remove_exact(&self.paths)?;
            return Ok(Some(summary));
        }

        let member_evidence = evidence.discover_member_evidence(&checkpoint)?;
        let (retained_logs, truncated) = evidence.recovery_logs(instance)?;
        let summary = WorldTerminalSummary {
            schema: TERMINAL_SUMMARY_SCHEMA.to_owned(),
            instance: checkpoint.state.instance,
            provenance: checkpoint.state.provenance,
            outcome: TerminalOutcome::Failed {
                reason: SimulationEndReason::HostLost,
                detail: format!(
                    "world host process {} born {} exited without terminal evidence",
                    stale.registration.process.pid,
                    stale.registration.process.started_at_unix_s
                ),
            },
            progress: checkpoint.state.progress,
            members: checkpoint.state.members,
            member_evidence,
            failing: TerminalFailure {
                process: Some(stale.registration.process),
                producer: None,
            },
            evidence: retained_logs,
            cleanup: TerminalCleanup {
                complete: false,
                detail: Some(
                    "the exact orphaned native process group converged, but abrupt host loss prevented authoritative member cleanup"
                        .to_owned(),
                ),
            },
            retention: TerminalRetention {
                log_byte_limit: DEFAULT_LOG_BYTE_LIMIT,
                truncated,
            },
            ended_at_unix_ms: unix_ms()?,
        };
        let summary = evidence.publish_recovered_summary(instance, &summary)?;
        stale.remove_exact(&self.paths)?;
        Ok(Some(summary))
    }
}
impl StaleRegistration {
    fn remove_exact(self, paths: &WorldPaths) -> Result<()> {
        let registration_path = paths.registration_path(&self.registration.instance.to_string());
        let lease_path = paths.registry().join(&self.registration.lease);
        remove_exact_open_file(&registration_path, &self.registration_file)?;
        remove_exact_open_file(&lease_path, &self.lease_file)?;
        Ok(())
    }
}
