# interview-sim — a blackboard multi-agent demo on tars

A **system-design interview simulator**, rebuilt on tars to show three of its
features working together in a real (not toy) agent loop:

- **a Blackboard** as the single source of truth (the pattern tars documents in
  [Doc 19](../../../docs/architecture/19-blackboard-pipeline.md)),
- **provider-agnostic completion** — the same code runs on Anthropic, OpenAI,
  Gemini, or a local llama.cpp/MLX model by flipping one env var,
- **native structured output** (`response_schema=`) for the examiner agent.

It originated as a standalone project with its own hand-written provider
clients; this version deletes all of that and leans on `import tars`.

## What it does

Three agents debate around a shared blackboard until every rubric dimension is
satisfied (or a fatal flaw is found):

| Agent | Role | tars surface |
|---|---|---|
| **Actor** (`actor.py`) | front-stage interviewer — a *zero-knowledge* mouthpiece. Sees only a fixed meta-prompt + a one-line directive; never sees the rubric. | multi-turn `complete(system=…, messages=…)` |
| **Evaluator** (`evaluator.py`) | back-stage examiner — reads the board + the candidate's utterance, emits a **structured** `EvaluatorPatch` (status updates, +/- signals, probe suggestions). Never talks to the candidate. | `complete(response_schema=EvaluatorPatch.schema)` |
| **Candidate** (`candidate.py`) | (only in `--auto`) a self-play candidate with deliberate blind spots. | single-shot `complete(system=…, user=…)` |

The **engine** (`engine.py`) applies each patch to the blackboard, renders a live
"signal radar" scoreboard (5 states per dimension), and extracts the most urgent
probe as the next directive. The blackboard, rubric, and blueprint are typed
Pydantic models (`models.py`) — strong types in, structured output out.

```
candidate utterance ─▶ Evaluator ─▶ EvaluatorPatch ─▶ apply to Blackboard
                                                          │
                       Engine picks most-urgent probe ◀──┘
                                  │
                                  ▼
                            Actor speaks ─▶ next candidate utterance ─▶ …
```

## What tars replaced (the redesign)

The old project carried a bespoke LLM layer — an `LLMClient` ABC, a provider
`factory`, and separate `anthropic_client` / `openai_client` / `gemini_client` /
`mock_client`. All of it is gone, collapsed into [`runtime.py`](interview_sim/runtime.py)
(~90 lines, mostly comments) over a tars `Pipeline`.

The sharpest win is the **Evaluator**. Before, structured output was faked:

```python
# OLD: schema stuffed into the prompt, then cleaned up by hand
schema_hint = json.dumps(EvaluatorPatch.model_json_schema(), ...)
system = "你只输出 JSON … 请严格按照以下 JSON Schema 返回：\n" + schema_hint
raw = client.chat(system, user)
text = raw.strip()
if text.startswith("```"):           # strip markdown fences
    text = text.split("\n", 1)[1]
    if text.endswith("```"): text = text[:-3]
patch = EvaluatorPatch.model_validate_json(text.strip())   # hope it parses
```

After, tars steers the provider to the shape:

```python
# NEW: the schema is a first-class request parameter
raw = role.complete(
    system=system, user=user,
    response_schema=EvaluatorPatch.model_json_schema(),
)
patch = EvaluatorPatch.model_validate_json(raw)
```

No schema-in-prompt, no fence-stripping, and a malformed reply surfaces as a
typed `tars.TarsProviderError` instead of a `ValidationError` you have to guess at.

## Run it

```bash
# 1. Build the tars Python binding once (from the repo root).
cd ../../../crates/tars-py && maturin develop --release && cd -

# 2. Bootstrap a tars config and add your provider's API key.
tars init                       # writes ~/.tars/config.toml
export TARS_PROVIDER=anthropic  # any provider id in that config
export TARS_MODEL=claude-sonnet-4-5

# 3. Demo deps (Pydantic + rich for the scoreboard).
pip install -r requirements.txt

# 4a. Human candidate — you type answers, the interviewer probes you.
python -m interview_sim.main

# 4b. Self-play — AI candidate vs AI interviewer, N rounds.
python -m interview_sim.main --auto --turns=6
```

Swap the model/provider without touching code: `export TARS_PROVIDER=openai
TARS_MODEL=gpt-4o`, or point at a local model (`TARS_PROVIDER=llamacpp`). The
Evaluator uses **loose** structured-output mode, which is kinder to local GBNF
servers (see [the local-LLM bench notes](../../../docs/benchmarks/local-llm-bench.md)).

### See every call afterward

Point a role at an event store and the whole interview becomes queryable:

```bash
export TARS_EVENT_STORE=~/.tars/events
python -m interview_sim.main --auto
tars events list --since 1h
tars events show <event_id> --with-bodies   # the exact prompt + reply of any turn
```

## Files

```
interview_sim/
  runtime.py          ← the tars wiring (replaces the old llm/ package)
  actor.py            ← interviewer agent (multi-turn complete)
  evaluator.py        ← examiner agent (structured output)
  candidate.py        ← self-play candidate
  engine.py           ← blackboard loop + rich scoreboard rendering
  models.py           ← typed Blackboard / RubricNode / EvaluatorPatch (Pydantic)
  session_factory.py  ← blueprint + rubric → initial blackboard
  rubric.json         ← the capability dimensions (ability-oriented, tech-agnostic)
  blueprints/         ← problem definitions (e.g. ticketmaster.json)
```

## See also

- [`docs/USER-GUIDE.md`](../../../docs/USER-GUIDE.md) — the tars Python API used here
- [`docs/architecture/19-blackboard-pipeline.md`](../../../docs/architecture/19-blackboard-pipeline.md) — the blackboard pattern
- [`docs/observability.md`](../../../docs/observability.md) — `tars events` / trajectory inspection
