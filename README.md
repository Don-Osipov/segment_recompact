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

One line, on macOS or Linux with zsh or bash and Claude Code installed. There is nothing else to
set up, and running it again updates everything:

```bash
curl -fsSL https://raw.githubusercontent.com/Don-Osipov/segment_recompact/main/install.sh | sh
```

It installs the plugin with auto-update on, fetches the prebuilt binary for your platform
(checksum-verified; no Rust needed), and adds a small `claude` function to `~/.zshrc` or `~/.bashrc` that runs claude
through recompact. Aliases like `alias c='claude --model opus'` go through it too. Open a new
terminal and use claude as always:

- Type `/recompact` to compact the current session. It resumes in the same terminal.
- Sessions you expect to run long: type `/recompact on` in them (or start them with
  `claude --auto`). When a turn ends past 500k tokens (140k on Haiku), that session compacts in
  place and carries on, through as many handoffs as it takes; long autonomous turns are asked to
  reach a checkpoint first. Everything else stays untouched: automatic compaction is off by
  default. Set `RECOMPACT_WINDOW=200k` if your Opus or Sonnet plan has a 200k window.
- `/recompact off` turns it off for that session, `/recompact on 600k` changes its size,
  `/recompact status` shows where it stands, and `/recompact default on` makes it the default for
  new sessions. They answer instantly, without a model turn.

Anything removed stays readable through the `recall` tool. `claude -p`, subcommands, and flags the
launcher does not recognize run plain claude. New versions install themselves when claude starts
(the plugin's auto-update); the first run of each version downloads its binary from GitHub, so it
needs network access (or `cargo`). `recompact update` updates on demand, and `recompact doctor`
checks the whole setup. To undo the shell change: `~/.claude/recompact/bin/recompact uninstall`.

### Installing or updating with an agent

Paste this to Claude (or any coding agent): "Install segment_recompact: follow the agent steps in
https://github.com/Don-Osipov/segment_recompact". The steps, for the agent:

1. Run `curl -fsSL https://raw.githubusercontent.com/Don-Osipov/segment_recompact/main/install.sh | sh`.
   It is non-interactive, idempotent (the same command updates), and safe inside a Claude Code
   session. If piping to `sh` is not allowed, run the steps in `install.sh` one by one.
2. Run `~/.claude/recompact/bin/recompact doctor` and relay its lines. Every line should be `ok` or
   `note`; for a `fix` line, run the command it names, then run doctor again.
3. Tell the user: open a new terminal and start claude as usual. Sessions already running keep the
   old setup until restarted. In sessions they want to run long, `/recompact on`.

Do not edit `~/.zshrc` or Claude Code settings by hand, build from source, or run `recompact shell`
inside a claude session. If doctor reports that the shell file already defines `claude`, ask the
user before changing that definition. To update later: `~/.claude/recompact/bin/recompact update`.

Already have the plugin (for example through a team's settings)? Type `/recompact setup` once
inside claude instead.

### Share with a team

Add this to a repository's `.claude/settings.json`. Everyone who trusts the folder gets the
plugin and its updates; each person then types `/recompact setup` once for in-place compaction:

```json
{
  "extraKnownMarketplaces": {
    "segment-recompact": {
      "source": { "source": "git", "url": "https://github.com/Don-Osipov/segment_recompact.git" },
      "autoUpdate": true
    }
  },
  "enabledPlugins": { "segment-recompact@segment-recompact": true }
}
```

### Manual install and development

```bash
claude plugin marketplace add https://github.com/Don-Osipov/segment_recompact.git
claude plugin install segment-recompact@segment-recompact
recompact install        # inside claude, the plugin's bin/ is on PATH; or /recompact setup
```

`bin/recompact` is a launcher script: it runs `target/release/recompact` when you have built one
(`cargo build --release` in `plugins/segment_recompact`), and otherwise downloads the release
binary for the plugin's version into `~/.claude/recompact/bin`. Every version bump merged to
`main` publishes that release. For development, add the marketplace as a local path
(`claude plugin marketplace add /path/to/segment_recompact`) so edits apply without reinstalling.
`recompact version` prints the running version.

## How it works

```
recompact continue <session> --threshold 150000 --summarize-with haiku   # hands-off: plan, summarize, verify
recompact assemble <session.jsonl> --mode mask                            # zero-cost: mask bulky tool output
recompact extract  <session.jsonl> → worksheet; you write summaries.json; recompact assemble … → twin
recompact verify   <twin.jsonl> --source <session.jsonl>
recompact recall   --query "words" | <id>                                 # read back anything removed
```

A compacted twin:

- keeps every user turn, including messages typed mid-turn, verbatim;
- keeps the last turn verbatim up to a tail budget, and summarizes or masks older agent work;
- carries, beneath each summary, the files it changed, its errors verbatim, and the identifiers
  later turns still use, chosen by code from the session's future rather than by the summarizer;
- drops stale harness ceremony and persisted thinking, which Claude Code re-creates or strips anyway;
- ends with an orientation note: a mechanical brief of where the work stood (files, commits, PRs,
  the last check and its result, the most recent asks) and the user's standing instructions quoted
  verbatim. A SessionStart hook adds the compaction's age and the repo's drift since.

Everything removed stays addressable. Each summary footer and each marker carries an 8-character id
that the `recall` MCP tool resolves across every project directory, and `recall(query=…)` searches
the originals behind a session's whole compaction lineage.

Sizes are what the model is sent: visible text at the session model's measured tokenizer ratio,
plus system and tool overhead. (The raw record size overstates a resumed twin by a median 3.6x.)

## Safety

- The original session file is **never modified** — opened read-only; output is create-new-only in
  the same project dir, and `verify` checksums the original to prove it. Rollback is deleting the
  new file.
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
