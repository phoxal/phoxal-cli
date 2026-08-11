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
  '(organization|framework)?#[0-9]{3,}|round[- ]?[0-9]+|WS[0-9]+|[Ff]inding [A-Z]?[0-9]+|Product decision|blocker [0-9]+|medium [0-9]+|Phase [0-9]+|Part [0-9]+|^\s*///?\.\s*$' \
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
