#!/usr/bin/env bash
# The Webots controller pin must exist in the registry the CLI installs it from.
#
# The controller is on its own release train, so the pin in this repository can
# name a version that repository has not published. Nothing catches that until
# an operator runs `phoxal simulation webots run` and `cargo install` fails on a
# version the index has never heard of. This resolves the exact pin against the
# live sparse index, which is the same document Cargo would read.
set -euo pipefail

catalog="crates/catalog/src/lib.rs"

constant() {
  local name="$1"
  local value
  value=$(sed -n "s/^pub const ${name}: &str = \"\\(.*\\)\";\$/\\1/p" "${catalog}")
  if [[ -z "${value}" ]]; then
    echo "failed to read ${name} from ${catalog}" >&2
    exit 2
  fi
  printf '%s' "${value}"
}

package=$(constant WEBOTS_CONTROLLER_PACKAGE)
version=$(constant WEBOTS_CONTROLLER_VERSION)

# Cargo's sparse-index layout. Every package this repository pins is far longer
# than three characters, so only the general form is implemented; a shorter name
# is refused rather than looked up at a path that does not exist.
if (( ${#package} < 4 )); then
  echo "cannot derive the sparse-index path for the short package name ${package}" >&2
  exit 2
fi
index="https://phoxal.github.io/registry/${package:0:2}/${package:2:2}/${package}"

echo "resolving ${package}@${version} in ${index}"
entries=$(curl --fail --silent --show-error --location --retry 3 --max-time 60 "${index}")

if ! grep -q "\"vers\":\"${version}\"" <<<"${entries}"; then
  echo "registry pin failed: ${package}@${version} is not in ${index}" >&2
  echo "published versions:" >&2
  sed -n 's/.*"vers":"\([^"]*\)".*/  \1/p' <<<"${entries}" >&2
  exit 1
fi

echo "ok: ${package}@${version} is published"
