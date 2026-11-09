//! `fs.grep`: searches file contents for a regex with ripgrep's engine, linked in so the host
//! needs no `rg`. Offered as a named tool, it is what the model reaches for instead of `bash.run`
//! with `grep` or `find`.
//!
//! Where it may search is the read jail's decision (see `read_jail`); a search is always
//! confined, so it cannot walk the whole disk. The walk lists directories by path, but each file
//! is opened from the search root without following a symlink, so a line and the name it is shown
//! under come only from a file below the root. Each file is read once, as a stream: a hit and the
//! lines around it are kept, the rest is not, and a line longer than [`MAX_LINE_BYTES`] skips the
//! file. A file with a NUL byte is taken as binary: the search of it stops at the block of bytes
//! that holds the first NUL, so a hit in that block or after it is not returned, even one before
//! the NUL, and the result says how many files were cut short this way. An entry it cannot read
//! is skipped, and the result names the first few of each kind with why. No match is a success, not an
//! `is_error` result: it is an answer the model acts on, and the session aborts after a run of
//! all-error turns.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use async_trait::async_trait;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use tars_types::JsonSchema;

use super::SkipList;
use super::read_jail::{Reach, ReadJail, ReadPath};
use crate::tool::{Tool, ToolEnv, ToolError, ToolOutput};
use grep::searcher::{Searcher, Sink, SinkContext, SinkMatch};
use tars_sandbox::{ResolveRootError, canonical_root};

/// Caps the matches one call returns.
pub const DEFAULT_MAX_MATCHES: NonZeroUsize = NonZeroUsize::new(200).expect("200 is not zero");
/// Cuts each returned line to this many chars; one line of a minified bundle can be tens of KB.
const MAX_LINE_CHARS: usize = 400;

/// Caps the lines of context on each side of a hit, so a search does not return whole files.
const MAX_CONTEXT: usize = 40;

/// Caps the bytes the searcher holds for one line and its context; a file with a longer line,
/// such as a large file with no newline, is skipped rather than read whole into memory.
const MAX_LINE_BYTES: usize = 8 << 20;

pub struct GrepTool {
    /// Holds the directory a resolved search path must stay inside, when set.
    root: Option<PathBuf>,
    max_matches: NonZeroUsize,
}

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    /// Lines of source on each side of a hit, at most [`MAX_CONTEXT`]. `0` returns only
    /// `path:line: text`, a pointer that costs the model another turn to follow.
    #[serde(default)]
    context: usize,
}

struct SearchOutcome {
    /// Holds one entry per hit, or with context one block per run of nearby hits.
    matches: Vec<String>,
    hits: usize,
    /// Says whether a match past the cap was seen, so more matches exist than are returned.
    truncated: bool,
    skipped: Skipped,
    /// Counts the files the search stopped in at a NUL byte.
    binary: usize,
}

impl GrepTool {
    /// Returns a tool with no root of its own; a call then searches under its `cwd` and its
    /// readable roots.
    pub fn new() -> Self {
        Self {
            root: None,
            max_matches: DEFAULT_MAX_MATCHES,
        }
    }

    /// Confines searches to `root`.
    ///
    /// # Errors
    ///
    /// Fails when `root` cannot be canonicalized.
    pub fn with_root(root: impl AsRef<Path>) -> Result<Self, ResolveRootError> {
        Ok(Self {
            root: Some(canonical_root(root.as_ref())?),
            max_matches: DEFAULT_MAX_MATCHES,
        })
    }

    /// Sets the match cap; the default is 200.
    pub fn max_matches(mut self, n: NonZeroUsize) -> Self {
        self.max_matches = n;
        self
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
         engine: respects .gitignore, fast; a file is searched up to its first \
         NUL byte). Prefer this over `bash.run` with grep/find — it is faster \
         and cannot escape the workspace. Returns `path:line: text` matches, or \
         with `context` a block of lines around each run of hits. To find files \
         BY NAME use fs.glob instead."
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "GrepArgs",
                tars_types::json_object! {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex to search file contents for." },
                        "path": { "type": "string", "description": "Directory or file to search. Relative to the working directory. Defaults to the working directory." },
                        "glob": { "type": "string", "description": "Optional file-name glob filter, e.g. `*.rs` or `**/*.ts`." },
                        "case_insensitive": { "type": "boolean", "description": "Case-insensitive match (default false)." },
                        "context": { "type": "integer", "description": "Lines of source to show on each side of a hit, at most 40. Omit or 0 for just `path:line: text`." }
                    },
                    "required": ["pattern"]
                },
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        env: &ToolEnv,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let parsed: GrepArgs = serde_json::from_value(args).map_err(ToolError::InvalidArguments)?;

        let search_root = match ReadJail::of(self.root.as_deref(), env, Reach::Walk)
            .admit(Path::new(parsed.path.as_deref().unwrap_or(".")))
        {
            Ok(p) => p,
            Err(refusal) => return Ok(ToolOutput::error(refusal.to_string())),
        };

        let pattern = parsed.pattern.clone();
        let glob = parsed.glob.clone();
        let ci = parsed.case_insensitive;
        let ctxn = parsed.context.min(MAX_CONTEXT);
        let cap = self.max_matches.get();
        let walk_cancel = cancel.clone();

        // The blocking walk also checks `cancel`, so a cancelled call stops it as well as
        // returning at once.
        let handle = tokio::task::spawn_blocking(move || {
            run_search(
                &search_root,
                &pattern,
                glob.as_deref(),
                ci,
                cap,
                ctxn,
                &walk_cancel,
            )
        });
        let joined = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            r = handle => r,
        };

        let outcome = match joined {
            Ok(Ok(o)) => o,
            Ok(Err(SearchError::Cancelled)) => return Err(ToolError::Cancelled),
            Ok(Err(SearchError::BadRegex { pattern, source })) => {
                return Ok(ToolOutput::titled_error(
                    "invalid search",
                    format!("invalid regex `{pattern}`: {source}"),
                ));
            }
            Ok(Err(SearchError::BadGlob { glob, source })) => {
                return Ok(ToolOutput::titled_error(
                    "invalid search",
                    format!("invalid glob `{glob}`: {source}"),
                ));
            }
            Ok(Err(SearchError::GlobBuild(source))) => {
                return Ok(ToolOutput::titled_error(
                    "invalid search",
                    format!("glob build failed: {source}"),
                ));
            }
            Ok(Err(SearchError::Root { path, source })) => {
                return Ok(ToolOutput::titled_error(
                    "cannot open the search root",
                    format!("opening `{}`: {source}", path.display()),
                ));
            }
            Err(join_err) => {
                return Err(ToolError::TaskLost {
                    tool: "fs.grep",
                    source: join_err,
                });
            }
        };

        let skipped = outcome.skipped.unreadable.note("that could not be read")
            + &outcome.skipped.long_lines.note(LONG_LINE_SKIP)
            + &binary_note(outcome.binary);
        if outcome.matches.is_empty() {
            return Ok(ToolOutput::titled_success(
                format!("no matches for `{}`", parsed.pattern),
                format!("(no matches){skipped}"),
            ));
        }
        let n = outcome.hits;
        let mut body = outcome.matches.join("\n");
        let title = if outcome.truncated {
            body.push_str(&format!(
                "\n\n(truncated at {cap} matches — narrow the pattern, path, or glob)"
            ));
            format!("{n}+ matches (truncated)")
        } else {
            format!("{n} match{}", if n == 1 { "" } else { "es" })
        };
        body.push_str(&skipped);
        Ok(ToolOutput::titled_success(title, body))
    }
}

/// Returns the note that says how many files were taken as binary and searched only up to their
/// first NUL byte, so no match there is not read as no match at all; empty when none was.
fn binary_note(binary: usize) -> String {
    let what = "taken as binary at a NUL byte and searched no further, so matches from there on \
                are not listed";
    match binary {
        0 => String::new(),
        1 => format!("\n\n(1 file was {what})"),
        n => format!("\n\n({n} files were {what})"),
    }
}

/// The entries a search left out, by why: those it could not read, named with the error, and the
/// files it did not search because a line in them is longer than [`MAX_LINE_BYTES`], named apart
/// so they are not taken for files that could not be read.
#[derive(Debug, Default)]
struct Skipped {
    unreadable: SkipList,
    long_lines: SkipList,
}

const LONG_LINE_SKIP: &str = "with a line longer than 8 MiB, which is not searched";

/// Returns whether a search of a file that opened failed because a line outgrew the searcher's
/// heap limit: grep-searcher reports that as `Other` with no OS error, and the sink here never
/// fails, so every other failure of a search is the operating system's.
fn is_line_limit(kind: std::io::ErrorKind, os_error: Option<i32>) -> bool {
    kind == std::io::ErrorKind::Other && os_error.is_none()
}

/// Collects the hits of one file as the searcher streams it, each a `path:line: text` line, or
/// with context a block of the lines around a run of hits, the hits marked `>`.
struct Hits<'a> {
    display: &'a str,
    context: usize,
    /// Holds the hits of the files searched before this one.
    found: usize,
    max_matches: usize,
    /// Counts this file's hits.
    hits: usize,
    /// Says whether a hit past the cap was seen; it is not recorded.
    more: bool,
    out: Vec<String>,
    /// Holds the block being built.
    block: Option<Block>,
    /// Says whether the searcher met a NUL byte and stopped.
    binary: bool,
}

/// The lines around a run of hits. A block of context lines only, with no hit in it, is not
/// shown.
#[derive(Default)]
struct Block {
    first_hit: Option<u64>,
    text: String,
}

impl Hits<'_> {
    fn line(&mut self, number: Option<u64>, bytes: &[u8], hit: bool) {
        let text = String::from_utf8_lossy(bytes);
        let text: String = text
            .trim_end_matches(['\n', '\r'])
            .chars()
            .take(MAX_LINE_CHARS)
            .collect();
        let n =
            number.expect("the searcher is built with `line_number(true)`, so every line has one");
        if self.context == 0 {
            self.out.push(format!("{}:{n}: {text}", self.display));
            return;
        }
        let block = self.block.get_or_insert_with(Block::default);
        if hit && block.first_hit.is_none() {
            block.first_hit = Some(n);
        }
        let mark = if hit { ">" } else { " " };
        block.text.push_str(&format!("{mark}{n:>5} {text}\n"));
    }

    fn end_block(&mut self) {
        if let Some(Block {
            first_hit: Some(first),
            text,
        }) = self.block.take()
        {
            self.out.push(format!("{}:{first}:\n{text}", self.display));
        }
    }
}

impl Sink for Hits<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        // A hit past the cap is seen, not kept: it is what says the result is cut short.
        if self.found + self.hits == self.max_matches {
            self.more = true;
            return Ok(false);
        }
        self.line(mat.line_number(), mat.bytes(), true);
        self.hits += 1;
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, Self::Error> {
        self.line(ctx.line_number(), ctx.bytes(), false);
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, Self::Error> {
        self.end_block();
        Ok(true)
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, Self::Error> {
        self.binary = true;
        Ok(false)
    }
}

/// Says why the search as a whole failed. A file that cannot be read is skipped and named, so
/// there is no IO variant.
enum SearchError {
    Cancelled,
    BadRegex {
        pattern: String,
        source: grep::regex::Error,
    },
    BadGlob {
        glob: String,
        source: ignore::Error,
    },
    GlobBuild(ignore::Error),
    /// The search root could not be opened to walk from.
    Root {
        path: PathBuf,
        source: std::io::Error,
    },
}

fn run_search(
    search_root: &ReadPath,
    pattern: &str,
    glob: Option<&str>,
    case_insensitive: bool,
    max_matches: usize,
    context: usize,
    cancel: &CancellationToken,
) -> Result<SearchOutcome, SearchError> {
    use grep::regex::RegexMatcherBuilder;
    use grep::searcher::{BinaryDetection, SearcherBuilder};

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(case_insensitive)
        .build(pattern)
        .map_err(|source| SearchError::BadRegex {
            pattern: pattern.to_owned(),
            source,
        })?;

    let root = search_root.path();
    let mut builder = ignore::WalkBuilder::new(root);
    // `.gitignore` applies even when the search root is not a git repository.
    builder.require_git(false);
    // Ignore files above the search root are not read: a worktree at `<repo>/.arc/worktrees/x`
    // under a `<repo>/.gitignore` listing `.arc/` would otherwise be ignored whole, and every
    // search would find nothing. Ignore files inside the root still apply.
    builder.parents(false);
    if let Some(g) = glob {
        let mut ob = ignore::overrides::OverrideBuilder::new(root);
        ob.add(g).map_err(|source| SearchError::BadGlob {
            glob: g.to_owned(),
            source,
        })?;
        let ov = ob.build().map_err(SearchError::GlobBuild)?;
        builder.overrides(ov);
    }

    let opener = search_root.walk().map_err(|source| SearchError::Root {
        path: root.to_path_buf(),
        source,
    })?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(context)
        .after_context(context)
        .heap_limit(Some(MAX_LINE_BYTES))
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();
    let mut matches: Vec<String> = Vec::new();
    let mut hit_count = 0usize;
    let mut truncated = false;
    let mut skipped = Skipped::default();
    let mut binary = 0usize;

    for result in builder.build() {
        if cancel.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        // An entry the walk cannot read (a directory without permission, a broken link) is
        // skipped and named, so the model knows the matches cover only what could be read.
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => {
                skipped.unreadable.push(e.to_string());
                continue;
            }
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let display = path
            .strip_prefix(root)
            .ok()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| {
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string())
            });

        let file = match opener.open_file(path) {
            Ok(file) => file,
            Err(e) => {
                skipped.unreadable.push(format!("`{display}`: {e}"));
                continue;
            }
        };
        match file.metadata() {
            Ok(m) if m.is_file() => {}
            Ok(_) => {
                skipped
                    .unreadable
                    .push(format!("`{display}`: not a regular file"));
                continue;
            }
            Err(e) => {
                skipped.unreadable.push(format!("`{display}`: {e}"));
                continue;
            }
        }

        let mut hits = Hits {
            display: &display,
            context,
            found: hit_count,
            max_matches,
            hits: 0,
            more: false,
            out: Vec::new(),
            block: None,
            binary: false,
        };
        if let Err(e) = searcher.search_file(&matcher, &file, &mut hits) {
            if is_line_limit(e.kind(), e.raw_os_error()) {
                skipped.long_lines.push(format!("`{display}`"));
            } else {
                skipped.unreadable.push(format!("`{display}`: {e}"));
            }
        }
        hits.end_block();
        binary += usize::from(hits.binary);
        hit_count += hits.hits;
        matches.extend(hits.out);
        if hits.more {
            truncated = true;
            break;
        }
    }
    Ok(SearchOutcome {
        matches,
        hits: hit_count,
        truncated,
        skipped,
        binary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    /// Returns a context whose `cwd` is `dir`, the confinement a rootless tool needs.
    fn in_dir(dir: &Path) -> ToolEnv {
        ToolEnv {
            cwd: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

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
        let required: Vec<&str> = schema.schema()["required"]
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
                &in_dir(dir.path()),
                CancellationToken::new(),
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
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "nothing relevant\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "ZZZ_NOPE", "path": dir.path().to_str().unwrap()}),
                &in_dir(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "no-match must be success, not error");
        assert!(r.content.contains("no matches"));
    }

    #[tokio::test]
    async fn searches_root_a_parent_gitignore_marks_ignored() {
        let repo = tempfile::tempdir().unwrap();
        write(repo.path(), ".gitignore", ".arc/\n");
        let worktree = repo.path().join(".arc/worktrees/fix-X");
        write(&worktree, "static_layer.rs", "fn parse_ruff_output() {}\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "parse_ruff_output", "path": worktree.to_str().unwrap()}),
                &in_dir(&worktree),
                CancellationToken::new(),
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
                &in_dir(dir.path()),
                CancellationToken::new(),
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

    /// Nests the files, since a whitelist override cannot prune files at the search root. An
    /// empty result here would be believed, and the agent would read whole files instead.
    #[tokio::test]
    async fn a_glob_still_finds_files_below_the_root() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/one/src/keep.rs", "needle\n");
        write(dir.path(), "crates/one/src/skip.txt", "needle\n");
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "needle", "path": dir.path().to_str().unwrap(), "glob": "*.rs"}),
                &in_dir(dir.path()),
                CancellationToken::new(),
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
                &in_dir(dir.path()),
                CancellationToken::new(),
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
                json!({"pattern": "hello", "path": dir.path().to_str().unwrap(), "case_insensitive": true}), &in_dir(dir.path()), CancellationToken::new())
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
                &in_dir(dir.path()),
                CancellationToken::new(),
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
        let tool: Arc<dyn Tool> =
            Arc::new(GrepTool::new().max_matches(NonZeroUsize::new(3).unwrap()));
        let r = tool
            .execute(
                json!({"pattern": "hit", "path": dir.path().to_str().unwrap()}),
                &in_dir(dir.path()),
                CancellationToken::new(),
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
                &in_dir(dir.path()),
                CancellationToken::new(),
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
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("outside the allowed root"));
    }

    #[tokio::test]
    async fn cwd_acts_as_jail_when_no_explicit_root() {
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "secret.rs", "needle\n");
        let ctx = ToolEnv {
            cwd: Some(inside.path().to_path_buf()),
            ..Default::default()
        };
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "needle", "path": outside.path().to_str().unwrap()}),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "absolute path outside cwd must be blocked");
        assert!(r.content.contains("outside the allowed root"));
    }

    #[tokio::test]
    async fn readable_root_lets_search_reach_outside_cwd() {
        let inside = tempfile::tempdir().unwrap();
        let dep = tempfile::tempdir().unwrap();
        write(dep.path(), "config.rs", "pub enum ConfigError { needle }\n");
        let ctx = ToolEnv {
            cwd: Some(inside.path().to_path_buf()),
            readable_roots: vec![dep.path().to_path_buf()],
            ..Default::default()
        };
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let r = tool
            .execute(
                json!({"pattern": "needle", "path": dep.path().to_str().unwrap()}),
                &ctx,
                CancellationToken::new(),
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
            .execute(
                json!({"no_pattern": "x"}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .expect_err("should reject malformed args");
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

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
                &in_dir(dir.path()),
                CancellationToken::new(),
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

    #[tokio::test]
    async fn no_context_is_still_just_the_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.rs"), "a\nneedle\nb\n").unwrap();
        let tool: Arc<dyn Tool> = Arc::new(GrepTool::new());
        let out = tool
            .execute(
                json!({ "pattern": "needle", "path": dir.path().to_str().unwrap() }),
                &in_dir(dir.path()),
                CancellationToken::new(),
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
            .execute(
                json!({ "pattern": "NEEDLE" }),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
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

    #[tokio::test]
    async fn a_rootless_search_without_a_cwd_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "needle\n");
        let r = GrepTool::new()
            .execute(
                json!({"pattern": "needle", "path": dir.path().to_str().unwrap()}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert!(!r.content.contains("a.rs"), "{}", r.content);
    }

    #[test]
    fn the_schema_offers_context() {
        let t = GrepTool::new();
        assert_eq!(
            t.input_schema().schema()["properties"]["context"]["type"],
            "integer"
        );
    }

    #[tokio::test]
    async fn a_stale_readable_root_does_not_refuse_a_search_inside_cwd() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "a.rs", "NEEDLE\n");
        let mut ctx = in_dir(root.path());
        ctx.readable_roots = vec![root.path().join("deps-not-fetched-yet")];
        let r = GrepTool::new()
            .execute(
                json!({ "pattern": "NEEDLE" }),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("a.rs:1"), "{}", r.content);
    }

    async fn grep(dir: &Path, args: serde_json::Value) -> ToolOutput {
        let mut args = args;
        args["path"] = json!(dir.to_str().unwrap());
        GrepTool::new()
            .execute(args, &in_dir(dir), CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_line_longer_than_the_cap_skips_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut long = b"needle".to_vec();
        long.resize(MAX_LINE_BYTES + 1, b'a');
        std::fs::write(dir.path().join("long.txt"), long).unwrap();
        write(dir.path(), "short.txt", "needle\n");
        let r = grep(dir.path(), json!({"pattern": "needle", "context": 1})).await;
        assert!(r.content.contains("short.txt:1:"), "{}", r.content);
        assert!(!r.content.contains("long.txt:1:"), "{}", r.content);
        assert!(r.content.contains("skipped 1 entry"), "{}", r.content);
    }

    #[tokio::test]
    async fn a_file_skipped_for_a_long_line_is_named_in_the_note_without_its_hits() {
        let dir = tempfile::tempdir().unwrap();
        let mut long = b"needle".to_vec();
        long.resize(MAX_LINE_BYTES + 1, b'a');
        std::fs::write(dir.path().join("long.txt"), long).unwrap();
        let r = grep(dir.path(), json!({"pattern": "needle"})).await;
        assert!(
            r.content.contains(
                "(skipped 1 entry with a line longer than 8 MiB, which is not searched: `long.txt`)"
            ),
            "{}",
            r.content
        );
        assert!(!r.content.contains("long.txt:1:"), "{}", r.content);
    }

    #[tokio::test]
    async fn a_line_at_the_cap_is_searched() {
        let dir = tempfile::tempdir().unwrap();
        let mut line = b"needle".to_vec();
        line.resize(MAX_LINE_BYTES / 2, b'a');
        line.push(b'\n');
        std::fs::write(dir.path().join("long.txt"), line).unwrap();
        let r = grep(dir.path(), json!({"pattern": "needle"})).await;
        assert!(r.content.contains("long.txt:1:"), "{}", r.content);
    }

    #[tokio::test]
    async fn a_file_with_a_nul_byte_is_not_searched_past_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob"), b"\0\nneedle\n").unwrap();
        let r = grep(dir.path(), json!({"pattern": "needle"})).await;
        assert!(r.content.contains("no matches"), "{}", r.content);
    }

    #[tokio::test]
    async fn a_hit_lost_to_a_nul_byte_is_said_to_be_unsearched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"needle\n\0tail\n").unwrap();
        let r = grep(dir.path(), json!({"pattern": "needle"})).await;
        assert!(
            r.content.contains("1 file was taken as binary"),
            "{}",
            r.content
        );
    }

    #[test]
    fn no_binary_file_adds_no_note() {
        assert_eq!(binary_note(0), "");
    }

    #[test]
    fn binary_files_are_counted() {
        assert!(binary_note(1).contains("1 file was taken as binary"));
        assert!(binary_note(2).contains("2 files were taken as binary"));
    }

    #[tokio::test]
    async fn nearby_hits_share_one_window() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\nhit\nb\nhit\nc\nd\ne\n");
        let r = grep(dir.path(), json!({"pattern": "hit", "context": 1})).await;
        assert_eq!(r.title.as_deref(), Some("2 matches"));
        assert_eq!(
            r.content,
            "t.rs:2:\n     1 a\n>    2 hit\n     3 b\n>    4 hit\n     5 c\n"
        );
    }

    #[tokio::test]
    async fn exactly_cap_matches_are_all_there_is_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "needle\nneedle\n");
        let r = GrepTool::new()
            .max_matches(NonZeroUsize::new(2).unwrap())
            .execute(
                json!({"pattern": "needle"}),
                &in_dir(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(r.title.as_deref(), Some("2 matches"), "{}", r.content);
        assert!(!r.content.contains("truncated"), "{}", r.content);
    }

    #[tokio::test]
    async fn a_match_past_the_cap_in_another_file_says_truncated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "needle\nneedle\n");
        write(dir.path(), "b.txt", "needle\n");
        let r = GrepTool::new()
            .max_matches(NonZeroUsize::new(2).unwrap())
            .execute(
                json!({"pattern": "needle"}),
                &in_dir(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.title.as_deref(),
            Some("2+ matches (truncated)"),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn a_file_with_a_line_past_the_limit_is_named_as_such_not_as_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = "x".repeat(MAX_LINE_BYTES + 1024);
        body.push_str("needle\n");
        write(dir.path(), "min.js", &body);
        let r = GrepTool::new()
            .execute(
                json!({"pattern": "needle"}),
                &in_dir(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.content.contains("could not be read"), "{}", r.content);
        assert!(
            r.content
                .contains("skipped 1 entry with a line longer than 8 MiB, which is not searched"),
            "{}",
            r.content
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_file_is_named_with_its_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "locked.rs", "needle\n");
        let locked = dir.path().join("locked.rs");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&locked).is_ok() {
            return; // running as root: nothing is unreadable.
        }
        let r = GrepTool::new()
            .execute(
                json!({"pattern": "needle"}),
                &in_dir(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            r.content
                .contains("skipped 1 entry that could not be read: `locked.rs`: "),
            "{}",
            r.content
        );
    }
}
