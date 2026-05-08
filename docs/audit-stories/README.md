# Audit stories

Concrete write-ups of moments where the system caught itself being
wrong. Each entry is one specific incident — a flag that turned into a
real bug, a "this can't happen" that did, an estimate that doubled
when the failing test got written.

These are not retrospectives or post-mortems in the SRE sense (no one
paged, nothing went down). They're closer to **engineering field
notes**: what we believed, what was actually true, how the gap got
caught, and what specifically a future reviewer would do differently.

## Why these exist

Each story aims to teach one transferable habit. Specifically:

- *"Read-the-code-and-conclude-X" claims are weaker than they feel.*
  The cheapest way to find out whether they hold is to write the test
  first.
- The first estimate after an audit is usually too low because the
  audit operated on the model in your head, not the code as written.
- Doc-and-implementation drift is normal; surface it when found, do
  not paper over it.

## Index

- [`case-001-cache-validator-audit.md`](./case-001-cache-validator-audit.md)
  — arc-flagged "single-validator-chain assumption" turned out to be a
  structural bug; writing two failing tests changed the W4 estimate
  from 0.5d to 1-2d (chose A2 path, shipped same day).
  Companion (consumer side, when arc writes it):
  `arc/docs/design-review-corpus/case-003-*` — the dogfood-prep flag
  → tars audit → fix shipped → ready-to-dogfood story.

## Cross-repo numbering

Case numbers in this directory are **independent of arc's
`design-review-corpus/case-NNN-*`**. The two repos own different
review cadences and their numbering happens to drift.

Cross-references when a case has a companion:

| tars audit-story | arc case |
|---|---|
| (when written) — B-31 v5 "upstream typed schema collapses downstream defenses" | [`arc/docs/design-review-corpus/case-001-tars-preflight-api`](https://github.com/example/arc) — same incident, consumer-side review of how tars-py shipping `CapabilityRequirements` as a frozen pyclass + `check_capabilities_for` cleaned up arc's hand-written dataclass + drift guard. |
| `case-001-cache-validator-audit.md` (this dir) | (pending) `arc/docs/design-review-corpus/case-003-*` — dogfood-prep flag → audit → W4 fix |

## When to add a new entry

Heuristic: if at any point you said *"actually it's worse than that"*
or *"the test made me revise my estimate"*, write it up while the
context is fresh. Two days later you'll only remember the cleaned-up
narrative, not the wrong turns that taught the lesson.

Keep them tight (≤200 lines). Cite commit hashes, source line numbers,
and exact test names so a future reader can rerun the audit.
