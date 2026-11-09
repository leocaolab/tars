//! Decides where `fs.read_file`, `fs.list_dir`, `fs.grep` and `fs.glob` may read, one rule for
//! all four.
//!
//! A call is confined when its tool has a root, when the sandbox confines, or when it walks a
//! tree: an unconfined walk could cover the whole disk. A confined call reads under the tool's
//! root, else under `cwd`, under every readable root, and under the sandbox's writable roots, so
//! what a call may write it may read back. A root that cannot be resolved is dropped and named
//! when a path is refused; with no root left, nothing is readable. It does not open anything and
//! does not know which tool asks.
//!
//! A hard link under a root to a file outside it is read: package managers (pnpm's
//! `node_modules`, cargo's `target`) fill trees with such links, a read changes nothing, and a
//! program able to make one, such as `bash.run`, can already read outside the roots, since the
//! OS jail does not confine reads. The write jail refuses them.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use super::fs_open::{self, Access, Dir, WalkRoot};
use super::roots::RootSet;
use crate::tool::ToolEnv;
use crate::{SandboxMode, SandboxPolicy};
use tars_sandbox::canonical_root;

/// Says whether a tool reads the one path it is given or walks the tree below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reach {
    Path,
    Walk,
}

/// Where one call may read, and what a relative path is joined to.
pub(super) struct ReadJail {
    /// Holds the call's `cwd`, else the tool's root; `None` leaves a relative path as given.
    base: Option<PathBuf>,
    /// Holds the roots a confined call reads under; `None` when nothing confines the call.
    roots: Option<RootSet>,
}

/// Says why an admitted path could not be opened.
pub(super) enum OpenError {
    NotFound,
    /// The leaf is not the kind of file the open asked for: a FIFO, socket or device for a
    /// read, anything but a directory for a listing.
    WrongKind,
    /// A component of the path became a symbolic link after the path was checked.
    Symlink,
    Io(io::Error),
}

/// A path the jail admitted, and so how it is opened.
pub(super) enum ReadPath {
    /// Nothing confines the call; the path is opened as given, symlinks followed.
    AsGiven(PathBuf),
    /// The path is canonical and under a root; it is opened without following a symlink at
    /// any component, so one swapped in after the check is refused.
    Canonical(PathBuf),
}

/// Says why a path will not be read; its `Display` is the sentence the model sees.
pub(super) enum ReadRefusal {
    CannotResolve { path: PathBuf, source: io::Error },
    Outside { path: PathBuf, roots: RootSet },
}

/// Returns whether a call is confined: by a root on the tool, by a sandbox mode that confines,
/// or because it walks a tree.
fn confines(has_tool_root: bool, mode: SandboxMode, reach: Reach) -> bool {
    has_tool_root || mode != SandboxMode::DangerFullAccess || reach == Reach::Walk
}

impl ReadJail {
    /// Returns the jail for one call from the tool's canonical root and the call's context.
    pub(super) fn of(tool_root: Option<&Path>, env: &ToolEnv, reach: Reach) -> Self {
        let base = env.cwd.clone().or_else(|| tool_root.map(Path::to_path_buf));
        if !confines(tool_root.is_some(), env.sandbox.mode(), reach) {
            return Self { base, roots: None };
        }
        let confining = match (tool_root, env.cwd.as_deref()) {
            (Some(r), _) => Some(Ok(r.to_path_buf())),
            (None, Some(c)) => Some(canonical_root(c)),
            (None, None) => None,
        };
        let writable: &[PathBuf] = match &env.sandbox {
            SandboxPolicy::WorkspaceWrite { writable_roots, .. } => writable_roots,
            SandboxPolicy::ReadOnly { .. } | SandboxPolicy::DangerFullAccess { .. } => &[],
        };
        let roots = confining
            .into_iter()
            .chain(env.readable_roots.iter().map(|r| canonical_root(r)))
            .chain(writable.iter().map(|r| canonical_root(r)))
            .collect();
        Self {
            base,
            roots: Some(roots),
        }
    }

    /// Resolves `input` and returns how to open it: as given when unconfined, else canonical
    /// once a root admits it.
    pub(super) fn admit(self, input: &Path) -> Result<ReadPath, ReadRefusal> {
        let combined = match &self.base {
            Some(base) => base.join(input),
            None => input.to_path_buf(),
        };
        let Some(roots) = self.roots else {
            return Ok(ReadPath::AsGiven(combined));
        };
        let canonical =
            std::fs::canonicalize(&combined).map_err(|source| ReadRefusal::CannotResolve {
                path: combined,
                source,
            })?;
        if roots.admits(&canonical) {
            Ok(ReadPath::Canonical(canonical))
        } else {
            Err(ReadRefusal::Outside {
                path: canonical,
                roots,
            })
        }
    }
}

impl ReadPath {
    pub(super) fn path(&self) -> &Path {
        match self {
            Self::AsGiven(p) | Self::Canonical(p) => p,
        }
    }

    /// Opens the admitted path for reading without blocking on a FIFO.
    pub(super) fn open(&self) -> Result<File, OpenError> {
        match self {
            Self::AsGiven(p) => fs_open::open_following(p, Access::Read),
            Self::Canonical(p) => fs_open::open_canonical_read(p),
        }
        .map_err(|e| {
            let wrong_kind = fs_open::is_special_file(&e);
            self.open_error(e, wrong_kind)
        })
    }

    /// Opens the admitted path as a directory to list.
    pub(super) fn open_dir(&self) -> Result<Dir, OpenError> {
        match self {
            Self::AsGiven(p) => fs_open::open_dir_following(p),
            Self::Canonical(p) => fs_open::open_canonical_dir(p),
        }
        .map_err(|e| {
            let wrong_kind = e.kind() == io::ErrorKind::NotADirectory;
            self.open_error(e, wrong_kind)
        })
    }

    fn open_error(&self, source: io::Error, wrong_kind: bool) -> OpenError {
        let confined = matches!(self, Self::Canonical(_));
        if source.kind() == io::ErrorKind::NotFound {
            OpenError::NotFound
        } else if wrong_kind {
            OpenError::WrongKind
        } else if confined && fs_open::is_symlink_refusal(&source) {
            OpenError::Symlink
        } else {
            OpenError::Io(source)
        }
    }

    /// Returns how to open what a walk finds below the admitted path: the way that path is
    /// opened.
    pub(super) fn walk(&self) -> io::Result<WalkOpener> {
        match self {
            Self::AsGiven(_) => Ok(WalkOpener::AsGiven),
            Self::Canonical(p) => WalkRoot::open(p).map(WalkOpener::Beneath),
        }
    }
}

/// Opens the paths a walk found below an admitted path.
pub(super) enum WalkOpener {
    AsGiven,
    Beneath(WalkRoot),
}

impl WalkOpener {
    /// Opens `path` for reading without blocking on a FIFO.
    pub(super) fn open_file(&self, path: &Path) -> io::Result<File> {
        match self {
            Self::AsGiven => fs_open::open_following(path, Access::Read),
            Self::Beneath(b) => b.open_file(path),
        }
    }

    /// Returns whether `path` is a regular file; below a canonical root, reached without
    /// following a symlink.
    pub(super) fn is_file(&self, path: &Path) -> io::Result<bool> {
        match self {
            Self::AsGiven => std::fs::metadata(path).map(|m| m.is_file()),
            Self::Beneath(b) => b.is_file(path),
        }
    }
}

impl fmt::Display for ReadRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CannotResolve { path, source } => {
                write!(f, "cannot resolve path `{}`: {source}", path.display())
            }
            Self::Outside { path, roots } => {
                write!(
                    f,
                    "path `{}` resolves outside the allowed root(s): {roots}",
                    path.display()
                )?;
                if roots.is_unbounded() {
                    f.write_str(
                        " (no root, no cwd and no readable roots), so nothing bounds where it \
                         would read",
                    )?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_one_path_with_no_root_and_no_sandbox_is_unconfined() {
        assert!(!confines(false, SandboxMode::DangerFullAccess, Reach::Path));
    }

    #[test]
    fn a_tool_root_confines() {
        assert!(confines(true, SandboxMode::DangerFullAccess, Reach::Path));
    }

    #[test]
    fn a_confining_sandbox_confines() {
        assert!(confines(false, SandboxMode::ReadOnly, Reach::Path));
        assert!(confines(false, SandboxMode::WorkspaceWrite, Reach::Path));
    }

    #[test]
    fn a_walk_is_always_confined() {
        assert!(confines(false, SandboxMode::DangerFullAccess, Reach::Walk));
    }

    fn ctx(cwd: Option<&Path>, readable_roots: Vec<PathBuf>) -> ToolEnv {
        ToolEnv {
            cwd: cwd.map(Path::to_path_buf),
            readable_roots,
            ..Default::default()
        }
    }

    #[test]
    fn an_unresolvable_readable_root_is_dropped_and_the_rest_still_admit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x").unwrap();
        let gone = dir.path().join("deps-not-fetched-yet");
        let jail = ReadJail::of(None, &ctx(Some(dir.path()), vec![gone]), Reach::Walk);
        assert!(jail.admit(Path::new("a.rs")).is_ok());
    }

    #[test]
    fn a_refusal_names_the_root_that_was_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let gone = dir.path().join("deps-not-fetched-yet");
        let jail = ReadJail::of(
            None,
            &ctx(Some(dir.path()), vec![gone.clone()]),
            Reach::Walk,
        );
        let Err(refusal) = jail.admit(outside.path()) else {
            panic!("a path outside every root is refused");
        };
        let text = refusal.to_string();
        assert!(text.contains(&gone.display().to_string()), "{text}");
        assert!(text.contains("cannot be resolved"), "{text}");
    }

    #[test]
    fn with_no_root_no_cwd_and_no_readable_root_a_walk_reads_nothing() {
        let Err(refusal) =
            ReadJail::of(None, &ctx(None, vec![]), Reach::Walk).admit(Path::new("/"))
        else {
            panic!("nothing bounds this walk, so it is refused");
        };
        assert!(refusal.to_string().contains("nothing bounds"));
    }

    #[test]
    fn a_writable_root_outside_cwd_is_readable() {
        let wt = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(scratch.path().join("note.txt"), "hi").unwrap();
        let ctx = ToolEnv {
            cwd: Some(wt.path().to_path_buf()),
            sandbox: SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![wt.path().to_path_buf(), scratch.path().to_path_buf()],
                network: true,
            },
            ..Default::default()
        };
        let jail = ReadJail::of(None, &ctx, Reach::Path);
        assert!(jail.admit(&scratch.path().join("note.txt")).is_ok());
    }

    #[test]
    fn a_read_only_sandbox_adds_no_writable_root() {
        let wt = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("note.txt"), "hi").unwrap();
        let ctx = ToolEnv {
            cwd: Some(wt.path().to_path_buf()),
            sandbox: SandboxPolicy::read_only(true),
            ..Default::default()
        };
        let jail = ReadJail::of(None, &ctx, Reach::Path);
        assert!(jail.admit(&other.path().join("note.txt")).is_err());
    }

    #[test]
    fn a_relative_path_joins_cwd_when_there_is_one() {
        let cwd = tempfile::tempdir().unwrap();
        let jail = ReadJail::of(None, &ctx(Some(cwd.path()), vec![]), Reach::Path);
        let Ok(ReadPath::AsGiven(p)) = jail.admit(Path::new("a.rs")) else {
            panic!("an unconfined read is opened as given");
        };
        assert_eq!(p, cwd.path().join("a.rs"));
    }
}
