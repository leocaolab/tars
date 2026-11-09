//! Where a memory directory lives, and how a model-supplied `/memories/...` path
//! becomes a real path under it — without ever leaving it.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;
use tars_types::TenantId;
use thiserror::Error;

use super::error::{IoOp, MemoryError};

/// The prefix every path the model sends starts with (API doc: "Validate that all
/// paths start with `/memories`").
pub(super) const PREFIX: &str = "/memories";

/// A project's key under `projects/`: one path segment, so it can never climb out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectKey(String);

impl ProjectKey {
    pub fn parse(raw: &str) -> Result<Self, MemoryDirError> {
        single_segment(raw)
            .map(|s| Self(s.to_string()))
            .ok_or_else(|| MemoryDirError::NotASegment {
                what: "project key",
                value: raw.to_string(),
            })
    }
}

/// Which memory a session gets: the project's (default) or the user's — the same two
/// scopes Claude Code has (project auto memory; a sub-agent's `memory: user`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryScope {
    User,
    Project(ProjectKey),
}

/// A memory directory's location could not be built.
#[derive(Debug, Error)]
pub enum MemoryDirError {
    #[error(
        "{what} `{value}` is not a single path segment (it contains `/` or `\\`, or is `.`/`..`/empty)"
    )]
    NotASegment { what: &'static str, value: String },
    #[error("cannot create memory directory {}", path.display())]
    Create { path: PathBuf, source: io::Error },
    #[error("cannot resolve memory directory {}", path.display())]
    Canonicalize { path: PathBuf, source: io::Error },
}

/// Returns `$TARS_HOME/tenants/<tenant>/memory/` or
/// `$TARS_HOME/tenants/<tenant>/projects/<project>/memory/`.
pub fn memory_dir(
    tars_home: &Path,
    tenant: &TenantId,
    scope: &MemoryScope,
) -> Result<PathBuf, MemoryDirError> {
    let tenant = single_segment(tenant.as_str()).ok_or_else(|| MemoryDirError::NotASegment {
        what: "tenant id",
        value: tenant.as_str().to_string(),
    })?;
    let tenant_dir = tars_home.join("tenants").join(tenant);
    Ok(match scope {
        MemoryScope::User => tenant_dir.join("memory"),
        MemoryScope::Project(key) => tenant_dir.join("projects").join(&key.0).join("memory"),
    })
}

fn single_segment(raw: &str) -> Option<&str> {
    let bad = raw.is_empty() || raw == "." || raw == ".." || raw.contains(['/', '\\']);
    (!bad).then_some(raw)
}

/// The real directory `/memories` maps to: created and canonicalized once, at
/// construction, so every later containment check compares canonical to canonical.
#[derive(Clone, Debug)]
pub struct MemoryRoot(PathBuf);

impl MemoryRoot {
    /// Creates the directory if needed, as the SDKs do before the first call, so the first
    /// `view /memories` lists an empty directory rather than failing.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, MemoryDirError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|source| MemoryDirError::Create {
            path: dir.to_path_buf(),
            source,
        })?;
        let canonical =
            std::fs::canonicalize(dir).map_err(|source| MemoryDirError::Canonicalize {
                path: dir.to_path_buf(),
                source,
            })?;
        Ok(Self(canonical))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// A model-facing path that passed the lexical checks: `/memories` plus zero or more
/// plain segments. Displays as its normalized `/memories/...` form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct VirtualPath {
    segments: Vec<String>,
}

impl VirtualPath {
    /// Parses `raw` lexically, refusing anything outside the prefix and every traversal shape
    /// the API doc lists: `../`, `..\`, and their URL-encoded forms.
    pub(super) fn parse(raw: &str) -> Result<Self, MemoryError> {
        let decoded = percent_decode_str(raw).decode_utf8_lossy();
        if decoded.contains('\\') || decoded.split('/').any(|s| s == "..") {
            return Err(MemoryError::Traversal(raw.to_string()));
        }
        let rest = match raw.strip_prefix(PREFIX) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
            _ => return Err(MemoryError::OutsidePrefix(raw.to_string())),
        };
        let segments = rest
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .map(str::to_string)
            .collect();
        Ok(Self { segments })
    }

    pub(super) fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// Returns whether this is the root-level file `name`, such as `/memories/MEMORY.md`.
    pub(super) fn is_root_file(&self, name: &str) -> bool {
        matches!(self.segments.as_slice(), [only] if only == name)
    }

    fn joined_onto(&self, root: &MemoryRoot) -> PathBuf {
        self.segments.iter().fold(root.0.clone(), |p, s| p.join(s))
    }
}

impl fmt::Display for VirtualPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PREFIX)?;
        self.segments.iter().try_for_each(|s| write!(f, "/{s}"))
    }
}

/// Maps a checked virtual path onto disk and checks that the canonical form of its nearest
/// existing ancestor is still under the root, which catches a symlink inside the directory that
/// points out of it.
pub(super) async fn resolve(root: &MemoryRoot, path: &VirtualPath) -> Result<PathBuf, MemoryError> {
    let target = path.joined_onto(root);
    let existing = nearest_existing(&target).await;
    let canonical = tokio::fs::canonicalize(&existing)
        .await
        .map_err(|source| MemoryError::Io {
            op: IoOp::Resolve,
            path: path.to_string(),
            disk: existing.clone(),
            source,
        })?;
    if canonical.starts_with(&root.0) {
        Ok(target)
    } else {
        Err(MemoryError::Traversal(path.to_string()))
    }
}

async fn nearest_existing(target: &Path) -> PathBuf {
    let mut anchor = target;
    while tokio::fs::symlink_metadata(anchor).await.is_err() {
        match anchor.parent() {
            Some(parent) => anchor = parent,
            None => break,
        }
    }
    anchor.to_path_buf()
}
