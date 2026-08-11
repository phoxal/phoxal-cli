//! The `phoxal` + `phoxald` pair, as a packaging unit.
//!
//! The two binaries ship in one archive, install together, and upgrade
//! together, so the client resolves the daemon as its own sibling and can
//! report whether the installation it is part of is whole.
//!
//! That is a statement about an installation, never about a robot project. A
//! project selects its own framework in its own Cargo graph, and the installed
//! CLI product version is not a compatibility identity for anything the
//! project builds - so no project-bound command gates on the two halves
//! reporting the same product version. What remains here is resolution (how a
//! machine finds its daemon), self-upgrade and bootstrap packaging (how the
//! archive lands), and the `doctor` report.
//!
//! Resolution is deliberately *not* `PATH`-first. A `phoxal` run out of a build
//! directory must not silently drive an older installed daemon, so the answer is
//! the sibling next to `current_exe` whenever `current_exe` resolves at all.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The daemon executable's file name.
pub(crate) const DAEMON_BINARY: &str = "phoxald";

/// The client executable's file name.
pub(crate) const CLIENT_BINARY: &str = "phoxal";

/// This client's version. The daemon reports the same one when the pair is
/// exact, because one workspace version governs both packages.
pub(crate) fn client_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The directory the installed pair lives in: whatever directory this client
/// was executed from.
pub(crate) fn install_directory() -> Result<PathBuf> {
    let current = std::env::current_exe().context("failed to locate the running `phoxal`")?;
    let current = current.canonicalize().unwrap_or(current);
    current
        .parent()
        .map(Path::to_path_buf)
        .context("the running `phoxal` has no parent directory")
}

/// Resolve `phoxald`.
///
/// `PATH` is the fallback for the one case where `current_exe` cannot be
/// resolved at all; every other case answers with the sibling, present or not,
/// so that a missing sibling is reported as a broken pair rather than papered
/// over by an unrelated daemon on `PATH`.
pub(crate) fn resolve_daemon() -> PathBuf {
    match install_directory() {
        Ok(directory) => directory.join(DAEMON_BINARY),
        Err(_) => PathBuf::from(DAEMON_BINARY),
    }
}

/// What the sibling daemon turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PairStatus {
    /// Both halves are present and report the same version.
    Exact { version: String },
    /// No `phoxald` next to this `phoxal`.
    Missing { expected: PathBuf },
    /// A `phoxald` is there, but it is not this client's version.
    Mismatched { found: String, expected: String },
    /// A `phoxald` is there and could not be asked what it is.
    Unreadable { path: PathBuf, error: String },
}

impl PairStatus {
    pub(crate) const fn is_exact(&self) -> bool {
        matches!(self, Self::Exact { .. })
    }

    /// One line for `doctor`.
    pub(crate) fn summary(&self) -> String {
        match self {
            Self::Exact { version } => format!("CLI pair: phoxal and phoxald are both {version}"),
            Self::Missing { expected } => format!(
                "CLI pair: `{DAEMON_BINARY}` is missing at {}",
                expected.display()
            ),
            Self::Mismatched { found, expected } => format!(
                "CLI pair: `{DAEMON_BINARY}` is {found} but this `{CLIENT_BINARY}` is {expected}"
            ),
            Self::Unreadable { path, error } => format!(
                "CLI pair: `{DAEMON_BINARY}` at {} did not report a version: {error}",
                path.display()
            ),
        }
    }

    /// What an incomplete installation earns in a report: what is wrong with
    /// the installed archive, and how to make it whole again.
    ///
    /// It is an installation problem, so the remedy is reinstalling the CLI -
    /// never aligning a robot project with a product version.
    pub(crate) fn failure(&self) -> Option<String> {
        if self.is_exact() {
            return None;
        }
        Some(format!(
            "{}. `{CLIENT_BINARY}` and `{DAEMON_BINARY}` ship, install, and upgrade as one \
             archive, so a half-installed or mixed installation is a packaging problem. Reinstall \
             the CLI with `{CLIENT_BINARY} self upgrade`, or with `curl -fsSL \
             https://phoxal.com/install.sh | sh`",
            self.summary()
        ))
    }
}

/// Probe the sibling daemon.
pub(crate) fn status() -> PairStatus {
    let daemon = resolve_daemon();
    classify(client_version(), &daemon, probe_daemon_version(&daemon))
}

/// Ask a `phoxald` what it is.
fn probe_daemon_version(daemon: &Path) -> Result<String> {
    if !daemon.is_file() {
        bail!("not installed");
    }
    let output = Command::new(daemon)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to run {} --version", daemon.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "{} --version exited with {}",
        daemon.display(),
        output.status
    );
    let stdout = String::from_utf8(output.stdout).context("--version output was not UTF-8")?;
    parse_version_line(&stdout)
        .map(ToString::to_string)
        .with_context(|| format!("unexpected --version output {:?}", stdout.trim()))
}

/// Read `phoxald <version>`, which is the exact line the daemon prints.
fn parse_version_line(stdout: &str) -> Option<&str> {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let version = line.strip_prefix(DAEMON_BINARY)?.trim();
    (!version.is_empty()).then_some(version)
}

/// Turn a probe result into a status. Pure, so every branch is testable without
/// a daemon on disk.
fn classify(expected: &str, daemon: &Path, probed: Result<String>) -> PairStatus {
    match probed {
        Ok(found) if found == expected => PairStatus::Exact { version: found },
        Ok(found) => PairStatus::Mismatched {
            found,
            expected: expected.to_string(),
        },
        Err(_) if !daemon.is_file() => PairStatus::Missing {
            expected: daemon.to_path_buf(),
        },
        Err(error) => PairStatus::Unreadable {
            path: daemon.to_path_buf(),
            error: format!("{error:#}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_line_the_daemon_prints_is_the_one_this_parses() {
        assert_eq!(parse_version_line("phoxald 0.35.0\n"), Some("0.35.0"));
        assert_eq!(
            parse_version_line("\n  phoxald 1.2.3-rc.1  \n"),
            Some("1.2.3-rc.1")
        );
        assert_eq!(parse_version_line("phoxal 0.35.0"), None);
        assert_eq!(parse_version_line("phoxald"), None);
        assert_eq!(parse_version_line(""), None);
    }

    #[test]
    fn an_equal_version_is_the_only_exact_pair() {
        let daemon = PathBuf::from("/usr/local/bin/phoxald");
        assert_eq!(
            classify("0.35.0", &daemon, Ok("0.35.0".to_string())),
            PairStatus::Exact {
                version: "0.35.0".to_string()
            }
        );
        assert_eq!(
            classify("0.35.0", &daemon, Ok("0.34.0".to_string())),
            PairStatus::Mismatched {
                found: "0.34.0".to_string(),
                expected: "0.35.0".to_string(),
            }
        );
    }

    #[test]
    fn an_absent_sibling_is_reported_as_missing_rather_than_unreadable() {
        let temp = tempfile::tempdir().expect("temp dir");
        let daemon = temp.path().join(DAEMON_BINARY);
        assert_eq!(
            classify("0.35.0", &daemon, Err(anyhow::anyhow!("not installed"))),
            PairStatus::Missing {
                expected: daemon.clone()
            }
        );

        std::fs::write(&daemon, "not an executable").expect("write");
        let status = classify("0.35.0", &daemon, Err(anyhow::anyhow!("permission denied")));
        assert!(
            matches!(&status, PairStatus::Unreadable { path, .. } if path == &daemon),
            "{status:?}"
        );
    }

    /// Every non-exact status reports a fix, and the fix is always about the
    /// installation: reinstall the archive. A robot project is never asked to
    /// match a product version, because the project selects its own framework.
    #[test]
    fn only_an_exact_pair_has_no_failure_and_every_failure_names_the_fix() {
        assert_eq!(
            PairStatus::Exact {
                version: "0.35.0".to_string()
            }
            .failure(),
            None
        );
        for status in [
            PairStatus::Missing {
                expected: PathBuf::from("/usr/local/bin/phoxald"),
            },
            PairStatus::Mismatched {
                found: "0.34.0".to_string(),
                expected: "0.35.0".to_string(),
            },
            PairStatus::Unreadable {
                path: PathBuf::from("/usr/local/bin/phoxald"),
                error: "permission denied".to_string(),
            },
        ] {
            let failure = status
                .failure()
                .expect("an incomplete installation always reports one");
            assert!(failure.contains("packaging problem"), "{failure}");
            assert!(failure.contains("self upgrade"), "{failure}");
            assert!(
                !failure.contains("cargo update"),
                "the remedy is never a project change: {failure}"
            );
        }
    }

    /// The daemon is resolved next to this executable and never from `PATH`
    /// while `current_exe` resolves, so a stale installed daemon can never
    /// answer for a client run out of a build directory.
    #[test]
    fn the_daemon_is_resolved_beside_this_executable() {
        let directory = install_directory().expect("this test binary has a directory");
        assert_eq!(resolve_daemon(), directory.join(DAEMON_BINARY));
    }
}
