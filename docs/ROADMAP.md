# Roadmap

This fork develops segment_recompact into a reversible, evidence-graded, three-tier compaction
system, with an evaluation harness that measures what actually survives. Each principle below
cites the strongest evidence behind it.

## Design principles

1. **Reversible, not lossy.** Every compacted segment keeps a pointer back to the raw records it
   replaced, and the original session file is never modified. Retrieval-backed (reversible) memory
   holds ~0.95 recall flat as compaction count grows; irreversible summarization degrades to
   0.33-0.56 and is worst at high compaction frequency (arXiv 2607.08032).
2. **Mechanical before generative.** Masking and truncation cannot hallucinate; LLM rewriting can.
   Observation masking matches LLM summarization on SWE-bench at roughly half the cost, and a
   hybrid beats both by 7-11% (arXiv 2508.21433). Most of the measured win in Anthropic's own
   context-management evals comes from clearing stale tool results, not from summarizing prose.
3. **Facts live in structure; prose carries continuity.** File paths, exit codes, test state, and
   decisions belong in code-derived structured fields. "Artifact trail" (which files were touched)
   is the worst-scoring dimension of every production compactor measured (Factory AI evaluation,
   2.19-2.45 out of 5). Prose summaries reliably drop paths; parsers do not.
4. **Never summarize a summary.** Every pass re-derives from the original raw session. Recursive
   re-summarization is the documented decay mechanism: constraint violations compound from 0% to
   78% across four compaction rounds (arXiv 2606.22528), and recursive summary drift is the reason
   Sourcegraph Amp removed live compaction entirely.
5. **User turns are inviolable.** Kept verbatim, with fail-open classification: any user record the
   tool cannot positively identify as a tool result stays verbatim. Misclassification may cost
   compression, never content.
6. **Verified over narrated.** Summaries must grade claims: verified (exit code, test output),
   observed (text seen in a truncated or killed stream), or claimed (the agent's own narrative).
   A killed process's partial output must never be recorded as a confirmed result
   (arXiv 2607.13071).

## Phase 0: correctness (this branch)

- [x] Active-path extraction: walk the parentUuid chain from the leaf; drop abandoned retry
      branches and pre-compaction history instead of re-linearizing the whole file.
- [x] Fail-open genuine-user classification (image-first and unknown content shapes stay verbatim).
- [x] Compaction-summary pinning: segments carrying an isCompactSummary record are never collapsed,
      and the summary text is rendered into the worksheet.
- [x] `verify` subcommand: chain integrity, tool-pair atomicity, usage stripping, tail validity,
      verbatim user-turn fidelity against the source's active path.
- [x] lib/bin split with integration tests over synthetic sessions; portable UUID generation; CI.
- [x] `probe` subcommand: schema sanity-check for a session file, so Claude Code format drift fails
      loudly before any surgery. Validated against 1,713 local sessions (zero parse failures; the
      39 hard failures are genuine user-less session stubs).
- [x] Release workflow: binaries for macOS arm64/x64 and Linux x64 on tag push.

## Phase 1: three-lane compression engine

- [x] Mechanical lane in the worksheet: statuses instead of payloads for empty and duplicate
      results, head+tail truncation (0.6 head ratio) so trailing errors survive, char counts,
      larger verbatim budget for errors, tool names on every result.
- [x] Deterministic index lane (code, not model): files touched with role, commands run, tool
      counts, error count; in the worksheet as `derived_index` and embedded in synthetic records
      as `recompactIndex`; regenerated fresh from raw on every pass.
- [x] Provenance and rehydration: synthetic records embed
      `recompactProvenance {source, sourceSessionId, coveredUuids}`;
      `recompact rehydrate <compacted.jsonl> [ordinal]` lists summaries or dumps the verbatim
      originals from the untouched source.
- [x] Iteration invariant (early Phase 2 pull-forward): segments carrying a `recompactSynthetic`
      record are pinned verbatim, so a second pass can never summarize a summary.
- [ ] Superseded-file-read elision and errored-call input dropping in the assembled output.
- [ ] Narrative lane rubric: structured fields for decisions and rejected alternatives, verbatim
      key-phrase quoting, mandatory epistemic grading in SKILL.md.
- [ ] Mask mode: a no-LLM compaction mode that keeps assistant text verbatim and replaces old
      tool_result contents with placeholders (pairs stay atomic).

## Phase 2: iterated recompaction lifecycle

- Checked invariants: extract refuses to summarize recompactSynthetic records; every pass
  re-derives from the raw ancestor (lineage header locates it); same inputs produce byte-identical
  output.
- Content-hash summary cache: unchanged segments reuse their summaries, so incremental passes cost
  LLM calls only for new segments and keep the compacted prefix byte-stable.
- Pinned ledger (JSON): constraints, decisions, and mid-session corrections, with explicit
  supersession entries, re-injected verbatim on every pass.
- Promotion pass: durable facts move to companion documents at each boundary; the resume seam
  restates the last user ask and instructs the resumed agent to verify load-bearing claims against
  live reality before acting on them.

## Phase 3: evaluation harness

- Metrics: next-action preservation (arXiv 2607.02911), fact recall and constraint survival as a
  function of recompaction count, token reduction.
- Arms: mask-only, hybrid, prose-only, native /compact, docs-only, docs+hybrid.
- Guideline-optimization loop (arXiv 2510.00615): every eval failure becomes a rubric amendment.
- Publish the numbers. No tool in this niche has published fidelity measurements.

## Phase 4: automation and UX

- Headless end-to-end mode (supervised mode remains the default); configurable summarizer model.
- Preview/diff of kept-vs-summarized before finalizing; files-touched rehydration manifest.
- Handoff export: same worksheet, alternative output as a fresh-thread brief.

## Phase 5: exploratory

- Sub-task folding inside oversized segments.
- Queryable-original mode: the compacted session as a table of contents over a greppable archive.
- Symbolic state notation for tool-chain-heavy segments.
- Cross-session distillation into project memory.

## Rejected directions

- Soft-prompt/gist-token/activation compression (ICAE, activation beacons, 500xCompressor):
  requires model weight access and produces non-readable states, incompatible with a
  stock-CLI-resumable text artifact.
- Knowledge-graph memory for single sessions: graphs pay off across many sessions, not inside one
  transcript.
- Live/proxy operation: offline hindsight is this tool's structural edge; live compaction has a
  different trust and failure surface.
- Replacing user-turn segmentation with learned fold points: gives up determinism and the
  verbatim-user-turn guarantee for gains sub-splitting captures more safely.
