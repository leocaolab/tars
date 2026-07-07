//! Path-resolution law (Doc 06 §7): where a workspace root is, and where its
//! store lives.
//!
//! tars **never** discovers the location itself (no `current_dir()`): the
//! consumer resolves the root from its entry and injects it. This module is
//! the deterministic, entry-independent resolver the consumer calls.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Result of [`resolve_workspace_root`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceResolution {
    /// A workspace root was found — either the closest `.<tool>/` marker
    /// walking up, or (failing that) a `.git` git-root.
    Workspace(PathBuf),
    /// No `.<tool>/` marker and no `.git` anywhere up the tree → standalone
    /// (the global `~/.tars` fallback, partitioned by session).
    Standalone,
}

/// Resolve the workspace root for `tool` from a filesystem `entry` (§7).
///
/// ```text
/// 1. canonicalize(entry)                     — symlink / trailing-slash → one path
/// 2. walk up; the FIRST level with an existing `.<tool>/` wins
///      (the marker is the persisted declaration; the CLOSEST marker wins)
/// 3. else the first `.git` git-root, which also STOPS the climb
///      (marker beats `.git` — the monorepo rule)
/// 4. else Standalone
/// ```
///
/// The explicit-open / `--workspace` case (highest priority in §7) is handled
/// by the consumer passing that dir straight to
/// [`Tars::for_workspace`](crate::Tars::for_workspace) — it does not walk up.
/// This function is the CLI walk-up path.
pub fn resolve_workspace_root(
    tool: &str,
    entry: &Path,
) -> std::io::Result<WorkspaceResolution> {
    let canon = entry.canonicalize()?;
    let marker = format!(".{tool}");

    let mut cursor: Option<&Path> = Some(canon.as_path());
    while let Some(dir) = cursor {
        // Marker is checked BEFORE `.git` at every level, so the closest
        // marker always beats a higher git-root (the monorepo trap).
        if dir.join(&marker).is_dir() {
            return Ok(WorkspaceResolution::Workspace(dir.to_path_buf()));
        }
        // A `.git` here is the git-root fallback AND the ceiling: we do not
        // climb past a git boundary hunting for a marker.
        if dir.join(".git").exists() {
            return Ok(WorkspaceResolution::Workspace(dir.to_path_buf()));
        }
        cursor = dir.parent();
    }
    Ok(WorkspaceResolution::Standalone)
}

/// Where a scope's observability store is placed (§7). Only *placement* is
/// fixed here; the sink internals (MPSC single writer, backend) are Task 4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreScope {
    /// `<root>/.<tool>/tars/` — the default; data follows the project.
    Workspace(PathBuf),
    /// `~/.tars/ws/<path-hash>/` — fallback for a read-only workspace dir or
    /// `[store] location = "tars_home"`, and the home for standalone scopes.
    TarsHome(PathBuf),
    /// Opt-out (`[store] enabled = false`): no persistent store.
    Off,
}

impl StoreScope {
    /// The directory the store files live in, if any (`Off` has none).
    pub fn dir(&self) -> Option<&Path> {
        match self {
            StoreScope::Workspace(p) | StoreScope::TarsHome(p) => Some(p.as_path()),
            StoreScope::Off => None,
        }
    }
}

/// Per-workspace store dir under the tool marker: `<root>/.<tool>/tars/`.
pub fn workspace_store_dir(tool: &str, root: &Path) -> PathBuf {
    root.join(format!(".{tool}")).join("tars")
}

/// tars-home fallback dir for a workspace root: `<home>/ws/<hash>/`.
/// The hash is a stable SHA-256 of the canonical root, so a restart maps the
/// same project to the same dir (CUJ-5 reconnect).
pub fn tars_home_store_dir(home: &Path, root: &Path) -> PathBuf {
    let mut h = Sha256::new();
    h.update(root.to_string_lossy().as_bytes());
    let hash = h.finalize();
    let short: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();
    home.join("ws").join(short)
}

/// Standalone store dir: `<home>/standalone/<tool>/<session>/` (no workspace).
/// Per-tool nesting keeps arc's standalone I/O isolated from concer's (§7).
pub fn standalone_store_dir(home: &Path, tool: &str, session: &str) -> PathBuf {
    home.join("standalone").join(tool).join(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn marker_beats_git_the_monorepo_rule() {
        // /mono/.git + /mono/backend/.arc ; CLI runs in /mono/backend/src.
        let tmp = tempfile::tempdir().unwrap();
        let mono = tmp.path().join("mono");
        let backend_src = mono.join("backend").join("src");
        fs::create_dir_all(&backend_src).unwrap();
        fs::create_dir_all(mono.join(".git")).unwrap();
        fs::create_dir_all(mono.join("backend").join(".arc")).unwrap();

        let got = resolve_workspace_root("arc", &backend_src).unwrap();
        // Must stop at the backend marker, NOT climb to /mono/.git.
        assert_eq!(
            got,
            WorkspaceResolution::Workspace(mono.join("backend").canonicalize().unwrap())
        );
    }

    #[test]
    fn git_root_is_the_fallback_when_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let deep = root.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();

        let got = resolve_workspace_root("arc", &deep).unwrap();
        assert_eq!(
            got,
            WorkspaceResolution::Workspace(root.canonicalize().unwrap())
        );
    }

    #[test]
    fn canonicalize_collapses_symlink_and_trailing_slash() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(real.join(".arc")).unwrap();
        let link = tmp.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        return; // symlink test is unix-only

        // Trailing slash + symlink both resolve to the same canonical root.
        let via_link = resolve_workspace_root("arc", &link.join("")).unwrap();
        let via_real = resolve_workspace_root("arc", &real).unwrap();
        assert_eq!(via_link, via_real);
        assert_eq!(
            via_real,
            WorkspaceResolution::Workspace(real.canonicalize().unwrap())
        );
    }

    #[test]
    fn no_marker_no_git_is_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("bare").join("dir");
        fs::create_dir_all(&bare).unwrap();
        // tmp dirs live under the OS temp root, which is not itself a git
        // repo, so the walk reaches the fs root without a marker or .git.
        let got = resolve_workspace_root("arc", &bare).unwrap();
        assert_eq!(got, WorkspaceResolution::Standalone);
    }

    #[test]
    fn tars_home_hash_is_stable_for_a_root() {
        let home = Path::new("/home/u/.tars");
        let root = Path::new("/projects/x");
        assert_eq!(
            tars_home_store_dir(home, root),
            tars_home_store_dir(home, root)
        );
    }
}
