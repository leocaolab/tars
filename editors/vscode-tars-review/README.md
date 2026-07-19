# TARS Review — VSCode extension

Review your Rust workspace with the **TARS agent-2 reviewer** and see each finding as an inline
**comment thread** pinned at `file:line`, right in the editor.

```
Command Palette → "TARS: Review with agent"
   → spawns the review-cli binary over the workspace's .rs files
   → each finding becomes a CommentThread at file:line (message + severity)
```

## How it works

The extension shells out to the `review-cli` binary (Rust, in `crates/tars-agent2`), which reviews
each file **map-reduce** through `tars_pipeline::LlmService` — real DeepSeek when `DEEPSEEK_API_KEY`
is set, otherwise a mock stream (same code path). `review-cli` prints a JSON array of findings:

```json
[{ "file": "/abs/path/foo.rs", "line": 12, "message": "…", "status": "open", "severity": "high" }]
```

`file` is an absolute path, so the extension resolves it directly with `vscode.Uri.file(file)` and
creates a `vscode.CommentController` thread at `line - 1` (a `null` line pins the thread at the top
of the file).

## Setup

### 1. Build the reviewer binary

From the tars repo root:

```bash
cargo build --bin review-cli          # → target/debug/review-cli
```

### 2. Point the extension at it

By default the extension looks for `${workspaceFolder}/target/debug/review-cli`. If your binary is
elsewhere, set it:

```jsonc
// .vscode/settings.json
{
  "tars-review.cliPath": "/absolute/path/to/target/debug/review-cli",
  "tars-review.maxFiles": 40,               // cap files per run (API cost guard)
  "tars-review.reviewActiveFileOnly": false // true = review only the active editor's file
}
```

### 3. Provide the API key (for real findings)

`review-cli` reads `DEEPSEEK_API_KEY` from the environment. The extension inherits VSCode's process
env, so **export the key before launching VSCode**:

```bash
export DEEPSEEK_API_KEY=sk-...
code /path/to/your/repo
```

Without a key, the reviewer still runs and emits a placeholder finding per file (mock mode) — useful
to verify the wiring without spending tokens.

## Running the extension

### Dev (F5)

```bash
cd editors/vscode-tars-review
npm install
npm run compile         # tsc → out/extension.js
```

Then open this folder in VSCode and press **F5** ("Run Extension") to launch an Extension
Development Host. In that window, open a Rust repo and run **"TARS: Review with agent"** from the
Command Palette. Findings appear as comment threads in the gutter/editor. Run **"TARS: Clear review
comments"** to dispose them.

### Packaging a .vsix

```bash
npm install -g @vsce/cli   # or: npx @vscode/vsce
npx @vscode/vsce package   # → vscode-tars-review-0.1.0.vsix
code --install-extension vscode-tars-review-0.1.0.vsix
```

## Commands & settings

| Command | Title |
| --- | --- |
| `tars.review` | TARS: Review with agent |
| `tars.clearReview` | TARS: Clear review comments |

| Setting | Default | Meaning |
| --- | --- | --- |
| `tars-review.cliPath` | `""` | Path to `review-cli` (empty → `${workspaceFolder}/target/debug/review-cli`) |
| `tars-review.maxFiles` | `40` | Max `.rs` files reviewed per run |
| `tars-review.reviewActiveFileOnly` | `false` | Review only the active editor file |

## Honest limits

- **The live comment render is a manual verification step.** The Rust side (`review-cli` producing
  valid Finding JSON) and the TypeScript compile (`tsc`, no errors) are verified automatically; but
  actually seeing the comment threads render requires launching an Extension Development Host (F5) —
  which can't be driven headless here. The Comments API wiring (`createCommentController` →
  `createCommentThread(uri, range, comments)`) is written against the documented VSCode API shape.
- The reviewer is a **noisy LLM judgment**, not a deterministic check — findings vary run to run,
  and there is no "reconcile to zero" convergence (by design; see the `tars-agent2` crate docs).
- `**/target/**` is excluded from the file scan; large workspaces are capped by `maxFiles`.
