#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
shellcheck install.sh scripts/*.sh
scripts/test-install.sh
cargo build --locked --release
scripts/test-release-install.sh target/release/canonforge
