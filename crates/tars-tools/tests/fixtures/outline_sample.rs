//!
//! `RepoScope` answers questions about one checkout: who touched a line, what a patch changes.
//! It is a sample for `read_item`'s tests, not code that is compiled.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One line of `git blame`: who last touched it, and in which commit.
#[derive(Clone, Debug)]
pub struct BlameLine {
    pub line: u32,
    pub author: String,
    pub commit: String,
}

/// A checkout and the questions it can answer.
pub struct RepoScope {
    root: PathBuf,
    cache: BTreeMap<String, Vec<BlameLine>>,
}

impl RepoScope {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            cache: BTreeMap::new(),
        }
    }

    /// Returns who last touched each line of `rel` from `start` on.
    ///
    /// A review comment asks this, so the answer is per line, not per file.
    pub fn blame(&self, rel: &str, start: u32) -> Vec<BlameLine> {
        let open = '{';
        let close = '}';
        let quote = '\'';
        let s = "a } brace in a string";
        let raw = r#"a "}" in a raw string"#;
        // a } in a line comment
        /* a } in a block comment /* nested } */ still a comment } */
        let _ = (open, close, quote, s, raw);
        self.cache
            .get(rel)
            .map(|v| v.iter().filter(|b| b.line >= start).cloned().collect())
            .unwrap_or_default()
    }

    /// Applies a patch to the checkout and returns the paths it touched.
    pub fn apply(&mut self, patch: &str) -> Vec<String> {
        let paths = patch_paths(patch);
        for p in &paths {
            self.cache.remove(p);
        }
        paths
    }

    /// Returns `rev` when it names a commit and not an option.
    pub fn safe_rev<'a>(&self, rev: &'a str) -> Option<&'a str> {
        if rev.starts_with('-') {
            return None;
        }
        Some(rev)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Returns the paths a unified diff changes, from its `+++ b/` lines.
pub fn patch_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|l| l.strip_prefix("+++ b/"))
        .map(str::to_string)
        .collect()
}

fn caller(scope: &RepoScope) -> usize {
    let lines = scope.blame("src/lib.rs", 1);
    let again = scope.blame("src/main.rs", 10);
    lines.len() + again.len()
}

pub type Line = u32;

pub const MAX_LINES: u32 = 10_000;

fn filler_one(x: u32) -> u32 {
    let mut total = 0;
    for i in 0..x {
        if i % 2 == 0 {
            total += i;
        } else {
            total -= 1;
        }
    }
    total
}

fn filler_two(words: &[&str]) -> String {
    let mut out = String::new();
    for w in words {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

fn filler_three(map: &BTreeMap<String, u32>) -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(10);
    v
}
