//! A concrete reference world for the **"make a red `cargo test` green"** CUJ:
//!
//! - [`File`] — a versioned file component. `write` overwrites the file *on disk* and bumps its
//!   content-hash version (the `onUpdate` contract). Because the effect lands on disk, a
//!   [`ShellCheck`] running a real build/test sees it.
//! - [`ShellCheck`] — the **deterministic Diff**: it shells out to a command (`cargo test`,
//!   `cargo build`, a lint) and maps exit-0 → [`CheckResult::Green`], non-zero → `Red` carrying
//!   the real combined output. Deterministic in the world's on-disk state, so a genuine fixed
//!   point exists (doc 14 §3.7 law 8 — the property the noisy-LLM verify lacked).

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Command;

use crate::diff::{Check, CheckResult};
use crate::effect::Observation;
use crate::world::{CompId, Component, Version, World};

/// Content-hash of a string — the version for a [`File`]. Same content ⇒ same version (a memo
/// hit); a `write` that changes content ⇒ a new version (`onUpdate` → the render refreshes).
fn hashv(s: &str) -> Version {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// A versioned file component backed by a real path on disk. `render` shows the current content;
/// the `write` handler overwrites the file and bumps the version.
pub struct File {
    id: CompId,
    path: PathBuf,
    content: String,
    version: Version,
}

impl File {
    /// Open (or seed) a file component. The current on-disk content is loaded so the render is
    /// truthful from crank 0; if the file does not exist yet, it starts empty.
    pub fn open(id: impl Into<CompId>, path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            id: id.into(),
            version: hashv(&content),
            path,
            content,
        })
    }

    /// The `write` handler: parse `{"content": "..."}`, overwrite the file, bump the version.
    /// On a parse/IO failure it returns the truth (raw args + real error), never a sentinel.
    fn write(&mut self, raw_args: &str) -> Observation {
        let parsed: serde_json::Value = match serde_json::from_str(raw_args) {
            Ok(v) => v,
            Err(e) => return self.fail("write", raw_args, format!("args are not JSON: {e}")),
        };
        let content = match parsed.get("content").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return self.fail(
                    "write",
                    raw_args,
                    "missing string field `content` in args".to_string(),
                );
            }
        };
        if let Err(e) = std::fs::write(&self.path, &content) {
            return self.fail("write", raw_args, format!("write to {:?} failed: {e}", self.path));
        }
        // onUpdate: content changed → new content-hash version → the render refreshes.
        self.content = content;
        self.version = hashv(&self.content);
        Observation::Applied {
            component: self.id.clone(),
            handler: "write".to_string(),
            new_version: self.version,
            render: self.render(),
        }
    }

    /// The raw file content (the source an observer/reviewer reads — distinct from [`render`],
    /// which wraps it with the path + a code fence for the agent's marks view). Its identity is
    /// the [`Component::version`] (content-hash), so a memoized derivation over the content keys
    /// on `(id, version)`.
    ///
    /// [`render`]: Component::render
    pub fn content(&self) -> &str {
        &self.content
    }

    fn fail(&self, handler: &str, raw_args: &str, error: String) -> Observation {
        Observation::Failed {
            component: self.id.clone(),
            handler: handler.to_string(),
            raw_args: raw_args.to_string(),
            error,
        }
    }
}

impl Component for File {
    fn id(&self) -> CompId {
        self.id.clone()
    }
    fn version(&self) -> Version {
        self.version
    }
    fn render(&self) -> String {
        format!("path: {}\n```\n{}\n```", self.path.display(), self.content)
    }
    fn handlers(&self) -> Vec<String> {
        vec!["write".to_string()]
    }
    fn handle(&mut self, handler: &str, args: &str) -> Observation {
        match handler {
            "write" => self.write(args),
            other => self.fail(
                other,
                args,
                format!("unknown handler `{other}` (File exposes: write)"),
            ),
        }
    }
}

/// The **deterministic Diff**: run a shell command; exit-0 = Green, non-zero = Red with the real
/// combined stdout+stderr. This is the oracle that makes the fixed point exist — a `cargo test`
/// whose verdict is its exit code, not an LLM's opinion.
pub struct ShellCheck {
    id: String,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
}

impl ShellCheck {
    /// e.g. `ShellCheck::new("cargo-test", "cargo", ["test"], repo_dir)`.
    pub fn new(
        id: impl Into<String>,
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            id: id.into(),
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
        }
    }
}

impl Check for ShellCheck {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn eval(&self, _world: &World) -> CheckResult {
        // The check reads the *on-disk* state that File effects mutate — the world is the
        // filesystem here, so we don't index into `_world`. Blocking `output()` is fine: the
        // check IS the (deterministic) work.
        let out = match Command::new(&self.program)
            .args(&self.args)
            .current_dir(&self.cwd)
            .output()
        {
            Ok(o) => o,
            // A check we can't even run is red, carrying the real spawn error — not a sentinel.
            Err(e) => {
                return CheckResult::Red {
                    detail: format!(
                        "could not run `{} {}`: {e}",
                        self.program,
                        self.args.join(" ")
                    ),
                };
            }
        };
        if out.status.success() {
            CheckResult::Green
        } else {
            let mut detail = format!(
                "`{} {}` exited with {}\n",
                self.program,
                self.args.join(" "),
                out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            );
            detail.push_str(&String::from_utf8_lossy(&out.stdout));
            detail.push_str(&String::from_utf8_lossy(&out.stderr));
            CheckResult::Red { detail }
        }
    }
}
