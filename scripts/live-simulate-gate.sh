#!/usr/bin/env bash
# Live split-recovery gate: robot-v1 simulate default (phoxal/phoxal-cli#11).
#
# Proves the separated repos run together end to end:
#
#   robot.yaml → phoxal-cli → real phoxal.lock (real GHCR digests)
#             → generated .phoxal/run/ → router → Webots → mandatory runtime set
#
# Two phases:
#
#   Smoke (default) — no Docker daemon, no Webots needed:
#     1. phoxal-cli update --pin-digests   (resolve REAL GHCR image digests)
#     2. assert phoxal.lock pins every runtime image as repo@sha256:… (not a tag)
#     3. phoxal-cli simulate default --locked --dry-run
#        (locked resolution + compose generation against the real-digest lock)
#
#   Live (--live) — needs a running Docker daemon + Webots on PATH:
#     4. phoxal-cli simulate default --locked   (the full live gate)
#
# The smoke phase is CI-safe and is what proves the digest plumbing
# (phoxal/phoxal-cli#10) and the published image set (phoxal/framework#31) line
# up. The live phase is the manual integration gate.
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
step() { printf "\n${cyan}▶ %s${reset}\n" "$1"; }
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

command -v docker >/dev/null 2>&1 \
  || fail "docker (with buildx) is required to resolve real image digests — install Docker"

# --- 1. update --pin-digests -----------------------------------------------

step "Gate — robot-v1: phoxal-cli update --pin-digests (real GHCR digests)"
if ! (cd "${robot_dir}" && "${CLI_BIN}" update --pin-digests); then
  fail "update --pin-digests failed.
  - missing image / digest: the phoxal/framework GHCR runtime images may not be
    published for the resolved runtime set. Verify with framework's
    scripts/verify-runtime-release.sh, then re-run.
  - rate limited / auth: set GITHUB_TOKEN, or 'docker login ghcr.io' for private packages."
fi
ok "phoxal.lock written with --pin-digests"

# --- 2. assert real digest pins (not tag refs) -----------------------------

step "Gate — phoxal.lock pins every runtime image as a real digest"
lock="${robot_dir}/phoxal.lock"
[[ -f "${lock}" ]] || fail "phoxal.lock not found at ${lock}"
# The lockfile images block: every runtime line must be repo@sha256:…, never a
# bare repo:version tag ref (that would be an unresolved / fake-digest recovery
# state — see phoxal/phoxal-cli#10).
tag_refs="$(awk '/^  images:/{f=1;next} /^[a-z]/{f=0} f && /ghcr.io\/phoxal\/runtime-/ && !/@sha256:/{print}' "${lock}")"
pinned="$(grep -c '@sha256:' "${lock}")"
if [[ -n "${tag_refs}" ]]; then
  printf '%s\n' "${tag_refs}" >&2
  fail "phoxal.lock has unpinned runtime images (tag refs above). The GHCR images
  for this runtime set are not resolvable — publish them (framework#31) and re-run
  update --pin-digests."
fi
ok "all runtime images pinned to real sha256 digests (${pinned} pins)"

# --- 3. locked dry-run (structural) ----------------------------------------

step "Gate — phoxal-cli simulate ${WORLD} --locked --dry-run"
if ! (cd "${robot_dir}" && rm -rf .phoxal/run .phoxal/cache \
        && "${CLI_BIN}" simulate "${WORLD}" --locked --dry-run >/dev/null); then
  fail "simulate ${WORLD} --locked --dry-run failed (locked resolution drift, or
  missing world ${WORLD}.wbt). Re-run update and retry."
fi
compose="${robot_dir}/.phoxal/run/docker-compose.yml"
[[ -f "${compose}" ]] || fail "compose not generated at ${compose}"
# The generated compose must reference the real digest pins, not tag refs.
if awk '/image: ghcr.io\/phoxal\/runtime-/ && !/@sha256:/{exit 1}' "${compose}"; then
  ok "compose generated; runtime services reference real digest pins"
else
  fail "generated compose references a runtime image by tag, not digest"
fi

if [[ "${live}" -eq 0 ]]; then
  printf "\n${green}Smoke gate green.${reset} Real-digest phoxal.lock + locked compose verified.\n"
  cat <<EOF

To run the full LIVE gate (needs a running Docker daemon + Webots on PATH):

  ${BASH_SOURCE[0]} --live ${robot_dir}

The live run (phoxal-cli simulate ${WORLD} --locked) should show:
  - phoxal-local-zenoh singleton starts or is safely reused;
  - the generated compose starts the per-robot router;
  - all mandatory runtime services start from the pinned GHCR images;
  - Webots launches the staged world;
  - runtimes connect to tcp/router:7447 and read /robot;
  - host tools (rerun-proxy/joypad, when requested) connect via tcp/127.0.0.1:7447.
EOF
  exit 0
fi

# --- 4. live gate ----------------------------------------------------------

step "Live host diagnosis"
"${CLI_BIN}" doctor >/dev/null 2>&1 \
  || fail "phoxal-cli doctor failed unexpectedly"
ok "host diagnosis complete; live simulate will enforce required preflight"

step "Live gate — phoxal-cli simulate ${WORLD} --locked"
warn "this is the interactive live gate; it runs until you stop it (Ctrl-C)."
warn "watch for: router healthy, every runtime service Up from its GHCR image,"
warn "Webots window with the staged world, and runtimes reading /robot over the bus."
(cd "${robot_dir}" && "${CLI_BIN}" simulate "${WORLD}" --locked) \
  || fail "live simulate failed — see logs above (image pull, runtime startup, or bus connection)."
printf "\n${green}Live gate completed.${reset}\n"
