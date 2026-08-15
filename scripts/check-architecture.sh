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

for required_manifest in cli/Cargo.toml phoxal-client/Cargo.toml; do
  if [[ ! -f "${required_manifest}" ]]; then
    echo "architecture policy failed: missing top-level package ${required_manifest}" >&2
    exit 1
  fi
done
for retired_path in client "crates/client"-lib supervisor; do
  if [[ -e "${retired_path}" ]]; then
    echo "architecture policy failed: retired package path ${retired_path} still exists" >&2
    exit 1
  fi
done

fail_if_found "the retired launch environment ABI must not return" \
  'LaunchEnv|EncodedParticipantEnv|encode_participant_env|PHOXAL_(EXECUTION|PARTICIPANT|ROBOT|BUNDLE|CONNECT)' \
  cli phoxal-client crates
fail_if_found "runtime identity is RobotId plus execution-scoped participant identities" \
  'RobotNamespace|RobotIdentity|RobotKey|ParticipantInstanceKey' cli phoxal-client crates
fail_if_found "attachments must not reconstruct authored source" \
  'phoxal_manifest|robot\.yaml' cli/src/attach
fail_if_found "raw Zenoh is owned below the typed bus contract" \
  '(^|[^[:alnum:]_])zenoh::' cli phoxal-client crates
fail_if_found "tracker history belongs in GitHub, not Rust source" \
  "${STALE_TRACKER_COMMENT_PATTERN}" \
  cli phoxal-client crates --glob '*.rs'
fail_if_found "comment-only mangled empty-parenthetical narration must not return" \
  "${MANGLED_COMMENT_PATTERN}" \
  cli phoxal-client crates --glob '*.rs'
fail_if_found "the application package stays bin-only" '^\[lib\]' cli/Cargo.toml
# The remote protocol has one owner. `phoxal` and the crates it renders with
# reach a running robot through `phoxal-client` and never name a wire crate.
fail_if_found "remote protocol ownership is phoxal-client's" \
  '^phoxal-(protocol|bus) *=' cli/Cargo.toml crates/ui/Cargo.toml crates/observation/Cargo.toml
fail_if_found "the retired catch-all core crate must not return" \
  'phoxal-cli-core|phoxal_cli_core' Cargo.toml cli phoxal-client crates release-plz.toml
fail_if_found "the retired CLI-owned supervisor topology must not return" \
  '([Dd]aemon([^[:alnum:]_-]|$)|DaemonEnded|stop_daemon|CLI pair|sibling (supervisor|executable))' \
  cli phoxal-client crates --glob '*.rs'
fail_if_found "retired identifiers must not remain outside immutable history and exact cleanup owners" \
  "${RETIRED_IDENTIFIER_PATTERN}" . \
  --glob '!**/CHANGELOG.md' \
  --glob '!cli/src/application/service.rs' \
  --glob '!crates/host/src/paths.rs'

# `phoxal-client` is a reusable application boundary, not a second CLI layer.
fail_if_found "phoxal-client must remain extraction-ready" \
  '^[[:space:]]*[[:alnum:]_-]+[[:space:]]*=.*path[[:space:]]*=|phoxal-cli-' \
  phoxal-client/Cargo.toml
fail_if_found "the retired attachment and public transport surfaces must not return" \
  'Attachment(Port|Config)?|AttachError|pub[[:space:]]+mod[[:space:]]+transport' \
  phoxal-client/src phoxal-client/tests
fail_if_found "phoxal-client must not expose raw session construction or access" \
  '^[[:space:]]*pub([[:space:]]+use)?[^;]*(BusOwner|BusConfig|BusHandle)|pub[[:space:]]+(async[[:space:]]+)?fn[[:space:]]+(bus|session)[[:space:]]*\(' \
  phoxal-client/src
fail_if_found "phoxal-client must not contain CLI policy or remediation" \
  'phoxal self upgrade|phoxal (attach|run|deploy|install|rollback)|systemd|joypad|TUI|exit code|progress reporting' \
  phoxal-client/src phoxal-client/tests

# UI and observation consumers must use `phoxal-client`; bypassing it would
# couple application policy to the wire owner and defeat extraction.
fail_if_found "remote client consumers must not bypass phoxal-client" \
  'phoxal_(bus|protocol)::|phoxal_client::transport|\.bus\(\)' \
  cli/src crates/ui/src crates/observation/src
