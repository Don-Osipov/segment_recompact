# segment_recompact

Offline, segment-wise, **structure-preserving** compaction of Claude Code session transcripts.

Claude Code's built-in compaction summarizes the whole conversation into one prose blob at a token
threshold. `segment_recompact` takes a different tack — a retrospective, offline pass over a
session `.jsonl` that:

- **operates on the active path only** (abandoned retry branches and pre-auto-compaction history
  are unreachable on resume, so they are dropped, never resurrected),
- **segments the session by genuine user turn**,
- **keeps every user turn verbatim** (never compressed),
- **collapses each segment's agent turns + tool results into one summary** that Claude writes,
- keeps the most recent *K* turns verbatim for clean resume,
- emits a **shorter, resume-compatible** `.jsonl` — a normal (just smaller) session in a new file.

This fork extends the original tool; the direction (reversible compaction, mechanical-first
compression, an evaluation harness) is laid out in [`docs/ROADMAP.md`](./docs/ROADMAP.md).

A small Rust helper does the deterministic surgery (parsing, segmenting, re-chaining); **Claude is
the summarizer.** It's an ad-hoc, human-in-the-loop procedure, not a turnkey one-command tool — the
value is a disciplined process plus correct structural surgery.

## Install

**Prerequisite: a Rust toolchain** (`cargo`, from [rustup.rs](https://rustup.rs)). No prebuilt
binary ships with the plugin; the helper is compiled from source at install time, so without
`cargo` the install produces a plugin that cannot run. macOS and Linux only (the build hook is a
POSIX shell command).

```bash
claude plugin marketplace add Don-Osipov/segment_recompact
claude plugin install segment-recompact@segment-recompact
```

The marketplace source can be a GitHub `owner/repo`, a git URL, or a local path
(`claude plugin marketplace add /path/to/segment_recompact`), which is the one to use if you are
hacking on the tool: a directory-sourced marketplace picks up your edits with no reinstall.

The plugin's `Setup` hook runs `cargo build --release` and places the binary at `bin/recompact`.
(The skill also builds it on first use if it's missing, so a skipped Setup run is self-healing.)
The plugin's `bin/` is added to PATH, so `recompact` works as a bare command; the skill still
invokes it by full path, since shell variables do not persist between invocations.

Then, in any session:

```
/recompact
```

To confirm the install before relying on it, `recompact` with no arguments prints the usage block:
`extract`, `assemble`, `verify`, `probe`, `rehydrate`, `continue`, `shell`, `resume`, and `scan`.
A binary listing only `extract` and `assemble` is a stale build from an early version.

To pick up later changes, `claude plugin update segment-recompact@segment-recompact`.

## How it works

```
recompact extract  <session.jsonl>  ->  work/segments.json   (Rust: active path, classify, segment)
   Claude reads each segment, writes summaries -> work/summaries.json
recompact assemble <session.jsonl> work/summaries.json  ->  <newId>.jsonl  (Rust: rebuild + re-chain)
recompact verify   <newId>.jsonl --source <session.jsonl>   (Rust: chain, tool pairs, user-turn fidelity)
   then: claude --resume <newId>

# or the zero-LLM express lane: keep all prose, elide stale tool-result bulk
recompact assemble <session.jsonl> --mode mask   ->  <newId>.jsonl
# and reversibility: list summaries / recover the verbatim originals they replaced
recompact rehydrate <newId>.jsonl [ordinal]
```

The skill walks Claude through it, including a **mandatory backup + rollback note** before any
write, a summary-quality rubric (preserve decisions/results, reference files by path rather than
reproducing code, keep the connective tissue for the next user turn), and a verification suite.

## Safety

- The original session file is **never modified** — opened read-only; output is create-new-only in
  the same project dir; the original is also backed up before the run.
- The assembled file strips stale `usage` metadata so `/context` reports the compacted size, not
  the original's.

## Caveats (read before relying on it)

- **Reverse-engineered format.** It reads/writes Claude Code's `.jsonl` internals, which are
  undocumented and change across versions. Re-verify after a Claude Code update.
- **`/context` reads `usage`, not a re-tokenization.** The helper strips `usage` from emitted
  records so the compacted session reports its true (small) size; if a future format change moves
  where the meter reads from, this may need updating.
- **Resume from a real terminal, not the VSCode extension picker.** The extension's session picker
  only lists sessions it created, so an externally-built compacted session won't appear there. Use
  `claude --resume <newId>` in a standalone terminal.
- **Human-in-the-loop.** Claude writes the summaries during the run; quality depends on the model
  and the rubric. The most-recent *K* turns are kept verbatim to hedge recent-context fidelity.

## Layout

```
segment_recompact/                         # marketplace repo
├── .claude-plugin/marketplace.json
└── plugins/segment_recompact/             # the plugin
    ├── .claude-plugin/plugin.json
    ├── skills/recompact/SKILL.md          # the /recompact skill
    ├── src/lib.rs + src/main.rs           # the helper (all subcommands)
    ├── tests/                             # integration suite, one file per phase
    ├── hooks/hooks.json                   # Setup hook: cargo build on install
    └── bin/                               # built binary lands here (gitignored)
```

## License

Copyright © Stephen Roylance.
