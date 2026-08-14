#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

readonly max_lines=300
paths_file="$(mktemp)"
trap 'rm -f -- "$paths_file"' EXIT

git ls-files -z --cached --others --exclude-standard -- \
  src tests examples benches >"$paths_file"

violations=0

# Include non-ignored new files so the local gate checks them before they are staged.
while IFS= read -r -d '' path; do
  [[ "$path" == *.rs ]] || continue
  [[ -f "$path" ]] || continue
  if grep -Fq 'too_many_lines' "$path"; then
    printf '%s: inline function-size bypass is forbidden\n' "$path" >&2
    violations=1
  fi
  lines="$(awk 'END { print NR + 0 }' "$path")"
  if (( lines > max_lines )); then
    printf '%s: %d lines (maximum %d)\n' "$path" "$lines" "$max_lines" >&2
    violations=1
  fi
done <"$paths_file"

if (( violations != 0 )); then
  exit 1
fi
