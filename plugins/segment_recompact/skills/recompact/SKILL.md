---
name: recompact
description: Offline, segment-wise compaction of a Claude Code session .jsonl. Segments by user turn, keeps user turns verbatim, and replaces each segment's agent turns + tool results with a hand-written summary, producing a shorter resume-compatible session. Use when asked to recompact / compress / shrink a session transcript, or to experiment with better compaction.
user_invocable: true
---

# recompact

An **ad-hoc, offline** alternative to Claude Code's built-in compaction. Instead of summarizing the
whole conversation into one prose blob at a token threshold, this:

- **segments the session by genuine user turn**,
- **keeps every user turn verbatim** (never compressed),
- **collapses each segment's agent turns + tool results into one summary** that *you (Claude)* write,
- emits a **shorter, resume-compatible `.jsonl`** — a normal (just smaller) session, written to a
  new file; the original is never touched.

The deterministic surgery (parsing, segmenting, rebuilding, re-chaining) is done by a small Rust
helper bundled with this plugin; **you (Claude) are the summarizer** — the lossy intelligence
happens between `extract` and `assemble`. This is a learn-by-doing process; there is no automated
model backend.

## Helper binary

The helper lives at `${CLAUDE_PLUGIN_ROOT}/bin/recompact`, built from this plugin's `src/` by the
Setup hook on install. **Before the first run, make sure it exists; if not, build it** (requires a
Rust toolchain):

```bash
[ -x "${CLAUDE_PLUGIN_ROOT}/bin/recompact" ] || \
  ( cd "${CLAUDE_PLUGIN_ROOT}" && cargo build --release && mkdir -p bin && cp target/release/recompact bin/recompact )
```

In the commands below, invoke it by full path as `"${CLAUDE_PLUGIN_ROOT}/bin/recompact"` (the
plugin's `bin/` is also added to PATH on install, so the bare name `recompact` often works). Shell
variables do **not** persist between separate command invocations, so use the full path in each one.

## What this does

1. Confirms the target session and **takes a full backup + writes a rollback note** (hard precondition).
2. `recompact extract` → a worksheet (`segments.json`) of segments and their agent activity.
3. **You read it and write one summary per segment** into `summaries.json`, per the rubric below.
4. `recompact assemble` → a new `<newSessionId>.jsonl` in the same project dir.
5. Verification (structure, non-mutation, fidelity), then the real test: `claude --resume`.

## Procedure

### Step 0 — Pick the session, confirm

Resolve the target `.jsonl`. Sessions live at `~/.claude/projects/<munged-cwd>/<sessionId>.jsonl`,
where `<munged-cwd>` is the working directory with `/` and `.` replaced by `-`
(e.g. `/home/sdr/wirt` → `-home-sdr-wirt`). Accept either a full path or a `sessionId` + project.

Do **not** recompact the *currently live* session you are running inside — its last turn is in
flight. Recompact a session you've stepped away from, or one the user names.

Confirm with the user which session, and the `--keep K` window (default `K=1`: the last K segments
stay verbatim for clean resume).

### Step 1 — Backup + rollback note (BEFORE any write — non-negotiable)

We're poking at reverse-engineered resume behavior; "we only write a new file" is a hope, not a
guarantee. So, independent of that:

```bash
TS=$(date +%Y%m%d-%H%M%S)
PROJ=<munged-cwd>            # e.g. -home-sdr-wirt
tar czf ~/recompact-backup-${PROJ}-${TS}.tgz -C ~/.claude/projects "${PROJ}"
cp ~/.claude/history.jsonl ~/recompact-backup-history-${TS}.jsonl
```

Then write the rollback note (and echo its key facts to the user):

```bash
WORK=/var/tmp/recompact-work/${TS}; mkdir -p "$WORK"
cat > "$WORK/ROLLBACK.md" <<EOF
# Rollback — recompact ${TS}
- Backup: ~/recompact-backup-${PROJ}-${TS}.tgz  (+ ~/recompact-backup-history-${TS}.jsonl)
- Restore: tar xzf ~/recompact-backup-${PROJ}-${TS}.tgz -C ~/.claude/projects
- Original sessionId: <origId> — untouched; \`claude --resume <origId>\` returns to the working session.
- New sessionId: <to be filled by assemble> — deleting its .jsonl is safe and additive-only.
- Invariant: the tool only CREATES <newId>.jsonl and modifies no existing file (verified by checksum).
EOF
```

Backups go to `~` (matching `cleanslate`), **never `/tmp`** (tmpfs, fills up). Work files go under
`/var/tmp/recompact-work/`.

### Step 2 — Extract

```bash
"${CLAUDE_PLUGIN_ROOT}/bin/recompact" extract <session.jsonl> --out "$WORK/segments.json" --keep 1
```

It prints record/segment counts, approx tokens, and which segment indices need summaries. The
worksheet `segments.json` has, per segment: `index`, `user_text` (verbatim, for your context),
`needs_summary`, `kept_verbatim`, `activity` (the agent's text + tool calls + truncated results),
and `covered_uuids` (a lossless pointer back to the archived original).

Note: stored `thinking` blocks are usually empty (reasoning isn't persisted), so summarize from
what was **said and done** (assistant text + tool calls/results), not hidden reasoning. If a tool
result was truncated in the worksheet and you need the full thing, read it from the original file.

### Step 3 — Summarize (this is the actual work)

Read `segments.json`. For **every segment with `needs_summary: true`**, write a summary and put it
in `summaries.json` as a map from the segment's index (string) to the summary text:

```bash
# you author this file with the Write tool, e.g.:
# { "0": "…", "1": "…", "2": "…" }
```

Each summary replaces that segment's entire agent turn — so it must let the *following* user turn
still make sense. Rubric:

- **State what the agent did and the outcome** in a few sentences.
- **Preserve the non-recoverable**: errors hit + how resolved, decisions + rationale, values/answers
  discovered, build/test results (pass/fail + which), anything the next user turn reacts to.
- **Reference recoverable state by pointer, not content**: "edited `src/foo.rs` (added `bar`)", not
  the diff; inline a file's content only if that specific value mattered to the thread.
- **Drop**: superseded reads, verbose successful output, dead-end exploration, duplicate listings.
- **Keep the connective tissue**: if the next user turn says "now the other one," the summary must
  make "the other one" resolvable.
- **Stay faithful**: never claim a success that didn't happen; preserve uncertainty the agent had.

Write in first person past tense ("I read…, found…, then edited…"), as the assistant's own recap —
because that's exactly the role the record plays on resume.

### Step 4 — Assemble

```bash
"${CLAUDE_PLUGIN_ROOT}/bin/recompact" assemble <session.jsonl> "$WORK/summaries.json" --keep 1
# prints the new sessionId on stdout and writes <newId>.jsonl into the project dir
```

`assemble` errors out if any needed summary is missing, refuses to overwrite an existing file, opens
the original read-only, and drops any in-flight `tool_use` with no matching `tool_result`. Fill the
new sessionId into `ROLLBACK.md`.

### Step 5 — Verify

```bash
SRC=<session.jsonl>; NEW=~/.claude/projects/${PROJ}/<newId>.jsonl
# 0. non-mutation: original byte-identical (it never opened for write, but prove it)
md5sum "$SRC"   # compare against the value you captured pre-run
# 1. structure: every line parses
jq -e . "$NEW" >/dev/null && echo "json OK"
# 2. one sessionId; chain root→leaf unbroken; ends on assistant/last-prompt, not mid-tool-call
jq -r 'select(.sessionId)|.sessionId' "$NEW" | sort -u
jq -rc '.type' "$NEW" | tail -3
# 3. no dangling tool_use (see the validator pattern in this skill's notes / verify run)
# 4. user turns preserved verbatim: the set of genuine user-turn texts must be identical
for f in "$SRC" "$NEW"; do jq -rc 'select(.type=="user" and (.message.content|type=="array") and .message.content[0].type=="text" and (has("sourceToolAssistantUUID")|not) and (.isMeta!=true)) | .message.content[0].text' "$f" | md5sum; done
```

Spot-check 2–3 summaries against the following user turn for fidelity. Report before/after token and
record counts (from the `assemble` line).

### Step 6 — The real test: resume

Have the user run `claude --resume <newId>` and continue. This is the empirical validation that the
hand-built file is actually resume-compatible — only the user can drive the interactive resume. If
anything looks wrong, roll back per `ROLLBACK.md` (the new file is additive; deleting it is enough).

## Notes

- **Resume compatibility is empirically validated, not documented.** The output uses only normal
  record types (no reverse-engineered `compact_boundary`). Re-verify after a Claude Code version bump.
- The synthetic summary record is tagged `recompactSynthetic: true` so a compacted session is
  identifiable later.
- **`assemble` strips `message.usage` from every record.** `/context` reports the last assistant
  message's `usage` (cache_read + cache_creation + input), not a re-tokenization — so verbatim
  records copied from the source would otherwise make the compacted session report the *original's*
  token count (and possibly trigger autocompact). After resuming, sanity-check with `/context`:
  it should read a fraction of the original, not ~full.
- **Resume the compacted session from a real terminal, not the VSCode extension UI** — the
  extension's session picker only lists sessions it created, so externally-built files won't appear
  there.
- `--keep K` trades reduction for recent-context fidelity. Bigger K = safer resume, less savings.
- Reduction is dominated by how much verbose tool output the collapsed segments contained; a
  read/build/test-heavy session compresses far more than a discussion-only one.
- Rebuild the helper after editing it: `cd "${CLAUDE_PLUGIN_ROOT}" && cargo build --release && cp target/release/recompact bin/recompact`.
