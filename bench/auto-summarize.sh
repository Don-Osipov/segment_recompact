#!/usr/bin/env bash
# Headless summarizer prototype: one cheap-model call per worksheet unit, epistemic rubric inline.
set -euo pipefail
SCRATCH="$(cd "$(dirname "$0")" && pwd)"
WS="$SCRATCH/live-ws.json"
OUT="$SCRATCH/sums"
mkdir -p "$OUT"

digest_unit() { # key -> compact text digest on stdout
  local key="$1"
  jq -r --arg k "$key" '
    def dig: [.[] |
      if .kind=="assistant_text" then "ASSISTANT: " + (.text // "" | .[0:900])
      elif .kind=="delivered_message" then "AGENT-REPORT(" + (.delivered // "?") + "): " + (.text // "" | .[0:700])
      elif .kind=="tool_use" then "TOOL " + (.name // "?") + " " + ((.input // {} | tojson) | .[0:150])
      elif .kind=="tool_result" then "RESULT[" + (.status // "?") + "," + ((.chars // 0)|tostring) + "]: " + (.result // "" | .[0:250])
      elif .kind=="compact_summary" then "COMPACT-SUMMARY: " + (.text // "" | .[0:400])
      elif .kind=="thinking" then empty
      else (.kind // "?") end
    ] | join("\n") | .[0:8000];
    if ($k | test("\\.")) then
      ($k | split(".")[0] | tonumber) as $s |
      .segments[] | select(.index==$s) |
      "USER ASKED: " + (.user_text[0:600]) + "\n--- unit \($k) activity ---\n" +
      (.parts[] | select(.key==$k) | .activity | dig)
    else
      .segments[] | select((.index|tostring)==$k) |
      "USER ASKED: " + (.user_text[0:600]) + "\n--- activity ---\n" + (.activity | dig)
    end
  ' "$WS"
}

summarize_one() {
  local key="$1"
  [ -s "$OUT/$key.txt" ] && return 0
  {
    echo "You are compacting a Claude Code session transcript. Below is one unit of agent activity that followed the user message shown. Write the replacement summary in first person past tense, as the assistant's own recap. Preserve exactly: decisions and their reasons, rejected approaches, discovered values/names/numbers/ids (quote them verbatim), errors and their outcomes, file paths. Grade claims honestly: state success only where the activity shows it verified; mark observed-but-unverified as such. 3 to 6 sentences. Plain text only, no preamble, no headers."
    echo
    digest_unit "$key"
  } | claude -p --model haiku > "$OUT/$key.tmp" 2>/dev/null && mv "$OUT/$key.tmp" "$OUT/$key.txt"
  [ -s "$OUT/$key.txt" ] || { echo "EMPTY: $key" >&2; return 1; }
  echo "done: $key" >&2
}
export -f digest_unit summarize_one
export WS OUT

xargs -P 6 -I{} bash -c 'summarize_one "$@"' _ {} < "$SCRATCH/need-keys.txt"
ls "$OUT"/*.txt | wc -l
# build summaries.json
jq -n '[inputs] | add' > /dev/null 2>&1 || true
python3 - "$OUT" "$SCRATCH/auto-sums.json" <<'EOF'
import json, os, sys
d, out = sys.argv[1], sys.argv[2]
m = {}
for f in os.listdir(d):
    if f.endswith('.txt'):
        m[f[:-4]] = open(os.path.join(d, f)).read().strip()
json.dump(m, open(out, 'w'), indent=1)
print(f"{len(m)} summaries -> {out}")
EOF
