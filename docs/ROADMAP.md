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
- [ ] Stop-hook / scheduled-job packaging for the continue loop (today it is a documented
      one-liner; a shipped hook config would make it turnkey).

## Track C: confidence

- [x] 30 integration tests over synthetic sessions covering every failure class identified in the
      research review (branches, compact boundaries, image-first turns, in-flight tool calls,
      provenance round-trips, cache hits, ledger supersession, split seams, planner floors,
      churn guard, dangling lineage).
- [x] bench/smoke.sh: repeatable headless end-to-end proof (summarize-mode recall of a fact that
      exists only in the synthetic summary; mask-mode continuity over elided records).
- [ ] One iterated-lifecycle smoke scenario (compact, resume+continue, compact, probe planted
      facts) so the multi-pass invariants are guarded by a live test.
- [ ] Rubric hardening as practice: when a resumed session stumbles, rehydrate the segment, name
      what was lost, amend the rubric (the ACON loop, arXiv 2510.00615, applied manually).

## Descoped (deliberately, with reasons)

- **Published benchmark suite** (six-arm comparisons, decay curves as a function of pass count,
  corpus-wide next-action-preservation): research deliverables, not tool deliverables. The
  smoke bench keeps the empirical honesty; the rest is parked until the tool goal is met.
- **Soft-prompt / gist-token / activation compression**: requires model weight access and
  produces non-readable states; incompatible with a stock-CLI-resumable text artifact.
- **Knowledge-graph memory** for single sessions: graphs pay off across many sessions, not inside
  one transcript.
- **Live/proxy operation**: offline hindsight is this tool's structural edge (future-file-overlap
  salience is impossible live); live compaction has a different trust and failure surface.
- **Queryable-original mode / symbolic state canvases / cross-session memory distillation**:
  interesting, not on the critical path; rehydrate is the primitive the first would build on.
