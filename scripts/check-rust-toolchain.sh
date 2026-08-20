#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

pinned_version="$(
  sed -n 's/^channel = "\([^"]*\)"$/\1/p' rust-toolchain.toml
)"
readonly pinned_version
minimum_version="$(
  sed -n 's/^rust-version = "\([^"]*\)"$/\1/p' Cargo.toml
)"
readonly minimum_version
if [[ ! "$pinned_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'Rust toolchain must use an exact release: %s\n' "$pinned_version" >&2
  exit 1
fi
if [[ "$minimum_version" != "$pinned_version" ]]; then
  printf 'Cargo rust-version %s does not match pinned Rust %s\n' \
    "$minimum_version" "$pinned_version" >&2
  exit 1
fi

case "${1:-}" in
  --print)
    printf '%s\n' "$pinned_version"
    ;;
  '')
    actual_version="$(rustc --version | awk '{ print $2 }')"
    if [[ "$actual_version" != "$pinned_version" ]]; then
      printf 'rustc %s does not match pinned Rust %s\n' \
        "$actual_version" "$pinned_version" >&2
      printf 'Select the toolchain from rust-toolchain.toml before running CI\n' >&2
      exit 1
    fi
    ;;
  *)
    printf 'usage: %s [--print]\n' "$0" >&2
    exit 2
    ;;
esac
