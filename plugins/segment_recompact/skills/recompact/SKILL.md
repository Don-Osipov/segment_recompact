---
name: recompact
description: Offline, segment-wise compaction of a Claude Code session .jsonl, plus recovery of anything a past compaction removed. Keeps every user turn verbatim, replaces older agent work with summaries or masked payloads that carry provenance to the untouched originals, and closes the file with an orientation brief for whoever resumes it. Use when asked to recompact / compress / shrink a session transcript, and also when asked to recover, rehydrate, or look up content from an earlier or compacted session ("what did the research say", "restore that elided result", "[recompact: …] marker", "recompact summary", "recall"), or when the current transcript contains such markers and the verbatim original would help.
user_invocable: true
---

# recompact

An offline alternative to Claude Code's built-in compaction. It reads a session `.jsonl`, writes a
smaller, resume-compatible twin next to it (the original is never touched), and closes the twin
with an orientation note. User turns stay verbatim. Older agent work becomes a summary (written by
you or by a headless model) or a masked copy whose bulky tool output is replaced by an addressable
marker. Everything removed can be read back with `recall`.

Helper binary: `"${CLAUDE_PLUGIN_ROOT}/bin/recompact"` (also on PATH as `recompact`). Build it if
missing (needs a Rust toolchain); remove the old binary first, because copying over a signed macOS
binary gets it killed on launch:

```bash
[ -x "${CLAUDE_PLUGIN_ROOT}/bin/recompact" ] || ( cd "${CLAUDE_PLUGIN_ROOT}" && cargo build --release \
  && mkdir -p bin && rm -f bin/recompact && cp target/release/recompact bin/recompact )
```

## What do you need?

| Situation | Do this |
|---|---|
| You are inside a compacted session (preamble "This transcript was compacted by segment_recompact", footers `[recompact summary … · recall <id>]`, markers `[recompact: elided …]`) | Read **Waking up in a twin** below. Do not compact again. |
| You need an exact detail a summary or marker dropped | `recall` tool: `query="words"` to search, `selector="<id>"` to read one item |
| Compact a session, hands-off | `recompact continue <session> --threshold 150000 --summarize-with haiku` |
| Compact with zero model cost | `recompact assemble <session.jsonl> --mode mask` |
| Compact with the best summaries (you write them) | **Manual procedure** below |
| Keep a long session alive indefinitely | `recompact shell <session> --threshold 150000 --summarize-with haiku` |

Inside Claude Code, commands that take a session default to the current one: Claude Code exports
`CLAUDE_CODE_SESSION_ID` to Bash and to the recall server.

## Waking up in a twin

The last assistant message is the orientation note. It holds a **State when compacted** list
(files changed, taken from the tool calls rather than prose; commits and branches; the last
observed state of each PR; the last validation command and how it ended; the most recent asks)
and the user's **standing instructions, verbatim**. On resume, a SessionStart hook adds how long ago the
compaction was, how long the work itself has been idle, and what changed in the repo since.

1. **Already-compacted check.** If the last user turn before the note asked for /recompact, that
   request produced this session; it has run. Do not compact again unless the user explicitly asks
   for a second pass after substantial new work.
2. **Treat the brief and the summaries as a snapshot.** Re-check anything external before acting
   on it: PR state (`gh pr view`), deploys, database rows, running jobs, branch heads.
3. **Recall before re-deriving.** A summary is honest but lossy. Lines beginning `⟨carried⟩` were
   copied mechanically from the original (changed files, verbatim errors, identifiers later turns
   used). For anything else exact (an error message, a query result, a file as it was, a
   screenshot), call `recall` instead of re-running the work or guessing.
4. **User turns are verbatim**, and so are messages the user typed mid-turn (shown as "The user sent
   a new message while you were working").

## Recall

One MCP tool, `recall`, served by this plugin:

- `recall(query="max_client_conn pgbouncer")` searches the originals behind every summary and
  marker in this session's history, across all compaction generations, and returns ranked snippets
  with ids. Exact identifiers work best.
- `recall(selector="1a2b3c4d")` returns one item in full. The id is what a footer prints
  (`recall 1a2b3c4d`) or what a marker prints (`rehydrate 1a2b3c4d`). Ids resolve across every
  project directory; no session or file is needed. A summary's id expands to the records it replaced.
- `recall()` lists this session's summaries with their ids.
- Large payloads come in 8,000-character chunks (`chunk=2`, …); multi-record expansions come back as
  an index of ids.

Without the tool, in a shell: `recompact recall --query "words"` or `recompact recall <id>`.
`recompact rehydrate <compacted.jsonl> <selector>` prints raw records.

## Compacting

All numbers are **context tokens**, what `/context` shows. The helper measures what the model is
actually sent (message text, tool calls and results, the rendered text of attachments; never the
JSON envelope, and never persisted thinking) at the session model's measured tokenizer ratio, plus
~35k for system prompt and tools (`--overhead` overrides it). A session's live size comes from its
last usage record, which also counts preserved thinking. On Opus/Fable that can be a third of
it, and none of it survives into the twin.

What every compaction does, whatever the mode:

- keeps every user turn and every mid-turn user message verbatim;
- drops harness ceremony outside the recent tail (per-turn reminders, tool/skill/agent listings,
  CLAUDE.md copies, prompt snapshots), which resume re-announces in their current versions, and
  removes persisted thinking everywhere;
- keeps the last `--keep` turns (default 1) verbatim, but at most `--tail-budget` tokens of them
  (default 80000): an oversized final turn keeps only its newest parts;
- appends beneath each summary its changed files, its errors verbatim, and the identifiers that
  later turns still use (`⟨carried⟩` lines);
- closes the file with the orientation note and a title ending "(recompact N)";
- reports a **retention** figure: how many identifiers referenced across turns remain visible
  (the rest are recall-only).

### Hands-off: `continue`

```bash
recompact continue <session.jsonl | sessionId> --threshold 150000 --summarize-with haiku \
  [--escalate-with sonnet --escalate-above 0.4] [--keep 1] [--error-floor]
```

Resolves the newest descendant (compacted twins and `/branch` copies, whichever moved last), and
when its live size exceeds the threshold, plans per-unit treatments toward it: verbatim, mask, or a
summary written headlessly (~10 units per call, no MCP servers, cached by content hash so repeat
runs pay only for new material). Units with no agent activity get a mechanical summary; a unit
the summarizer never returns is masked rather than failing the run; summaries are saved after every
batch. Verifies the result, removes it if verification fails, and always prints a resumable id.
Error-bearing units may be summarized because their error text is carried verbatim;
`--error-floor` keeps them masked instead.

### Zero cost: mask mode

```bash
recompact assemble <session.jsonl> --mode mask [--keep 1]
```

Keeps every record's prose; replaces tool results over 500 characters with a marker carrying a
one-line preview and a recall id; keeps errors (head+tail to 2,000 characters); truncates oversized
tool inputs. Cannot hallucinate. On tool-heavy sessions it approaches summary-level savings.

Add `--target <tokens>` (to `assemble`) to plan treatments toward a budget; `--plan` prints the
per-unit table (salience, treatment, floor) without writing.

### Manual procedure (best summaries)

1. **Work dir and checksum** (the original is opened read-only; prove it):
   ```bash
   WORK=/var/tmp/recompact-work/$(date +%Y%m%d-%H%M%S); mkdir -p "$WORK"
   shasum -a 256 <session.jsonl> | tee "$WORK/source.sha256"
   ```
   Never use `/tmp` (tmpfs).
2. **Extract** the worksheet:
   ```bash
   recompact extract <session.jsonl> --out "$WORK/segments.json" --keep 1
   ```
   It lists the unit keys that need summaries (e.g. `3`, `12.0`, `12.1` for a turn split into
   parts). Units with no agent activity are summarized mechanically and parts kept by the tail
   budget are marked `kept_verbatim`; neither needs a summary. Each unit shows the user's ask, the
   agent's text, tool calls, and results (truncated head+tail, with status `ok` / `error` /
   `empty` / `duplicate` and full length), plus a code-derived `derived_index` of files and
   commands.
3. **Write `summaries.json`**: `{"<key>": "<summary>", …}`, one per listed key. Rubric:
   - First person, past tense, as the assistant's own recap; the next user turn must still make
     sense after the raw activity is gone.
   - What was asked, what I did, the outcome, decisions and why, **approaches tried and rejected
     with the reason**, what was left unfinished.
   - Quote exact values, names, ids, and commands verbatim.
   - Grade every outcome: VERIFIED (an exit code, test output, or query result proves it),
     OBSERVED (partial or interrupted output), CLAIMED (asserted without evidence). A killed,
     timed-out, or erroring command is never a success.
   - Skip file lists and error dumps: those are carried beneath the summary mechanically.
   - Optional `"ledger"`: standing constraints, corrections, and decisions to pin before the tail.
     A new ledger replaces the old one wholesale; assemble warns about identifiers it drops.
4. **Assemble** (prints the new id and a `resume with:` command that restores model and effort):
   ```bash
   recompact assemble <session.jsonl> "$WORK/summaries.json" --keep 1 [--cache <cache.json>]
   ```
5. **Verify**:
   ```bash
   shasum -a 256 -c "$WORK/source.sha256"
   recompact verify <new.jsonl> --source <session.jsonl>
   ```
   Checks: one sessionId, a linear parent chain, tool pairs intact, usage stripped, the tail
   pointing at the leaf, user turns and mid-turn user messages preserved verbatim.
6. **Resume** with the printed command, e.g.
   `claude --resume <newId> --model 'claude-opus-5-5[1m]' --effort max`. Resume by id: a title
   match can still reach the original.

Compacting the session you are running in is safe at a stopping point: the twin is a new file and
the live session keeps appending to the original. Your own context does not shrink; the payoff is
the next resume.

## Keeping a session alive: `shell`

```bash
recompact shell <sessionId> --threshold 150000 --summarize-with haiku [--goal "…"] [--auto]
```

Runs `claude --resume` with your terminal attached; when it exits, adopts the live head, compacts
if over threshold, and respawns. An agent can hand off without a keystroke by ending the CLI with
SIGTERM (`kill -TERM <pid>` as the entire command; exit 143). An active `/goal` survives and is
re-engaged with a kick prompt. Old summaries consolidate into coarser epoch digests re-derived from
the raw originals, so context stays bounded across unlimited cycles.

## Notes and gotchas

- **Only the active path is processed.** Abandoned branches and pre-auto-compaction history are
  dropped, never resurrected.
- **Provenance survives moves.** A summary records its source path and session id; when Claude Code
  relocates a session between project dirs (worktree resumes), recall finds it by id.
- **Durability.** Originals live under `~/.claude/projects`, subject to `cleanupPeriodDays`; a
  deleted original leaves its summary as the best copy.
- **Stale descendant.** `continue`/`resume` follow a twin or branch only while it is fresher than
  its parent; a twin abandoned in favor of the parent is skipped.
- **Empty units** (resume scaffolding, system notices) never go to a summarizer.
- **Resume compatibility is empirical.** The output uses only normal record types; run
  `recompact probe <session.jsonl>` after a Claude Code upgrade to flag unknown record or block
  types before trusting surgery.
- `recompact scan [project-dir]` lists sessions with sizes (live usage where known), turns, and
  flags (`compacted`, `superseded`, `est` for estimate-only).
