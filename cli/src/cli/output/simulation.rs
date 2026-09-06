//! Plain finite output for simulation lifecycle commands.

use phoxal::world::api::session::WorldLifecycle;
use phoxal::world::api::session::state::WorldSessionState;
use phoxal_cli_host::world::{LocalWorldRegistration, WorldMemberEvidence, WorldTerminalSummary};

pub(crate) fn live_status(state: &WorldSessionState) -> String {
    let mut lines = vec![
        format!("instance:  {}", state.instance),
        format!("world:     {}", state.provenance.world),
        format!("digest:    {}", state.provenance.digest),
        format!("lifecycle: {}", lifecycle(state.lifecycle)),
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
    lines.extend(state.members.iter().map(|member| {
        format!(
            "  {}  {:?}  {}",
            member.robot, member.phase, member.execution
        )
    }));
    lines.join("\n")
}

pub(crate) fn terminal_status(
    summary: &WorldTerminalSummary,
    ended: &[WorldMemberEvidence],
) -> String {
    let mut lines = vec![
        format!("instance:  {}", summary.instance),
        format!("world:     {}", summary.provenance.world),
        format!("digest:    {}", summary.provenance.digest),
        format!("lifecycle: {}", summary.outcome.kind()),
        format!("reason:    {:?}", summary.outcome.reason()),
    ];
    if let Some(detail) = summary.outcome.detail() {
        lines.push(format!("detail:    {detail}"));
    }
    lines.extend([
        format!("train:     {}", summary.provenance.framework),
        format!(
            "adapter:   {} {}",
            summary.provenance.adapter, summary.provenance.adapter_version
        ),
        format!("simulator: {}", summary.provenance.simulator_version),
        format!("platform:  {}", summary.provenance.platform),
        format!("seed:      {}", summary.provenance.random_seed),
        format!("quantum:   {} ns", summary.provenance.time_step_ns),
        format!("step:      {}", summary.progress.completed_step()),
        format!("world ns:  {}", summary.progress.elapsed_ns()),
        format!(
            "members:   {} at shutdown, {} ended",
            summary.members.len(),
            ended.len()
        ),
    ]);
    lines.extend(summary.members.iter().map(|member| {
        format!(
            "  {}  at-shutdown/{:?}  {}",
            member.robot, member.phase, member.execution
        )
    }));
    lines.extend(ended.iter().map(|member| {
        format!(
            "  {}  ended/{:?}  {}  cleanup/{:?}",
            member.terminal.robot,
            member.terminal.reason,
            member.terminal.execution,
            member.terminal.cleanup
        )
    }));
    if let Some(process) = summary.failing.process {
        lines.push(format!(
            "failure:   process {} born {}",
            process.pid, process.started_at_unix_s
        ));
    }
    if let Some(producer) = summary.failing.producer {
        lines.push(format!("failure:   producer {producer}"));
    }
    lines.push(format!(
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
    ));
    if !summary.retention.truncated.is_empty() {
        lines.push(format!(
            "truncated: {}",
            summary.retention.truncated.join(", ")
        ));
    }
    lines.join("\n")
}

pub(crate) fn list(
    live: &[LocalWorldRegistration],
    terminal: &[WorldTerminalSummary],
    include_terminal: bool,
) -> String {
    let mut lines = live
        .iter()
        .map(|registration| {
            format!(
                "{}  live      {}  {}  train {}",
                registration.instance,
                registration.world.id,
                registration.world.digest,
                registration.framework
            )
        })
        .collect::<Vec<_>>();
    lines.extend(terminal.iter().map(|summary| {
        format!(
            "{}  {:<9} {}  {}  train {}",
            summary.instance,
            summary.outcome.kind(),
            summary.provenance.world,
            summary.provenance.digest,
            summary.provenance.framework
        )
    }));
    if live.is_empty() && !include_terminal {
        lines.push("no live world sessions".to_owned());
    }
    lines.join("\n")
}

pub(crate) fn logs(logs: &[(String, Vec<u8>)]) -> String {
    if logs.is_empty() {
        return "no retained world process logs\n".to_owned();
    }
    let mut output = String::new();
    for (name, bytes) in logs {
        output.push_str(&format!("== {name} ==\n"));
        output.push_str(&String::from_utf8_lossy(bytes));
        if !bytes.ends_with(b"\n") {
            output.push('\n');
        }
    }
    output
}

pub(crate) fn lifecycle(lifecycle: WorldLifecycle) -> String {
    match lifecycle {
        WorldLifecycle::Starting => "starting".to_owned(),
        WorldLifecycle::Ready { motion } => format!("ready/{motion:?}").to_lowercase(),
        WorldLifecycle::Stopping => "stopping".to_owned(),
        WorldLifecycle::Failed { reason } => format!("failed/{reason:?}").to_lowercase(),
    }
}
