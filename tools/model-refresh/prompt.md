You are updating tars's shipped model table, `crates/tars-config/data/provider.toml`,
because `tars models update` found it behind the providers' live APIs. Your
working directory is the tars repository. Read the header comments of
`provider.toml` and `crates/tars-config/src/model_kb.rs` first: they define every
field and the invariants the tests enforce.

## What each finding asks for

- `shipped_missing_model` — the newest model of a series has no row. Add one,
  with every field sourced (below). Look at the neighbouring rows of the same
  family for which fields exist.
- `shipped_model_not_listed` — a row the API no longer lists. Confirm on the
  provider's deprecation / models page. If it is retired, set
  `status = "deprecated"` and `retire = "<date>"` when the page gives one. Do not
  delete the row, and never leave a provider's `default` or a series without a
  resolvable row.
- `shipped_limit_differs` — the API reports a different `context` /
  `max_output`. The API's number is the fact: set the field to it.

## Sourcing rules — no number without evidence

- Prices come from the provider's official pricing page only. Quote the line.
  If the page shows a time-limited rate, use the current one and put the later
  rate in a comment, as the Gemini 3.6–3.8 flash rows do.
- `context` / `max_output`: the API report (the finding carries it) or the
  official model page.
- `thinking` and `thinking_param`: read the official thinking docs, then test
  live — the keys are in your environment (`GEMINI_API_KEY`, `OPENAI_API_KEY`,
  `DEEPSEEK_API_KEY`). For a Gemini 3.x model, send one tiny request with
  `thinkingLevel: "minimal"`. If the API rejects it, the row is
  `thinking = "only"`, else `"optional"`. Record the exact response.
- `status`: "ga" or "preview" as the docs label it.
- If any required fact cannot be confirmed, do NOT add or change that row. A
  per-token GA row must carry prices (a test enforces it), so an unpriceable
  model stays out. Say so in the PR body instead.
- Set `verified` at the top of the file to today's date only if you verified
  every change you made.

## Hard limits

- Edit only `crates/tars-config/data/provider.toml`. Nothing else in the repo.
- Never print or write an API key. Use `$GEMINI_API_KEY` in curl, not its value.
- Run `cargo test -p tars-config` after editing. It must pass.

## The PR body (write it to the path given below, in Chinese)

For each change: the row and field, old → new value, and the evidence — the
URL with the quoted line, or the curl request and the response it gave. Then a
section listing every finding you did not act on, and why.
