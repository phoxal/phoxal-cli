//! Build responsibilities for check.

use super::{RawParticipantReport, raw_participant_report_from_extracted_metadata};
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::check as graph_check;
use phoxal_cli_core::check::participant_metadata;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::check::source::SourceParticipantKind;
use phoxal_cli_core::project::tooling::{cargo_binary_name, cargo_package_name};
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;

/// Build a source participant's contract report by compiling and inspecting
/// it. `offline` appends `--offline` to the underlying `cargo build`
/// (organization#951 WS4 review, round 2): the check-time source build is a
/// Cargo invocation like any other, and a caller that requested the whole
/// operation stay offline must have that hold here too.
pub(crate) fn build_participant_report_from_source(
    participant: &SourceParticipant,
    offline: bool,
    reporter: &dyn crate::Reporter,
) -> Result<RawParticipantReport> {
    build_participant_report_from_source_with_diagnostics(
        participant,
        |participant| build_participant_report_by_building(participant, offline, reporter),
        Some(reporter),
    )
}

/// Core of [`build_participant_report_from_source`], parameterized over the (expensive)
/// builder so tests can exercise it against a fake build closure instead of
/// spawning a real `cargo build`.
pub(crate) fn build_participant_report_from_source_with_diagnostics(
    participant: &SourceParticipant,
    mut build_by_building: impl FnMut(&SourceParticipant) -> Result<RawParticipantReport>,
    ui: Option<&dyn crate::Reporter>,
) -> Result<RawParticipantReport> {
    let raw = build_by_building(participant)?;
    report_source_build_progress(
        ui,
        format!(
            "built participant report for {} {}",
            participant.kind_label(),
            participant.name
        ),
    );
    Ok(raw)
}

pub(crate) fn report_source_build_progress(ui: Option<&dyn crate::Reporter>, message: String) {
    if let Some(ui) = ui {
        ui.info(message);
    }
}

/// The expected `artifact.kind` label for a [`SourceParticipant`]'s kind -
/// shared between [`build_participant_report_by_building`] (which checks the
/// binary-owned kind against this expectation) and
/// [`validate_source_artifact_identity`] (which still checks a fake/injected
/// report against it in tests).
pub(crate) fn expected_kind_for_source_participant(kind: SourceParticipantKind) -> &'static str {
    kind.shared_kind().label()
}

pub(crate) fn build_participant_report_by_building(
    participant: &SourceParticipant,
    offline: bool,
    reporter: &dyn crate::Reporter,
) -> Result<RawParticipantReport> {
    let crate_dir = participant.crate_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize source crate {}",
            participant.crate_dir.display()
        )
    })?;
    let binary_name = cargo_binary_name(&crate_dir, None)?;
    let package_name = cargo_package_name(&crate_dir)?;
    let binary_path =
        build_and_locate_binary(&crate_dir, &package_name, &binary_name, offline, reporter)?;
    let meta =
        participant_metadata::extract_participant_metadata(&binary_path).with_context(|| {
            format!(
                "failed to extract participant metadata from {}",
                binary_path.display()
            )
        })?;
    raw_participant_report_from_extracted_metadata(
        expected_kind_for_source_participant(participant.kind),
        &participant.expected_artifact_id,
        &binary_path,
        meta,
    )
}

/// Builds `binary_name` in `crate_dir` and locates its resulting executable
/// path via cargo's own `--message-format=json` build log, rather than
/// guessing `<dir>/target/debug/<bin>` by hand: a crate that is a workspace
/// member (e.g. a `phoxal/framework` `component/<name>` driver) compiles into
/// the *workspace-root* `target/`, not `<crate_dir>/target/`, so a fixed path
/// would miss it. Cargo's own artifact messages are workspace-aware
/// regardless of layout.
pub(crate) fn build_and_locate_binary(
    crate_dir: &Path,
    package_name: &str,
    binary_name: &str,
    offline: bool,
    reporter: &dyn crate::Reporter,
) -> Result<PathBuf> {
    // `run_output` fully captures the child's stdout/stderr, so emit one
    // append-only status line before starting the captured build.
    reporter.info(format!(
        "building `{binary_name}` in {}",
        crate_dir.display()
    ));
    let mut args = vec![
        "build",
        "--locked",
        "--quiet",
        "--message-format=json",
        "-p",
        package_name,
        "--bin",
        binary_name,
    ];
    if offline {
        args.push("--offline");
    }
    let result = crate::build::shell::run_output("cargo", args, Some(crate_dir), Some(reporter))
        .with_context(|| {
            format!("failed to spawn `cargo build -p {package_name} --bin {binary_name}`")
        });
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            reporter.error(format!("failed to build `{binary_name}`: {error:#}"));
            return Err(error);
        }
    };
    if !output.status.success() {
        bail!(
            "failed to build `{binary_name}` in {}\nstdout:\n{}\nstderr:\n{}",
            crate_dir.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .context("cargo build --message-format=json wrote non-UTF8 stdout")?;
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if message.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        let Some(target) = message.get("target") else {
            continue;
        };
        let is_bin = target
            .get("kind")
            .and_then(Value::as_array)
            .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("bin")));
        if !is_bin || target.get("name").and_then(Value::as_str) != Some(binary_name) {
            continue;
        }
        if let Some(executable) = message.get("executable").and_then(Value::as_str) {
            return Ok(PathBuf::from(executable));
        }
    }
    bail!(
        "cargo build for `{binary_name}` in {} did not report an executable path",
        crate_dir.display()
    )
}

pub(crate) fn validate_source_artifact_identity(
    participant: &SourceParticipant,
    raw: &RawParticipantReport,
) -> Result<()> {
    validate_artifact_identity(
        participant.kind_label(),
        participant.expected_artifact_id.as_str(),
        expected_kind_for_source_participant(participant.kind),
        raw,
    )
}

pub(crate) fn validate_artifact_identity(
    label: &str,
    expected_id: &str,
    expected_kind: &str,
    raw: &RawParticipantReport,
) -> Result<()> {
    if raw.artifact.id != expected_id {
        bail!(
            "{label} participant report artifact.id '{}' does not match expected artifact id '{}'",
            raw.artifact.id,
            expected_id
        );
    }
    if raw.artifact.kind != expected_kind {
        bail!(
            "{label} participant report artifact.kind '{}' does not match the expected kind '{}'",
            raw.artifact.kind,
            expected_kind
        );
    }
    Ok(())
}

impl TryFrom<RawParticipantReport> for graph_check::ParticipantApis {
    type Error = anyhow::Error;

    fn try_from(raw: RawParticipantReport) -> Result<Self> {
        let artifact_id = raw.artifact.id;
        let participant_kind = graph_check::ParticipantKind::parse(&raw.artifact.kind);
        let participant_class = graph_check::ParticipantClass::parse(&raw.participant_class)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "participant report carries unknown class '{}'",
                    raw.participant_class
                )
            })?;
        Ok(Self {
            // Default the participant id to the artifact id; callers that launch
            // one artifact per instance (component drivers) override it with the
            // concrete instance id below.
            participant_id: artifact_id.clone(),
            artifact_id,
            participant_kind,
            participant_class,
            config_schema: raw.config_schema,
            scope: graph_check::ParticipantScope::Graph,
        })
    }
}
