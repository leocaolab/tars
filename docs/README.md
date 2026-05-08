# tars documentation

Three subtrees, three audiences:

| Path | What's inside | Who reads it |
|------|---------------|--------------|
| [`USER-GUIDE.md`](./USER-GUIDE.md) | 5-minute getting-started, the three call shapes, when not to use TARS | Developers integrating tars into their app |
| [`architecture/`](./architecture/) | 18 design docs (00-17). Per-layer rationale, trade-offs, milestone plan, audit pins. | Maintainers, contributors, peer reviewers, architects evaluating the design |
| [`audit-stories/`](./audit-stories/) | Field notes — moments where the system caught itself being wrong. Time-stamped, citation-heavy, written while context was fresh. | Engineers learning from concrete pre/post estimate-revision cases |
| [`comparison.md`](./comparison.md) | Head-to-head positioning vs LangChain, LiteLLM, Letta, AutoGen, NVIDIA NIM | Anyone deciding "do I want this or X?" |

## Reading order

**If you want to use tars** — start with `USER-GUIDE.md`. Most consumers
never need to leave it.

**If you want to understand why tars is shaped this way** — start with
[`architecture/00-overview.md`](./architecture/00-overview.md), then
follow the role-based reading paths it suggests. Doc 00 is the map;
each per-layer doc covers one concern in depth.

**If you're evaluating tars vs alternatives** — start with
[`comparison.md`](./comparison.md), then dip into the architecture
doc for whichever specific concern is the deciding factor.

**If you found something surprising in the code** — check
[`audit-stories/`](./audit-stories/) first. The deliberate weirdness
usually has a story attached.

## Status notes

Documentation is **design-ahead** in places. The architecture corpus
describes the full target shape; not every layer is shipped.
[CHANGELOG.md](../CHANGELOG.md) is authoritative for shipped state;
[TODO.md](../TODO.md) tracks what's deferred and the trigger
conditions to revisit.

If you spot a doc that describes a layer not yet in `crates/`, that's
expected — design first, then build.
