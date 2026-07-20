//! General git-artifact resolver: a shallow, deterministic on-disk checkout
//! for ANY `robot.yaml` `artifacts.pins` entry pinned to a git source
//! (`phoxal::model::robot::v0::ArtifactPin::Git`), not just components.
//!
//! Lives under the project's `.phoxal/git/<hash>` (see
//! [`crate::host_paths::git_artifacts_dir`]), one directory per distinct
//! `(url, rev)` pair so the same pin never spawns more than one checkout.
//!
//! A git-sourced artifact is a LOCAL source, exactly like a `path:` pin: it
//! never enters the official binary store. Its contracts are recomputed live
//! every run by building it and reading its compiled-in
//! `#[derive(phoxal::Api)]` metadata section, never by executing it (see
//! `crate::check::build_emit_apis_from_source`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::host_paths;
use crate::resolver::is_full_commit_sha;
use crate::shell;

/// Deterministic cache-dir name for a resolved git source: the hex sha256 of
/// `<url>\n<rev>`. Keying on both means the SAME pin (same url + rev) always
/// reuses one directory, while a different rev of the same repo (or the same
/// rev pinned from a different fork URL) gets its own directory - a name
/// derived from the URL alone would incorrectly collide those.
fn cache_key(url: &str, rev: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hasher.update(b"\n");
    hasher.update(rev.as_bytes());
    hex::encode(hasher.finalize())
}

/// The deterministic on-disk checkout directory for `(url, rev)`, whether or
/// not it has been cloned yet.
pub fn cache_dir(url: &str, rev: &str) -> Result<PathBuf> {
    Ok(host_paths::git_artifacts_dir()?.join(cache_key(url, rev)))
}

/// Ensure a shallow checkout of `url` at commit `rev` exists under
/// `.phoxal/git/<hash>`, cloning it if missing; idempotent - reuses an
/// existing checkout without touching the network again. `rev` must already
/// be a resolved full commit SHA (see `resolver::resolve_git_ref`): the
/// checkout is content-addressed by the exact commit, so an unresolved
/// branch/tag name would not be a stable cache key.
///
/// The clone is a genuinely shallow, single-commit fetch - `git init` + `git
/// fetch --depth 1 origin <rev>` + `git checkout --detach FETCH_HEAD` - rather
/// than `git clone --depth 1 --single-branch --branch <rev>`: the latter only
/// works when `rev` names a branch/tag, but every caller here has already
/// resolved `rev` to a full commit SHA, and shallow-fetching a bare SHA needs
/// the fetch-by-commit form (works against GitHub and any server with
/// `uploadpack.allowReachableSHA1InWant` / `allowAnySHA1InWant` enabled).
/// Stages into a tempdir alongside the destination and atomically renames it
/// into place so a crash or a concurrent `phoxal-cli` invocation never leaves
/// (or observes) a half-written checkout.
pub fn ensure_git_artifact(url: &str, rev: &str) -> Result<PathBuf> {
    if !is_full_commit_sha(rev) {
        bail!(
            "git artifact {url} must be resolved to a full commit SHA before staging (got '{rev}'); \
             this is an internal resolver invariant - if you are seeing this from a \
             `robot.yaml` git pin, run with network access so the ref can resolve, or pin an \
             explicit commit SHA"
        );
    }
    let dest = cache_dir(url, rev)?;
    if dest.join(".git").is_dir() {
        return Ok(dest);
    }
    if dest.exists() {
        bail!(
            "git artifact cache {} already exists but is not a git checkout",
            dest.display()
        );
    }

    let parent = dest
        .parent()
        .context("git artifact cache path did not have a parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create git artifact cache {}", parent.display()))?;
    let staging = tempfile::TempDir::new_in(parent)
        .with_context(|| format!("failed to create staging checkout in {}", parent.display()))?;
    run_shallow_clone_commands(url, rev, staging.path())?;

    match std::fs::rename(staging.path(), &dest) {
        Ok(()) => {}
        Err(error) if dest.join(".git").is_dir() => {
            tracing::debug!(
                "git artifact cache {} appeared during clone; keeping existing destination ({error})",
                dest.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to move git artifact cache to {}", dest.display())
            });
        }
    }
    Ok(dest)
}

/// The exact argv sequence [`ensure_git_artifact`] runs to shallow-clone
/// `url` at `rev` into `dest` - pulled out as a pure function so tests can
/// assert the command shape without actually invoking `git`/the network.
fn shallow_clone_commands(url: &str, rev: &str, dest: &Path) -> Vec<Vec<String>> {
    let dest = dest.to_string_lossy().to_string();
    vec![
        vec!["init".to_string(), "--quiet".to_string(), dest],
        vec![
            "remote".to_string(),
            "add".to_string(),
            "origin".to_string(),
            url.to_string(),
        ],
        vec![
            "fetch".to_string(),
            "--depth".to_string(),
            "1".to_string(),
            "--quiet".to_string(),
            "origin".to_string(),
            rev.to_string(),
        ],
        vec![
            "checkout".to_string(),
            "--quiet".to_string(),
            "--detach".to_string(),
            "FETCH_HEAD".to_string(),
        ],
    ]
}

fn run_shallow_clone_commands(url: &str, rev: &str, dest: &Path) -> Result<()> {
    for (index, args) in shallow_clone_commands(url, rev, dest)
        .into_iter()
        .enumerate()
    {
        // `git init` runs with no cwd (it takes the destination as an arg,
        // since the dir doesn't exist as a git repo yet); every later step
        // runs inside the now-initialized `dest`.
        let cwd = (index > 0).then_some(dest);
        shell::run_status("git", args, cwd)
            .with_context(|| format!("failed to shallow-fetch {rev} from {url}"))?;
    }
    Ok(())
}

/// Resolve an optional subdirectory within a checked-out (or path-pinned)
/// source tree. Rejects an absolute path or one containing `..` - the caller
/// gets a clear error instead of a checkout escaping its own root.
pub fn subdir(root: PathBuf, directory: Option<&Path>) -> Result<PathBuf> {
    let Some(directory) = directory else {
        return Ok(root);
    };
    for part in directory.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            bail!(
                "git artifact subdirectory {} must be a relative path without '..'",
                directory.display()
            );
        }
    }
    Ok(root.join(directory))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_is_deterministic_and_keyed_by_url_and_rev() -> Result<()> {
        let _guard = crate::host_paths::test_support::ScratchPhoxalHome::new()?;
        let sha_a = "0123456789abcdef0123456789abcdef01234567";
        let sha_b = "abcdef0123456789abcdef0123456789abcdef01";

        let a1 = cache_dir("https://github.com/phoxal/framework", sha_a)?;
        let a2 = cache_dir("https://github.com/phoxal/framework", sha_a)?;
        assert_eq!(a1, a2, "the same (url, rev) must reuse one directory");

        let b = cache_dir("https://github.com/phoxal/framework", sha_b)?;
        assert_ne!(a1, b, "a different rev must get its own directory");

        let fork = cache_dir("https://github.com/someone/fork", sha_a)?;
        assert_ne!(
            a1, fork,
            "the same rev pinned from a different url must get its own directory"
        );

        assert_eq!(
            a1.parent(),
            Some(host_paths::git_artifacts_dir()?.as_path())
        );
        Ok(())
    }

    #[test]
    fn shallow_clone_uses_depth_one_fetch_by_commit_not_branch_clone() {
        let dest = Path::new("/tmp/example-dest");
        let commands = shallow_clone_commands(
            "https://github.com/phoxal/framework",
            "0123456789abcdef0123456789abcdef01234567",
            dest,
        );
        assert_eq!(commands.len(), 4);
        assert_eq!(commands[0][0], "init");
        assert_eq!(
            commands[1],
            [
                "remote",
                "add",
                "origin",
                "https://github.com/phoxal/framework"
            ]
        );
        assert_eq!(
            commands[2],
            [
                "fetch",
                "--depth",
                "1",
                "--quiet",
                "origin",
                "0123456789abcdef0123456789abcdef01234567"
            ]
        );
        assert_eq!(
            commands[3],
            ["checkout", "--quiet", "--detach", "FETCH_HEAD"]
        );
        assert!(
            commands
                .iter()
                .all(|command| !command.contains(&"--single-branch".to_string())),
            "a bare commit SHA cannot be cloned with --branch; the shallow fetch-by-commit form is required"
        );
    }

    #[test]
    fn ensure_git_artifact_rejects_an_unresolved_rev() {
        let error = ensure_git_artifact("https://github.com/phoxal/framework", "main")
            .expect_err("a non-SHA rev must be rejected before touching the network");
        assert!(error.to_string().contains("full commit SHA"), "{error}");
    }
}
