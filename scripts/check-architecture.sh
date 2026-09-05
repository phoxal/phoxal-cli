#!/usr/bin/env bash
set -euo pipefail

fail_if_found() {
  local message="$1"
  shift
  set +e
  rg -n "$@"
  local status=$?
  set -e
  case "${status}" in
    0)
      echo "architecture policy failed: ${message}" >&2
      exit 1
      ;;
    1) ;;
    *)
      echo "architecture policy could not be evaluated: ${message}" >&2
      exit "${status}"
      ;;
  esac
}

MANGLED_COMMENT_PATTERN='^[[:space:]]*//.*[^=[:space:]][[:space:]]\(\)'
STALE_TRACKER_COMMENT_PATTERN='(^[[:space:]]*///?[[:space:]]*\.[[:space:]]*$)|(^[[:space:]]*//.*(([Oo]rganization|[Ff]ramework|[Dd]ocs|[Ii]ssue|[Pp][Rr]|[Tt]racker)[[:space:]]*#[0-9]+|[Ff]inding[[:space:]]+[A-Z][[:alnum:]_-]*|[Rr]ound[-[:space:]]?[0-9]+|WS[0-9]+|[Pp]roduct decision|[Bb]locker[[:space:]]+[0-9]+|[Mm]edium[[:space:]]+[0-9]+|[Pp]hase[[:space:]]+[0-9]+|[Pp]art[[:space:]]+[0-9]+))'

verify_comment_policy_patterns() {
  local rejected
  local allowed

  rejected=$'// stale narration ().\n/// Finding C: history\n/// docs #21\n// tracker #42\n'
  allowed=$'// `Config = ()`\n// `Config  = ()`\n// backticked `()`\n// resolve()\n// Instant::now()\n'

  if ! printf '%s' "${rejected}" | rg -q "${MANGLED_COMMENT_PATTERN}"; then
    echo "architecture policy self-check failed: mangled-comment pattern missed a fixture" >&2
    exit 1
  fi
  if printf '%s' "${allowed}" | rg -q "${MANGLED_COMMENT_PATTERN}"; then
    echo "architecture policy self-check failed: mangled-comment pattern rejected valid Rust notation" >&2
    exit 1
  fi
  if ! printf '%s' "${rejected}" | rg -q "${STALE_TRACKER_COMMENT_PATTERN}"; then
    echo "architecture policy self-check failed: stale-tracker pattern missed a fixture" >&2
    exit 1
  fi
}

verify_comment_policy_patterns

# Keep the forbidden spellings out of this policy file itself: joining shell
# fragments produces the real search expression only at runtime. The scan can
# then cover the whole active repository without exempting its own source.
RETIRED_IDENTIFIER_PATTERN='phoxal''d|phoxal-''api|phoxal_''api|phoxal-cli-''(client|supervisor)|crates/''client-lib|phoxal\.service'
RETIRED_SIMULATION_PATTERN='PHOXAL_SIMULATOR_''WEBOTS_PATH|webots-''proto|WEBOTS_CONTROLLER_''(PACKAGE|VERSION)|prepare_''simulation|stage_''webots|simulation webots ''run'
NATIVE_CONTROLLER_ESCAPE='<''extern>'

for required_manifest in cli/Cargo.toml; do
  if [[ ! -f "${required_manifest}" ]]; then
    echo "architecture policy failed: missing top-level package ${required_manifest}" >&2
    exit 1
  fi
done
for retired_path in client "crates/client"-lib supervisor phoxal-client; do
  if [[ -e "${retired_path}" ]]; then
    echo "architecture policy failed: retired package path ${retired_path} still exists" >&2
    exit 1
  fi
done

fail_if_found "the retired launch environment ABI must not return" \
  'LaunchEnv|EncodedParticipantEnv|encode_participant_env|PHOXAL_(EXECUTION|PARTICIPANT|ROBOT|BUNDLE|CONNECT)' \
  cli crates
fail_if_found "runtime identity is RobotId plus execution-scoped participant identities" \
  'RobotNamespace|RobotIdentity|RobotKey|ParticipantInstanceKey' cli crates
fail_if_found "attachments must not reconstruct authored source" \
  'phoxal::authoring|robot\.yaml' cli/src/attach
fail_if_found "raw Zenoh is owned below the typed bus contract" \
  '(^|[^[:alnum:]_])zenoh::' cli crates
fail_if_found "tracker history belongs in GitHub, not Rust source" \
  "${STALE_TRACKER_COMMENT_PATTERN}" \
  cli crates --glob '*.rs'
fail_if_found "comment-only mangled empty-parenthetical narration must not return" \
  "${MANGLED_COMMENT_PATTERN}" \
  cli crates --glob '*.rs'
fail_if_found "the application package stays bin-only" '^\[lib\]' cli/Cargo.toml
fail_if_found "the retired catch-all core crate must not return" \
  'phoxal-cli-core|phoxal_cli_core' Cargo.toml cli crates release-plz.toml
fail_if_found "the retired CLI-owned supervisor topology must not return" \
  '([Dd]aemon([^[:alnum:]_-]|$)|DaemonEnded|stop_daemon|CLI pair|sibling (supervisor|executable))' \
  cli crates --glob '*.rs'
fail_if_found "retired identifiers must not remain outside immutable history and exact cleanup owners" \
  "${RETIRED_IDENTIFIER_PATTERN}" . \
  --glob '!**/CHANGELOG.md' \
  --glob '!cli/src/application/service.rs' \
  --glob '!crates/host/src/paths.rs'
fail_if_found "the retired CLI-owned Webots path must not return" \
  "${RETIRED_SIMULATION_PATTERN}" . \
  --glob '!**/CHANGELOG.md'
fail_if_found "production controllers must be generated inside the adapter host" \
  "${NATIVE_CONTROLLER_ESCAPE}" cli crates \
  --glob '*.rs'

# The framework is one library. The retired internal packages and the extracted
# client crate must not come back as a dependency of anything here; which
# framework packages actually resolve, and under which consumer profiles, is
# proven against `cargo metadata` in `cli/tests/framework_boundary.rs`.
fail_if_found "the framework is consumed as one library" \
  '^phoxal-(protocol|bus|bundle|manifest|model|runtime-contract|client) *=|phoxal_(protocol|bus|bundle|manifest|model|runtime_contract|client)' \
  Cargo.toml cli crates

# Raw transport ownership is the framework's and is `pub(crate)` there. Naming
# one of these would mean the boundary had been reopened.
fail_if_found "session consumers must not reach for raw transport ownership" \
  'BusOwner|BusConfig|BusHandle|\.bus\(\)' \
  cli/src crates/ui/src crates/observation/src
