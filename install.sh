#!/bin/sh

set -eu

repository=${CANONFORGE_REPOSITORY:-yazanabuashour/canonforge}
release_base=${CANONFORGE_RELEASE_BASE:-}
version=${CANONFORGE_VERSION:-latest}
install_dir=${CANONFORGE_INSTALL_DIR:-}
[ -n "$install_dir" ] || install_dir=${HOME:+$HOME/.local/bin}

usage() {
    cat <<'EOF'
Install a checksummed Canonforge release.

Usage: install.sh [options]

Options:
  --repository REPOSITORY   GitHub repository
  --release-base URL        Release root
  --version VERSION         Release to install; defaults to latest
  --install-dir DIRECTORY   Destination; defaults to ~/.local/bin
  --help                    Show this help

Environment variables:
  CANONFORGE_REPOSITORY, CANONFORGE_RELEASE_BASE, CANONFORGE_VERSION,
  CANONFORGE_INSTALL_DIR
EOF
}

die() {
    printf 'canonforge installer: %s\n' "$*" >&2
    exit 1
}

need_value() {
    [ "$#" -ge 2 ] && [ -n "$2" ] || die "$1 requires a value"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --repository) need_value "$@"; repository=$2; shift 2 ;;
        --repository=*) repository=${1#*=}; shift ;;
        --release-base) need_value "$@"; release_base=$2; shift 2 ;;
        --release-base=*) release_base=${1#*=}; shift ;;
        --version) need_value "$@"; version=$2; shift 2 ;;
        --version=*) version=${1#*=}; shift ;;
        --install-dir) need_value "$@"; install_dir=$2; shift 2 ;;
        --install-dir=*) install_dir=${1#*=}; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

[ -n "$install_dir" ] || die "--install-dir is required when HOME is unset"

case "$version" in
    latest) tag=latest ;;
    v*) tag=$version ;;
    *) tag=v$version ;;
esac
case "$tag" in
    latest|v[0-9A-Za-z]*) ;;
    *) die "invalid version: $version" ;;
esac
case "$tag" in
    *[!0-9A-Za-z._+-]*) die "invalid version: $version" ;;
esac

if [ -z "$release_base" ]; then
    case "$repository" in
        */*) ;;
        *) die "invalid GitHub repository: $repository" ;;
    esac
    case "$repository" in
        *[!0-9A-Za-z._/-]*|*//*|/*|*/|*/*/*) die "invalid GitHub repository: $repository" ;;
    esac
    release_base="https://github.com/$repository/releases"
fi
release_base=${release_base%/}
case "$release_base" in
    https://*|file://*) ;;
    *) die "release base must use https:// (or file:// for local testing)" ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is required"
[ "$(uname -s)" = Linux ] || die "Canonforge currently supports Linux"

case "$(uname -m)" in
    x86_64|amd64) architecture=x86_64 ;;
    arm64|aarch64) architecture=aarch64 ;;
    *) die "unsupported architecture: $(uname -m)" ;;
esac

if ldd --version 2>&1 | grep -qi musl; then
    die "musl Linux is not supported by the release binary; build from source"
fi
libc=$(getconf GNU_LIBC_VERSION 2>/dev/null) || die "could not verify glibc"
case "$libc" in
    glibc\ *) glibc_version=${libc#glibc } ;;
    *) die "unsupported Linux libc: $libc" ;;
esac
# Release runner receipts: Ubuntu 22.04 x86_64 has glibc 2.35; Ubuntu 24.04
# aarch64 has glibc 2.39.
case "$architecture" in
    x86_64) minimum_glibc=2.35 ;;
    aarch64) minimum_glibc=2.39 ;;
esac
awk -v have="$glibc_version" -v need="$minimum_glibc" 'BEGIN {
    split(have, h, "."); split(need, n, ".")
    exit !((h[1] + 0 > n[1] + 0) || (h[1] + 0 == n[1] + 0 && h[2] + 0 >= n[2] + 0))
}' || die "glibc $glibc_version is too old; $architecture releases require glibc $minimum_glibc or newer"

target="$architecture-unknown-linux-gnu"
asset="canonforge-$target"
if [ "$tag" = latest ]; then
    download_root="$release_base/latest/download"
else
    download_root="$release_base/download/$tag"
fi

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/canonforge-install.XXXXXX") || die "could not create a temporary directory"
staging=
cleanup() {
    rm -rf "$tmp_dir"
    [ -z "$staging" ] || rm -f "$staging"
}
trap cleanup 0
trap 'exit 1' 1 2 3 15

download() {
    url=$1
    destination=$2
    case "$url" in
        https://*) protocol='=https' ;;
        file://*) protocol='=file' ;;
        *) die "refusing unsupported download URL: $url" ;;
    esac
    curl --proto "$protocol" --tlsv1.2 --fail --location --silent --show-error \
        --output "$destination" -- "$url"
}

archive="$tmp_dir/$asset"
checksums="$tmp_dir/SHA256SUMS"
printf 'Downloading Canonforge %s for %s...\n' "$tag" "$target" >&2
download "$download_root/$asset" "$archive"
download "$download_root/SHA256SUMS" "$checksums"

expected=$(awk -v asset="$asset" '
    {
        name = $2
        sub(/^\*/, "", name)
        if (name == asset && length($1) == 64 && $1 !~ /[^0-9A-Fa-f]/) {
            count++
            digest = tolower($1)
        }
    }
    END { if (count == 1) print digest }
' "$checksums")
[ -n "$expected" ] || die "SHA256SUMS does not contain exactly one checksum for $asset"

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$archive" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$archive" | awk '{ print $1 }')
else
    die "sha256sum or shasum is required"
fi
actual=$(printf '%s' "$actual" | tr 'A-F' 'a-f')
[ "$actual" = "$expected" ] || die "checksum verification failed for $asset"

mkdir -p "$install_dir" || die "could not create install directory: $install_dir"
staging=$(mktemp "$install_dir/.canonforge.install.XXXXXX") || die "could not stage in install directory: $install_dir"
cp "$archive" "$staging" || die "could not write to install directory: $install_dir"
chmod 755 "$staging"
mv -fT -- "$staging" "$install_dir/canonforge"
staging=
printf 'Installed Canonforge to %s/canonforge\n' "$install_dir" >&2
case ":${PATH:-}:" in
    *:"$install_dir":*) ;;
    *) printf 'Add %s to PATH to run canonforge.\n' "$install_dir" >&2 ;;
esac
