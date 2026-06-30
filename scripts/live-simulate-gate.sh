#!/usr/bin/env bash
# Live split-recovery gate: robot-v1 simulate default (phoxal/phoxal-cli#11).
#
# Proves the separated repos run together end to end:
#
#   robot.yaml -> phoxal-cli -> live resolution (GHCR images, git component
#             commits, GitHub release tools) -> generated .phoxal/run/ -> router
#             -> Webots -> mandatory runtime set
#
# There is NO lockfile: every run resolves live. Production reproducibility is
# the `phoxal-cli deploy build` digest-pinned (@sha256) bundle, exercised
# separately. This gate exercises live resolve + compose generation + simulate.
#
# Two phases:
#
#   Smoke (default) -- no Docker daemon, no Webots needed:
#     1. phoxal-cli simulate default --dry-run   (live resolve + compose generation)
#     2. assert the generated compose references the mandatory runtime services
#
#   Live (--live) -- needs a running Docker daemon + Webots on PATH:
#     3. phoxal-cli simulate default --pull       (the full live gate)
#
# The smoke phase is CI-safe and proves the resolver + compose plumbing line up
# with the published image set (phoxal/framework#31). The live phase is the
# manual integration gate.
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

# --- 1. live dry-run (resolve + compose generation) ------------------------

step "Gate -- robot-v1: phoxal-cli simulate ${WORLD} --dry-run (live resolve)"
if ! (cd "${robot_dir}" && rm -rf .phoxal/run .phoxal/cache \
        && "${CLI_BIN}" simulate "${WORLD}" --dry-run >/dev/null); then
  fail "simulate ${WORLD} --dry-run failed (live resolution, or missing world
  ${WORLD}.wbt). If a git component ref cannot be resolved offline, pin it to a
  commit SHA in robot.yaml or run with network access."
fi
compose="${robot_dir}/.phoxal/run/docker-compose.yml"
[[ -f "${compose}" ]] || fail "compose not generated at ${compose}"
ok "compose generated from live resolution"

# --- 2. assert the mandatory runtime services are present ------------------

step "Gate -- generated compose references the mandatory runtime set"
if grep -q 'ghcr.io/phoxal/runtime-' "${compose}"; then
  pins="$(grep -c 'ghcr.io/phoxal/runtime-' "${compose}")"
  ok "compose references ${pins} official runtime images"
else
  fail "generated compose references no ghcr.io/phoxal/runtime- images"
fi

if [[ "${live}" -eq 0 ]]; then
  printf "\n${green}Smoke gate green.${reset} Live resolve + compose generation verified.\n"
  cat <<EOF

To run the full LIVE gate (needs a running Docker daemon + Webots on PATH):

  ${BASH_SOURCE[0]} --live ${robot_dir}

The live run (phoxal-cli simulate ${WORLD} --pull) should show:
  - phoxal-local-zenoh singleton starts or is safely reused;
  - the generated compose starts the per-robot router;
  - all mandatory runtime services start from the GHCR images;
  - Webots launches the staged world;
  - runtimes connect to tcp/router:7447 and read /robot;
  - host tools (joypad, when requested) connect via tcp/127.0.0.1:7447.
EOF
  exit 0
fi

# --- 3. live gate ----------------------------------------------------------

command -v docker >/dev/null 2>&1 \
  || fail "docker is required for the live gate -- install Docker and start the daemon"

step "Live host diagnosis"
"${CLI_BIN}" doctor >/dev/null 2>&1 \
  || fail "phoxal-cli doctor failed unexpectedly"
ok "host diagnosis complete; live simulate will enforce required preflight"

step "Live gate -- phoxal-cli simulate ${WORLD} --pull"
warn "this is the interactive live gate; it runs until you stop it (Ctrl-C)."
warn "watch for: router healthy, every runtime service Up from its GHCR image,"
warn "Webots window with the staged world, and runtimes reading /robot over the bus."
(cd "${robot_dir}" && "${CLI_BIN}" simulate "${WORLD}" --pull) \
  || fail "live simulate failed -- see logs above (image pull, runtime startup, or bus connection)."
printf "\n${green}Live gate completed.${reset}\n"
