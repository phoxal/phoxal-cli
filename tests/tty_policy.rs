use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_phoxal-cli"))
}

#[test]
fn interactive_foreground_verbs_fail_clearly_without_a_tty() {
    for args in [vec!["run"], vec!["simulation", "run", "default"]] {
        let output = bin().args(&args).output().expect("CLI should run");
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("require a terminal") && stderr.contains("TTY"),
            "expected actionable TTY error for {args:?}, got {stderr:?}"
        );
    }
}

#[test]
fn finite_commands_remain_pipe_friendly() {
    let output = bin().arg("version").output().expect("version should run");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("phoxal-cli ")
    );

    let missing = tempfile::tempdir().expect("tempdir").path().join("missing");
    let output = bin()
        .args(["--project-path", missing.to_str().unwrap(), "version"])
        .output()
        .expect("failing finite command should run");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error"));
    assert!(stderr.contains("failed to resolve project path"));
    assert!(!stderr.contains('\x1b'));
}

#[test]
fn piped_finite_command_emits_no_terminal_decoration() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = bin()
        .args(["--project-path", temp.path().to_str().unwrap(), "doctor"])
        .output()
        .expect("piped doctor should run");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains('\x1b'));
    assert!(!stderr.contains("phoxal ·"));
}
