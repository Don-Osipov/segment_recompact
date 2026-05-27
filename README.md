# segment_recompact

Offline, segment-wise, **structure-preserving** compaction of Claude Code session transcripts.

Claude Code's built-in compaction summarizes the whole conversation into one prose blob at a token
threshold. `segment_recompact` takes a different tack — a retrospective, offline pass over a
session `.jsonl` that:

- **segments the session by genuine user turn**,
- **keeps every user turn verbatim** (never compressed),
- **collapses each segment's agent turns + tool results into one summary** that Claude writes,
- keeps the most recent *K* turns verbatim for clean resume,
- emits a **shorter, resume-compatible** `.jsonl` — a normal (just smaller) session in a new file.

A small Rust helper does the deterministic surgery (parsing, segmenting, re-chaining); **Claude is
the summarizer.** It's an ad-hoc, human-in-the-loop procedure, not a turnkey one-command tool — the
value is a disciplined process plus correct structural surgery.

## Install

Requires a **Rust toolchain** (`cargo`) — the helper builds from source on install.

```bash
# add this repo as a marketplace (GitHub repo, git URL, or local path)
claude plugin marketplace add <your-org>/segment_recompact     # or: claude plugin marketplace add /path/to/segment_recompact
claude plugin install segment-recompact@segment-recompact
```

The plugin's `Setup` hook runs `cargo build --release` and places the binary at
`bin/recompact`. (The skill also builds it on first use if it's missing, so a missing Setup run is
self-healing.)

Then, in any session:

```
/recompact
```

## How it works

```
recompact extract  <session.jsonl>  ->  work/segments.json   (Rust: parse, classify, segment)
   Claude reads each segment, writes summaries -> work/summaries.json
recompact assemble <session.jsonl> work/summaries.json  ->  <newId>.jsonl  (Rust: rebuild + re-chain)
   verify, then: claude --resume <newId>
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
    ├── src/main.rs + Cargo.toml           # the helper (extract / assemble)
    ├── hooks/hooks.json                   # Setup hook: cargo build on install
    └── bin/                               # built binary lands here
```

## License

Copyright © Stephen Roylance.
