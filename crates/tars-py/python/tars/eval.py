"""``tars.eval`` — minimal evaluation helpers over the pipeline event store.

The full Doc 16 evaluation framework (Evaluator traits, online/offline
runners, samplers) was re-scoped to this thin surface: you write an
evaluator *script* (cron / CI / notebook) that reads finished calls,
computes a metric, and writes the score back as an ``EvaluationScored``
event in the same store — queryable alongside the calls it grades.

Example — score the snippet-validation reject rate over the last day::

    import tars.eval as ev

    store = "/abs/path/to/events"
    for call in ev.read_calls(store, since_secs=86_400, tag="dogfood"):
        rejected = call.get("validation_reason") is not None
        ev.write_score(
            store,
            call_event_id=call["event_id"],
            evaluator_name="reject_flag",
            score=1.0 if rejected else 0.0,
            tenant_id=call["tenant_id"],
        )

Both helpers go through the Rust event store, so the on-disk schema
can't drift from a hand-rolled SQL writer.
"""

from __future__ import annotations

from tars._tars_py import eval_read_calls as read_calls
from tars._tars_py import eval_write_score as write_score

__all__ = ["read_calls", "write_score"]
