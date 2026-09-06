//! Read-only world inspection, terminal evidence, and textual presentation.

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

pub(super) fn print_live_status(state: &WorldSessionState) {
    println!("{}", format_live_status(state));
}

pub(super) fn format_live_status(state: &WorldSessionState) -> String {
    let mut lines = vec![
        format!("instance:  {}", state.instance),
        format!("world:     {}", state.provenance.world),
        format!("digest:    {}", state.provenance.digest),
        format!("lifecycle: {}", lifecycle_text(state.lifecycle)),
        format!("train:     {}", state.provenance.framework),
        format!(
            "adapter:   {} {}",
            state.provenance.adapter, state.provenance.adapter_version
        ),
        format!("simulator: {}", state.provenance.simulator_version),
        format!("step:      {}", state.progress.completed_step()),
        format!("world ns:  {}", state.progress.elapsed_ns()),
        format!("members:   {}", state.members.len()),
    ];
    for member in &state.members {
        lines.push(format!(
            "  {}  {:?}  {}",
            member.robot, member.phase, member.execution
        ));
    }
    lines.join("\n")
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

pub(super) fn lifecycle_text(lifecycle: WorldLifecycle) -> String {
    match lifecycle {
        WorldLifecycle::Starting => "starting".to_owned(),
        WorldLifecycle::Ready { motion } => format!("ready/{motion:?}").to_lowercase(),
        WorldLifecycle::Stopping => "stopping".to_owned(),
        WorldLifecycle::Failed { reason } => format!("failed/{reason:?}").to_lowercase(),
    }
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

pub(super) fn print_terminal_status(summary: &WorldTerminalSummary, ended: &[WorldMemberEvidence]) {
    println!("instance:  {}", summary.instance);
    println!("world:     {}", summary.provenance.world);
    println!("digest:    {}", summary.provenance.digest);
    println!("lifecycle: {}", summary.outcome.kind());
    println!("reason:    {:?}", summary.outcome.reason());
    if let Some(detail) = summary.outcome.detail() {
        println!("detail:    {detail}");
    }
    println!("train:     {}", summary.provenance.framework);
    println!(
        "adapter:   {} {}",
        summary.provenance.adapter, summary.provenance.adapter_version
    );
    println!("simulator: {}", summary.provenance.simulator_version);
    println!("platform:  {}", summary.provenance.platform);
    println!("seed:      {}", summary.provenance.random_seed);
    println!("quantum:   {} ns", summary.provenance.time_step_ns);
    println!("step:      {}", summary.progress.completed_step());
    println!("world ns:  {}", summary.progress.elapsed_ns());
    println!(
        "members:   {} at shutdown, {} ended",
        summary.members.len(),
        ended.len()
    );
    for member in &summary.members {
        println!(
            "  {}  at-shutdown/{:?}  {}",
            member.robot, member.phase, member.execution
        );
    }
    for member in ended {
        println!(
            "  {}  ended/{:?}  {}  cleanup/{:?}",
            member.terminal.robot,
            member.terminal.reason,
            member.terminal.execution,
            member.terminal.cleanup
        );
    }
    if let Some(process) = summary.failing.process {
        println!(
            "failure:   process {} born {}",
            process.pid, process.started_at_unix_s
        );
    }
    if let Some(producer) = summary.failing.producer {
        println!("failure:   producer {producer}");
    }
    println!(
        "cleanup:   {}{}",
        if summary.cleanup.complete {
            "complete"
        } else {
            "incomplete"
        },
        summary
            .cleanup
            .detail
            .as_deref()
            .map_or_else(String::new, |detail| format!(" ({detail})"))
    );
    if !summary.retention.truncated.is_empty() {
        println!("truncated: {}", summary.retention.truncated.join(", "));
    }
}
