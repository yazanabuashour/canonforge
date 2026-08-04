#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
canonforge="$repo_root/target/debug/canonforge"

if [ ! -x "$canonforge" ]; then
  printf 'error: build Canonforge first with cargo build --locked\n' >&2
  exit 1
fi

demo_root="$(mktemp -d "${TMPDIR:-/tmp}/canonforge-demo.XXXXXX")"
trap 'rm -rf "$demo_root"' EXIT
umask 077

mkdir "$demo_root/sources" "$demo_root/output"
printf '# Orchard notes\n\nThe fictional north orchard uses drip irrigation.\n' \
  >"$demo_root/sources/orchard.md"
printf '%s\n' \
  '{"schema_version":1,"units":[{"unit_id":"markdown:orchard","source_type":"canonical-markdown","locator":{"file":"orchard.md","line":1},"metadata":{"collection":"demo"}}]}' \
  >"$demo_root/assignments.json"
(cd "$demo_root/sources" && sha256sum orchard.md) >"$demo_root/SHA256SUMS"

"$canonforge" compile \
  --assignments "$demo_root/assignments.json" \
  --source-root "$demo_root/sources" \
  --checksums "$demo_root/SHA256SUMS" \
  --output "$demo_root/output/evidence-package"
"$canonforge" validate --package "$demo_root/output/evidence-package"
"$canonforge" inspect --package "$demo_root/output/evidence-package"
