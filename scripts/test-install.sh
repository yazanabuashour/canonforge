#!/bin/sh

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/canonforge-installer-test.XXXXXX")
trap 'rm -rf "$tmp"' 0
trap 'exit 1' 1 2 3 15

fail() {
    printf 'test-install: %s\n' "$*" >&2
    exit 1
}

[ "$(uname -s)" = Linux ] || fail "installer tests require Linux"
case "$(uname -m)" in
    x86_64|amd64) architecture=x86_64 ;;
    arm64|aarch64) architecture=aarch64 ;;
    *) fail "unsupported test architecture" ;;
esac
asset="canonforge-$architecture-unknown-linux-gnu"

sha() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    else
        shasum -a 256 "$1" | awk '{ print $1 }'
    fi
}

make_release() {
    directory=$1
    mkdir -p "$directory"
    printf '#!/bin/sh\nprintf "canonforge fixture 1.2.3\\n"\n' > "$directory/$asset"
    chmod 755 "$directory/$asset"
    printf '%s  %s\n' "$(sha "$directory/$asset")" "$asset" > "$directory/SHA256SUMS"
}

if HOME='' CANONFORGE_INSTALL_DIR='' "$root/install.sh" >/dev/null 2>&1; then
    fail "installer accepted an empty install directory"
fi
if "$root/install.sh" --version ../escape --release-base file:///tmp >/dev/null 2>&1; then
    fail "installer accepted an unsafe version"
fi

pinned="$tmp/releases/download/v1.2.3"
latest="$tmp/releases/latest/download"
make_release "$pinned"
make_release "$latest"

"$root/install.sh" --release-base "file://$tmp/releases" --version v1.2.3 \
    --install-dir "$tmp/pinned-bin"
[ "$("$tmp/pinned-bin/canonforge")" = "canonforge fixture 1.2.3" ] || fail "pinned install failed"

"$root/install.sh" --release-base="file://$tmp/releases" \
    --install-dir="$tmp/latest-bin"
[ -x "$tmp/latest-bin/canonforge" ] || fail "latest install is not executable"

mkdir "$tmp/directory-bin" "$tmp/directory-bin/canonforge"
if "$root/install.sh" --release-base "file://$tmp/releases" \
    --install-dir "$tmp/directory-bin" >/dev/null 2>&1; then
    fail "installer replaced a directory"
fi
[ -d "$tmp/directory-bin/canonforge" ] || fail "failed install removed a directory"

printf 'tampered\n' >> "$pinned/$asset"
before=$(sha "$tmp/pinned-bin/canonforge")
if "$root/install.sh" --release-base "file://$tmp/releases" --version 1.2.3 \
    --install-dir "$tmp/pinned-bin" >/dev/null 2>&1; then
    fail "tampered release installed"
fi
[ "$(sha "$tmp/pinned-bin/canonforge")" = "$before" ] || fail "failed install replaced the binary"

if "$root/install.sh" --release-base http://example.invalid/releases \
    --install-dir "$tmp/http-bin" >/dev/null 2>&1; then
    fail "installer accepted plain HTTP"
fi

libc_bin="$tmp/libc-bin"
mkdir "$libc_bin"
printf '%s\n' '#!/bin/sh' "case \"\$1\" in -s) echo Linux;; -m) echo x86_64;; esac" > "$libc_bin/uname"
printf '%s\n' '#!/bin/sh' 'echo "musl libc"' > "$libc_bin/ldd"
chmod +x "$libc_bin/uname" "$libc_bin/ldd"
if PATH="$libc_bin:$PATH" "$root/install.sh" --release-base "file://$tmp/releases" \
    --install-dir "$tmp/musl-bin" >/dev/null 2>&1; then
    fail "installer accepted musl Linux"
fi
printf '%s\n' '#!/bin/sh' 'echo "ldd (GNU libc)"' > "$libc_bin/ldd"
printf '%s\n' '#!/bin/sh' 'echo "glibc 2.34"' > "$libc_bin/getconf"
chmod +x "$libc_bin/getconf"
if PATH="$libc_bin:$PATH" "$root/install.sh" --release-base "file://$tmp/releases" \
    --install-dir "$tmp/old-glibc-bin" >/dev/null 2>&1; then
    fail "installer accepted unsupported glibc"
fi

printf '%s  %s\n' "$(sha "$latest/$asset")" "$asset" >> "$latest/SHA256SUMS"
if "$root/install.sh" --release-base "file://$tmp/releases" \
    --install-dir "$tmp/duplicate-bin" >/dev/null 2>&1; then
    fail "installer accepted a duplicate checksum"
fi

printf 'installer tests passed\n'
