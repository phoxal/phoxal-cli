#!/usr/bin/env bash
# Live split-recovery gate: robot-v1 simulate default (phoxal/phoxal-cli#11).
#
# Proves the separated repos still resolve together:
#
#   robot.yaml -> phoxal-cli -> live resolution (native artifact catalog, git
#             component commits, host tools) -> dry-run launch report
#
# There is NO lockfile: every run resolves live. Production reproducibility
# belongs to the native deploy release artifact, exercised separately. This gate
# exercises live resolve without writing local launch directories.
#
# Two phases:
#
#   Smoke (default):
#     1. phoxal-cli simulate default --dry-run   (live resolve + no writes)
#     2. assert no .phoxal/run or .phoxal/webots directory was generated
#
#   Live (--live):
#     native supervisor launch is pending follow-up 04.
#
# The smoke phase is CI-safe and proves the resolver still lines up with the
# configured artifact catalog.
#
# Usage:
#   scripts/live-simulate-gate.sh [--live] [ROBOT_DIR]
#
# ROBOT_DIR defaults to ../robot-v1 (sibling-clone recovery layout). WORLD is
# `default` (robot-v1 ships worlds/default.wbt).

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
robot_dir="${robot_dir:-${CLI_REPO}/../robot-v1}"
WORLD="default"

red="\033[31m"; green="\033[32m"; yellow="\033[33m"; cyan="\033[36m"; reset="\033[0m"
step() { printf "\n${cyan}> %s${reset}\n" "$1"; }
ok()   { printf "${green}OK${reset}   %s\n" "$1"; }
warn() { printf "${yellow}WARN${reset} %s\n" "$1"; }
fail() { printf "${red}FAIL${reset} %s\n" "$1" >&2; exit 1; }

# --- resolve target + CLI binary -------------------------------------------

[[ -f "${robot_dir}/robot.yaml" ]] \
  || fail "no robot.yaml in ${robot_dir} (pass ROBOT_DIR; expected the robot-v1 sibling clone)"
robot_dir="$(cd "${robot_dir}" && pwd)"

CLI_BIN="${CLI_REPO}/target/debug/phoxal-cli"
if [[ ! -x "${CLI_BIN}" ]]; then
  step "building phoxal-cli"
  (cd "${CLI_REPO}" && cargo build --quiet -p phoxal-cli) || fail "phoxal-cli build failed"
fi

# --- 1. live dry-run (resolve, no launch-directory writes) ------------------

step "Gate -- robot-v1: phoxal-cli simulate ${WORLD} --dry-run (live resolve)"
if ! (cd "${robot_dir}" && rm -rf .phoxal/run .phoxal/webots .phoxal/cache \
        && "${CLI_BIN}" simulate "${WORLD}" --dry-run >/dev/null); then
  fail "simulate ${WORLD} --dry-run failed (live resolution, or missing world
  ${WORLD}.wbt). If a git component ref cannot be resolved offline, pin it to a
  commit SHA in robot.yaml or run with network access."
fi
[[ ! -e "${robot_dir}/.phoxal/run" ]] || fail "dry-run wrote .phoxal/run"
[[ ! -e "${robot_dir}/.phoxal/webots" ]] || fail "dry-run wrote .phoxal/webots"
ok "dry-run resolved without local launch-directory writes"

if [[ "${live}" -eq 0 ]]; then
  printf "\n${green}Smoke gate green.${reset} Live resolve dry-run verified.\n"
  cat <<EOF

The full live supervisor run is pending follow-up 04:

  ${BASH_SOURCE[0]} --live ${robot_dir}
EOF
  exit 0
fi

# --- 3. live gate ----------------------------------------------------------

warn "native simulate supervision is pending follow-up 04; this slice only resolves and reports the plan"
exit 2
