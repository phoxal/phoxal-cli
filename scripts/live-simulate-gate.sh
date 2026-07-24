#!/usr/bin/env bash
# Live split-recovery gate against the framework-owned hello-rover example.
#
# Proves the separated repos still resolve together:
#
#   robot.yaml -> phoxal -> live resolution (native artifact suite, git
#             component commits, host tools) -> dry-run launch report
#
# There is NO lockfile: every run resolves live. Production reproducibility
# belongs to the native deploy release artifact, exercised separately. This gate
# exercises live resolve without writing local launch directories.
#
# Two phases:
#
#   Smoke (default):
#     1. phoxal simulation run default --dry-run (live resolve + no writes)
#     2. assert no .phoxal/build or .phoxal/webots directory was generated
#
#   Live (--live):
#     launch the full staged Webots simulation and supervise it until Ctrl-C.
#
# The smoke phase is CI-safe and proves the resolver still lines up with the
# configured artifact suite.
#
# Usage:
#   scripts/live-simulate-gate.sh [--live] [ROBOT_DIR]
#
# ROBOT_DIR defaults to ../framework/examples/hello-rover in the sibling-clone
# layout. WORLD is default.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_REPO="$(cd "${SCRIPT_DIR}/.." && pwd)"

live=0
robot_dir=""
for arg in "$@"; do
  case "${arg}" in
    --live) live=1 ;;
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

# --- 1. live dry-run (resolve, no launch-directory writes) ------------------

step "Gate -- hello-rover: phoxal simulation run ${WORLD} --dry-run (live resolve)"
if ! (cd "${robot_dir}" && rm -rf .phoxal/build .phoxal/webots \
        && "${CLI_BIN}" simulation run "${WORLD}" --dry-run >/dev/null); then
  fail "simulation run ${WORLD} --dry-run failed (live resolution, or missing world
  ${WORLD}.wbt). If a git component ref cannot be resolved offline, pin it to a
  commit SHA in robot.yaml or run with network access."
fi
[[ ! -e "${robot_dir}/.phoxal/build" ]] || fail "dry-run wrote .phoxal/build"
[[ ! -e "${robot_dir}/.phoxal/webots" ]] || fail "dry-run wrote .phoxal/webots"
ok "dry-run resolved without local launch-directory writes"

if [[ "${live}" -eq 0 ]]; then
  printf "\n${green}Smoke gate green.${reset} Live resolve dry-run verified.\n"
  cat <<EOF

Run the full live supervisor gate with:

  ${BASH_SOURCE[0]} --live ${robot_dir}
EOF
  exit 0
fi

# --- 2. live gate ----------------------------------------------------------

step "Gate -- hello-rover: phoxal simulation run ${WORLD} (Ctrl-C after inspection)"
if ! (cd "${robot_dir}" && "${CLI_BIN}" simulation run "${WORLD}"); then
  fail "simulation run ${WORLD} failed"
fi
ok "live staged simulation shut down cleanly"
