#!/bin/sh
# Install or update segment_recompact (Claude Code session compaction). Safe to run again; each
# run brings everything to the latest version:
#   curl -fsSL https://raw.githubusercontent.com/Don-Osipov/segment_recompact/main/install.sh | sh
# Non-interactive, and fine to run from inside a Claude Code session. It installs the plugin,
# turns on its auto-update, fetches its binary, and makes interactive `claude` compact in place.
# Undo: `~/.claude/recompact/bin/recompact uninstall`, then
#       `claude plugin uninstall segment-recompact@segment-recompact`.
set -eu

command -v claude >/dev/null 2>&1 || {
  echo "Claude Code is not installed: https://docs.claude.com/en/docs/claude-code/setup" >&2
  exit 1
}

echo "==> plugin"
# Adding a marketplace replaces one of the same name, so a known one (a local checkout, say) is
# left as it is.
if ! claude plugin marketplace list 2>/dev/null | grep -q "segment-recompact"; then
  claude plugin marketplace add https://github.com/Don-Osipov/segment_recompact.git >/dev/null
fi
claude plugin marketplace update segment-recompact >/dev/null
claude plugin install segment-recompact@segment-recompact >/dev/null
claude plugin update segment-recompact@segment-recompact

# The newest installed version (just installed or updated) is the most recently written one.
launcher="$(ls -dt "$HOME"/.claude/plugins/cache/segment-recompact/segment-recompact/*/bin/recompact 2>/dev/null | head -n 1)"
[ -n "$launcher" ] && [ -x "$launcher" ] || {
  echo "The plugin installed, but its launcher was not found under ~/.claude/plugins/cache." >&2
  exit 1
}

echo "==> binary"
"$launcher" version
echo "==> setup"
"$launcher" install
echo "==> check"
"$launcher" doctor || true
