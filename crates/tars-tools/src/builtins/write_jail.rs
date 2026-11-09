//! Decides where `fs.write_file` and `fs.edit_file` may write, and opens the file there.
//!
//! A confined write lands only under the tool's root, when it has one, and, under
//! `WorkspaceWrite`, under the sandbox's writable roots but never under the directories
//! `tars_sandbox::protected_dirs` names for the workdir and those roots, the ones the OS jail of
//! `bash.run` protects; `ReadOnly` refuses every write. The workdir is the directory a relative
//! path is joined to: `cwd`, else the tool's root, else the process's own. A writable root that
//! cannot be resolved is dropped and named when a path is refused; with none left, nothing is
//! writable. The admitted location is canonical and is opened without following a symlink at any
//! component, so a symlink at the leaf, or a directory swapped for one after the check, refuses
//! the write. A confined leaf is written only when it is a regular file with no other name:
//! writing a hard link changes a file that may live outside the roots.

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Seek, Write};
use std::path::{Component, Path, PathBuf};

use super::fs_open::{self, Access};
use super::model_can_fix;
use super::roots::RootSet;
use crate::SandboxPolicy;
use crate::tool::{FsOp, ToolEnv, ToolError, ToolOutput};
use tars_sandbox::{FindGitDirError, ResolveRootError, canonical_root, protected_dirs};

/// Where one call may write, and what a relative path is joined to.
pub(super) struct WriteJail {
    /// Holds the call's `cwd`, else the tool's root; `None` leaves a relative path as given, to
    /// the process's own working directory.
    base: Option<PathBuf>,
    scope: Scope,
}

enum Scope {
    /// Every write is refused.
    ReadOnly,
    /// Nothing confines the call; the path is written as given, symlinks followed.
    Unconfined,
    Confined(Bounds),
}

/// What a confined write must sit under; every bound named holds.
pub(super) enum Bounds {
    /// Under the tool's root, which is canonical.
    Root(PathBuf),
    WritableRoots(WritableRoots),
    /// Under the tool's root and under one of the sandbox's writable roots.
    Both {
        root: PathBuf,
        writable: WritableRoots,
    },
}

/// The sandbox's writable roots for one call, and the directories none of them reaches.
pub(super) struct WritableRoots {
    roots: RootSet,
    /// Holds the protected directories of the workdir and the roots, each canonical up to its
    /// last component.
    protected: Vec<PathBuf>,
}

/// Says why a location breaks the bounds.
#[derive(Debug, PartialEq, Eq)]
enum Breach {
    Outside,
    /// The location is under this protected directory.
    Protected(PathBuf),
}

/// A location the jail admitted, and so how it is opened.
pub(super) enum WriteTarget {
    /// Nothing confines the call; missing parents are created and the path opened as given.
    AsGiven(PathBuf),
    /// `dir` is canonical, `missing` holds plain names of directories still to be made below
    /// it, and `leaf` is the file's name; each is opened without following a symlink.
    Beneath {
        dir: PathBuf,
        missing: PathBuf,
        leaf: OsString,
    },
}

/// Says why a write was refused; its `Display` is the sentence the model sees.
pub(super) enum WriteRefusal {
    ReadOnly {
        path: PathBuf,
    },
    Outside {
        path: PathBuf,
        bounds: Bounds,
    },
    Protected {
        path: PathBuf,
        protected: PathBuf,
    },
    /// A `..` follows a directory that does not exist yet; the kernel would resolve it only
    /// while creating the directories, past the check.
    DotDotAfterMissing {
        path: PathBuf,
        existing: PathBuf,
    },
    NamesNoFile {
        path: PathBuf,
    },
    /// The path, or the nearest part of it that exists, could not be resolved.
    CannotResolve {
        path: PathBuf,
        source: io::Error,
    },
    Symlink {
        path: PathBuf,
    },
    NotRegular {
        path: PathBuf,
    },
    HardLink {
        path: PathBuf,
        links: u64,
    },
}

/// Says why a write did not happen.
pub(super) enum WriteError {
    Refused(WriteRefusal),
    /// The filesystem failed while opening or writing an admitted path.
    Io {
        op: FsOp,
        path: PathBuf,
        source: io::Error,
    },
    /// The workdir cannot be resolved, so what to protect cannot be told. It is the host's to
    /// fix, not the model's.
    Workdir(ResolveRootError),
    /// A `.git` that may need protecting cannot be read. It is the host's to fix.
    GitDir(FindGitDirError),
}

/// Says what a write opens the target for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WriteAccess {
    /// Writes, creating the file when it is missing.
    Create,
    /// Reads and rewrites a file that must already exist.
    Edit,
}

impl From<WriteAccess> for Access {
    fn from(access: WriteAccess) -> Self {
        match access {
            WriteAccess::Create => Access::Create,
            WriteAccess::Edit => Access::Edit,
        }
    }
}

/// Says what the leaf must be before it is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeafRule {
    /// Anything the path names; an unconfined write may go to `/dev/null`.
    Any,
    /// A regular file, since an edit reads it.
    Regular,
    /// A regular file with no other name.
    RegularOneName,
}

/// Says why an opened leaf is not written.
#[derive(Debug, PartialEq, Eq)]
enum LeafRefusal {
    NotRegular,
    HardLink { links: u64 },
}

/// Returns what the leaf must be: a confined leaf a regular file with one name, an unconfined
/// one a regular file only when it is to be read.
fn leaf_rule(target: &WriteTarget, access: WriteAccess) -> LeafRule {
    match (target, access) {
        (WriteTarget::Beneath { .. }, _) => LeafRule::RegularOneName,
        (WriteTarget::AsGiven(_), WriteAccess::Create) => LeafRule::Any,
        (WriteTarget::AsGiven(_), WriteAccess::Edit) => LeafRule::Regular,
    }
}

/// Returns why an opened leaf must not be written, if it must not.
fn leaf_refusal(rule: LeafRule, is_file: bool, links: u64) -> Option<LeafRefusal> {
    match rule {
        LeafRule::Any => None,
        _ if !is_file => Some(LeafRefusal::NotRegular),
        LeafRule::RegularOneName if links > 1 => Some(LeafRefusal::HardLink { links }),
        LeafRule::Regular | LeafRule::RegularOneName => None,
    }
}

/// Returns whether `path` is `protected` or below it, comparing names without ASCII case: on a
/// case-insensitive disk, as macOS's default is, `.GIT/hooks` is `.git/hooks`.
fn under_protected(path: &Path, protected: &Path) -> bool {
    let mut below = path.components();
    protected.components().all(|c| {
        below.next().is_some_and(|d| {
            c.as_os_str()
                .as_encoded_bytes()
                .eq_ignore_ascii_case(d.as_os_str().as_encoded_bytes())
        })
    })
}

/// Returns whether every component of `dirs` is a plain name, so creating them stays below
/// where they start.
fn is_plain(dirs: &Path) -> bool {
    dirs.components().all(|c| matches!(c, Component::Normal(_)))
}

impl Bounds {
    /// Returns how the canonical `location` breaks the bounds, if it does.
    fn breach(&self, location: &Path) -> Option<Breach> {
        let (inside, protected): (bool, &[PathBuf]) = match self {
            Self::Root(root) => (location.starts_with(root), &[]),
            Self::WritableRoots(w) => (w.roots.admits(location), &w.protected),
            Self::Both { root, writable } => (
                location.starts_with(root) && writable.roots.admits(location),
                &writable.protected,
            ),
        };
        if !inside {
            return Some(Breach::Outside);
        }
        protected
            .iter()
            .find(|p| under_protected(location, p))
            .map(|p| Breach::Protected(p.clone()))
    }

    /// Refuses `path`, whose real location is `location`, when that breaks the bounds.
    fn admit(self, location: &Path, path: PathBuf) -> Result<(), WriteRefusal> {
        match self.breach(location) {
            None => Ok(()),
            Some(Breach::Outside) => Err(WriteRefusal::Outside { path, bounds: self }),
            Some(Breach::Protected(protected)) => Err(WriteRefusal::Protected { path, protected }),
        }
    }
}

/// Returns `dir` or its nearest ancestor that exists, and the part of `dir` below it that does
/// not. A symlink counts as existing even when it dangles, so canonicalizing it fails instead of
/// the walk stepping past it. A relative `dir` none of whose ancestors exists hangs off `.`.
fn nearest_existing(dir: &Path) -> Result<(PathBuf, &Path), WriteError> {
    for anchor in dir.ancestors().filter(|a| !a.as_os_str().is_empty()) {
        match std::fs::symlink_metadata(anchor) {
            Ok(_) => {
                let missing = dir
                    .strip_prefix(anchor)
                    .expect("`ancestors` yields only prefixes of `dir`");
                return Ok((anchor.to_path_buf(), missing));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(WriteError::Refused(WriteRefusal::CannotResolve {
                    path: anchor.to_path_buf(),
                    source,
                }));
            }
        }
    }
    Ok((PathBuf::from("."), dir))
}

/// Returns `path` canonicalized. A path that cannot be resolved is refused to the model, as a
/// read refuses it, whatever the errno: it is the path the model gave, and a race on it (macOS
/// can report `EINVAL`) must not end the run.
fn resolve(path: &Path) -> Result<PathBuf, WriteError> {
    std::fs::canonicalize(path).map_err(|source| {
        WriteError::Refused(WriteRefusal::CannotResolve {
            path: path.to_path_buf(),
            source,
        })
    })
}

impl WriteJail {
    /// Returns the jail for one call from the tool's canonical root and the call's context.
    ///
    /// # Errors
    ///
    /// Fails under `WorkspaceWrite` when the workdir cannot be canonicalized or a `.git` that may
    /// need protecting cannot be read: what to protect could not be told then.
    pub(super) fn of(tool_root: Option<&Path>, env: &ToolEnv) -> Result<Self, WriteError> {
        let base = env.cwd.clone().or_else(|| tool_root.map(Path::to_path_buf));
        let writable = match &env.sandbox {
            SandboxPolicy::ReadOnly { .. } => {
                return Ok(Self {
                    base,
                    scope: Scope::ReadOnly,
                });
            }
            SandboxPolicy::DangerFullAccess { .. } => None,
            SandboxPolicy::WorkspaceWrite { writable_roots, .. } => {
                let workdir = canonical_root(base.as_deref().unwrap_or(Path::new(".")))
                    .map_err(WriteError::Workdir)?;
                let roots: RootSet = writable_roots.iter().map(|r| canonical_root(r)).collect();
                let anchors: Vec<PathBuf> = std::iter::once(workdir)
                    .chain(roots.canonical().iter().cloned())
                    .collect();
                let protected = protected_dirs(&anchors).map_err(WriteError::GitDir)?;
                Some(WritableRoots { roots, protected })
            }
        };
        let scope = match (tool_root, writable) {
            (None, None) => Scope::Unconfined,
            (Some(r), None) => Scope::Confined(Bounds::Root(r.to_path_buf())),
            (None, Some(w)) => Scope::Confined(Bounds::WritableRoots(w)),
            (Some(r), Some(w)) => Scope::Confined(Bounds::Both {
                root: r.to_path_buf(),
                writable: w,
            }),
        };
        Ok(Self { base, scope })
    }

    fn combine(&self, input: &Path) -> PathBuf {
        match &self.base {
            Some(base) => base.join(input),
            None => input.to_path_buf(),
        }
    }

    /// Admits a file to create or overwrite, whose parent directories may not exist yet; their
    /// names below the nearest existing one must be plain.
    pub(super) fn admit_create(self, input: &Path) -> Result<WriteTarget, WriteError> {
        let path = self.combine(input);
        let bounds = match self.scope {
            Scope::ReadOnly => return Err(WriteError::Refused(WriteRefusal::ReadOnly { path })),
            Scope::Unconfined => return Ok(WriteTarget::AsGiven(path)),
            Scope::Confined(bounds) => bounds,
        };
        let (Some(parent), Some(leaf)) = (path.parent(), path.file_name()) else {
            return Err(WriteError::Refused(WriteRefusal::NamesNoFile { path }));
        };
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let (existing, missing) = nearest_existing(parent)?;
        if !is_plain(missing) {
            return Err(WriteError::Refused(WriteRefusal::DotDotAfterMissing {
                path,
                existing,
            }));
        }
        let dir = resolve(&existing)?;
        let missing = missing.to_path_buf();
        let leaf = leaf.to_os_string();
        bounds
            .admit(&dir.join(&missing).join(&leaf), path)
            .map_err(WriteError::Refused)?;
        Ok(WriteTarget::Beneath { dir, missing, leaf })
    }

    /// Admits a file that must already exist, resolved whole, so a symlink to a file outside is
    /// refused.
    pub(super) fn admit_existing(self, input: &Path) -> Result<WriteTarget, WriteError> {
        let path = self.combine(input);
        let bounds = match self.scope {
            Scope::ReadOnly => return Err(WriteError::Refused(WriteRefusal::ReadOnly { path })),
            Scope::Unconfined => return Ok(WriteTarget::AsGiven(path)),
            Scope::Confined(bounds) => bounds,
        };
        let canonical = resolve(&path)?;
        let (Some(dir), Some(leaf)) = (canonical.parent(), canonical.file_name()) else {
            return Err(WriteError::Refused(WriteRefusal::NamesNoFile { path }));
        };
        let (dir, leaf) = (dir.to_path_buf(), leaf.to_os_string());
        bounds
            .admit(&canonical, path)
            .map_err(WriteError::Refused)?;
        Ok(WriteTarget::Beneath {
            dir,
            missing: PathBuf::new(),
            leaf,
        })
    }
}

impl WriteTarget {
    /// Returns the location the target names.
    pub(super) fn path(&self) -> PathBuf {
        match self {
            Self::AsGiven(p) => p.clone(),
            Self::Beneath { dir, missing, leaf } => dir.join(missing).join(leaf),
        }
    }

    /// Opens the target for `access` without blocking on a FIFO, making missing parents for
    /// [`WriteAccess::Create`], and refuses a leaf its rule does not allow. It blocks on the
    /// filesystem, so it runs on the blocking pool.
    pub(super) fn open(&self, access: WriteAccess) -> Result<File, WriteError> {
        let file = match self {
            Self::AsGiven(p) => {
                if access == WriteAccess::Create
                    && let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty())
                {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| self.failure(FsOp::CreateParent, e))?;
                }
                fs_open::open_following(p, access.into())
                    .map_err(|e| self.failure(FsOp::Open, e))?
            }
            Self::Beneath { dir, missing, leaf } => {
                let parent = fs_open::open_canonical_dir(dir)
                    .and_then(|d| fs_open::make_dirs(d, missing))
                    .map_err(|e| self.failure(FsOp::CreateParent, e))?;
                fs_open::open_leaf(&parent, leaf, access.into())
                    .map_err(|e| self.failure(FsOp::Open, e))?
            }
        };
        let meta = file
            .metadata()
            .map_err(|e| self.failure(FsOp::Inspect, e))?;
        let path = self.path();
        match leaf_refusal(leaf_rule(self, access), meta.is_file(), links(&meta)) {
            None => Ok(file),
            Some(LeafRefusal::NotRegular) => {
                Err(WriteError::Refused(WriteRefusal::NotRegular { path }))
            }
            Some(LeafRefusal::HardLink { links }) => {
                Err(WriteError::Refused(WriteRefusal::HardLink { path, links }))
            }
        }
    }

    /// Returns why opening the target failed: a symlink a confined open met, a leaf that is not
    /// a file one opens, or the filesystem.
    fn failure(&self, op: FsOp, source: io::Error) -> WriteError {
        let path = self.path();
        if matches!(self, Self::Beneath { .. }) && fs_open::is_symlink_refusal(&source) {
            WriteError::Refused(WriteRefusal::Symlink { path })
        } else if fs_open::is_special_file(&source) {
            WriteError::Refused(WriteRefusal::NotRegular { path })
        } else {
            WriteError::Io { op, path, source }
        }
    }
}

/// Replaces what `file` holds with `bytes`. Only a regular file is truncated first: a device an
/// unconfined write may reach (`/dev/null`) has no length to set, and Linux refuses `ftruncate`
/// on it with `EINVAL`.
pub(super) fn replace_contents(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    if file.metadata()?.is_file() {
        file.set_len(0)?;
        file.rewind()?;
    }
    file.write_all(bytes)?;
    file.flush()
}

#[cfg(unix)]
fn links(meta: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::nlink(meta)
}

/// Returns one: the count is read only for a confined target, which this OS cannot open.
#[cfg(not(unix))]
fn links(_: &std::fs::Metadata) -> u64 {
    1
}

impl WriteError {
    /// Returns a refusal, and a filesystem failure the model can fix, as an `is_error` result
    /// the model can recover from; any other filesystem failure, and a jail that could not be
    /// built, as a [`ToolError`].
    pub(super) fn into_tool(self) -> Result<ToolOutput, ToolError> {
        match self {
            Self::Refused(r) => Ok(ToolOutput::error(r.to_string())),
            Self::Io { op, path, source } => {
                if model_can_fix(source.kind()) {
                    Ok(ToolOutput::error(format!(
                        "{op} `{}`: {source}",
                        path.display()
                    )))
                } else {
                    Err(ToolError::Io { op, path, source })
                }
            }
            Self::Workdir(e) => Err(ToolError::WriteWorkdir(e)),
            Self::GitDir(e) => Err(ToolError::WriteGitDir(e)),
        }
    }
}

impl fmt::Display for Bounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(root) => write!(f, "`{}`", root.display()),
            Self::WritableRoots(w) => write!(f, "{}", w.roots),
            Self::Both { root, writable } => write!(
                f,
                "the tool's root `{}`, and one of the writable roots {}",
                root.display(),
                writable.roots
            ),
        }
    }
}

impl fmt::Display for WriteRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown = |p: &Path| p.display().to_string();
        match self {
            Self::ReadOnly { path } => write!(
                f,
                "write to `{}` denied: sandbox mode is read-only",
                shown(path)
            ),
            Self::Outside { path, bounds } => write!(
                f,
                "path `{}` resolves outside the allowed write root(s): {bounds}",
                shown(path)
            ),
            Self::Protected { path, protected } => write!(
                f,
                "path `{}` is under `{}`, which a workspace-write sandbox never lets a write \
                 reach; nothing was written",
                shown(path),
                shown(protected)
            ),
            Self::DotDotAfterMissing { path, existing } => write!(
                f,
                "`{}` steps out of `{}` with `..` after a directory that does not exist; \
                 nothing was created",
                shown(path),
                shown(existing)
            ),
            Self::NamesNoFile { path } => write!(f, "`{}` names no file", shown(path)),
            Self::CannotResolve { path, source } => write!(
                f,
                "cannot resolve path `{}`: {source}; nothing was written",
                shown(path)
            ),
            Self::Symlink { path } => write!(
                f,
                "`{}` is, or is reached through, a symbolic link; a confined write does not \
                 follow it",
                shown(path)
            ),
            Self::NotRegular { path } => write!(
                f,
                "`{}` is not a regular file; nothing was written",
                shown(path)
            ),
            Self::HardLink { path, links } => write!(
                f,
                "`{}` has {links} names (a hard link); writing it would change a file that may \
                 be outside the allowed roots, so nothing was written",
                shown(path)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writable(roots: &[&str], protected: &[&str]) -> WritableRoots {
        WritableRoots {
            roots: roots.iter().map(|r| Ok(PathBuf::from(r))).collect(),
            protected: protected.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn a_path_under_every_bound_is_admitted() {
        let b = Bounds::Both {
            root: PathBuf::from("/wt"),
            writable: writable(&["/wt", "/tmp"], &["/wt/.git"]),
        };
        assert_eq!(b.breach(Path::new("/wt/a.rs")), None);
    }

    #[test]
    fn a_path_outside_one_bound_is_not_admitted() {
        let b = Bounds::Both {
            root: PathBuf::from("/wt"),
            writable: writable(&["/tmp"], &["/wt/.git"]),
        };
        assert_eq!(b.breach(Path::new("/wt/a.rs")), Some(Breach::Outside));
    }

    #[test]
    fn no_writable_roots_admit_nothing() {
        let b = Bounds::WritableRoots(writable(&[], &["/wt/.git"]));
        assert_eq!(b.breach(Path::new("/wt/a.rs")), Some(Breach::Outside));
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_under_the_root() {
        let b = Bounds::Root(PathBuf::from("/wt"));
        assert_eq!(b.breach(Path::new("/wt2/a.rs")), Some(Breach::Outside));
    }

    #[test]
    fn the_workdirs_git_directory_is_never_written_under_a_writable_root() {
        let b = Bounds::WritableRoots(writable(&["/wt"], &["/wt/.git"]));
        assert_eq!(
            b.breach(Path::new("/wt/.git/hooks/pre-commit")),
            Some(Breach::Protected(PathBuf::from("/wt/.git")))
        );
        assert_eq!(b.breach(Path::new("/wt/.github/ci.yml")), None);
    }

    #[test]
    fn every_protected_directory_is_never_written() {
        let b = Bounds::WritableRoots(writable(
            &["/repo"],
            &["/repo/pkg/.git", "/repo/.git", "/repo/gitdir"],
        ));
        assert_eq!(
            b.breach(Path::new("/repo/.git/hooks/pre-commit")),
            Some(Breach::Protected(PathBuf::from("/repo/.git")))
        );
        assert_eq!(
            b.breach(Path::new("/repo/gitdir/config")),
            Some(Breach::Protected(PathBuf::from("/repo/gitdir")))
        );
        assert_eq!(b.breach(Path::new("/repo/pkg/src/a.rs")), None);
    }

    fn beneath() -> WriteTarget {
        WriteTarget::Beneath {
            dir: PathBuf::from("/wt"),
            missing: PathBuf::new(),
            leaf: "a".into(),
        }
    }

    fn as_given() -> WriteTarget {
        WriteTarget::AsGiven(PathBuf::from("a"))
    }

    #[test]
    fn a_tool_root_alone_does_not_protect_git() {
        let b = Bounds::Root(PathBuf::from("/wt"));
        assert_eq!(b.breach(Path::new("/wt/.git/config")), None);
    }

    #[test]
    fn the_protected_directory_is_matched_without_ascii_case() {
        let p = Path::new("/wt/.git");
        assert!(under_protected(Path::new("/wt/.GIT/hooks"), p));
        assert!(under_protected(Path::new("/wt/.git"), p));
        assert!(!under_protected(Path::new("/wt/.gitignore"), p));
        assert!(!under_protected(Path::new("/wt"), p));
    }

    #[test]
    fn a_confined_leaf_must_be_a_regular_file_with_one_name() {
        let rule = leaf_rule(&beneath(), WriteAccess::Create);
        assert_eq!(leaf_refusal(rule, true, 1), None);
        assert_eq!(
            leaf_refusal(rule, true, 2),
            Some(LeafRefusal::HardLink { links: 2 })
        );
        assert_eq!(leaf_refusal(rule, false, 1), Some(LeafRefusal::NotRegular));
    }

    #[test]
    fn an_unconfined_write_may_go_to_a_device() {
        assert_eq!(
            leaf_refusal(leaf_rule(&as_given(), WriteAccess::Create), false, 1),
            None
        );
    }

    #[test]
    fn an_unconfined_edit_reads_only_a_regular_file() {
        let rule = leaf_rule(&as_given(), WriteAccess::Edit);
        assert_eq!(leaf_refusal(rule, false, 1), Some(LeafRefusal::NotRegular));
        assert_eq!(leaf_refusal(rule, true, 2), None);
    }

    #[test]
    fn plain_names_stay_below_where_they_start() {
        assert!(is_plain(Path::new("a/b")));
        assert!(is_plain(Path::new("")));
    }

    #[test]
    fn a_parent_step_does_not_stay_below() {
        assert!(!is_plain(Path::new("nope/../..")));
    }

    #[test]
    fn a_refusal_under_both_bounds_names_each_once() {
        let r = WriteRefusal::Outside {
            path: PathBuf::from("/x"),
            bounds: Bounds::Both {
                root: PathBuf::from("/wt"),
                writable: writable(&["/wt"], &["/wt/.git"]),
            },
        };
        assert_eq!(
            r.to_string(),
            "path `/x` resolves outside the allowed write root(s): the tool's root `/wt`, and \
             one of the writable roots `/wt`"
        );
    }
}
