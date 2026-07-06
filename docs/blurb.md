# TARS — profile / blog blurb

Ready-to-paste copy for the [github.com/leocaolab](https://github.com/leocaolab) org
profile and blog. Keep it in sync with the README's Philosophy section when the
positioning changes.

---

## Full blurb (profile README / blog intro)

**TARS · Rust · Apache-2.0**

A Rust-first, **strongly-typed** multi-agent LLM runtime. A dozen providers behind one
trait, a composable middleware pipeline, an Agent you hand tasks to, Python + Node
bindings — observability built in, not bolted on.

**The bet: correctness is a type-system property, not a matter of discipline.**

- Typed error hierarchy — `Permanent / Retryable / RateLimited / Auth`, a real sum type,
  not a string you grep.
- Parse, don't validate — hand it a `T`, get back a valid `T` or a typed error.
- Multi-tenancy enforced at every layer; cache hit/miss observable per call.
- The same `Pipeline` runs identically local (in-mem) and in a service (Redis + S3).

**Sharp boundaries, on purpose.** No **MCP** — a flat, untyped, insecure `Json→Text` bag
with no composition law ([why](https://github.com/leocaolab/tars/blob/main/docs/architecture/33-no-mcp.md)).
No autonomous agent-**planning** — for open-ended coding/research, use Claude or Codex;
TARS owns the typed, sandboxed execution *underneath*. Skeptical of **RAG** — exact
retrieval beats fuzzy vectors, and agents aren't latency-bound.

🔗 **github.com/leocaolab/tars**

---

## One-liner (repo About / social bio)

> Rust-first, strongly-typed multi-agent LLM runtime — a dozen providers behind one
> trait, composable pipeline, Python+Node bindings. Typed errors, sandboxed delegates,
> parse-don't-validate. No MCP, no fuzzy RAG — sharp boundaries on purpose.
