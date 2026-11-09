# tars observability — how to see what's happening

Two surfaces: the call log, one row per LLM call, and the raw tracing
stream. Pick the one that matches the question you're trying
to answer; you don't usually need both at once.

| Surface | Granularity | Storage | When to use |
|---|---|---|---|
| **Call log** | One row per LLM call | `<event_store_dir>/tenant=<t>/llm_call/date=<d>/*.parquet` | "Which calls were slow? Which model? Cache hit? What did the prompt/response actually look like?" |
| **tracing stream** | Live events to stderr as they happen | stderr (RAM) | "I'm running it now, what's the latency on this call?" |

A run's own trajectory is not here: the durable store records it.

---

## 1. Call log — one `llm_call` row per LLM call

A pipeline given an event store directory (the builder's
`.event_store(dir)`) lands one `llm_call` row per `complete` / `stream` —
completed, failed or cancelled — in the Parquet call log under that
directory, with the full request and response bodies inline in the row;
scores land as `eval_score` rows under the call they score. A pipeline
without one writes no row. No `tars` command writes the call log:
`tars probe` and `tars bench` call the provider directly, and the pipelines
of `tars eval run` and `judge` have no event store.

This is the right place to answer **"why did that call take so long?"**
or **"what was the prompt that produced this output?"**. Every subcommand
takes `--tenant`: a call log is read one tenant at a time.

### List recent calls

```bash
tars events list --tenant T                    # default: last 7d, 50 rows
tars events list --tenant T --since 1d --tag dogfood
tars events list --tenant T --since all --limit 200 --json | jq -c .
```

```
call_id                               at                   tenant          model                         outcome       tags
------------------------------------------------------------------------------------------------------------------------
0199a3f2-6dc0-7a9d-88d2-d1c13f376b28  2026-09-29 11:47:44  tenant-test     qwen/qwen3-coder-30b          ok
0199a2b1-86c3-7c54-8c54-a446e9208c54  2026-09-28 11:53:02  tenant-test     gemini-3-flash-preview        err:network
```

`--json` prints one object per call with the same
fields plus `actual_model`, `usage` and `provider_latency_ms`.

### See one call in full

```bash
tars events show --tenant T <call_id>                 # metadata, body states, scores
tars events show --tenant T <call_id> --with-bodies   # plus both bodies
```

A body the writer's policy did not keep shows as `withheld`; nothing stands
in for its text. `show` takes a call's own id (a UUIDv7).

### Useful one-liners

```bash
# Slowest 10 calls in the last day.
tars events list --tenant T --since 1d --limit 100000 --json \
  | jq -s 'sort_by(-.provider_latency_ms)
           | .[:10]
           | .[] | {model: .actual_model,
                    latency_ms: .provider_latency_ms,
                    out_tok: .usage.output_tokens}'

# All failed calls and their kinds.
tars events list --tenant T --since 7d --json \
  | jq -c 'select(.outcome.state=="failed") | {at_ms, model, kind: .outcome.kind}'
```

### Tags

If you set `tags` on a `RequestContext`, every call within that scope
gets tagged. Use them to slice — common patterns: `tags: ["dogfood"]`,
`tags: ["batch_2026_05_19"]`, `tags: ["experiment:tools_disabled"]`.

```bash
tars events list --tenant T --tag dogfood --since 1d
```

---

## 2. tracing stream — live, on stderr

Every call that goes through a pipeline passes the `TelemetryMiddleware`,
which emits a `tracing` event for it. In the `tars` command line those are
the calls of `tars eval run` and `tars eval judge`; `tars probe` and
`tars bench` call the provider directly and print their own lines instead.
The events go to **stderr**, not stdout, so stdout stays clean for piping.

```bash
tars eval run --corpus cases/ --tenant T -v     # info-level (one line per LLM call)
tars eval run --corpus cases/ --tenant T -vv    # debug
tars eval run --corpus cases/ --tenant T -vvv   # trace (very chatty)

# Fine-grained targeting via the standard RUST_LOG env (wins over -v).
RUST_LOG=tars_llm::middleware::telemetry=info,tars_llm=debug \
  tars eval run --corpus cases/ --tenant T
```

A setting the telemetry setup cannot use (an unknown `TARS_LOG_FORMAT`, an
invalid `RUST_LOG`, an `OTEL_TRACES_SAMPLER_ARG` that is not a number in
`0.0..=1.0`, a variable that is not Unicode) is skipped, and `tars` prints it
on stderr as a `warning: …` line before running the command. Those lines are
written directly, so `RUST_LOG` does not filter them out.

### JSON output — for log aggregators

By default the stderr stream is human-readable (ANSI colours, indented
fields). To pipe into Datadog / Loki / ELK / a JSONL log file, switch
formats with `--log-format json`:

```bash
tars eval run --corpus cases/ --tenant T --log-format json 2>tars.jsonl

# Or via env var, useful for containerised deploys
# (`--log-format` wins over `TARS_LOG_FORMAT` when both are set):
TARS_LOG_FORMAT=json tars eval run --corpus cases/ --tenant T
```

Sample JSON record (one event per line, `\n`-delimited; the event name is the
`event` field):

```json
{"timestamp":"…","level":"INFO","event":"llm.call.finished",
 "tars.trace_id":"…","tenant_id":"…","model":"claude-opus-4-7","elapsed_ms":847,
 "stop_reason":"EndTurn","input_tokens":1234,"output_tokens":567,
 "cached_input_tokens":0,"thinking_tokens":0,
 "target":"tars_llm::middleware::telemetry"}
```

`tars.trace_id` is the id the caller gave the request (`RequestContext`): a
session id, a job name, a uuid. It is not the OpenTelemetry trace id; with
OTLP export on, that one leads the line as `trace_id` (see below).

### Events you'll see

| Event | When | Key fields |
|---|---|---|
| `llm.call.start` | Request reaches the telemetry layer | `model`, `messages`, `tools`, `tars.trace_id`, `tenant_id` |
| `llm.call.opened` | First byte from the provider | `elapsed_ms` (=TTFB / spawn-to-first-byte for CLI backends) |
| `llm.call.finished` | Stream terminated normally | `elapsed_ms`, full `usage`, `stop_reason` |
| `llm.call.failed` | Provider call failed to open | `elapsed_ms`, `error_class`, `error` message |
| `llm.call.stream_error` | Mid-stream error | `elapsed_ms`, `error_class`, `error` message |

### Combine with `jq` for real-time slicing

```bash
tars eval run --corpus cases/ --tenant T --log-format json 2>&1 \
  | jq -c 'select(.event=="llm.call.finished")
           | {model, latency: .elapsed_ms, out_tok: .output_tokens}'
```

### Ship spans to an OTLP collector (Jaeger / Tempo / Datadog)

The `tracing` spans and the metrics below can be exported to any
OpenTelemetry collector over OTLP/gRPC. Each LLM call is one span, named
and shaped as OpenTelemetry GenAI's client inference span (`chat <model>`,
`gen_ai.*` attributes: provider, model, token usage, finish reason,
`error.type`), under whatever span the caller was in. It holds no prompt or
completion text. It's **opt-in** — the exporter pulls a heavy tonic/grpc
stack, so it's off by default:

```bash
# Build tars with the exporter compiled in (one-time).
cargo build --release -p tars-cli --bin tars --features otlp

# Point it at a collector and run as usual.
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
  tars eval run --corpus cases/ --tenant T --provider deepseek
```

Spans are tagged `service.name=tars-cli`. Without
`OTEL_EXPORTER_OTLP_ENDPOINT` (or without the `--features otlp` build)
there's zero overhead — stderr logging is unchanged. Both exporters
flush on process exit.

Alongside traces, the same build exports **metrics** derived from the
per-call events (no extra config):

| Instrument | Type | Attributes |
|---|---|---|
| `tars.llm.calls` | counter | `model`, `outcome` (ok/error) |
| `tars.llm.latency_ms` | histogram | `model`, `outcome` |
| `tars.llm.tokens` | counter | `model` |

These go to the same OTLP endpoint (a collector can fan traces and
metrics to different backends). Metrics shine in a long-running process;
a short command emits only its own calls' worth. The `model` attribute is
cardinality-capped (100 distinct values, then an `__over_cardinality__`
bucket) so a misconfig can't melt the metrics backend.

To keep only a fraction of traces at high volume, set the standard
`OTEL_TRACES_SAMPLER_ARG` (parent-based trace-id ratio, `0.0..=1.0`). Unset
or empty keeps every trace; a value that is not a number in that range, or
is not Unicode, also keeps every trace and is reported as a warning.

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
  OTEL_TRACES_SAMPLER_ARG=0.1 \
  tars eval run --corpus cases/ --tenant T   # keep ~10% of root traces
```

---

### Send to Datadog

**Through the Datadog Agent** — the path that works today. Turn on the
Agent's OTLP receiver in `datadog.yaml` (or set
`DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT=0.0.0.0:4317`), then
point tars at it:

```yaml
otlp_config:
  receiver:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
```

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
  tars eval run --corpus cases/ --tenant T --provider deepseek
```

Headers a collector wants go in the standard
`OTEL_EXPORTER_OTLP_HEADERS` (`key=value,key2=value2`); the exporter reads
it and sends them on both the span and the metric exports.

**Straight to Datadog's OTLP intake** (no Agent) — not yet. The intake
takes traces only over HTTP (`grpc is not supported`) and metrics only
with delta temporality; tars exports over gRPC with the SDK's default
cumulative metrics, so the intake refuses both. See Datadog's
[OTLP intake docs](https://docs.datadoghq.com/opentelemetry/setup/otlp_ingest/).

Each LLM call's span hangs under whatever `tracing` span the caller is in.
One run of the `tars` CLI is one trace: with export on, the run opens a
`tars <command>` root span (attributes `tars.command`,
`process.executable.name`, `process.exit.code`), and every call it makes
hangs under it, so `OTEL_TRACES_SAMPLER_ARG` keeps or drops a run whole.
Without export no root span is opened and the logs are unchanged. The CLI
takes no upstream `traceparent`; each run starts its own trace.
A Rust service that receives a request with a W3C `traceparent` continues
that trace by calling `tars_telemetry::continue_trace(&span, headers)` on
its request span before entering it; the LLM calls made inside then join
the upstream trace. The Node binding does not export traces, so it has no
trace to continue.

With export on, the HTTP requests `tars` sends to a provider carry the call
span's W3C `traceparent`, so a gateway that traces (LiteLLM, your own proxy)
continues tars's trace. Public provider APIs ignore it. A header your
`extras` already set is kept.

**What arrives today:**
- The three `tars.llm.*` metrics above.
- One `chat <model>` span per LLM call, with the `llm.call.*` events inside
  it, under the spans of the program that embeds tars.
- Log-to-trace links: with `--log-format json` and OTLP export on, a line
  emitted inside a span leads with that span's OpenTelemetry `trace_id` and
  `span_id` (lowercase hex), the fields Datadog joins logs to traces by.
  Ship stderr with the Agent's log collection to get the lines into Datadog
  Logs. Without export there are no OpenTelemetry ids, and none are written.
- Datadog LLM Observability: untried. The spans carry the OpenTelemetry
  GenAI attributes; the metrics are still named `tars.llm.*`.

## How the layers relate

```
call log (llm_call, per tenant)  ← tars events show --tenant T <call_id>
  └─ llm_wire (raw bytes, same call_id)

(throughout: tracing events on stderr — controlled by RUST_LOG / -v)
```

`LlmService::call` returns the id of each attempt, the ones retried away
included, in `ServiceCall::call_ids`, in the order they were made. Show each by
id; the id names the day its row is filed under, so the lookup opens one
partition:

```bash
tars events show --tenant T <call_id> --with-bodies
```

In the Parquet file a `call_id` is the 16-byte binary column. An id that is not
found names a row the call log dropped (a full buffer, counted in the sink's
health), not one to look for by time. To find calls whose ids you did not keep,
narrow the log to the run's tenant and window (and its cohort tag, if the
caller tags its calls):

```bash
tars events list --tenant T --since 2h --json \
  | jq -r '.call_id' \
  | xargs -I{} tars events show --tenant T {} --with-bodies
```

---

## Practical "I want to debug X → look at Y"

| Symptom | First place to look |
|---|---|
| "This call is slow" | `tars events list --tenant T --since 1d --json \| jq -s 'sort_by(.provider_latency_ms) \| reverse'` |
| "Show me the actual prompt that produced this output" | `tars events show --tenant T <call_id> --with-bodies` |
| "How many retries did this call need?" | `tars events show --tenant T <call_id> \| jq '.retry_attempts \| length'` |
| "I want to grep across runs" | `--log-format json 2>tars.jsonl`, then `jq` or `rg` over `tars.jsonl` |
| "Are token counts wildly off from response size?" | You're probably using a `*_cli` backend with default flags. See [providers/claude-cli.md](./providers/claude-cli.md) — the agent loop is reporting inflated `usage`. |

---

## See also

- [`USER-GUIDE.md`](./USER-GUIDE.md) — calling shapes, validators, errors
- [`providers/claude-cli.md`](./providers/claude-cli.md) — token-bloat caveats for the CLI backend
