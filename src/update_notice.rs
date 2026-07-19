use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use semver::Version;
use serde::{Deserialize, Serialize};

const LATEST_RELEASE_URL: &str = "https://github.com/phoxal/phoxal-cli/releases/latest";
const RELEASE_TAG_URL: &str = "https://github.com/phoxal/phoxal-cli/releases/tag";
const USER_AGENT: &str = "phoxal-cli-update-check";
const REQUEST_TIMEOUT: Duration = Duration::from_millis(800);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const UPGRADE_COMMAND: &str = "phoxal-cli self upgrade";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CliUpdate {
    current: String,
    latest: String,
    upgrade_command: &'static str,
    release_notes_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateNotice {
    Artifacts(Vec<String>),
    Cli(CliUpdate),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NoticePolicy {
    pub(crate) artifact_consuming: bool,
    pub(crate) quiet: bool,
    pub(crate) interactive: bool,
    /// A rich session owns the terminal through its TUI. Human notices are
    /// routed into Diagnostics while it is active; stderr remains the
    /// fallback only when no session ever accepted the notice.
    pub(crate) tui: bool,
}

#[derive(Debug)]
struct InvocationState {
    policy: NoticePolicy,
    notice: Option<UpdateNotice>,
    routed_notice: Option<UpdateNotice>,
    pending_cli: Option<Receiver<Option<CliUpdate>>>,
}

fn invocation() -> &'static Mutex<Option<InvocationState>> {
    static INVOCATION: OnceLock<Mutex<Option<InvocationState>>> = OnceLock::new();
    INVOCATION.get_or_init(|| Mutex::new(None))
}

pub(crate) fn begin(policy: NoticePolicy) {
    *invocation()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(InvocationState {
        policy,
        notice: None,
        routed_notice: None,
        pending_cli: None,
    });
}

/// Offers a notice to the once-per-top-level-invocation gate.
///
/// The first non-empty notice wins. Watch rebuilds never call this because
/// their resolver options set `emit_update_notice` to false.
pub(crate) fn offer(notice: UpdateNotice) {
    let mut invocation = invocation()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = invocation.as_mut() else {
        return;
    };
    let previous = state.notice.clone();
    offer_to_state(state, notice);
    if state.notice != previous {
        route_current_to_session(state);
    }
}

/// Non-blockingly collect a pending CLI update and deliver the current notice
/// to an installed rich session. Called when the controller installs
/// Diagnostics and on redraws so both cached notices offered before controller
/// construction and background checks that finish later reach the TUI.
pub(crate) fn poll_session() {
    let mut invocation = invocation()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = invocation.as_mut() else {
        return;
    };
    poll_pending_cli(state);
    route_current_to_session(state);
}

fn route_current_to_session(state: &mut InvocationState) {
    if !state.policy.tui || state.notice == state.routed_notice {
        return;
    }
    let Some(notice) = state.notice.as_ref() else {
        return;
    };
    let result = crate::session::diagnostics::try_route(
        crate::session::event::DiagnosticSource::Cli,
        crate::session::event::DiagnosticLevel::Warn,
        &format_human(notice),
    );
    // A full/closed session channel still owns the terminal. Treat Dropped as
    // delivered so backpressure never causes a fallback stderr write that
    // corrupts the screen after teardown. NoSession is deliberately retained
    // for a later poll or the normal stderr fallback in `finish`.
    if !matches!(result, crate::session::diagnostics::RouteResult::NoSession) {
        state.routed_notice = state.notice.clone();
    }
}

fn offer_to_state(state: &mut InvocationState, notice: UpdateNotice) {
    match (&state.notice, &notice) {
        (None, _) | (Some(UpdateNotice::Cli(_)), UpdateNotice::Artifacts(_)) => {
            state.notice = Some(notice);
        }
        _ => {}
    }
}

pub(crate) fn finish() {
    let mut invocation = invocation()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(state) = invocation.as_mut() {
        poll_pending_cli(state);
    }
    let state = invocation.take();
    let Some(state) = state else {
        return;
    };
    if let Some(message) = render_human(
        state.policy,
        state.notice.as_ref(),
        state.routed_notice.as_ref(),
    ) {
        eprintln!("{message}");
    }
}

fn render_human(
    policy: NoticePolicy,
    notice: Option<&UpdateNotice>,
    routed_notice: Option<&UpdateNotice>,
) -> Option<String> {
    if !policy.artifact_consuming || policy.quiet || !policy.interactive {
        return None;
    }
    let notice = notice?;
    (Some(notice) != routed_notice).then(|| format_human(notice))
}

fn format_human(notice: &UpdateNotice) -> String {
    match notice {
        UpdateNotice::Artifacts(newer) => format!(
            "warning: newer artifact versions available: {}; run `phoxal update`",
            newer.join(", ")
        ),
        UpdateNotice::Cli(update) => format!(
            "Update available! {} -> {}, run `{}`, release notes: {}",
            update.current, update.latest, update.upgrade_command, update.release_notes_url
        ),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CacheEntry {
    checked_at_unix_secs: u64,
    latest_version: String,
    release_notes_url: String,
}

#[derive(Debug, Clone)]
struct LatestRelease {
    version: String,
    release_notes_url: String,
}

trait LatestReleaseSource: Send + Sync + 'static {
    fn fetch(&self) -> Result<LatestRelease>;
}

#[derive(Debug, Clone, Copy)]
struct GithubLatestRelease;

impl LatestReleaseSource for GithubLatestRelease {
    fn fetch(&self) -> Result<LatestRelease> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build update-check HTTP client")?;
        let response = client
            .get(LATEST_RELEASE_URL)
            .send()
            .context("failed to resolve latest phoxal-cli release")?;
        if !response.status().is_redirection() {
            bail!(
                "latest phoxal-cli release returned {} instead of a redirect",
                response.status()
            );
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .context("latest phoxal-cli release redirect has no Location")?
            .to_str()
            .context("latest phoxal-cli release Location is not UTF-8")?;
        let tag = location
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .context("latest phoxal-cli release Location has no tag")?;
        let version = parse_version(tag)
            .with_context(|| format!("latest phoxal-cli release tag '{tag}' is invalid"))?;
        Ok(LatestRelease {
            release_notes_url: format!("{RELEASE_TAG_URL}/v{version}"),
            version: version.to_string(),
        })
    }
}

pub(crate) fn start_cli_check() {
    let Some(cache_path) = crate::host_paths::cli_update_cache_path().ok() else {
        return;
    };
    let Some(now) = unix_now().ok() else {
        return;
    };

    // Fresh-cache checks are local and immediate, which guarantees that the
    // daily cached answer still displays even for a very short command.
    if let Some(cache) = read_cache(&cache_path).filter(|cache| cache_is_fresh(cache, now)) {
        if let Some(update) = update_from_release(
            env!("CARGO_PKG_VERSION"),
            &cache.latest_version,
            &cache.release_notes_url,
        ) {
            offer(UpdateNotice::Cli(update));
        }
        return;
    }

    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = check_with_source(
            &GithubLatestRelease,
            &cache_path,
            now,
            env!("CARGO_PKG_VERSION"),
        );
        let _ = sender.send(result);
    });
    if let Some(state) = invocation()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        state.pending_cli = Some(receiver);
    }
}

/// Polls only; it never waits. A slow or unreachable request remains detached
/// and cannot hold up output or process exit.
fn poll_pending_cli(state: &mut InvocationState) {
    let Some(receiver) = state.pending_cli.as_ref() else {
        return;
    };
    match receiver.try_recv() {
        Ok(Some(update)) => {
            state.pending_cli = None;
            offer_to_state(state, UpdateNotice::Cli(update));
        }
        Ok(None) | Err(TryRecvError::Disconnected) => state.pending_cli = None,
        Err(TryRecvError::Empty) => {}
    }
}

fn check_with_source(
    source: &impl LatestReleaseSource,
    cache_path: &Path,
    now: u64,
    current_version: &str,
) -> Option<CliUpdate> {
    let cached = read_cache(cache_path);
    let release = if let Some(cache) = cached.filter(|cache| cache_is_fresh(cache, now)) {
        LatestRelease {
            version: cache.latest_version,
            release_notes_url: cache.release_notes_url,
        }
    } else {
        let release = source.fetch().ok()?;
        // Reject malformed releases before making them the latest-known value.
        parse_version(&release.version).ok()?;
        let cache = CacheEntry {
            checked_at_unix_secs: now,
            latest_version: release.version.clone(),
            release_notes_url: release.release_notes_url.clone(),
        };
        let _ = write_cache(cache_path, &cache);
        release
    };
    update_from_release(
        current_version,
        &release.version,
        &release.release_notes_url,
    )
}

fn update_from_release(
    current_version: &str,
    latest_version: &str,
    release_notes_url: &str,
) -> Option<CliUpdate> {
    let current = parse_version(current_version).ok()?;
    let latest = parse_version(latest_version).ok()?;
    (latest > current).then(|| CliUpdate {
        current: current.to_string(),
        latest: latest.to_string(),
        upgrade_command: UPGRADE_COMMAND,
        release_notes_url: release_notes_url.to_string(),
    })
}

fn parse_version(raw: &str) -> Result<Version> {
    Version::parse(
        raw.trim()
            .strip_prefix('v')
            .or_else(|| raw.trim().strip_prefix('V'))
            .unwrap_or(raw.trim()),
    )
    .with_context(|| format!("invalid phoxal-cli version '{raw}'"))
}

fn cache_is_fresh(cache: &CacheEntry, now: u64) -> bool {
    now.saturating_sub(cache.checked_at_unix_secs) < CACHE_TTL.as_secs()
}

fn read_cache(path: &Path) -> Option<CacheEntry> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_cache(path: &Path, cache: &CacheEntry) -> Result<()> {
    let parent = path.parent().context("update cache path has no parent")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create update cache directory {}",
            parent.display()
        )
    })?;
    let partial = partial_cache_path(path);
    fs::write(&partial, serde_json::to_vec(cache)?)
        .with_context(|| format!("failed to write update cache {}", partial.display()))?;
    fs::rename(&partial, path)
        .with_context(|| format!("failed to activate update cache {}", path.display()))
}

fn partial_cache_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".partial");
    PathBuf::from(name)
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn policy() -> NoticePolicy {
        NoticePolicy {
            artifact_consuming: true,
            quiet: false,
            interactive: true,
            tui: false,
        }
    }

    #[test]
    fn notice_is_suppressed_for_quiet_non_interactive_and_non_artifact_sessions() {
        let notice = UpdateNotice::Artifacts(vec!["update".to_string()]);
        for suppressed in [
            NoticePolicy {
                quiet: true,
                ..policy()
            },
            NoticePolicy {
                interactive: false,
                ..policy()
            },
            NoticePolicy {
                artifact_consuming: false,
                ..policy()
            },
        ] {
            assert_eq!(render_human(suppressed, Some(&notice), None), None);
        }
    }

    #[test]
    fn cli_version_comparison_handles_newer_equal_older_and_unparseable() {
        let notes = "https://github.com/phoxal/phoxal-cli/releases/tag/v1.1.0";
        assert!(update_from_release("1.0.0", "v1.1.0", notes).is_some());
        assert!(update_from_release("1.0.0", "1.0.0", notes).is_none());
        assert!(update_from_release("1.1.0", "1.0.0", notes).is_none());
        assert!(update_from_release("1.0.0", "not-a-version", notes).is_none());
        assert!(update_from_release("not-a-version", "1.1.0", notes).is_none());
    }

    #[test]
    fn human_cli_banner_has_the_upgrade_command_and_release_notes() {
        let notice = UpdateNotice::Cli(
            update_from_release(
                "1.0.0",
                "1.1.0",
                "https://github.com/phoxal/phoxal-cli/releases/tag/v1.1.0",
            )
            .unwrap(),
        );
        assert_eq!(
            render_human(policy(), Some(&notice), None).unwrap(),
            "Update available! 1.0.0 -> 1.1.0, run `phoxal-cli self upgrade`, release notes: https://github.com/phoxal/phoxal-cli/releases/tag/v1.1.0"
        );
    }

    #[test]
    fn a_notice_is_suppressed_only_after_the_tui_received_it() {
        let notice = UpdateNotice::Artifacts(vec!["update".to_string()]);
        assert_eq!(
            render_human(
                NoticePolicy {
                    tui: true,
                    ..policy()
                },
                Some(&notice),
                Some(&notice),
            ),
            None
        );
        assert!(
            render_human(
                NoticePolicy {
                    tui: true,
                    ..policy()
                },
                Some(&notice),
                None,
            )
            .is_some(),
            "an offer made before Diagnostics is installed must retain its stderr fallback"
        );
    }

    struct FakeSource {
        calls: AtomicUsize,
        result: Result<LatestRelease, &'static str>,
    }

    impl FakeSource {
        fn release(version: &str) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                result: Ok(LatestRelease {
                    version: version.to_string(),
                    release_notes_url: format!("{RELEASE_TAG_URL}/v{version}"),
                }),
            }
        }

        fn failure(message: &'static str) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                result: Err(message),
            }
        }
    }

    impl LatestReleaseSource for FakeSource {
        fn fetch(&self) -> Result<LatestRelease> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.result {
                Ok(release) => Ok(release.clone()),
                Err(message) => bail!("{message}"),
            }
        }
    }

    fn cache_path() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".phoxal/cli-update.json");
        (temp, path)
    }

    #[test]
    fn fresh_cache_is_used_without_network() {
        let (_temp, path) = cache_path();
        write_cache(
            &path,
            &CacheEntry {
                checked_at_unix_secs: 100,
                latest_version: "1.1.0".to_string(),
                release_notes_url: format!("{RELEASE_TAG_URL}/v1.1.0"),
            },
        )
        .unwrap();
        let source = FakeSource::failure("network must not be called");

        let update = check_with_source(&source, &path, 101, "1.0.0");

        assert_eq!(source.calls.load(Ordering::SeqCst), 0);
        assert_eq!(update.unwrap().latest, "1.1.0");
    }

    #[test]
    fn stale_cache_is_refreshed_and_persisted() {
        let (_temp, path) = cache_path();
        write_cache(
            &path,
            &CacheEntry {
                checked_at_unix_secs: 100,
                latest_version: "1.1.0".to_string(),
                release_notes_url: format!("{RELEASE_TAG_URL}/v1.1.0"),
            },
        )
        .unwrap();
        let source = FakeSource::release("1.2.0");
        let now = 100 + CACHE_TTL.as_secs();

        let update = check_with_source(&source, &path, now, "1.0.0");

        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert_eq!(update.unwrap().latest, "1.2.0");
        let cache = read_cache(&path).unwrap();
        assert_eq!(cache.checked_at_unix_secs, now);
        assert_eq!(cache.latest_version, "1.2.0");
    }

    #[test]
    fn failed_or_timed_out_check_is_silent_and_non_fatal() {
        let (_temp, path) = cache_path();
        let source = FakeSource::failure("request timed out");

        let update = check_with_source(&source, &path, 100, "1.0.0");

        assert!(update.is_none());
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert!(!path.exists());
    }

    #[test]
    fn one_notice_slot_prefers_artifact_updates_over_cli_updates() {
        let mut state = InvocationState {
            policy: policy(),
            notice: None,
            routed_notice: None,
            pending_cli: None,
        };
        let cli = UpdateNotice::Cli(
            update_from_release(
                "1.0.0",
                "1.1.0",
                "https://github.com/phoxal/phoxal-cli/releases/tag/v1.1.0",
            )
            .unwrap(),
        );
        offer_to_state(&mut state, cli.clone());
        offer_to_state(
            &mut state,
            UpdateNotice::Artifacts(vec!["artifact update".to_string()]),
        );
        offer_to_state(&mut state, cli);

        assert_eq!(
            state.notice,
            Some(UpdateNotice::Artifacts(vec!["artifact update".to_string()]))
        );
    }

    #[test]
    fn pending_network_check_poll_never_waits_for_a_result() {
        let (sender, receiver) = mpsc::channel();
        let mut state = InvocationState {
            policy: policy(),
            notice: None,
            routed_notice: None,
            pending_cli: Some(receiver),
        };

        poll_pending_cli(&mut state);

        assert!(state.pending_cli.is_some());
        assert!(state.notice.is_none());
        drop(sender);
    }
}
