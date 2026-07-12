//! Process-level proof of the output-mode matrix (see
//! `phoxal_cli::output_mode`): `--message-format json` must be byte-identical
//! to what the CLI produced before the theme/progress/identity foundation
//! landed, and no identity/welcome/progress decoration may leak into stdout
//! or stderr under json, under a machine verb, or over a piped (non-TTY)
//! stderr.
//!
//! These spawn the real `phoxal-cli` binary (via `CARGO_BIN_EXE_phoxal-cli`)
//! rather than calling library functions directly, because the thing under
//! test - TTY detection, ANSI escape emission, stdout/stderr separation - is
//! only observable from outside the process; a piped `Command` child is
//! exactly the "non-TTY" row of the matrix.

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_phoxal-cli"))
}

/// Captured from this crate's `main` branch (pre-theme/progress/identity)
/// with `cargo run -- version --message-format json`, piped stdout/stderr.
/// `version` needs no robot project and no network, so it is the cleanest
/// verb to pin byte-for-byte.
const BASELINE_VERSION_JSON_STDOUT: &str = "{\n  \"cli_version\": \"0.8.1\",\n  \"participant_metadata_sections\": [\n    \".phoxal_api_meta\",\n    \"__phoxal_meta\"\n  ],\n  \"wire_codec\": \"phoxal/v0;codec=1\"\n}\n";

#[test]
fn version_json_output_is_byte_identical_to_the_pre_foundation_baseline() {
    let output = bin()
        .args(["version", "--message-format", "json"])
        .output()
        .expect("phoxal-cli version --message-format json should run");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        BASELINE_VERSION_JSON_STDOUT,
        "--message-format json stdout must stay byte-identical - no identity/progress bytes may be mixed in"
    );
    assert!(
        output.stderr.is_empty(),
        "the output-mode matrix promises stderr carries nothing at all under --message-format json, got: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn welcome_flag_does_not_defeat_json_mode_silence() {
    // `--welcome` is a global flag parsed before the verb; json's
    // "stderr carries nothing" rule must still win over it.
    let output = bin()
        .args(["--welcome", "version", "--message-format", "json"])
        .output()
        .expect("phoxal-cli --welcome version --message-format json should run");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        BASELINE_VERSION_JSON_STDOUT
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_mode_keeps_stderr_empty_on_a_top_level_failure() {
    let missing = tempfile::tempdir().expect("tempdir").path().join("missing");
    let output = bin()
        .args([
            "--project-path",
            missing.to_str().unwrap(),
            "version",
            "--message-format",
            "json",
        ])
        .env("RUST_LOG", "trace")
        .output()
        .expect("failing JSON invocation should run");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.is_empty(),
        "JSON mode must suppress tracing and the top-level human diagnostic: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn welcome_flag_does_not_defeat_the_machine_verb_suppression() {
    // `version` is a machine verb: identity/welcome stay suppressed even
    // under `--welcome` and even in human mode.
    let output = bin()
        .args(["--welcome", "version"])
        .output()
        .expect("phoxal-cli --welcome version should run");

    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.is_empty(),
        "a machine verb must suppress the welcome card too, got: {stderr:?}"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("phoxal-cli "));
}

#[test]
fn piped_stderr_never_carries_ansi_escapes_or_the_identity_line_outside_a_robot_project() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = bin()
        .args(["--project-path", temp.path().to_str().unwrap(), "doctor"])
        .output()
        .expect("phoxal-cli doctor should run");

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.contains('\x1b'),
        "a piped (non-TTY) stderr must never carry ANSI escapes, got: {stderr:?}"
    );
    // No `robot.yaml` under a bare tempdir: the identity line has nothing to
    // discover and must stay silent rather than error the command.
    assert!(!stderr.contains("phoxal ·"));
}

#[test]
fn plain_flag_also_yields_ansi_free_stderr_on_a_would_be_tty_command() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = bin()
        .args([
            "--project-path",
            temp.path().to_str().unwrap(),
            "--plain",
            "doctor",
        ])
        .output()
        .expect("phoxal-cli --plain doctor should run");

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains('\x1b'));
}
