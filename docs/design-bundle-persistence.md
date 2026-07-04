# Design-bundle persistence — hot local cache, git as canonical history

> Design note (design-only). How tars-editor (positioned as a *design tool*, not
> just a debug GUI) persists a **design bundle** — a set of documents, the
> discussion around them, the intermediate products, and the escalation/decision
> record — **locally, no cloud, no Firebase**, with **git as the eventual durable
> history**. Reuses the same two-track split as [`observability-scaling.md`](./observability-scaling.md).

## 0. Thesis

A design session produces four things: **documents** (versioned), a **discussion**
(threaded, corrections, agent reviews), **intermediate products** (drafts, agent
outputs, superseded versions), and an **escalation/decision** record (where the
autonomous pipeline paused for a human). All of it persists **locally**; the
**canonical, durable history is git** — it is where the bundle eventually lands.

**Two layers, one is a cache of the other:**
- **Hot layer** — a rebuildable local store (SQLite blackboard) for *real-time
  coordination* (pipeline ↔ agents ↔ user), fast append, queries, and escalation
  notification. Single machine, concurrency-safe, fast.
- **Cold / canonical layer — git** — documents as files + the discussion/event
  log as committed append-only files + branches for alternatives. Durable,
  portable, diffable, branchable, human-readable, no lock-in.

**git is the source of truth; the hot layer is a rebuildable working cache.**

## 1. Why git for the history (and why not a hand-rolled body store)

git's object model *already is* what a design-history store needs:
content-addressed blobs (document versions, deduped by SHA) + an append-only
commit DAG (history) + diffs (version deltas, free) + branches (explore approach
A vs B) + fully local (`.git`, offline, push optional). An earlier draft proposed
a `BodyStore`/`ContentRef` content-addressed store for document versions — that
is **reinventing git**. Use git. (Use-a-library-else-hand-roll.)

## 2. Where git is awkward — and the boundary that draws

1. **High-frequency micro-events** — one commit per discussion turn / agent output
   = thousands of noisy commits. git is a *coarse checkpoint*, not a high-freq log.
2. **Discussion relationships** — "comment on line X of doc version Y" has no native
   git home (GitHub's review comments live in its DB, *not* in git; git stores only
   file versions).
3. **Real-time multi-writer coordination** — pipeline + agents + user writing
   concurrently → merge conflicts; git has no atomic cross-writer append. **git is
   not a live bus.**

→ These draw the line: **git is the archive; the hot layer is the live bus.**

## 3. The bundle, concretely

| Part | Cold (git) | Hot (blackboard cache) |
|---|---|---|
| **Documents** | markdown files, versioned by commits | current working copies |
| **Discussion / intermediate / escalation / decision** | one **committed append-only log** (JSONL: `{turn, author, ts, kind, refs}`) | live `append_event` timeline |
| **"comment → which doc version"** | a `ref` = **git SHA** (stable version handle) | in-memory ref |
| **Alternatives (approach A vs B)** | **git branch** | active branch pointer |
| **Escalation / decision** | events in the committed log (audit) | events on the timeline (real-time) |

## 4. Materialize policy — commit at *meaningful* checkpoints, not per event

The hot layer appends every micro-event; git receives a commit only at:
an **accepted document version**, a **milestone** (design approved / review passed /
implementation stage done), or **session end**. Between checkpoints the discussion
accumulates in the hot log and is folded into the next commit's log file + message.
This keeps git history readable (a handful of meaningful commits) while losing
nothing (the hot layer holds the fine grain; it materializes in batches).

## 5. Escalation is double-written

When the autonomous pipeline pauses for a human decision:
- **Hot**: `append_event(EscalationRaised{ stage, blocker, tried, decision_needed, refs })`
  → tars-editor (subscribed to the timeline) surfaces it: an in-app pending-decision
  queue + a **Tauri native desktop notification**. No cloud push — the pipeline
  *pauses* (its state IS the hot store, nothing lost) and resumes on the user's
  `DecisionMade` event.
- **Cold**: the same events land in the committed log at the next materialize →
  a permanent audit of *what the AI was stuck on and how the human decided*.

**Real-time via the hot layer; durable audit via git.** (See
[`observability-scaling.md`](./observability-scaling.md) §"the blackboard is the bus".)

## 6. Reuse & seams

- **git** — existing, the cold canonical layer (no new dep).
- **Hot layer** — the existing `BlackboardStore` trait (`tars-storage`,
  `append_event`/`read_timeline`) as the live timeline; SQLite impl is the local
  default. It is *rebuildable from git*, so it is a cache, not a second truth.
- **tars-editor** — `tars-desktop` (today persists only `events.sqlite`,
  `lib.rs:104`); grows a bundle-open/materialize surface over the two layers.
- Both storage traits stay swappable — a future multi-machine sync is a drop-in
  impl, not a rebuild (same principle as observability scaling).

## 7. Non-goals

- **No cloud, no Firebase.** Everything is local (git `.git` + a local SQLite
  cache). A remote is an *optional* `git push`, on the user's terms.
- **git is not the real-time bus.** Never drive live pipeline↔agent↔user
  coordination through commits/merges — that is the hot layer's job.
- **Do not commit every micro-event.** Materialize at meaningful checkpoints.

## 8. One-line

Documents, discussion, intermediate products, and the escalation/decision trail
all **materialize into git** (versioned, diffable, branchable, local, no lock-in) —
that is the durable home the bundle eventually lands in. A rebuildable local
blackboard cache carries the *real-time* coordination and escalation notification
between commits. **git holds what to keep; the hot layer coordinates the moment.**
