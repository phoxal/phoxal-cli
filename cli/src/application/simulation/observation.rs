//! Read-only world inspection, terminal evidence, and textual presentation.

use super::connect::{connect_verified, current_verified};
use super::*;

pub(super) enum StatusReport {
    Live(Box<WorldSessionState>),
    Terminal {
        summary: Box<WorldTerminalSummary>,
        members: Vec<WorldMemberEvidence>,
    },
}

pub(super) async fn load_status(stores: &Stores, instance: &str) -> Result<StatusReport> {
    if let Some(registration) = stores.registry.find(instance)? {
        let (_, state) = current_verified(&registration).await?;
        return Ok(StatusReport::Live(Box::new(state)));
    }
    let summary = stores.recover_terminal(instance).await?.with_context(|| {
        format!(
            "no live or retained terminal world session `{instance}` was found; `phoxal simulation list --all` shows discoverable sessions"
        )
    })?;
    let members = stores.evidence.read_member_evidence(&summary)?;
    Ok(StatusReport::Terminal {
        summary: Box::new(summary),
        members,
    })
}

pub(super) async fn load_logs(stores: &Stores, instance: &str) -> Result<Vec<(String, Vec<u8>)>> {
    if let Some(registration) = stores.registry.find(instance)? {
        connect_verified(&registration).await?;
        return stores.evidence.read_live_logs(instance);
    }
    let summary = stores.recover_terminal(instance).await?.with_context(|| {
        format!("no live or retained terminal world session `{instance}` was found")
    })?;
    stores.evidence.read_logs(&summary)
}

pub(super) struct ListReport {
    pub(super) live: Vec<LocalWorldRegistration>,
    pub(super) terminal: Vec<WorldTerminalSummary>,
}

pub(super) async fn load_list(stores: &Stores, all: bool) -> Result<ListReport> {
    let discoverable = stores.registry.registration_instances()?;
    let registered = stores.registry.list()?;
    let registered_ids = registered
        .iter()
        .map(|registration| registration.instance.to_string())
        .collect::<BTreeSet<_>>();
    if all {
        for instance in discoverable {
            if registered_ids.contains(&instance) {
                continue;
            }
            if let Err(error) = stores.recover_terminal(&instance).await {
                eprintln!(
                    "warning: stale world {instance} could not be finalized from durable evidence: {error:#}"
                );
            }
        }
    }
    let report = stores
        .evidence
        .prune(DEFAULT_TERMINAL_SESSION_LIMIT, &registered_ids)?;
    for path in report.incomplete {
        tracing::warn!(path = %path.display(), "incomplete world evidence was retained");
    }
    let mut live = Vec::new();
    for registration in registered {
        match connect_verified(&registration).await {
            Ok(_) => live.push(registration),
            Err(error) => eprintln!(
                "warning: world {} has a live local lease but its frozen bootstrap could not be verified: {error:#}",
                registration.instance
            ),
        }
    }
    let terminal = if all {
        stores
            .evidence
            .list_summaries()?
            .into_iter()
            .filter(|summary| !registered_ids.contains(&summary.instance.to_string()))
            .collect()
    } else {
        Vec::new()
    };
    Ok(ListReport { live, terminal })
}

pub(super) fn ensure_state_matches_registration(
    state: &WorldSessionState,
    registration: &LocalWorldRegistration,
) -> Result<()> {
    ensure!(
        state.instance == registration.instance,
        "world state instance {} disagrees with locator {}",
        state.instance,
        registration.instance
    );
    ensure!(
        state.provenance.framework == registration.framework
            && state.provenance.world == registration.world.id
            && state.provenance.digest == registration.world.digest,
        "world state provenance disagrees with the verified locator for {}",
        registration.instance
    );
    Ok(())
}

pub(super) async fn stop_world(
    registration: LocalWorldRegistration,
    stores: &Stores,
) -> Result<WorldTerminalSummary> {
    let client = connect_verified(&registration).await?;
    let state = client
        .control(WorldControl::Stop)
        .await
        .context("world host refused stop")?;
    ensure_state_matches_registration(&state, &registration)?;

    let instance = registration.instance.to_string();
    let summary = tokio::time::timeout(STOP_BUDGET, async {
        loop {
            if stores.registry.find(&instance)?.is_none() {
                if let Some(summary) = stores.evidence.read_summary(&instance)? {
                    return Ok::<_, anyhow::Error>(summary);
                }
                if let Some(summary) = stores.recover_terminal(&instance).await? {
                    return Ok::<_, anyhow::Error>(summary);
                }
            }
            tokio::time::sleep(TERMINAL_POLL_INTERVAL).await;
        }
    })
    .await
    .with_context(|| {
        format!(
            "timed out after {}s waiting for world {instance} to persist terminal evidence",
            STOP_BUDGET.as_secs()
        )
    })??;
    if matches!(summary.outcome, TerminalOutcome::Failed { .. }) {
        bail!(
            "world {instance} ended as {}/{:?} while stopping{}",
            summary.outcome.kind(),
            summary.outcome.reason(),
            summary
                .outcome
                .detail()
                .map_or_else(String::new, |detail| format!(": {detail}"))
        );
    }
    Ok(summary)
}
