//! `fs.grep` — search file CONTENTS for a regex across the work tree,
//! using ripgrep's own engine (`grep` + `ignore` crates) linked in.
//!
//! ## Why a first-class tool instead of `bash.run` + `grep`
//!
//! Three wins over telling the model to shell out:
//!
//! 1. **Awareness.** The capability shows up by name in the tool list, so
//!    the model reaches for it directly instead of falling back to its
//!    `grep`/`find` training prior.
//! 2. **Scope / safety.** Search is rooted at the optional jail
//!    (`with_root`) / [`ToolContext::cwd`] and CANNOT escape it — no more
//!    `find /` full-disk scans that time out.
//! 3. **Speed + no install dep.** ripgrep's matcher (`grep`) over its
//!    .gitignore-respecting parallel walker (`ignore`) — compiled in, so
//!    the host doesn't need `rg`/`fd` installed.
//!
//! "No match" is a SUCCESS (`is_error=false`), not a failure: an empty
//! search is a normal, informative outcome the model acts on — and it
//! avoids poisoning the agent loop's consecutive-error abort counter the
//! way a bare `grep` (which exits 1 on no-match) does.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use tars_types::JsonSchema;

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

/// Default cap on matches returned per call (keeps the LLM context bounded).
pub const DEFAULT_MAX_MATCHES: usize = 200;
/// Per-match line is truncated to this many chars (a minified bundle line
/// can be tens of KB — one such line would blow the context).
const MAX_LINE_CHARS: usize = 400;

/// The widest window a single hit may carry. A search that returns whole files is a
/// read with extra steps.
const MAX_CONTEXT: usize = 40;

pub struct GrepTool {
    /// Optional jail root. Same semantics as [`super::ReadFileTool::with_root`]:
    /// a resolved search path must stay inside it.
    root: Option<PathBuf>,
    max_matches: usize,
}

#[derive(Debug, Deserialize)]
struct GrepArgs {
    /// Regex to search file contents for.
    pattern: String,
    /// Directory or file to search. Absolute or relative to the working
    /// directory. Defaults to the working directory.
    #[serde(default)]
    path: Option<String>,
    /// Optional file-name glob filter (e.g. `*.rs`, `**/*.ts`).
    #[serde(default)]
    glob: Option<String>,
    /// Case-insensitive match (default false).
    #[serde(default)]
    case_insensitive: Option<bool>,
    /// Lines of surrounding source to return with each hit.
    ///
    /// `0` gives `path:line: text` and nothing else — a pointer, which then costs
    /// another turn to follow. Measured on one run: `grep "enum EventKind"` returned
    /// 117 bytes naming `task.rs:252`, and reading it took two further turns because
    /// the first guessed the wrong file. With a window the answer arrives once.
    #[serde(default)]
    context: Option<usize>,
}

struct SearchOutcome {
    matches: Vec<String>,
    truncated: bool,
}

impl GrepTool {
    /// Construct without a jail. Use only in trusted contexts.
    pub fn new() -> Self {
        Self {
            root: None,
            max_matches: DEFAULT_MAX_MATCHES,
        }
    }

    /// Constrain searches to `root`. Mirrors [`super::ListDirTool::with_root`].
    /// Returns `None` if `root` can't be canonicalized.
    pub fn with_root(root: impl AsRef<Path>) -> Option<Self> {
        let canonical_root = std::fs::canonicalize(root.as_ref()).ok()?;
        Some(Self {
            root: Some(canonical_root),
            max_matches: DEFAULT_MAX_MATCHES,
        })
    }

    /// Override the match cap (default 200). Chainable.
    pub fn max_matches(mut self, n: usize) -> Self {
        self.max_matches = n;
        self
    }

    /// Resolve the search path (defaulting to the cwd) and enforce the jail.
    ///
    /// Effective jail root = the explicit `with_root`, ELSE the per-call
    /// `ctx.cwd`. So even a consumer that only sets `cwd` (the worktree is
    /// known per-call, not at agent-build time) gets a hard boundary the
    /// search can't escape — this is the structural fix for the `find /`
    /// full-disk-scan class. Only when BOTH are absent is there no jail.
    fn resolve(
        &self,
        input: Option<&str>,
        cwd: Option<&Path>,
        readable_roots: &[PathBuf],
    ) -> Result<PathBuf, ToolResult> {
        let raw = Path::new(input.unwrap_or("."));
        let combined = if raw.is_absolute() {
            raw.to_path_buf()
        } else if let Some(cwd) = cwd {
            cwd.join(raw)
        } else if let Some(root) = &self.root {
            // With a jail and no per-call cwd, the jail IS the frame of reference. It
            // used to fall through to the process cwd, which made the documented
            // default (`path` omitted → ".") resolve outside the jail and fail every
            // time the agent's root was not also the process's working directory —
            // so the tool's own default argument was guaranteed to error.
            root.join(raw)
        } else {
            raw.to_path_buf()
        };
        let jail = match (&self.root, cwd) {
            (Some(r), _) => r.clone(),
            (None, Some(c)) => std::fs::canonicalize(c).map_err(|e| {
                ToolResult::error(format!("cannot resolve cwd `{}`: {e}", c.display()))
            })?,
            (None, None) => return Ok(combined),
        };
        let canonical = std::fs::canonicalize(&combined).map_err(|e| {
            ToolResult::error(format!("cannot resolve path `{}`: {e}", combined.display()))
        })?;
        // Allowed if under the (cwd / explicit-root) jail OR any extra READ-ONLY
        // root the caller granted (e.g. a dependency-source dir). Search may READ
        // those for reference; it still can't escape to the whole disk.
        let in_read_root = readable_roots.iter().any(|r| {
            std::fs::canonicalize(r)
                .map(|cr| canonical.starts_with(&cr))
                .unwrap_or(false)
        });
        if !canonical.starts_with(&jail) && !in_read_root {
            return Err(ToolResult::error(format!(
                "path `{}` resolves outside the allowed root `{}`",
                canonical.display(),
                jail.display(),
            )));
        }
        Ok(canonical)
    }
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "fs.grep"
    }

    fn description(&self) -> &str {
        "Search file CONTENTS for a regex across the working tree (ripgrep \
         engine: respects .gitignore, skips binaries, fast). Prefer this over \
         `bash.run` with grep/find — it is faster and cannot escape the \
         workspace. Returns `path:line: text` matches. To find files BY NAME \
         use fs.glob instead."
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "GrepArgs",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex to search file contents for." },
                        "path": { "type": "string", "description": "Directory or file to search. Relative to the working directory. Defaults to the working directory." },
                        "glob": { "type": "string", "description": "Optional file-name glob filter, e.g. `*.rs` or `**/*.ts`." },
                        "case_insensitive": { "type": "boolean", "description": "Case-insensitive match (default false)." }
                    },
                    "required": ["pattern"]
                }),
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: GrepArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let search_root = match self.resolve(
            parsed.path.as_deref(),
            ctx.cwd.as_deref(),
            &ctx.readable_roots,
        ) {
            Ok(p) => p,
            Err(result) => return Ok(result),
        };

        let pattern = parsed.pattern.clone();
        let glob = parsed.glob.clone();
        let ci = parsed.case_insensitive.unwrap_or(false);
        let ctxn = parsed.context.unwrap_or(0).min(MAX_CONTEXT);
        let cap = self.max_matches;
        let cancel = ctx.cancel.clone();
        let root_for_blocking = search_root.clone();

        // The walk + search is synchronous CPU/IO work — run it off the async
        // runtime, and race it against cancellation so a dropped turn returns
        // promptly (the blocking task also checks `cancel` cooperatively).
        let handle = tokio::task::spawn_blocking(move || {
            run_search(
                root_for_blocking,
                &pattern,
                glob.as_deref(),
                ci,
                cap,
                ctxn,
                &cancel,
            )
        });
        let joined = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = handle => r,
        };

        let outcome = match joined {
            Ok(Ok(o)) => o,
            Ok(Err(SearchError::Cancelled)) => return Err(ToolError::Cancelled),
            Ok(Err(SearchError::BadInput(msg))) => {
                return Ok(ToolResult::titled_error("invalid search", msg));
            }
            Err(join_err) => {
                return Err(ToolError::Execute(format!(
                    "grep task panicked: {join_err}"
                )));
            }
        };

        if outcome.matches.is_empty() {
            // Not an error — an empty result is informative, and keeping
            // is_error=false avoids poisoning the loop's abort counter.
            return Ok(ToolResult::titled_success(
                format!("no matches for `{}`", parsed.pattern),
                "(no matches)".to_string(),
            ));
        }
        let n = outcome.matches.len();
        let mut body = outcome.matches.join("\n");
        let title = if outcome.truncated {
            body.push_str(&format!(
                "\n\n(truncated at {cap} matches — narrow the pattern, path, or glob)"
            ));
            format!("{n}+ matches (truncated)")
        } else {
            format!("{n} match{}", if n == 1 { "" } else { "es" })
        };
        Ok(ToolResult::titled_success(title, body))
    }
}

/// Typed failure of the blocking search so `execute` can map each to the
/// right surface (Cancelled / is_error result). Per-file IO/binary errors
/// are non-fatal (the file is skipped), so there is no IO variant.
enum SearchError {
    Cancelled,
    BadInput(String),
}

fn run_search(
    search_root: PathBuf,
    pattern: &str,
    glob: Option<&str>,
    case_insensitive: bool,
    max_matches: usize,
    context: usize,
    cancel: &CancellationToken,
) -> Result<SearchOutcome, SearchError> {
    use grep::regex::RegexMatcherBuilder;
    use grep::searcher::SearcherBuilder;
    use grep::searcher::sinks::UTF8;

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(case_insensitive)
        .build(pattern)
        .map_err(|e| SearchError::BadInput(format!("invalid regex `{pattern}`: {e}")))?;

    let mut builder = ignore::WalkBuilder::new(&search_root);
    // Honor .gitignore even when the search root isn't itself a git repo
    // (the `ignore` crate gates gitignore on a .git dir by default). A code
    // search tool should skip target/, node_modules/, etc. regardless.
    builder.require_git(false);
    // Do NOT climb to parent-directory ignore files. The `ignore` crate
    // defaults to reading .gitignore from ABOVE the search root — which
    // silently breaks searching any tree that a parent repo ignores. The
    // canonical victim: an agent worktree at `<repo>/.arc/worktrees/fix-X`
    // while `<repo>/.gitignore` lists `.arc/`. The parent rule marks the
    // whole worktree ignored, the walk is pruned to zero files, and EVERY
    // `fs.grep` returns "(no matches)" for patterns that plainly exist —
    // forcing the agent to flail with `bash find|grep`, balloon its context,
    // and eventually truncate. Ignore files INSIDE the search root (target/,
    // node_modules/) are still honored.
    builder.parents(false);
    if let Some(g) = glob {
        let mut ob = ignore::overrides::OverrideBuilder::new(&search_root);
        ob.add(g)
            .map_err(|e| SearchError::BadInput(format!("invalid glob `{g}`: {e}")))?;
        let ov = ob
            .build()
            .map_err(|e| SearchError::BadInput(format!("glob build failed: {e}")))?;
        builder.overrides(ov);
    }

    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut matches: Vec<String> = Vec::new();
    let mut truncated = false;

    for result in builder.build() {
        if cancel.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue, // unreadable dir / broken symlink — skip
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let display = path
            .strip_prefix(&search_root)
            .ok()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| {
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string())
            });

        let mut hits: Vec<(u64, String)> = Vec::new();
        let sink = UTF8(|lnum, line| {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            let text: String = trimmed.chars().take(MAX_LINE_CHARS).collect();
            hits.push((lnum, text));
            // Returning false stops searching THIS file once the cap is hit.
            Ok(matches.len() + hits.len() < max_matches)
        });
        // Per-file IO/binary errors are non-fatal — skip the file.
        let _ = searcher.search_path(&matcher, path, sink);

        // The window, read once per file that actually hit. A hit with no surrounding
        // source is a pointer, and following a pointer costs a turn.
        let lines: Vec<String> = if context > 0 && !hits.is_empty() {
            std::fs::read_to_string(path)
                .map(|t| t.lines().map(str::to_string).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        for (lnum, text) in hits {
            if context == 0 || lines.is_empty() {
                matches.push(format!("{display}:{lnum}: {text}"));
                continue;
            }
            let i = lnum.saturating_sub(1) as usize;
            let from = i.saturating_sub(context);
            let to = (i + context + 1).min(lines.len());
            let mut block = format!("{display}:{lnum}:\n");
            for (n, l) in lines[from..to].iter().enumerate() {
                let ln = from + n + 1;
                let mark = if ln == lnum as usize { ">" } else { " " };
                let cut: String = l.chars().take(MAX_LINE_CHARS).collect();
                block.push_str(&format!("{mark}{ln:>5} {cut}\n"));
            }
            matches.push(block);
        }
        if matches.len() >= max_matches {
            truncated = true;
            break;
        }
    }
    Ok(SearchOutcome { matches, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn write(dir: &Path, name: &str, content: &str) {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn name_and_schema_pin_convention() {
        let t = GrepTool::new();
        assert_eq!(t.name(), "fs.grep");
        let schema = t.input_schema();
        assert!(schema.strict);
        let required: Vec<&str> = schema.schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["pattern"]);
    }

    #[tokio::test]
    async fn finds_matches_with_path_and_line() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "fn alpha() {}\nlet x = TARGET;\n");
        write(dir.path(), "b.rs", "no hits here\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "TARGET", "path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(
            r.content.contains("a.rs:2: let x = TARGET;"),
            "got: {}",
            r.content
        );
        assert!(!r.content.contains("b.rs"));
    }

    #[tokio::test]
    async fn no_match_is_success_not_error() {
        // The Bug-A sidestep: empty search must NOT be is_error.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "nothing relevant\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "ZZZ_NOPE", "path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "no-match must be success, not error");
        assert!(r.content.contains("no matches"));
    }

    #[tokio::test]
    async fn searches_root_a_parent_gitignore_marks_ignored() {
        // Regression: an agent worktree lives at `<repo>/.arc/worktrees/fix-X`
        // while `<repo>/.gitignore` lists `.arc/`. The `ignore` crate's
        // default parent-traversal marked the whole search root ignored and
        // pruned the walk to ZERO files, so every fs.grep returned
        // "(no matches)" for patterns that plainly exist — forcing the agent
        // to flail with `bash find|grep`. We must search the requested root
        // regardless of what a PARENT repo ignores.
        let repo = tempfile::tempdir().unwrap();
        write(repo.path(), ".gitignore", ".arc/\n");
        let worktree = repo.path().join(".arc/worktrees/fix-X");
        write(&worktree, "static_layer.rs", "fn parse_ruff_output() {}\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "parse_ruff_output", "path": worktree.to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "got: {}", r.content);
        assert!(
            r.content
                .contains("static_layer.rs:1: fn parse_ruff_output() {}"),
            "parent .gitignore must NOT blind a search of the requested root; got: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn glob_filters_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "keep.rs", "needle\n");
        write(dir.path(), "skip.txt", "needle\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "needle", "path": dir.path().to_str().unwrap(), "glob": "*.rs"}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.content.contains("keep.rs"));
        assert!(
            !r.content.contains("skip.txt"),
            "glob should exclude .txt: {}",
            r.content
        );
    }

    /// The same glob, one directory down.
    ///
    /// `glob_filters_files` writes both files at the search root, which is the one
    /// shape where a whitelist override cannot prune anything. A real repository is
    /// nested — `crates/tars-runtime/src/lib.rs` — and that is the shape a live run
    /// searched. Measured on one: `{"pattern":"trajectory","glob":"*.rs"}` came back
    /// `0 result(s) in 0 file(s)` against a tree with the word in hundreds of `.rs`
    /// files, and the agent went and read whole files instead — twenty-one `read`s in
    /// fifty turns. A search that finds nothing and says so calmly is worse than one
    /// that errors: the agent believes it.
    #[tokio::test]
    async fn a_glob_still_finds_files_below_the_root() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/one/src/keep.rs", "needle\n");
        write(dir.path(), "crates/one/src/skip.txt", "needle\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "needle", "path": dir.path().to_str().unwrap(), "glob": "*.rs"}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "got: {}", r.content);
        assert!(
            r.content.contains("keep.rs"),
            "nested .rs must be found; got: {}",
            r.content
        );
        assert!(
            !r.content.contains("skip.txt"),
            "glob should exclude .txt: {}",
            r.content
        );
    }

    /// An alternation is an ordinary regex and must match like one.
    ///
    /// Measured on the same run: `render_note|RenderNote|kind.*delta` returned zero
    /// against a tree where `render_note` is a module name, and
    /// `tars-trace|TraceError|bodies_shown_whole|to_text` returned zero while the
    /// agent was actively writing `tars-trace`. Two of the nine searches in that run
    /// were plain literals and both found what they were looking for; every
    /// alternation found nothing.
    #[tokio::test]
    async fn an_alternation_matches_either_side() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/one/src/a.rs", "fn render_note() {}\n");
        write(dir.path(), "crates/one/src/b.rs", "struct TraceError;\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "render_note|TraceError|nothing_here",
                       "path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "got: {}", r.content);
        assert!(r.content.contains("a.rs"), "left side: {}", r.content);
        assert!(r.content.contains("b.rs"), "right side: {}", r.content);
    }

    #[tokio::test]
    async fn case_insensitive_flag() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "HELLO world\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "hello", "path": dir.path().to_str().unwrap(), "case_insensitive": true}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("a.rs:1"), "got: {}", r.content);
    }

    #[tokio::test]
    async fn respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "ignored.rs\n");
        write(dir.path(), "ignored.rs", "needle\n");
        write(dir.path(), "tracked.rs", "needle\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "needle", "path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.content.contains("tracked.rs"));
        assert!(
            !r.content.contains("ignored.rs"),
            "gitignore'd file must be skipped: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn cap_truncates_and_flags() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::new();
        for i in 0..10 {
            body.push_str(&format!("hit {i}\n"));
        }
        write(dir.path(), "a.rs", &body);
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new().max_matches(3));
        let r = tool
            .execute(
                json!({"pattern": "hit", "path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("truncated at 3"), "got: {}", r.content);
    }

    #[tokio::test]
    async fn invalid_regex_is_recoverable_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "x\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "(unclosed", "path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("invalid regex"), "got: {}", r.content);
    }

    #[tokio::test]
    async fn jail_blocks_path_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "secret.rs", "needle\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::with_root(dir.path()).unwrap());
        let r = tool
            .execute(
                json!({"pattern": "needle", "path": outside.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("outside the allowed root"));
    }

    #[tokio::test]
    async fn cwd_acts_as_jail_when_no_explicit_root() {
        // The Bug-B guarantee: with only ctx.cwd set (no with_root), an
        // absolute path outside the cwd is refused — search can't escape the
        // working tree even though the tool was built with ::new().
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "secret.rs", "needle\n");
        let ctx = ToolContext {
            cwd: Some(inside.path().to_path_buf()),
            ..Default::default()
        };
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "needle", "path": outside.path().to_str().unwrap()}),
                ctx,
            )
            .await
            .unwrap();
        assert!(r.is_error, "absolute path outside cwd must be blocked");
        assert!(r.content.contains("outside the allowed root"));
    }

    #[tokio::test]
    async fn readable_root_lets_search_reach_outside_cwd() {
        // "deps read-only": a path under a granted readable_root (a dependency
        // dir outside the worktree) is allowed — so a coding agent can grep a
        // dependency type instead of looping in the worktree, while still being
        // unable to escape to arbitrary paths.
        let inside = tempfile::tempdir().unwrap();
        let dep = tempfile::tempdir().unwrap();
        write(dep.path(), "config.rs", "pub enum ConfigError { needle }\n");
        let ctx = ToolContext {
            cwd: Some(inside.path().to_path_buf()),
            readable_roots: vec![dep.path().to_path_buf()],
            ..Default::default()
        };
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "needle", "path": dep.path().to_str().unwrap()}),
                ctx,
            )
            .await
            .unwrap();
        assert!(
            !r.is_error,
            "granted readable_root must be searchable: {}",
            r.content
        );
        assert!(r.content.contains("config.rs"));
    }

    #[tokio::test]
    async fn invalid_args_yield_typed_error() {
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let err = tool
            .execute(json!({"no_pattern": "x"}), ToolContext::default())
            .await
            .expect_err("should reject malformed args");
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
    /// The default argument has to work. `path` is documented as optional, and with a
    /// jail set and no per-call cwd it used to resolve against the process's working
    /// directory — so omitting it failed with "outside the allowed root" whenever the
    /// agent's root was somewhere else, which for an agent working in a worktree is
    /// always.
    /// A hit with no surrounding source is a pointer, and a pointer costs a turn to
    /// follow. Measured: `grep "enum EventKind"` returned 117 bytes naming
    /// `task.rs:252`, and getting the definition took two further turns because the
    /// first one guessed the wrong file.
    #[tokio::test]
    async fn a_window_makes_the_hit_answerable_without_a_second_turn() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=40)
            .map(|i| {
                if i == 20 {
                    "pub enum EventKind {\n".to_string()
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        std::fs::write(dir.path().join("t.rs"), body).unwrap();

        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let out = tool
            .execute(
                json!({
                    "pattern": "enum EventKind",
                    "path": dir.path().to_str().unwrap(),
                    "context": 3
                }),
                ToolContext::default(),
            )
            .await
            .unwrap();
        let text = out.content;
        assert!(
            text.contains(">   20 pub enum EventKind {"),
            "the hit is marked: {text}"
        );
        assert!(
            text.contains("   17 line 17"),
            "and three lines before it: {text}"
        );
        assert!(text.contains("   23 line 23"), "and three after: {text}");
        assert!(!text.contains("line 16"), "and no more than asked: {text}");
    }

    /// …and asking for none still gives the locate-only shape, because that is a
    /// different question and a much cheaper answer.
    #[tokio::test]
    async fn no_context_is_still_just_the_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.rs"), "a\nneedle\nb\n").unwrap();
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let out = tool
            .execute(
                json!({ "pattern": "needle", "path": dir.path().to_str().unwrap() }),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(out.content.trim(), "t.rs:2: needle");
    }

    #[tokio::test]
    async fn omitting_the_path_searches_the_jail_not_the_process_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("hit.rs"), "fn NEEDLE() {}\n").unwrap();

        let tool: Arc<dyn Tool> = Arc::new(GrepTool::with_root(&root).unwrap());
        let r = tool
            .execute(json!({ "pattern": "NEEDLE" }), ToolContext::default())
            .await
            .unwrap();

        assert!(
            !r.is_error,
            "default path must not escape its own jail: {}",
            r.content
        );
        assert!(
            r.content.contains("hit.rs"),
            "it searched the jail: {}",
            r.content
        );
    }
}
