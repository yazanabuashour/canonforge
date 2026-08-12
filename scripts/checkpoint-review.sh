#!/bin/bash
set -Eeuo pipefail
set +m

repo_root="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || exit 1

if [ "${CHECKPOINT_REVIEW_ACTIVE:-}" = "1" ]; then
  printf 'Checkpoint review already active; recursive invocation skipped.\n'
  exit 0
fi
export CHECKPOINT_REVIEW_ACTIVE=1

usage() {
  printf '%s\n' \
    'Usage:' \
    "  checkpoint-review.sh \\" \
    "    --review correctness EFFORT \\" \
    "    --review simplification EFFORT \\" \
    "    --review test-reduction EFFORT \\" \
    "    [--review TYPE EFFORT]... \\" \
    '    [--custom-review EFFORT PROMPT]' \
    '' \
    'Required review types:' \
    '  correctness, simplification, test-reduction' \
    '' \
    'Optional review types:' \
    '  security, test-gaps, api-compat, concurrency, policy' \
    '' \
    'Valid efforts: low, medium, high, xhigh (extra high), max'
}

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
  usage
  exit 0
fi

if ! command -v git >/dev/null 2>&1; then
  printf 'error: git is required\n' >&2
  exit 127
fi

if ! command -v pi >/dev/null 2>&1; then
  printf 'error: pi CLI is required\n' >&2
  exit 127
fi

if ! command -v setsid >/dev/null 2>&1; then
  printf 'error: setsid from util-linux is required\n' >&2
  exit 127
fi

usable_fd=0
for fd_command in fd fdfind; do
  if command -v "$fd_command" >/dev/null 2>&1 &&
    "$fd_command" --max-results 1 -- '' >/dev/null 2>&1; then
    usable_fd=1
    break
  fi
done
if [ "$usable_fd" -ne 1 ]; then
  printf 'error: a usable fd or fdfind is required\n' >&2
  exit 127
fi

if ! command -v rg >/dev/null 2>&1 || ! rg --version >/dev/null 2>&1; then
  printf 'error: a usable rg is required\n' >&2
  exit 127
fi

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  printf 'error: not inside a git work tree\n' >&2
  exit 2
fi

REVIEW_MODEL="${REVIEW_MODEL:-openai-codex/gpt-5.6-sol}"
FILE_SEARCH_EXTENSION="${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}/dotfiles-package/extensions/file-search/index.ts"

correctness_effort=""
simplification_effort=""
test_reduction_effort=""
security_effort=""
test_gaps_effort=""
api_compat_effort=""
concurrency_effort=""
policy_effort=""
custom_effort=""
custom_prompt=""
requested_reviews=()

validate_effort() {
  case "$1" in
    low | medium | high | xhigh | max) ;;
    *)
      printf 'error: unknown reasoning effort: %s\n' "$1" >&2
      printf 'valid values: low, medium, high, xhigh (extra high), max\n' >&2
      exit 2
      ;;
  esac
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --review)
      if [ "$#" -lt 3 ]; then
        printf 'error: --review requires TYPE and EFFORT\n' >&2
        exit 2
      fi
      review_type="$2"
      effort="$3"
      validate_effort "$effort"
      case "$review_type" in
        correctness)
          effort_var=correctness_effort
          ;;
        simplification)
          effort_var=simplification_effort
          ;;
        test-reduction)
          effort_var=test_reduction_effort
          ;;
        security)
          effort_var=security_effort
          ;;
        test-gaps)
          effort_var=test_gaps_effort
          ;;
        api-compat)
          effort_var=api_compat_effort
          ;;
        concurrency)
          effort_var=concurrency_effort
          ;;
        policy)
          effort_var=policy_effort
          ;;
        *)
          printf 'error: unknown review type: %s\n' "$review_type" >&2
          printf 'valid values: correctness, simplification, test-reduction, security, test-gaps, api-compat, concurrency, policy\n' >&2
          exit 2
          ;;
      esac
      if [ -n "${!effort_var}" ]; then
        printf 'error: duplicate review type: %s\n' "$review_type" >&2
        exit 2
      fi
      printf -v "$effort_var" '%s' "$effort"
      requested_reviews+=("$review_type=$effort")
      shift 3
      ;;
    --custom-review)
      if [ "$#" -lt 3 ]; then
        printf 'error: --custom-review requires EFFORT and PROMPT\n' >&2
        exit 2
      fi
      if [ -n "$custom_effort" ]; then
        printf 'error: --custom-review may be specified only once\n' >&2
        exit 2
      fi
      validate_effort "$2"
      if [ -z "${3//[[:space:]]/}" ]; then
        printf 'error: custom review prompt is empty\n' >&2
        exit 2
      fi
      custom_effort="$2"
      custom_prompt="$3"
      requested_reviews+=("custom-review=$custom_effort")
      shift 3
      ;;
    *)
      printf 'error: unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

if [ -z "$correctness_effort" ] || [ -z "$simplification_effort" ] || [ -z "$test_reduction_effort" ]; then
  printf 'error: --review correctness EFFORT, --review simplification EFFORT, and --review test-reduction EFFORT are required\n' >&2
  exit 2
fi

run_pi() {
  effort="$1"
  shift
  exec setsid pi \
    --print \
    --no-session \
    --model "$REVIEW_MODEL" \
    --thinking "$effort" \
    --no-approve \
    --no-extensions \
    --extension "$FILE_SEARCH_EXTENSION" \
    --no-skills \
    --no-prompt-templates \
    --tools read,fd,rg \
    "$@"
}

if [ -z "$(git status --porcelain=v1 --untracked-files=all)" ]; then
  printf 'No uncommitted changes; no review checkpoint was run.\n'
  exit 0
fi

if [ ! -f "$FILE_SEARCH_EXTENSION" ]; then
  printf 'error: Pi file-search extension not found: %s\n' "$FILE_SEARCH_EXTENSION" >&2
  exit 2
fi

review_dir="$(mktemp -d "${TMPDIR:-/tmp}/checkpoint-review.XXXXXX")" || exit 1
summary_file="$review_dir/summary.txt"
report_file="$review_dir/report.md"
review_context_file="$review_dir/uncommitted-changes.diff"

snapshot_state() {
  {
    git status --porcelain=v1 --untracked-files=all
    git diff --cached --no-ext-diff --
    git diff --no-ext-diff --
    git ls-files --others --exclude-standard -z |
      while IFS= read -r -d '' path; do
        printf 'untracked:%s\n' "$path"
        if [ -f "$path" ]; then
          git hash-object -- "$path"
        fi
      done
  } | git hash-object --stdin
}

pids=()
names=()
logs=()

cleanup_children() {
  # Reviewers are read-only, so cancellation is immediate. Running and stopped
  # job queries both exclude completed jobs whose PIDs could be reused.
  local live_pids=()
  mapfile -t live_pids < <(
    jobs -pr
    jobs -ps
  )
  for pid in "${live_pids[@]}"; do
    kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
  done
  if [ "${#live_pids[@]}" -gt 0 ]; then
    wait "${live_pids[@]}" 2>/dev/null || true
  fi
}

cleanup_and_mark() {
  status=$?
  if [ "$#" -gt 0 ]; then
    status="$1"
  fi
  trap - INT TERM HUP EXIT
  cleanup_children
  exit "$status"
}

trap cleanup_and_mark EXIT
trap 'cleanup_and_mark 130' INT
trap 'cleanup_and_mark 143' TERM
trap 'cleanup_and_mark 129' HUP

start_focused_review() {
  name="$1"
  effort="$2"
  prompt="$3"

  log="$review_dir/${name}.log"

  run_pi "$effort" "@$review_context_file" "$prompt" >"$log" 2>&1 &

  pids+=("$!")
  names+=("$name")
  logs+=("$log")
}

review_prefix='Review the current uncommitted changes described in the attached diff. Use read-only tools to inspect relevant repository context. Inspect and report only. Do not edit files, invoke another review gate, or delegate.'
correctness_prompt="$review_prefix Focus on defects introduced by the diff: functional bugs, behavior regressions, data loss, broken error handling, and contract violations. Ignore style unless it hides a defect. Report only actionable findings with severity, file:line references, the failure scenario, and the smallest safe fix. If there are none, say exactly: No correctness findings."

# Size receipts: Google C++ and Python style guides recommend considering a
# function split at about 40 lines. The ESLint max-lines rule defaults to 300
# physical lines and describes 100 to 500 lines as the common range.
simplification_prompt="$review_prefix Focus on making the diff smaller, clearer, and harder to misuse without changing required behavior. Favor alternatives that complete the task while deleting more code than they add; treat that outcome as gold. Apply the Rule of Three, \"you ain't going to need it,\" and one-liners where they remain obvious. Identify unnecessary abstractions, wrappers, configuration, and compatibility machinery. Flag comments that narrate code instead of explaining contracts, constraints, or non-obvious decisions. Review documentation for reader focus, accuracy, stale or duplicate content, and commands, examples, links, or behavior that disagree with the implementation. Treat 40 physical lines per function and 300 physical lines per handwritten source file as review tripwires, not automatic failures; exempt generated code, third-party code, data tables, migrations, and schemas. Do not reward fewer lines when compression or hidden behavior harms obviousness or readability. Report only actionable simplifications with file:line references and explain why each alternative preserves behavior. If there are none, say exactly: No actionable simplification findings."
test_reduction_prompt="$review_prefix Focus only on reducing tests added or expanded by this diff. Keep tests that protect important behavior or catch plausible non-obvious regressions, boundary conditions, failure modes, or gotchas. Identify tautological tests, tests that merely restate the implementation or mocks, redundant or overlapping cases, low-value happy-path permutations, and brittle over-specified assertions that can be removed or consolidated without materially reducing defect detection. Do not request additional tests. Report only actionable removals or consolidations with file:line references and why the remaining coverage is sufficient. If there are none, say exactly: No actionable test-reduction findings."
test_gaps_prompt="$review_prefix Focus on missing, weak, or misleading validation for changed behavior, bug fixes, migrations, and compatibility-sensitive changes. Report only actionable test gaps with file:line references and the exact behavior that should be tested. If there are none, say exactly: No actionable test-gap findings."
security_prompt="$review_prefix Focus on concrete security regressions introduced or exposed by this diff: authentication, authorization, unsafe filesystem, shell, network, browser, or web-address handling, injection, path traversal, secret exposure, unsafe deserialization, privilege boundaries, and dependency or configuration weakening. Report only actionable findings with file:line references, impact, and the smallest safe fix. If there are none, say exactly: No actionable security findings."
api_compat_prompt="$review_prefix Focus on application programming interface, command-line interface, configuration, environment, schema, migration, generated client, documentation contract, rollout, and rollback compatibility regressions. Report only actionable risks with file:line references, the expected failure mode, and the smallest safe fix. If there are none, say exactly: No actionable interface or migration compatibility findings."
concurrency_prompt="$review_prefix Focus on concurrency, lifecycle, and operational correctness: races, async ordering, cancellation/cleanup, leaks, retry idempotency, transactions, stale cache/state, timing assumptions, and unsafe parallelism. Report only actionable findings with file:line references, the runtime scenario, and the smallest safe fix. If there are none, say exactly: No actionable concurrency/lifecycle findings."
policy_prompt="$review_prefix Focus on orchestration-policy quality: ambiguous delegation rules, over-orchestration risk, under-orchestration risk, thread/worktree/subagent/goal sequencing contradictions, review/commit contract contradictions, and coding-agent portability. Report only actionable findings with file:line references and the smallest wording or script change that resolves the issue. If there are none, say exactly: No actionable orchestration-policy findings."

printf 'Requested reviews: %s\n' "${requested_reviews[*]}"
printf 'Review output: %s\n' "$review_dir"
printf '\nChanged files:\n'
git status --short --untracked-files=all | tee "$review_dir/changed-files.txt"
git diff HEAD --stat >"$review_dir/diff-stat.txt"
review_state_hash="$(snapshot_state)"

{
  printf '## Worktree status\n\n'
  git status --short --untracked-files=all
  printf '\n## Tracked changes\n\n'
  git diff HEAD --no-ext-diff --
} >"$review_context_file"

while IFS= read -r -d '' path; do
  {
    printf '\n## Untracked file: %s\n\n' "$path"
    git diff --no-index -- /dev/null "$path" || [ "$?" -eq 1 ]
  } >>"$review_context_file"
done < <(git ls-files --others --exclude-standard -z)

if [ "$(snapshot_state)" != "$review_state_hash" ]; then
  printf '\nReview command failed: worktree changed while preparing reviewer context.\n' >&2
  printf 'Rerun the checkpoint review for the current diff.\n' >&2
  exit 1
fi

statuses=()
failed=0

wait_for_review() {
  local i="$1"
  if wait "${pids[$i]}"; then
    statuses[i]=0
  else
    statuses[i]=$?
    failed=1
  fi
}

# Standard checkpoint review: keep this cheap enough to run at every selected
# checkpoint.
# These are independent Pi processes, not interactive subagent threads, so they
# work in non-interactive checkpoint scripts.
start_focused_review "correctness-review" "$correctness_effort" "$correctness_prompt"
start_focused_review "simplification-review" "$simplification_effort" "$simplification_prompt"
start_focused_review "test-reduction-review" "$test_reduction_effort" "$test_reduction_prompt"

# Optional focused reviewers:
if [ -n "$test_gaps_effort" ]; then
  start_focused_review "test-gap-review" "$test_gaps_effort" "$test_gaps_prompt"
fi

if [ -n "$security_effort" ]; then
  start_focused_review "security-review" "$security_effort" "$security_prompt"
fi

if [ -n "$api_compat_effort" ]; then
  start_focused_review "api-compat-review" "$api_compat_effort" "$api_compat_prompt"
fi

if [ -n "$concurrency_effort" ]; then
  start_focused_review "concurrency-review" "$concurrency_effort" "$concurrency_prompt"
fi

if [ -n "$policy_effort" ]; then
  start_focused_review "policy-review" "$policy_effort" "$policy_prompt"
fi

if [ -n "$custom_effort" ]; then
  start_focused_review "custom-review" "$custom_effort" "$review_prefix $custom_prompt"
fi

for i in "${!pids[@]}"; do
  wait_for_review "$i"
done
trap - INT TERM HUP EXIT

write_summary() {
  {
    printf 'Review output: %s\n' "$review_dir"
    printf 'Review report: %s\n' "$report_file"
    printf 'Changed files:\n'
    cat "$review_dir/changed-files.txt"
    printf '\nReviewers:\n'
    for i in "${!names[@]}"; do
      printf '%s status=%s log=%s\n' "${names[$i]}" "${statuses[$i]}" "${logs[$i]}"
    done
  } >"$summary_file"
}

write_report() {
  : >"$report_file"
  for i in "${!names[@]}"; do
    name="${names[$i]}"
    log="${logs[$i]}"

    {
      printf '## %s\n\n' "$name"
      cat "$log"
      printf '\n'

      if [ "${statuses[$i]}" -ne 0 ]; then
        printf '[reviewer exited with status %s; full log: %s]\n' \
          "${statuses[$i]}" "$log"
      fi
      printf '\n'
    } >>"$report_file"
  done
}

write_report
printf '\n'
cat "$report_file"

if [ "$failed" -ne 0 ]; then
  write_summary
  printf '\nReview command failed:\n' >&2
  for i in "${!names[@]}"; do
    if [ "${statuses[$i]}" -ne 0 ]; then
      printf '  %s=%s log=%s\n' "${names[$i]}" "${statuses[$i]}" "${logs[$i]}" >&2
    fi
  done
  exit 1
fi

if [ "$(snapshot_state)" != "$review_state_hash" ]; then
  write_summary
  printf '\nReview command failed: worktree changed while reviewers were running.\n' >&2
  printf 'Rerun the checkpoint review for the current diff before committing.\n' >&2
  exit 1
fi

write_summary

printf '\nCheckpoint review complete. Address actionable findings before committing.\n'
printf 'Review report: %s\n' "$report_file"
printf 'Review metadata: %s\n' "$summary_file"
