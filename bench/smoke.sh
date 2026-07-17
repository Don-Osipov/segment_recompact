#!/usr/bin/env bash
# Headless end-to-end smoke benchmark for recompact. Exercises both compaction modes against a
# real `claude` CLI: seeds throwaway sessions in isolated project dirs, recompacts them, resumes
# the compacted files, and checks that the resumed model recalls what only the compacted
# artifacts carry (summarize mode: a codeword living solely in the synthetic summary; mask mode:
# continuity over records with elided payloads).
#
# Requirements: claude CLI (authenticated), jq, a built recompact binary.
# Cost: ~6 small-model calls.
#
# Usage: bench/smoke.sh [path-to-recompact-binary]
set -euo pipefail

BIN="${1:-"$(cd "$(dirname "$0")/.." && pwd)/plugins/segment_recompact/target/release/recompact"}"
MODEL="${RECOMPACT_BENCH_MODEL:-haiku}"
command -v claude >/dev/null || { echo "SKIP: claude CLI not found"; exit 2; }
command -v jq >/dev/null || { echo "SKIP: jq not found"; exit 2; }
[ -x "$BIN" ] || { echo "SKIP: recompact binary not found at $BIN (cargo build --release first)"; exit 2; }

note() { echo "[$1] $2"; }

# Claude Code maps a session to a project dir derived from the cwd ('/' and '.' become '-').
# A fresh dir under $HOME avoids the /var -> /private/var symlink mismatch mktemp would cause.
project_dir_for() { echo "$HOME/.claude/projects/$(echo "$1" | sed 's/[\/.]/-/g')"; }

cleanup_dirs=()
cleanup() { for d in "${cleanup_dirs[@]:-}"; do rm -rf "$d"; done; }
trap cleanup EXIT

new_workdir() {
  local w="$HOME/.recompact-bench-$$-$RANDOM"
  mkdir -p "$w"
  cleanup_dirs+=("$w" "$(project_dir_for "$w")")
  echo "$w"
}

run_claude() { # cwd prompt [extra claude args...]
  local cwd="$1" prompt="$2"
  shift 2
  (cd "$cwd" && echo "$prompt" | claude -p --model "$MODEL" --output-format json "$@")
}

# ----------------------------------------------------------------- scenario 1: summarize recall
scenario_summarize() {
  local work proj seed sid codeword src newid reply
  work="$(new_workdir)"
  proj="$(project_dir_for "$work")"

  seed="$(run_claude "$work" "Invent a codeword of the form ANIMAL-NUMBER (e.g. FALCON-7). State it once clearly. I will ask for it later.")"
  sid="$(jq -r .session_id <<<"$seed")"
  codeword="$(jq -r .result <<<"$seed" | grep -oE '[A-Z]{3,}-[0-9]+' | head -1 || true)"
  [ -n "$codeword" ] || { note FAIL "summarize-recall: no codeword in seed reply"; return 1; }
  (cd "$work" && echo "Acknowledged." | claude -p --model "$MODEL" --resume "$sid" >/dev/null)

  src="$proj/$sid.jsonl"
  "$BIN" extract "$src" --out "$work/ws.json" --keep 1 >/dev/null 2>&1
  jq -n --arg s "I was asked to invent a codeword and stated it once: $codeword. The user said they would ask for it later." '{"0": $s}' > "$work/sums.json"
  newid="$("$BIN" assemble "$src" "$work/sums.json" --keep 1 2>/dev/null)"
  "$BIN" verify "$proj/$newid.jsonl" --source "$src" >/dev/null 2>&1 || { note FAIL "summarize-recall: verify failed"; return 1; }

  reply="$(cd "$work" && echo "What was the codeword? Reply with only the codeword." | claude -p --model "$MODEL" --resume "$newid" --fork-session)"
  if grep -q "$codeword" <<<"$reply"; then
    note PASS "summarize-recall: $codeword recalled from the synthetic summary alone"
  else
    note FAIL "summarize-recall: expected $codeword, got: $reply"
    return 1
  fi
}

# ----------------------------------------------------------------- scenario 2: mask continuity
scenario_mask() {
  local work proj seed sid src newid reply
  work="$(new_workdir)"
  proj="$(project_dir_for "$work")"

  seed="$(run_claude "$work" "Use the Bash tool to run exactly: seq 1 200 . Then tell me the sum of the first 3 numbers it printed." --allowedTools "Bash(seq:*)")"
  sid="$(jq -r .session_id <<<"$seed")"
  (cd "$work" && echo "Good, thanks." | claude -p --model "$MODEL" --resume "$sid" >/dev/null)

  src="$proj/$sid.jsonl"
  newid="$("$BIN" assemble "$src" --mode mask --keep 1 2>/dev/null)"
  "$BIN" verify "$proj/$newid.jsonl" --source "$src" >/dev/null 2>&1 || { note FAIL "mask-continuity: verify failed"; return 1; }

  reply="$(cd "$work" && echo "What exact command did you run with the Bash tool earlier? Reply with just the command." | claude -p --model "$MODEL" --resume "$newid" --fork-session)"
  if grep -q "seq 1 200" <<<"$reply"; then
    note PASS "mask-continuity: command recalled over masked records"
  else
    note FAIL "mask-continuity: expected 'seq 1 200', got: $reply"
    return 1
  fi
}

pass=0
fail=0
scenario_summarize && pass=$((pass + 1)) || fail=$((fail + 1))
scenario_mask && pass=$((pass + 1)) || fail=$((fail + 1))
echo "smoke: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
