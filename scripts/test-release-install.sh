#!/bin/sh

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
binary=${1:-$root/target/release/canonforge}
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -n 1)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/canonforge-release-install.XXXXXX")
trap 'rm -rf "$tmp"' 0
trap 'exit 1' 1 2 3 15

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

[ -n "$version" ] || fail "Cargo package version is missing"
[ -x "$binary" ] || fail "release binary is not executable: $binary"

case "$(uname -m)" in
  x86_64|amd64) architecture=x86_64 ;;
  arm64|aarch64) architecture=aarch64 ;;
  *) fail "unsupported release test architecture" ;;
esac
asset="canonforge-$architecture-unknown-linux-gnu"
release="$tmp/releases/download/v$version"
mkdir -p "$release" "$tmp/home"
cp "$binary" "$release/$asset"
(cd "$release" && sha256sum "$asset" > SHA256SUMS)

HOME="$tmp/home" "$root/install.sh" \
  --release-base "file://$tmp/releases" \
  --version "$version"

installed=$(env -i HOME="$tmp/home" PATH="$tmp/home/.local/bin:/usr/bin:/bin" \
  canonforge --version)
[ "$installed" = "canonforge $version" ] || fail "clean environment could not execute the installed release"

printf 'release install test passed\n'
