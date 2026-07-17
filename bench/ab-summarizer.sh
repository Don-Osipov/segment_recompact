#!/usr/bin/env bash
# A/B the headless summarizer: base model alone vs salience escalation to a stronger model.
# Works on COPIES of a session file; the original is never touched. Human-judged output: prints
# both runs' summaries side by side plus token results, so the escalation default can be set by
# comparison rather than taste.
#
# Usage: bench/ab-summarizer.sh <session.jsonl> [threshold] [base-model] [strong-model]
set -euo pipefail

SRC="${1:?usage: ab-summarizer.sh <session.jsonl> [threshold] [base] [strong]}"
THRESHOLD="${2:-60000}"
BASE="${3:-haiku}"
STRONG="${4:-sonnet}"
RC="${RECOMPACT_BIN:-$(cd "$(dirname "$0")/.." && pwd)/plugins/segment_recompact/target/release/recompact}"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/a" "$WORK/b"
cp "$SRC" "$WORK/a/" && cp "$SRC" "$WORK/b/"
NAME="$(basename "$SRC")"

run_arm() { # dir label extra-args...
  local dir="$1" label="$2"
  shift 2
  local t0 t1 id
  t0=$(date +%s)
  id=$("$RC" continue "$dir/$NAME" --threshold "$THRESHOLD" "$@" 2>"$dir/log.txt") || {
    echo "[$label] FAILED:" && tail -3 "$dir/log.txt" && return 1
  }
  t1=$(date +%s)
  echo "== $label (took $((t1 - t0))s) =="
  grep -E '^continue:' "$dir/log.txt" | tail -2
  if [ "$id" != "${NAME%.jsonl}" ]; then
    echo "-- summaries --"
    jq -r 'select(.recompactSynthetic==true) | "[" + (.recompactProvenance.part // "?") + "] " + .message.content[0].text' "$dir/$id.jsonl"
  fi
}

run_arm "$WORK/a" "base:$BASE" --summarize-with "$BASE"
echo
run_arm "$WORK/b" "escalated:$BASE+$STRONG" --summarize-with "$BASE" --escalate-with "$STRONG" --escalate-above 0.4
