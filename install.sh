#!/bin/sh
# One-line setup for segment_recompact (Claude Code session compaction):
#   curl -fsSL https://raw.githubusercontent.com/Don-Osipov/segment_recompact/main/install.sh | sh
# Installs the plugin, fetches its binary, and makes interactive `claude` compact in place.
# Undo: `~/.claude/recompact/bin/recompact uninstall` and `claude plugin uninstall segment-recompact`.
set -eu

command -v claude >/dev/null 2>&1 || {
  echo "Claude Code is not installed: https://docs.claude.com/en/docs/claude-code/setup" >&2
  exit 1
}

echo "Adding the segment-recompact marketplace..."
claude plugin marketplace add https://github.com/Don-Osipov/segment_recompact.git >/dev/null 2>&1 ||
  claude plugin marketplace update segment-recompact >/dev/null 2>&1 || true
echo "Installing the plugin..."
claude plugin install segment-recompact@segment-recompact

launcher=""
for p in "$HOME"/.claude/plugins/cache/segment-recompact/segment-recompact/*/bin/recompact; do
  [ -x "$p" ] && launcher="$p"
done
[ -n "$launcher" ] || {
  echo "The plugin installed, but its launcher was not found under ~/.claude/plugins/cache." >&2
  exit 1
}

echo "Fetching the recompact binary..."
"$launcher" version
"$launcher" install
