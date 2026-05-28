#!/bin/sh
set -eu

fail() {
    printf '%s\n' "phoxal-cli install: $*" >&2
    exit 1
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os:$arch" in
        Darwin:arm64)
            printf '%s\n' "aarch64-apple-darwin"
            ;;
        Linux:x86_64)
            printf '%s\n' "x86_64-unknown-linux-gnu"
            ;;
        *)
            fail "only mac-silicon and linux-x64 are supported today"
            ;;
    esac
}

if command -v curl >/dev/null 2>&1; then
    fetch_stdout() {
        curl -fsSL "$1"
    }
    download_file() {
        curl -fsSL "$1" -o "$2"
    }
elif command -v wget >/dev/null 2>&1; then
    fetch_stdout() {
        wget -qO- "$1"
    }
    download_file() {
        wget -qO "$2" "$1"
    }
else
    fail "curl or wget is required"
fi

target=$(detect_target)

if [ "${PHOXAL_CLI_VERSION:-}" ]; then
    version=$PHOXAL_CLI_VERSION
    case "$version" in
        v*) ;;
        *) fail "PHOXAL_CLI_VERSION must start with v, for example v0.0.1" ;;
    esac
else
    latest_json=$(fetch_stdout "https://api.github.com/repos/phoxal/phoxal-cli/releases/latest")
    version=$(
        printf '%s\n' "$latest_json" |
            sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
            head -n 1
    )
    [ -n "$version" ] || fail "could not determine latest release tag"
fi

version_without_v=${version#v}
asset="phoxal-cli-${version_without_v}-${target}.tar.gz"
url="https://github.com/phoxal/phoxal-cli/releases/download/${version}/${asset}"

tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/phoxal-cli.XXXXXX")
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

archive="$tmpdir/$asset"
download_file "$url" "$archive"
tar -xzf "$archive" -C "$tmpdir"

binary="$tmpdir/phoxal-cli-${target}"
[ -f "$binary" ] || fail "release archive did not contain phoxal-cli-${target}"

prefix=${PREFIX:-/usr/local}
install_dir="$prefix/bin"
if ! { mkdir -p "$install_dir" 2>/dev/null && [ -w "$install_dir" ]; }; then
    install_dir="$HOME/.local/bin"
    mkdir -p "$install_dir" || fail "could not create $install_dir"
    printf '%s\n' "phoxal-cli install: $prefix/bin is not writable; installing to $install_dir" >&2
    printf '%s\n' "phoxal-cli install: add $install_dir to PATH if it is not already there" >&2
fi

destination="$install_dir/phoxal-cli"
cp "$binary" "$destination" || fail "could not install to $destination"
chmod 755 "$destination" || fail "could not chmod $destination"

printf '%s\n' "phoxal-cli installed to $destination"
