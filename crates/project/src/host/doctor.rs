//! Generic host prerequisite checks shared by project workflows.

fn run_stdout(
    executable: impl AsRef<std::ffi::OsStr>,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> anyhow::Result<String> {
    let output = std::process::Command::new(executable)
        .args(args)
        .output()
        .map_err(anyhow::Error::from)?;
    anyhow::ensure!(
        output.status.success(),
        "host probe exited with {}",
        output.status
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    Ok(String),
    Warn(String),
}

#[must_use]
pub fn probes() -> [ProbeStatus; 1] {
    [probe_rust_tools()]
}

pub fn probe_rust_tools() -> ProbeStatus {
    let rustup = run_stdout("rustup", ["--version"]);
    let cargo = run_stdout("cargo", ["--version"]);
    match (rustup, cargo) {
        (Ok(rustup), Ok(cargo)) => ProbeStatus::Ok(format!(
            "Rust tools: {}, {}",
            first_line(&rustup, "rustup installed"),
            first_line(&cargo, "cargo installed")
        )),
        (rustup, cargo) => {
            let mut missing = Vec::new();
            if rustup.is_err() {
                missing.push("rustup");
            }
            if cargo.is_err() {
                missing.push("cargo");
            }
            ProbeStatus::Warn(format!(
                "{} missing; user service and driver builds may fail - install Rust from https://rustup.rs and ensure rustup and cargo are on PATH",
                missing.join(" and ")
            ))
        }
    }
}

fn first_line<'a>(stdout: &'a str, fallback: &'a str) -> &'a str {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_probe_selects_the_first_nonempty_line() {
        assert_eq!(
            first_line("\n rustup 1.28.2 \nignored", "fallback"),
            "rustup 1.28.2"
        );
        assert_eq!(first_line("\n\t\n", "fallback"), "fallback");
    }
}
