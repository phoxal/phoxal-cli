#!/usr/bin/env bash
# Live Webots gate against the framework-owned hello-rover example.
#
# Proves the separated repos still resolve together:
#
#   robot.yaml -> source-only resolution -> fresh Webots project -> resident
#   supervision until Ctrl-C.
#
# Usage:
#   scripts/live-simulate-gate.sh [ROBOT_DIR]
#
# ROBOT_DIR defaults to ../framework/examples/hello-rover in the sibling-clone
# layout. WORLD is default.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_REPO="$(cd "${SCRIPT_DIR}/.." && pwd)"

robot_dir=""
for arg in "$@"; do
  case "${arg}" in
    -*) echo "unknown flag: ${arg}" >&2; exit 2 ;;
    *) robot_dir="${arg}" ;;
  esac
done
robot_dir="${robot_dir:-${CLI_REPO}/../framework/examples/hello-rover}"
WORLD="default"

red="\033[31m"; green="\033[32m"; cyan="\033[36m"; reset="\033[0m"
step() { printf "\n${cyan}> %s${reset}\n" "$1"; }
ok()   { printf "${green}OK${reset}   %s\n" "$1"; }
fail() { printf "${red}FAIL${reset} %s\n" "$1" >&2; exit 1; }

# --- resolve target + CLI binary -------------------------------------------

[[ -f "${robot_dir}/robot.yaml" ]] \
  || fail "no robot.yaml in ${robot_dir} (pass ROBOT_DIR; expected framework/examples/hello-rover)"
robot_dir="$(cd "${robot_dir}" && pwd)"

CLI_BIN="${CLI_REPO}/target/debug/phoxal"
if [[ ! -x "${CLI_BIN}" ]]; then
  step "building phoxal"
  (cd "${CLI_REPO}" && cargo build --quiet -p phoxal-cli) || fail "phoxal-cli build failed"
fi

# --- live gate -------------------------------------------------------------

step "Gate -- hello-rover: phoxal simulation webots run ${WORLD} (Ctrl-C after inspection)"
if ! (cd "${robot_dir}" && "${CLI_BIN}" simulation webots run "${WORLD}"); then
  fail "simulation webots run ${WORLD} failed"
fi
ok "live staged simulation shut down cleanly"
