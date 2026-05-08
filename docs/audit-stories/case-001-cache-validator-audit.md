# case-001 — Cache × Validator audit (B-20.W4)

**Date**: 2026-05-08
**Component**: `tars-pipeline` — `ValidationMiddleware` × `CacheLookupMiddleware` interaction
**Outcome**: structural bug found, A2-path fix shipped same day (commits `f751a88` failing tests, `f6eaab1` fix)
**Lesson**: writing the test changed my estimate by 4×.

> **Companion**: arc maintains a parallel case-003 in their own repo
> covering the same incident from the consumer side ("how we drove a
> tars-side audit + design decision through dogfood prep"). This file
> is the tars-side artifact.

## TL;DR

arc raised a "what if two callers share a Pipeline + cache but configure
different validator chains?" flag during dogfood prep. I read the code
in `tars-pipeline/src/validation.rs` and concluded the worry was real
but small — half a day to fix the re-emit logic. **I was wrong by a
factor of 4** because I missed a second, more structural bug. I only
found it after the user asked "did you add tests?" and I wrote two
failing tests.

The first test (cache stores raw) reproduced the half-day bug I had
already understood. The second test (cache hit re-runs validator chain)
was the one I almost didn't write — it surfaced that *Cache wraps
Validation* in the onion, so cache hits short-circuit before reaching
Validation entirely. That bug isn't fixable in `validation.rs` at all;
it requires changing layer order across the Pipeline builder.

## Timeline

| Time      | Action                                                                                            |
|-----------|---------------------------------------------------------------------------------------------------|
| T+0       | arc flags multi-caller / multi-chain cache risk in chat                                           |
| T+5min    | I read `validation.rs:225-232` and `cache.rs:wrap_stream_for_write`, reply "real bug, ~half day" |
| T+10min   | I file `B-20.W4` ticket, push, tell arc "明早 wheel"                                              |
| T+15min   | User: *"你加入测试了吗？"*                                                                        |
| T+20min   | I write two failing tests in `validation/tests.rs`                                                |
| T+25min   | Test 1 fails as expected ("hello" vs "hello world"); test 2 fails for a different reason         |
| T+30min   | I trace the second failure: cache hit never reaches Validation. New conclusion: structural fix.   |
| T+35min   | Update `B-20.W4` to call out two distinct bugs + revise estimate to 1-2 days                      |
| T+40min   | Commit failing tests with `#[ignore]` (`f751a88`) so they sit in history as a regression gate     |
| T+1h      | arc replies "选 A2" (cut `retriable: bool`, no consumer wants validation-driven retry)            |
| T+2.5h    | A2 implementation: onion change + drop `retriable` field + delete `#[ignore]` (`f6eaab1`)         |
| T+3h      | Wheel built, 21 pytest pass, 530 cargo pass, clippy clean, pushed                                 |

Three hours from "this is a real bug" to "shipped fix + regression
tests". Compared to the original "half day" claim, the *fix itself*
took roughly the original estimate; the cost was understanding the bug
correctly.

## What I claimed before writing the test

> Bug: `ValidationMiddleware` re-emits post-Filter events when
> `filtered_any=true` (`validation.rs:225-232`). Cache, sitting OUTSIDE
> Validation, sees the post-Filter stream and stores post-Filter
> instead of raw. Fix: always re-emit `events_held` (raw), let
> `rec.filtered_response` side-channel carry the filtered version to
> the outer caller. ~20 lines, half a day.

This was **technically accurate** for one bug (the re-emit) but
**missed** a second one of equal severity. The second bug is invisible
from reading `validation.rs` alone because it's about the *order* of
middleware layers, not the contents of any one layer.

## What writing the test surfaced

I wrote `b20_w4_cache_hit_reruns_validator_chain` mostly as a paranoia
check — "validators rerun on hit per Doc 15 §2; let's pin it." When it
failed, the failure said `telemetry.layers` did not contain
`"validation"` on the second call.

That was the moment I went back and traced the layer order:

```
Telemetry → CacheLookup → Retry → Validation → Provider
            ^^^^^^^^^^^   wraps   ^^^^^^^^^^
            outer                  inner
```

Cache hit short-circuits inside `CacheLookupMiddleware`, returning the
cached events *to its caller* without descending to Retry or
Validation. So the W1 design contract "validators rerun on hit" is not
just violated at the margin — it's structurally impossible with this
onion shape. No amount of `validation.rs`-internal cleanup would have
fixed it.

This is exactly the kind of thing you can't see by re-reading
`validation.rs` because *the relevant fact is not in `validation.rs`*.
The test made it visible by failing.

## Why "read the code first" failed here

Three pre-conditions that made this trap easy to fall into:

1. **The W1 doc and W1 implementation had drifted.** Doc 15 §2 said
   "cache stores raw, validators rerun on hit"; the code said neither.
   I was reading the code through the lens of what the doc claimed,
   which is a lossy way to read code.
2. **The bug spans two files.** `validation.rs` re-emits filtered;
   `lib.rs::Pipeline::from_provider` registers layers in an order that
   makes Cache wrap Validation. Either file alone looks fine.
3. **Confidence from "I just wrote this code last week".** I shipped
   W1+W2 days earlier and felt like I knew the code. That memory was
   the model of *what I intended*, not *what I committed*.

The test catches all three because it operates on the actual built
artifact, not on the intent.

## The estimate diff

| Stage                                | Estimate | Why it changed                                |
|--------------------------------------|----------|-----------------------------------------------|
| After reading the code               | 0.5 day  | "Just one re-emit branch, side-channel exists"|
| After writing the failing tests      | 1-2 days | "Layer order is wrong, not just the re-emit"  |
| Actual elapsed                       | ~3 hours | A2 path turned out smaller because cutting `retriable` simplified the API; arc's product clarity ("zero use cases for validation-driven retry") collapsed several branches we'd have otherwise debated. |

The point isn't "my estimate happened to be too high in the end." The
point is: **without the test, I would have shipped a half-day fix that
addressed only Bug 1 and silently retained Bug 2.** arc's first
dogfood run would have exposed it within hours and I would have been
back here writing the *real* fix anyway, except now from a less
confident position because my previous fix is now also part of git
history.

## What a future reviewer would do differently

When a flag like arc's lands ("are we safe in scenario X?"):

1. **Write the failing test before claiming a fix shape.** The cost is
   ~20 minutes; the savings on a wrong estimate is half a day plus the
   reputational cost of "actually it's worse than I said."
2. **Specifically test the boundary the flag touches.** arc's flag was
   "two callers, two chains, one cache." The first test I wrote
   covered only one of those (one caller, one chain). The second test
   forced me to think about the cross-call contract.
3. **When the doc says X and the code does Y, do not paper over it.**
   Pick one to follow. Before W4, the doc said "validators rerun on
   hit"; the code said "they don't." Either fix the doc to match the
   code or fix the code to match the doc — but stop letting them drift.

## What's now in place

- `b20_w4_cache_stores_raw_not_post_filter` and
  `b20_w4_cache_hit_reruns_validator_chain` in
  `crates/tars-pipeline/src/validation/tests.rs`. Both pass against
  current `main` (the W4 fix). They will fail loudly if anyone ever
  re-introduces a layer order where Cache wraps Validation, or
  re-introduces the post-Filter re-emit.
- Doc 15 §2 rewritten to match the W4 onion (`Telemetry → Validation →
  Cache → Retry → Provider`).
- `validation.rs` module docstring carries a "W4 history" note so
  whoever reads the file in 2027 sees that the layer order was
  deliberately chosen, not accidental.
- `Reject` no longer carries `retriable: bool`; `ValidationFailed` is
  always `Permanent`. Callers needing a model resample on validation
  failure must do so at their own layer with explicit prompt
  variation.

## Cost ledger (for calibration)

| Activity                                  | Time    | Value                                                 |
|-------------------------------------------|---------|-------------------------------------------------------|
| Read code, claim "half-day fix"           | 5 min   | Wrong; would have shipped incomplete fix              |
| Write 2 failing tests                     | 20 min  | Surfaced 2nd bug, revised estimate, locked regression |
| Update TODO ticket to reflect new scope   | 10 min  | Future reviewer doesn't repeat the audit              |
| Implement A2 path                         | 2.5 h   | Actual fix                                            |
| Doc updates (Doc 15 §2, validation.rs)    | 15 min  | Closes drift                                          |

Test-writing cost as a fraction of total: ~10%. Estimate-correction
value: hard to put a number on, but at minimum the cost of a wrong
estimate communicated externally (arc would have planned around a
half-day window).
