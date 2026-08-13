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

fail_if_found "phoxald must not depend on authored-source or catalog crates" \
  'phoxal-(manifest|cli-catalog)|cargo_metadata' supervisor/Cargo.toml supervisor/src
fail_if_found "the retired launch environment ABI must not return" \
  'LaunchEnv|EncodedParticipantEnv|encode_participant_env|PHOXAL_(EXECUTION|PARTICIPANT|ROBOT|BUNDLE|CONNECT)' \
  client supervisor crates
fail_if_found "runtime identity is RobotId plus execution-scoped participant identities" \
  'RobotNamespace|RobotIdentity|RobotKey|ParticipantInstanceKey' client supervisor crates
fail_if_found "attachments must not reconstruct authored source" \
  'phoxal_manifest|robot\.yaml' client/src/attach
fail_if_found "raw Zenoh is owned below the typed bus contract" \
  '(^|[^[:alnum:]_])zenoh::' client supervisor crates
fail_if_found "tracker history belongs in GitHub, not Rust source" \
  "${STALE_TRACKER_COMMENT_PATTERN}" \
  client supervisor crates --glob '*.rs'
fail_if_found "comment-only mangled empty-parenthetical narration must not return" \
  "${MANGLED_COMMENT_PATTERN}" \
  client supervisor crates --glob '*.rs'
fail_if_found "the two application packages stay bin-only" \
  '^\[lib\]' client/Cargo.toml supervisor/Cargo.toml
# The remote protocol has one owner. `phoxal` and the crates it renders with
# reach a running robot through `phoxal-client` and never name a wire crate,
# so a protocol change lands in one crate instead of five. `phoxald` serves
# that protocol and keeps its own direct wire dependencies.
fail_if_found "remote protocol ownership is phoxal-client's" \
  '^phoxal-(api|bus) *=' client/Cargo.toml crates/ui/Cargo.toml crates/observation/Cargo.toml
fail_if_found "the retired catch-all core crate must not return" \
  'phoxal-cli-core|phoxal_cli_core' Cargo.toml client supervisor crates release-plz.toml
