# Roadmap

The goal: a really good compaction tool that makes long-running Claude agentic sessions
fundamentally better, with the research embedded as design decisions. Not a benchmark suite, not
a paper; a tool you run without thinking, and a lifecycle that lets a session live for weeks.

## Design principles

1. **Reversible, not lossy.** Every compacted unit carries provenance to the raw records it
   replaced; the original session file is never modified; `rehydrate` recovers anything.
   Retrieval-backed memory holds ~0.95 recall flat as compaction count grows; irreversible
   summarization degrades to 0.33-0.56 (arXiv 2607.08032).
2. **Mechanical before generative.** Masking and truncation cannot hallucinate; LLM rewriting
   can. Observation masking matches LLM summarization at half the cost and a hybrid beats both
   (arXiv 2508.21433). The LLM's job is narrative continuity, never fact custody.
3. **The budget is an objective; the floors are constraints; floors win.** Output may exceed a
   token target where retention genuinely helps: error content, pinned records, the recent tail,
   decision-adjacent segments. The plan says exactly what held and why.
4. **Never summarize a summary.** Every pass re-derives from raw; synthetic summaries, compaction
   blobs, and ledger records are pinned verbatim. Recursive re-summarization is the documented
   decay mechanism (0% to 78% constraint violations over four rounds, arXiv 2606.22528).
5. **Human turns are inviolable; delivered content is not.** Anything a person typed stays
   verbatim forever, fail-open. Agent-authored content arriving on the user channel (teammate
   messages, task notifications, detected by exact sentinel prefix) is compressible activity.
6. **Verified over narrated.** Summaries grade claims (verified / observed / claimed); a killed
   process's partial output is never a confirmed result (arXiv 2607.13071); file paths come from
   the code-derived index, not prose memory.

## Track A: the compaction tool

- [x] Active-path extraction (abandoned branches and pre-auto-compaction history are dropped,
      never resurrected), fail-open genuine-user classification, compaction-summary pinning.
- [x] `verify` (chain, tool-pair atomicity, usage stripping, verbatim user-turn fidelity) and
      `probe` (schema drift alarm; swept 1,713 real sessions with zero parse failures).
- [x] Mask mode: zero-LLM compaction (payload elision, error head+tail, toolUseResult duplicates,
      empty-thinking signature carriers, delivered-content elision). 63% measured on a real
      980k-token session; resume proven headlessly.
- [x] Summarize mode with the epistemic-grading rubric, per-part synthetic records, provenance,
      `rehydrate` (list + verbatim dump).
- [x] Delegation-aware splitting: oversized segments split at safe seams (never through a
      tool pair), delegation results and delivered messages first; part keys ("3.0", "3.1") in
      worksheets, summaries, and caches.
- [x] Salience-floored budget planner: `--target` chooses per-unit treatments
      (verbatim/mask/summarize) by tokens-saved x (1 - salience); salience = error density +
      future-file overlap (hindsight only an offline tool has) + correction markers in the next
      human turn; `--plan` previews the table without writing.
- [ ] Attachment-record elision (0.46MB in the reference session; resume semantics unverified,
      so untouched until studied).
- [ ] Cross-file provenance: delegation units whose full subagent transcript is locatable on disk
      (agent-*.jsonl) carry that path too, so rehydrate can follow into the subagent's session.
- [ ] Salience floor for decision segments in summarize mode (currently error floors only).

## Track B: the long-running session lifecycle

- [x] Iteration invariants: content-hash summary cache (`--cache`), pinned ledger with wholesale
      supersession (`"ledger"` in summaries.json), multi-hop provenance proven by a
      double-recompaction test (A to B, continue, B to C; B-era summaries still point at A).
- [x] Autonomous continuation: `recompact continue <session>` resolves the newest descendant via
      the per-project lineage sidecar, mask-compacts toward the threshold when over it, verifies,
      churn-guards, and always prints a resumable id — the single loop step that lets a session
      compact itself and keep running without human input.
- [x] `recompact resume` (lineage resolution alone) and `recompact scan` (project discovery:
      sizes, mask estimates, superseded sessions).
- [x] Wrap-up flow documented: self-recompaction at a stopping point is safe; the payoff lands at
      the next resume.
- [ ] Promotion pass: durable facts flow to companion documents at each boundary; the resume seam
      restates the last ask and instructs verification of load-bearing claims against live
      reality.
- [x] Summarize-mode automation: `continue --summarize-with <model>` runs the full ladder
      autonomously — the budget planner picks the units masking cannot shrink, a headless
      summarizer fills them in contiguous batches (no MCP servers, content-hash cached, tolerant
      key matching), with `--escalate-with`/`--escalate-above` routing high-salience units to a
      stronger model. Live-verified against real haiku on a prose-heavy session masking could
      not reduce.
- [x] `recompact shell`: one continuous session at the terminal — spawn interactive claude,
      adopt the live head on exit (bridge-session ids), compact over threshold, respawn, with
      active goals surviving compaction and re-engaged by kick-prompt (both behaviors verified
      live before implementation). `--goal` arms a fresh goal; `--auto` for unattended cycling.
- [x] Post-adoption verification: `verify` splits a twin at its assembly boundary (the first
      last-prompt record) and checks the assembled prefix strictly, reporting post-resume growth
      informationally; the `--source` fidelity check distinguishes trailing source growth the
      assembly never saw (session teardown, continued use — tolerated with a note) from holes in
      the middle of what it did see (still a hard failure), using kept + coveredUuids provenance
      as the knowledge boundary. Found and fixed via live adoption of the tool on its own
      build session.
- [x] Rehydrate addressing matches provenance: selectors are a part key (exactly what
      `recompactProvenance.part` advertises), a listing ordinal, or a covered record uuid
      (returns that single verbatim record); unknown selectors list the known part keys.
- [x] Agent handoff without a keystroke: a SIGTERM exit (143) from inside the session means the
      agent handed control back on purpose — the shell cycles immediately; a human exit (0/130)
      keeps the confirmation prompt.
- [x] Bounded context for arbitrarily long loops — epoch consolidation. Prior-cycle summaries no
      longer accumulate forever: when the budget needs it, runs of old synthetic records are
      consolidated into coarser epoch digests whose summarizer input is re-derived from the RAW
      records their provenance covers (recursively, across generations, to ground truth). Summary
      text is never fed to the summarizer — the never-resummarize invariant holds by
      construction. Unresolvable provenance (deleted raw file) pins the records verbatim.
      Result: a recency gradient — verbatim tail, per-unit summaries, epoch digests — with total
      size bounded across unlimited cycles. Known limitation: a part mixing synthetic and real
      records stays pinned this cycle (it consolidates once fully synthetic in a later cycle);
      the epoch record's derived index is not yet merged from the covered units' indexes.
- [x] Image-aware compaction. Found live: three ~600KB screenshots pinned in user turns made a
      session read as ~616k tokens (base64 counted as text at 4 chars/token — ~150k phantom
      tokens per image), so continue reported "no meaningful reduction" and the loop stalled.
      Token estimation now counts every embedded image at flat visual weight (~1.6k tokens,
      matching how the API bills tiles, not bytes); user turns outside the keep tail have image
      bytes replaced with rehydratable markers (user TEXT stays verbatim — the fidelity check
      compares text and ignores markers); images inside tool results get the same treatment in
      the mask lane. Live result on the stalled session: 4.5MB → 2.2MB on disk, 13 stale
      screenshots elided, recent ones kept, verify green.
- [x] Agent legibility: everything a fresh model needs is model-VISIBLE. The record envelope
      (uuids, part keys, provenance) is never shown at inference time, so a session resumed into
      a twin could not even address the markers it was reading. Now: an orientation preamble
      record (regenerated each assembly, stripped from prior generations) teaches the reading
      rules and the exact recovery commands with real paths; every summary ends with
      "[recompact summary <key> — rehydratable]" (inherited footer-less summaries are annotated
      at emission, idempotently); every elision marker embeds an 8-char uuid prefix; rehydrate
      accepts uuid prefixes (>=8 hex chars) so a marker's selector is directly actionable; the
      skill triggers on recovery-flavored asks and documents waking-up-inside-a-twin and
      autonomous-loop patterns.
- [ ] Stop-hook / scheduled-job packaging for the continue loop (today it is a documented
      one-liner; a shipped hook config would make it turnkey).

## v1.0 (2026-09): from compacting to resuming

Driven by a month of real use (62 twins, 56 resumed) and the June–September 2026 literature.

- [x] **Token accounting that matches the API.** Visible text at measured tokenizer ratios
      (Opus 5.5 2.39, Fable ~2.65, Haiku 3.06 chars/token); the old JSON/4 estimate overstated a
      resumed twin by a median 3.6x, so thresholds over-compressed. Live size from the last usage
      record (it includes preserved thinking: 38% of one Fable session).
- [x] **Recall that resolves.** Every real v0.9 recall call failed (5/5). Now: session binding via
      `CLAUDE_CODE_SESSION_ID`, project-wide ids in every footer, cross-project-dir lookup,
      relocation-proof provenance, and `query` search over the lineage's originals. Verified live:
      a resumed Opus 5.5 session queried, then selected, then answered an exact figure that existed
      only in elided output.
- [x] **Orientation.** A third of resumed twins opened with "where did we leave off". The preamble now
      carries a mechanical state brief and the user's standing instructions verbatim (constraint
      pinning restores 0% violations in Governance Decay, arXiv 2606.22528); a SessionStart hook adds
      compaction age, idle time, and repo drift (the live-evidence rule of arXiv 2609.13800).
- [x] **Carried evidence.** Changed files, verbatim errors, and future-referenced identifiers go
      beneath each summary mechanically (offline hindsight: the Slipstream check of arXiv
      2605.08580 without a model; artifact tracking is every method's weakest dimension in Factory's
      probe evaluation). Errors carried verbatim retire the blunt error floor.
- [x] **Fidelity fix:** messages typed mid-turn arrive as `queued_command` attachments and were
      summarized away; now pinned and verified.
- [x] **Leaner twins:** ceremony attachments outside the tail (re-announced on resume, verified),
      persisted thinking (stripped by Claude Code on resume; a strict preserved-thinking risk),
      and the adaptive tail budget for multi-megabyte final turns.
- [x] **Robustness:** crash-safe summary cache, mechanical empty units, targeted mask fallback,
      `/branch` copies in lineage, bare ids across project dirs, titles carried, model and effort
      restored in the resume command.

## v1.1 (2026-09): compaction in place

The v1.0 flow to compact the session you were in took six steps: note the id, open a second
session, run the skill there, copy the new id, exit, resume. Now:

- [x] **`recompact shell` is a drop-in for `claude`.** It supervises claude; hooks inside report the
      live session and request handoffs. On a request it stops claude with SIGTERM (Claude Code
      exits cleanly on it, measured: terminal restored, exit 143), compacts, and resumes the twin in
      the same terminal with the same flags, model, and effort.
- [x] **A typed `/recompact` costs no model turn:** a UserPromptSubmit hook blocks the prompt and
      hands off (measured: the command never reaches the model).
- [x] **Automatic at turn boundaries:** a Stop hook hands off when the context is over the threshold,
      unless background tasks or session crons would be lost. A re-arm floor stops a twin that could
      not get below the threshold from compacting every turn.
- [x] **Checkpoints for long autonomous turns:** past the checkpoint size, a PostToolUse hook asks
      the agent to finish its step and stop; the resumed session gets a continue prompt. The
      compaction lands on a boundary the agent chose (compaction at resolved points).
- [x] **Prewarm:** from half the threshold, a detached background run writes the summaries the
      handoff would need into the cache. Measured on a real 5.9 MB session: 34 s in the background,
      then 0.23 s for the compaction itself.
- [x] Without the launcher, `recompact handoff` compacts the current session and puts the resume
      command on the clipboard; a Stop hook says when a session is large.
- [x] Summarizer runs no longer create a Claude Code project dir per call (115 accumulated).

Next, in evidence order:
- [ ] Probe-based evaluation harness (`recompact eval`): exact-match probes mined from the original
      (errors, ids, files, next step), answered by a resumed model; TRACE-style next-action agreement.
- [ ] Typed ledger with ids and delta updates (ACE, arXiv 2510.04618), human-sourced constraints only.
- [ ] Claim tags in the headless rubric parsed into the brief ("unverified claims").
- [ ] Sub-goal epoch folds instead of time-based ones.

## v1.12 (2026-11): what real sessions showed

Driven by forensics over 141 twins and 60 compaction sources since v1.0 (2026-09-24), and the
September to November 2026 literature.

- [x] **Split points survive Claude Code 2.1.300+ transcripts.** Its request-replay records
      (`api-request`, `-blob`, `-shape`) copy already-answered tool calls, so no split point
      followed them (101 of 137 sources). Twins drop them; Claude Code ignores them on load.
- [x] **Parallel tool calls keep their results.** Claude Code chains each next call of a batch off
      the previous call record, so every result but the last hangs off the active path. They were
      dropped (448 results in 54 of 60 sources), their calls deleted as unanswered, and no split
      point followed. Measured with both fixes on the same 60 sources at a 120k target: planned
      twin p50 198k to 119k tokens, fixed floor p50 187k to 104k.
- [x] **Recall follows a switch in place.** The recall server outlives the switch and kept the old
      session id: 51 of 158 real queries searched "0 records". It now resolves the live lineage
      head on each call.
- [x] **The summarizer summarizes.** Haiku followed instructions inside the transcript (a teammate
      asking for a status report) and answered it instead, so whole batches fell back to masking:
      every batch, both waves, in one real 28-unit run. The rubric is now the system prompt, the
      units are fenced as data, and the reply is held to a JSON schema (`--json-schema`): 28 of 28.
- [x] **Summarizer input keeps the ending.** Units over 8k chars were cut at the end, which holds
      the outcome (115 of 4,630 units, 18% of all digest text). Digests now get 16k (epochs 24k)
      and are thinned from the middle by priority: routine results, then tool calls, then older
      prose; the ask and the closing words always stay.
- [x] **Summaries inside one long turn consolidate.** An autonomous session is one turn, so all its
      earlier summaries sat in the tail and piled up (374 summaries, 139k tokens, in one
      19-generation chain). With epochs, parts beyond the tail budget consolidate from raw.
- [x] **A screenshot typed mid-turn is an image, not 250k tokens.** Attachment payloads count
      images at flat weight, and queued messages lose image bytes outside the tail.
- [x] **Replies that answer by reference keep what they answer.** When the next user turn is
      "option 2", "go ahead" or "items 2 and 7", the unit's closing message is carried verbatim
      beneath its summary (20% of turn boundaries, ~1.2k chars each).
- [x] **Claude Code's own compaction yields** in sessions with auto-compaction on: its idle
      compaction (2.1.299+, sessions of 200k+ tokens idle ~54 minutes) and its automatic one are
      blocked by the PreCompact hook; recompact still compacts at a turn's end. A typed /compact
      still runs, and so does Claude Code's own near the window's end or under 200k (a small
      window filling up).
- [x] **Brief fixes.** Recent asks no longer quote `<system-reminder>` blocks; a period inside a
      file name no longer ends a standing instruction; "Last check run" names a command that ran,
      not a heredoc or quoted text that mentions a test runner (24 of 85 lines were wrong).
- [x] **Rules as policy** (ARCBench, arXiv 2611.03223: kept rules were still violated 2.6x as often
      after compaction, mostly when a later request read as permission). The brief says a later
      request does not lift a standing instruction; scope and process instructions ("leave X
      alone", "ask me before", "wait for") are captured (they drop 36% of the time, arXiv
      2610.31778).
- [x] **The next step survives** (Compact-Bench, arXiv 2610.31710; AutoCompact, arXiv 2610.02163).
      The rubric asks for the planned next change, stated concretely, and attributes instructions
      found in tool output to their source; the checkpoint request asks for the next step and the
      evidence behind it.

Next, in evidence order:
- [ ] **Originals outlive Claude Code's 30-day cleanup.** 36 of 135 twins hold summaries whose
      source is gone, so recall and consolidation lose them. Transcripts here run 8.5 GB per 30
      days, so raising `cleanupPeriodDays` is costly; hard-linking the originals a live twin
      references is the likely fix.
- [ ] **Discard-and-rebuild churn.** 34 of 131 launcher twins were rebuilt because the session
      moved on; 25 rebuilds reused cached summaries only, so the new growth stayed verbatim.
- [x] `recompact eval` (v1.13, below).
- [ ] Overhead: the 35k system+tools default runs ~20k low for sessions with many MCP servers.

## v1.13 (2026-11): measure before changing

The research of the v1.12 cycle has no Claude result for twins of 100k to 250k on real coding
work, so each design change needs a measurement on the owner's own sessions first.

- [x] **`recompact eval`.** Cut points in long original sessions; for each arm, the details the
      next three turns used that the resumed context still shows, the files the model says it
      changed and the last check's result, and its first action on the real next message, which a
      hook denies so nothing runs (the replay of TRACE, arXiv 2608.06503). Points keep a frozen
      copy, and arms compare paired.
- Baseline, 39 points from 33 sessions of the 30 days to 2026-11-10, Opus 5.5:

  | arm | size | details kept | first action same | same tool | repeats a call |
  |---|---|---|---|---|---|
  | full history | 247k | 100% | 38% | 70% | 43% |
  | v1.12 twin | 115k | 89% | 27% | 62% | 51% |

  Paired: details kept -9 points (95% interval -12 to -5), first action same -11 (-26 to +4).
  Even the full history repeats an earlier call 43% of the time: days have passed at replay, so
  the model re-checks git, GitHub and Slack. The twin loses file paths and code names the next
  turns use (18 of 39 points lost at least one), and re-reads Slack threads and git state more.
  Cost at list prices: $60 for the twin arm, $112 for the full-history arm.

- [x] **A summarized stretch keeps its tool lookups.** It dropped the ToolSearch exchanges that
      loaded deferred tools (35 of 143 on the 200k points), so a resumed session had to look its
      Slack or web tools up again. They stay now, like messages typed mid-turn.
- [x] **The thread the human linked stays.** A fetch of a link the human typed is the task's own
      source; twins re-read Slack threads 5 times in 36 resumes, the full history twice. The
      newest four such results stay through summaries and masks, cut to 6,000 characters.
- [x] **A hidden result names what it held.** The marker lists its paths, links and refs, up to
      300 characters. With the two items above: details kept +1 to +2 points on both point
      sets, size +1k.
- [x] **Size, measured at the real operating point.** 40 points at 450k to 630k live tokens
      (`points-450k.json`), where the launcher compacts:

  | target | size | details kept | first action same | repeats a call |
  |---|---|---|---|---|
  | 80k | 108k | 79% | | |
  | 120k (default) | 125k | 84% | 14% | 61% |
  | 160k | 158k | 90% | | |
  | 200k | 196k | 93% | 15% | 63% |

  200k against 120k, paired, three samples a point: first action same +1 point (-5 to +7),
  repeats +2 (-8 to +12). A 200k twin keeps more but did not change what the model did next,
  so the default stays 120k.

What the measurement can and cannot see:
- Tier 0 is free, exact and repeatable (the same twin scores the same), so it is the regression
  check for every change to what a twin keeps.
- The first-action score is noisy: at 450k the model matches the real next action 15% of the
  time from any twin, and the arms disagree on few points. A 5-point effect needs some 200
  points. Through an account pool the repeat samples rarely reuse the first one's prompt cache,
  so each sample costs a full cache write (the two 450k runs: $95 and $132 at list prices).
  `--samples` defaults to 1.

Next:
- [ ] A cheaper behavior score: more scored items per point (probe questions about details the
      next turns used, mined mechanically), so fewer points reach the same power.
- [ ] Haiku against Sonnet as the summarizer, once that score exists.
