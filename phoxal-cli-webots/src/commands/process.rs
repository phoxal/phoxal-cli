use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use phoxal_cli_core::AppContext;

pub fn host_binary_path(workspace_root: &Path, binary_name: impl AsRef<str>) -> PathBuf {
    workspace_root
        .join("target")
        .join("debug")
        .join(binary_name.as_ref())
}

pub fn build_host_binaries(app: &AppContext, packages: Vec<String>) -> Result<()> {
    let mut packages = packages;
    packages.sort();
    packages.dedup();
    if packages.is_empty() {
        return Ok(());
    }

    let mut build = ProcessCommand::new("cargo");
    build.current_dir(app.project.root()).arg("build");
    for package in packages {
        build.arg("-p").arg(package);
    }

    let title = "Building host binaries for Webots workflow";
    app.ui.step(title, || {
        let status = app.ui.command_status(&mut build)?;
        if !status.success() {
            bail!("cargo build failed with status {status}");
        }
        Ok(())
    })?;
    Ok(())
}

pub fn spawn_tracked_process(
    app: &AppContext,
    log_dir: &Path,
    name: &str,
    mode: SpawnLogMode,
    command: &mut ProcessCommand,
) -> Result<AttachedLogFollowers> {
    #[cfg(unix)]
    command.process_group(0);

    let followers = match mode {
        SpawnLogMode::TracingFile => {
            let log_path = log_dir.join(format!("{name}.log"));
            File::create(&log_path)
                .with_context(|| format!("failed to create {}", log_path.display()))?;
            command
                .env("ROBOT_LOG_PATH", &log_path)
                .env("NO_COLOR", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            AttachedLogFollowers::single(name.to_string(), "log".to_string(), log_path)
        }
        SpawnLogMode::StdIoCapture => {
            let stdout_path = log_dir.join(format!("{name}.stdout.log"));
            let stderr_path = log_dir.join(format!("{name}.stderr.log"));
            File::create(&stdout_path)
                .with_context(|| format!("failed to create {}", stdout_path.display()))?;
            File::create(&stderr_path)
                .with_context(|| format!("failed to create {}", stderr_path.display()))?;
            let stdout = OpenOptions::new()
                .append(true)
                .open(&stdout_path)
                .with_context(|| format!("failed to open {}", stdout_path.display()))?;
            let stderr = OpenOptions::new()
                .append(true)
                .open(&stderr_path)
                .with_context(|| format!("failed to open {}", stderr_path.display()))?;
            command
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr));
            AttachedLogFollowers::multiple(vec![
                (name.to_string(), "stdout".to_string(), stdout_path),
                (name.to_string(), "stderr".to_string(), stderr_path),
            ])
        }
    };
    let _child = app.ui.command_spawn(command)?;
    app.ui.info(format!("Started {name}"));
    Ok(followers)
}

pub fn follow_attached_logs(
    app: &AppContext,
    attached_logs: Vec<AttachedLogFollowers>,
    robot_model: &str,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .context("failed to install Ctrl+C handler")?;
    rx.recv()
        .context("failed while waiting for Ctrl+C to detach session logs")?;

    for attached_log in attached_logs {
        attached_log.stop();
    }
    app.ui.info(format!(
        "Detached from '{robot_model}' session logs. The simulation is still running; use `cargo xtask webots down {robot_model}` to stop it.",
    ));
    Ok(())
}

pub fn spawn_log_follower(
    process_name: String,
    stream_name: String,
    path: PathBuf,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let Ok(file) = OpenOptions::new().read(true).open(&path) else {
            return;
        };
        let mut reader = BufReader::new(file);
        follow_log_file(&process_name, &stream_name, &mut reader, &stop);
    })
}

pub fn follow_log_file(
    process_name: &str,
    stream_name: &str,
    reader: &mut BufReader<File>,
    stop: &Arc<AtomicBool>,
) {
    let mut line = String::new();
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }

        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                sleep(Duration::from_millis(200));
            }
            Ok(_) => {
                if line.ends_with('\n') {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                }
                eprintln!("[{process_name}/{stream_name}] {line}");
            }
            Err(_) => return,
        }
    }
}

pub fn matches_webots_command_line(current_command: &str) -> bool {
    let lower = current_command.to_lowercase();
    if !lower.contains("webots") {
        return false;
    }

    // Ignore xtask and cargo processes to avoid false positives during discovery.
    // The current command line for xtask contains the subcommand "webots" but is not the simulator.
    if lower.contains("xtask") || lower.contains("cargo-xtask") {
        return false;
    }

    // On macOS, Webots is usually in a bundle.
    if lower.contains("/applications/webots.app") {
        return true;
    }

    // Otherwise, check if the binary itself is named webots or webots-bin.
    if let Some(first_arg) = current_command.split_whitespace().next() {
        let binary_path = std::path::Path::new(first_arg);
        if let Some(file_name) = binary_path.file_name().and_then(|f| f.to_str()) {
            let file_name_lower = file_name.to_lowercase();
            if file_name_lower == "webots" || file_name_lower == "webots-bin" {
                return true;
            }
        }
    }

    false
}

pub fn matches_xtask_session(args: &str, xtask_session: &str) -> bool {
    let marker = format!("--xtask-session={xtask_session}");
    args.contains(&marker)
}

pub fn discover_session_processes(xtask_session: &str) -> Result<Vec<OwnedProcess>> {
    discover_processes(|args| {
        matches_xtask_session(args, xtask_session).then(|| OwnedProcess {
            pid: 0,
            name: process_name_from_args(args).to_string(),
            args: args.to_string(),
        })
    })
}

pub fn discover_webots_processes() -> Result<Vec<OwnedProcess>> {
    discover_processes(|args| {
        matches_webots_command_line(args).then(|| OwnedProcess {
            pid: 0,
            name: "webots".to_string(),
            args: args.to_string(),
        })
    })
}

pub fn discover_processes<F>(mut filter: F) -> Result<Vec<OwnedProcess>>
where
    F: FnMut(&str) -> Option<OwnedProcess>,
{
    let my_pid = std::process::id();
    let output = ProcessCommand::new("ps")
        .arg("-axo")
        .arg("pid=,args=")
        .output()
        .context("failed to list running processes")?;
    if !output.status.success() {
        bail!(
            "failed to list running processes with status {}",
            output.status
        );
    }

    let mut processes = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((pid, args)) = parse_process_line(trimmed) else {
            continue;
        };
        if pid == my_pid {
            continue;
        }
        if let Some(mut process) = filter(args) {
            process.pid = pid;
            processes.push(process);
        }
    }
    Ok(processes)
}

pub fn ensure_no_webots_running() -> Result<()> {
    let webots_processes = discover_webots_processes()?;
    if webots_processes.is_empty() {
        return Ok(());
    }

    bail!(
        "found {} running Webots process(es) on this system. Stop them before starting a new session with `cargo xtask webots up`. Run `cargo xtask webots down` to stop them cleanly.",
        webots_processes.len()
    )
}

pub fn parse_process_line(line: &str) -> Option<(u32, &str)> {
    let mut parts = line.splitn(2, char::is_whitespace);
    let pid = parts.next()?.trim().parse().ok()?;
    let args = parts.next()?.trim();
    (!args.is_empty()).then_some((pid, args))
}

pub fn process_name_from_args(args: &str) -> &str {
    Path::new(args.split_whitespace().next().unwrap_or_default())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(args)
}

pub fn stop_owned_process(process: &OwnedProcess, force: bool, app: &AppContext) {
    let signal = if force { "-KILL" } else { "-TERM" };
    app.ui
        .info(format!("Stopping {} (PID {})", process.name, process.pid));
    let mut kill = ProcessCommand::new("kill");
    kill.arg(signal).arg(process.pid.to_string());
    let _ = kill.status();

    if !force {
        for _ in 0..10 {
            if !process_is_running(process.pid) {
                return;
            }
            sleep(Duration::from_millis(200));
        }
        let mut force_kill = ProcessCommand::new("kill");
        force_kill.arg("-KILL").arg(process.pid.to_string());
        let _ = force_kill.status();
    }
}

pub fn process_is_running(pid: u32) -> bool {
    let output = ProcessCommand::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("pid=")
        .output();
    matches!(output, Ok(ref out) if out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedProcess {
    pub pid: u32,
    pub name: String,
    args: String,
}

pub struct AttachedLogFollowers {
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl AttachedLogFollowers {
    pub fn single(process_name: String, stream_name: String, path: PathBuf) -> Self {
        Self::multiple(vec![(process_name, stream_name, path)])
    }

    fn multiple(followers: Vec<(String, String, PathBuf)>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = followers
            .into_iter()
            .map(|(process_name, stream_name, path)| {
                spawn_log_follower(process_name, stream_name, path, stop.clone())
            })
            .collect();
        Self { stop, threads }
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy)]
pub enum SpawnLogMode {
    TracingFile,
    StdIoCapture,
}
