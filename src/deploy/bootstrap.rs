//! Remote helper, sudoers, bootstrap, and operator reporting.

use super::{
    ACTIVE_ROOT, BootstrapScripts, DOWNLOAD_RETRIES, DeployReport, HELPER_PATH, HealthReport,
    IDENTITY_DIR, OPT_ROOT, PAYLOAD_STAGING_PREFIX, RELEASES_ROOT, RenderedPayload, SUDOERS_PATH,
    SYSTEMD_DIR,
};
use anyhow::Result;
use anyhow::bail;
use std::collections::BTreeSet;

pub(crate) fn helper_script() -> String {
    format!(
        r#"#!/bin/sh
set -eu
unit_dir="{SYSTEMD_DIR}"
opt_root="{OPT_ROOT}"
releases_root="{RELEASES_ROOT}"

valid_unit() {{
  case "$1" in
    phoxal.target|phoxal-router.service|phoxal-tool-*.service|phoxal-participant-*.service) ;;
    *) return 1 ;;
  esac
  case "$1" in
    *[!A-Za-z0-9_.@-]*) return 1 ;;
  esac
  return 0
}}

valid_stage_suffix() {{
  case "$1" in
    ""|*[!A-Za-z0-9_.@-]*) return 1 ;;
  esac
  return 0
}}

valid_payload_source() {{
  case "$1" in
    {PAYLOAD_STAGING_PREFIX}*) ;;
    *) return 1 ;;
  esac
  suffix="${{1#{PAYLOAD_STAGING_PREFIX}}}"
  valid_stage_suffix "$suffix"
}}

valid_generation() {{
  case "$1" in
    ""|*[!A-Fa-f0-9]*) return 1 ;;
  esac
  [ "${{#1}}" -eq 16 ]
}}

valid_name() {{
  case "$1" in
    ""|*[!A-Za-z0-9_.@+-]*) return 1 ;;
  esac
}}

release_partial() {{
  printf '%s/%s.partial' "$releases_root" "$1"
}}

case "${{1:-}}" in
  prepare-release)
    source="${{2:-}}"
    generation="${{3:-}}"
    valid_payload_source "$source" || exit 64
    valid_generation "$generation" || exit 64
    test -d "$source"
    partial="$(release_partial "$generation")"
    install -d -o phoxal -g phoxal -m 0755 "$opt_root" "$releases_root"
    rm -rf "$partial"
    install -d -o phoxal -g phoxal -m 0755 "$partial"
    cp -a "$source/." "$partial/"
    chown -R phoxal:phoxal "$partial"
    ;;
  download-artifact)
    generation="${{2:-}}"
    expected_size="${{3:-}}"
    expected_sha="${{4:-}}"
    archive_binary="${{5:-}}"
    install_binary="${{6:-}}"
    valid_generation "$generation" || exit 64
    valid_name "$archive_binary" || exit 64
    valid_name "$install_binary" || exit 64
    case "$expected_size" in ""|*[!0-9]*) exit 64 ;; esac
    case "$expected_sha" in ""|*[!a-f0-9]*) exit 64 ;; esac
    [ "${{#expected_sha}}" -eq 64 ]
    url="$(cat)"
    case "$url" in https://*) ;; *) exit 64 ;; esac
    partial_root="$(release_partial "$generation")"
    test -d "$partial_root"
    downloads="$partial_root/.downloads"
    install -d -o phoxal -g phoxal -m 0755 "$downloads" "$partial_root/bin"
    partial="$downloads/$expected_sha.partial"
    archive="$downloads/$expected_sha.archive"
    unpack="$downloads/$expected_sha.unpack"
    rm -f "$partial" "$archive"
    rm -rf "$unpack"
    curl --fail --location --silent --show-error --retry {curl_retries} --retry-all-errors --connect-timeout 10 --max-time 120 --output "$partial" "$url"
    actual_size="$(wc -c < "$partial" | tr -d ' ')"
    [ "$actual_size" = "$expected_size" ]
    actual_sha="$(sha256sum "$partial" | awk '{{print $1}}')"
    [ "$actual_sha" = "$expected_sha" ]
    mv "$partial" "$archive"
    install -d -o phoxal -g phoxal -m 0755 "$unpack"
    tar -xf "$archive" -C "$unpack"
    binary="$(find "$unpack" -type f -name "$archive_binary" -print -quit)"
    test -n "$binary"
    install -o phoxal -g phoxal -m 0755 "$binary" "$partial_root/bin/$install_binary"
    rm -rf "$archive" "$unpack"
    ;;
  activate-release)
    generation="${{2:-}}"
    valid_generation "$generation" || exit 64
    partial="$(release_partial "$generation")"
    release="$releases_root/$generation"
    test -d "$partial"
    test ! -e "$release"
    rm -rf "$partial/.downloads"
    mv "$partial" "$release"
    old="$(readlink "$opt_root/active" 2>/dev/null || true)"
    if [ -n "$old" ]; then
      ln -s "$old" "$opt_root/.previous.partial"
      mv -Tf "$opt_root/.previous.partial" "$opt_root/previous"
    fi
    ln -s "releases/$generation" "$opt_root/.active.partial"
    mv -Tf "$opt_root/.active.partial" "$opt_root/active"
    ;;
  rollback-release)
    previous="$(readlink "$opt_root/previous" 2>/dev/null || true)"
    test -n "$previous"
    failed="$(readlink "$opt_root/active")"
    ln -s "$previous" "$opt_root/.active.partial"
    mv -Tf "$opt_root/.active.partial" "$opt_root/active"
    ln -s "$failed" "$opt_root/.previous.partial"
    mv -Tf "$opt_root/.previous.partial" "$opt_root/previous"
    systemctl daemon-reload
    ;;
  install-unit)
    unit="${{2:-}}"
    valid_unit "$unit"
    ln -sfn "{ACTIVE_ROOT}/systemd/$unit" "$unit_dir/$unit"
    ;;
  remove-unit)
    unit="${{2:-}}"
    valid_unit "$unit"
    rm -f "$unit_dir/$unit"
    ;;
  enable-unit)
    unit="${{2:-}}"
    valid_unit "$unit"
    systemctl enable "$unit"
    ;;
  disable-unit)
    unit="${{2:-}}"
    valid_unit "$unit"
    systemctl disable "$unit" || true
    ;;
  daemon-reload)
    systemctl daemon-reload
    ;;
  restart-target)
    systemctl reset-failed 'phoxal*' || true
    systemctl restart phoxal.target
    ;;
  *)
    exit 64
    ;;
esac
"#,
        curl_retries = DOWNLOAD_RETRIES - 1,
    )
}

/// The grant is a static group rule, not a per-user line: every deploying
/// user is enrolled into the `phoxal-deploy` group instead of the sudoers
/// fragment naming a user directly, so a second operator's deploy no longer
/// silently revokes the first operator's grant by rewriting the fragment.
pub(crate) fn sudoers_fragment() -> String {
    format!("%phoxal-deploy ALL=(root) NOPASSWD: {HELPER_PATH} *\n")
}

/// A conservative allowlist for a username interpolated directly into the
/// bootstrap shell script (`usermod -aG phoxal-deploy <remote_user>`):
/// ASCII alphanumerics plus `_ . @ -`, non-empty. This is intentionally
/// stricter than what some systems permit for usernames - it only needs to
/// admit real remote-login usernames, not every value `useradd` would
/// accept.
pub(crate) fn validate_remote_username(remote_user: &str) -> Result<()> {
    let valid = !remote_user.is_empty()
        && remote_user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'@' | b'-'));
    if !valid {
        bail!(
            "DeployInvalidRemoteUser: {remote_user:?} is not a valid remote username (expected \
             ASCII letters, digits, or the characters _ . @ -, non-empty); refusing to bootstrap \
             the phoxal-deploy group grant for it"
        );
    }
    Ok(())
}

pub(crate) fn bootstrap_script(scripts: &BootstrapScripts) -> String {
    format!(
        r#"set -eu
if ! getent group phoxal >/dev/null; then
  groupadd --system phoxal
fi
if ! id phoxal >/dev/null 2>&1; then
  useradd --system --gid phoxal --home-dir /var/lib/phoxal --create-home --shell /usr/sbin/nologin phoxal
fi
if ! getent group phoxal-deploy >/dev/null; then
  groupadd --system phoxal-deploy
fi
usermod -aG phoxal-deploy -- {remote_user}
install -d -o phoxal -g phoxal -m 0755 {OPT_ROOT} {RELEASES_ROOT}
install -d -o phoxal -g phoxal -m 0700 {IDENTITY_DIR}
install -d -o phoxal -g phoxal -m 0755 /var/lib/phoxal
cat > {HELPER_PATH} <<'PHOXAL_HELPER'
{helper}
PHOXAL_HELPER
chown root:root {HELPER_PATH}
chmod 0755 {HELPER_PATH}
cat > {SUDOERS_PATH} <<'PHOXAL_SUDOERS'
{sudoers}
PHOXAL_SUDOERS
chown root:root {SUDOERS_PATH}
chmod 0440 {SUDOERS_PATH}
{HELPER_PATH} daemon-reload
systemctl enable phoxal.target || true
"#,
        remote_user = scripts.remote_user,
        helper = scripts.helper_script,
        sudoers = scripts.sudoers_fragment,
    )
}

pub(crate) fn stale_units(installed: &[String], desired: &[String]) -> Vec<String> {
    let desired = desired.iter().map(String::as_str).collect::<BTreeSet<_>>();
    installed
        .iter()
        .filter(|unit| managed_unit_name(unit))
        .filter(|unit| !desired.contains(unit.as_str()))
        .cloned()
        .collect()
}

pub(crate) fn managed_unit_name(unit: &str) -> bool {
    unit == "phoxal.target"
        || unit == "phoxal-router.service"
        || unit
            .strip_prefix("phoxal-tool-")
            .and_then(|rest| rest.strip_suffix(".service"))
            .is_some_and(phoxal_cli_core::project::resolver::is_launch_id)
        || unit
            .strip_prefix("phoxal-participant-")
            .and_then(|rest| rest.strip_suffix(".service"))
            .is_some_and(phoxal_cli_core::project::resolver::is_launch_id)
}

pub(crate) fn report_from_payload(
    mode: &'static str,
    payload: RenderedPayload,
    health: Option<HealthReport>,
) -> DeployReport {
    DeployReport {
        mode,
        target_arch: payload.target.arch,
        official_target_triple: payload.target.official_triple,
        local_target_triple: payload.target.local_triple,
        payload_root: payload.root.path().to_path_buf(),
        install_plan: payload.install_plan,
        rendered_units: payload.rendered_units,
        env_files: payload.env_files,
        release_json: payload.release_json,
        delivery: payload.delivery,
        health,
    }
}

pub(crate) fn report(report: DeployReport) -> Result<()> {
    println!("mode: {}", report.mode);
    println!("target_arch: {}", report.target_arch);
    println!("official_target: {}", report.official_target_triple);
    println!("local_target: {}", report.local_target_triple);
    println!("payload_root: {}", report.payload_root.display());
    println!("install plan:");
    println!("{}", serde_json::to_string_pretty(&report.install_plan)?);
    println!("rendered units:");
    for (path, contents) in &report.rendered_units {
        println!("--- {path}");
        print!("{contents}");
    }
    println!("env files:");
    for (path, contents) in &report.env_files {
        println!("--- {path}");
        print!("{contents}");
    }
    println!("release.json:");
    println!("{}", serde_json::to_string_pretty(&report.release_json)?);
    if let Some(delivery) = report.delivery {
        println!("official_delivery: {delivery:?}");
    }
    if let Some(health) = &report.health {
        println!("health:");
        println!("{}", serde_json::to_string_pretty(health)?);
    }
    Ok(())
}

pub(crate) fn format_health_failure(report: &HealthReport) -> String {
    let mut message = String::from("HealthReportFailed:");
    for unit in report.units.iter().filter(|unit| !unit.ready) {
        message.push_str("\n  - ");
        if let Some(participant) = &unit.participant {
            message.push_str(participant);
            message.push_str(" (");
            message.push_str(&unit.unit);
            message.push(')');
        } else {
            message.push_str(&unit.unit);
        }
        message.push_str(": ");
        message.push_str(&unit.active_state);
        if !unit.sub_state.is_empty() {
            message.push('/');
            message.push_str(&unit.sub_state);
        }
        if !unit.journal_excerpt.is_empty() {
            message.push_str("\n    journal:");
            for line in &unit.journal_excerpt {
                message.push_str("\n      ");
                message.push_str(line);
            }
        }
    }
    message
}
