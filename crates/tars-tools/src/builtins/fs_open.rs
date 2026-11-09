//! Opens the files the filesystem tools read and write. A path a jail admitted is canonical, so
//! it holds no symlink; it is opened one component at a time from `/`, each with `O_NOFOLLOW`,
//! and directories still to be made are made with `mkdirat` under the one just opened. A
//! directory on the path swapped for a symlink after the check then fails the open with
//! `ELOOP`, as a symlink at the leaf does, instead of leading it out of the roots. A walk opens
//! what it finds the same way, from the directory it started at ([`WalkRoot`]). Whether a path may
//! be opened is the jails' decision; this module only makes the open land on the path they
//! decided on.
//!
//! Every open is non-blocking, so a FIFO does not hang the thread that opens it.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags};

/// Says what an open is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Access {
    Read,
    /// Writes, creating the file when it is missing; it is not truncated on open, so a leaf
    /// refused after the open is left as it was.
    Create,
    /// Reads and writes a file that must already exist.
    Edit,
}

/// Returns whether an open failed because a symlink stood where none may be followed, at the
/// leaf or at a directory on the way.
pub(super) fn is_symlink_refusal(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

/// Returns whether a non-blocking open failed because the leaf is not a file one opens: a FIFO
/// no one reads (`ENXIO`, on writing), a Unix socket (`ENXIO` on Linux, `EOPNOTSUPP` on macOS).
pub(super) fn is_special_file(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        use rustix::io::Errno;
        [Errno::NXIO, Errno::OPNOTSUPP]
            .iter()
            .any(|n| e.raw_os_error() == Some(n.raw_os_error()))
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

#[cfg(unix)]
fn leaf_flags(access: Access) -> OFlags {
    let access = match access {
        Access::Read => OFlags::RDONLY,
        Access::Create => OFlags::WRONLY | OFlags::CREATE,
        Access::Edit => OFlags::RDWR,
    };
    access | OFlags::NONBLOCK | OFlags::CLOEXEC
}

#[cfg(unix)]
fn file_mode() -> Mode {
    Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH
}

/// Holds `O_EXEC` on Apple platforms, which rustix does not name; with `O_DIRECTORY` it is
/// `O_SEARCH`.
#[cfg(target_vendor = "apple")]
const APPLE_O_EXEC: u32 = 0x4000_0000;

/// Opens directories for the walk with only search permission, as resolving a path needs: on
/// Linux `O_PATH`, on Apple platforms `O_SEARCH`; elsewhere `O_RDONLY`, which also needs read
/// permission.
#[cfg(unix)]
fn dir_flags() -> OFlags {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let access = OFlags::PATH;
    #[cfg(target_vendor = "apple")]
    let access = OFlags::from_bits_retain(APPLE_O_EXEC);
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    let access = OFlags::RDONLY;
    access | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// Opens `path` as given, following symlinks, for a call nothing confines.
pub(super) fn open_following(path: &Path, access: Access) -> io::Result<File> {
    #[cfg(unix)]
    {
        let fd = rustix::fs::openat(CWD, path, leaf_flags(access), file_mode())?;
        Ok(File::from(fd))
    }
    #[cfg(not(unix))]
    {
        let mut open = std::fs::OpenOptions::new();
        match access {
            Access::Read => open.read(true),
            Access::Create => open.write(true).create(true),
            Access::Edit => open.read(true).write(true),
        };
        open.open(path)
    }
}

/// Opens the canonical `path` for reading without following a symlink at any component. `/`
/// opens as the directory it is, so a caller sees that it is not a regular file.
pub(super) fn open_canonical_read(path: &Path) -> io::Result<File> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return open_canonical_dir(path).map(Dir::into_file);
    };
    let dir = open_canonical_dir(parent)?;
    open_leaf(&dir, name, Access::Read)
}

/// A directory opened without following a symlink on the way to it.
#[cfg(unix)]
pub(super) struct Dir(std::os::fd::OwnedFd);

/// No directory is opened this way on this OS, so there is no value of this type.
#[cfg(not(unix))]
pub(super) enum Dir {}

impl Dir {
    fn into_file(self) -> File {
        #[cfg(unix)]
        {
            File::from(self.0)
        }
        #[cfg(not(unix))]
        {
            match self {}
        }
    }
}

/// Opens `name` in `dir` as a directory, refusing a symlink there with `ELOOP`: `O_NOFOLLOW`
/// with `O_DIRECTORY` reports one as `ENOTDIR`, the same as a file, so it is looked at again.
#[cfg(unix)]
fn open_subdir(dir: &std::os::fd::OwnedFd, name: &OsStr) -> io::Result<std::os::fd::OwnedFd> {
    match rustix::fs::openat(dir, name, dir_flags(), Mode::empty()) {
        Err(rustix::io::Errno::NOTDIR) => {
            let at = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            if FileType::from_raw_mode(at.st_mode) == FileType::Symlink {
                Err(rustix::io::Errno::LOOP.into())
            } else {
                Err(rustix::io::Errno::NOTDIR.into())
            }
        }
        r => Ok(r?),
    }
}

/// Opens the canonical directory `path` without following a symlink at any component.
pub(super) fn open_canonical_dir(path: &Path) -> io::Result<Dir> {
    #[cfg(unix)]
    {
        let mut components = path.components();
        if components.next() != Some(Component::RootDir) {
            return Err(not_canonical(path));
        }
        let mut dir = rustix::fs::openat(CWD, "/", dir_flags(), Mode::empty())?;
        for c in components {
            let Component::Normal(name) = c else {
                return Err(not_canonical(path));
            };
            dir = open_subdir(&dir, name)?;
        }
        Ok(Dir(dir))
    }
    #[cfg(not(unix))]
    {
        Err(unsupported(path))
    }
}

/// Makes each directory of `names`, plain names below `dir`, unless it exists, and returns the
/// last one; each is opened without following a symlink, so one swapped in is refused.
pub(super) fn make_dirs(dir: Dir, names: &Path) -> io::Result<Dir> {
    #[cfg(unix)]
    {
        let Dir(mut fd) = dir;
        for c in names.components() {
            let Component::Normal(name) = c else {
                return Err(not_canonical(names));
            };
            match rustix::fs::mkdirat(&fd, name, Mode::RWXU | Mode::RWXG | Mode::RWXO) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
            fd = open_subdir(&fd, name)?;
        }
        Ok(Dir(fd))
    }
    #[cfg(not(unix))]
    {
        let _ = names;
        match dir {}
    }
}

/// Opens `name` in `dir` without following a symlink at it.
pub(super) fn open_leaf(dir: &Dir, name: &OsStr, access: Access) -> io::Result<File> {
    #[cfg(unix)]
    {
        let flags = leaf_flags(access) | OFlags::NOFOLLOW;
        let fd = rustix::fs::openat(&dir.0, name, flags, file_mode())?;
        Ok(File::from(fd))
    }
    #[cfg(not(unix))]
    {
        let _ = (name, access);
        match *dir {}
    }
}

/// The canonical directory a walk started at, held open, so each path the walk finds below it
/// is opened from here, one component at a time and without following a symlink, rather than
/// from `/`.
pub(super) struct WalkRoot {
    root: PathBuf,
    dir: Dir,
}

impl WalkRoot {
    /// Opens the canonical `root`; when it is a file, its directory is held instead.
    pub(super) fn open(root: &Path) -> io::Result<Self> {
        match open_canonical_dir(root) {
            Ok(dir) => Ok(Self {
                root: root.to_path_buf(),
                dir,
            }),
            Err(e) if e.kind() == io::ErrorKind::NotADirectory => {
                let parent = root.parent().ok_or_else(|| not_canonical(root))?;
                Ok(Self {
                    root: parent.to_path_buf(),
                    dir: open_canonical_dir(parent)?,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Opens the directory holding `path` and returns it with the leaf's name.
    fn parent_of<'p>(&self, path: &'p Path) -> io::Result<(Dir, &'p OsStr)> {
        let rel = path
            .strip_prefix(&self.root)
            .map_err(|_| not_canonical(path))?;
        let (Some(name), Some(dirs)) = (rel.file_name(), rel.parent()) else {
            return Err(not_canonical(path));
        };
        #[cfg(unix)]
        {
            let mut fd = self.dir.0.try_clone()?;
            for c in dirs.components() {
                let Component::Normal(d) = c else {
                    return Err(not_canonical(path));
                };
                fd = open_subdir(&fd, d)?;
            }
            Ok((Dir(fd), name))
        }
        #[cfg(not(unix))]
        {
            let _ = (dirs, name);
            match self.dir {}
        }
    }

    /// Opens `path`, found below the root, for reading without following a symlink.
    pub(super) fn open_file(&self, path: &Path) -> io::Result<File> {
        let (dir, name) = self.parent_of(path)?;
        open_leaf(&dir, name, Access::Read)
    }

    /// Returns whether `path`, found below the root, is a regular file reached without
    /// following a symlink.
    pub(super) fn is_file(&self, path: &Path) -> io::Result<bool> {
        let (dir, name) = self.parent_of(path)?;
        #[cfg(unix)]
        {
            let at = rustix::fs::statat(&dir.0, name, AtFlags::SYMLINK_NOFOLLOW)?;
            Ok(FileType::from_raw_mode(at.st_mode) == FileType::RegularFile)
        }
        #[cfg(not(unix))]
        {
            let _ = (name, path);
            match dir {}
        }
    }
}

/// What one directory entry is, read without following it.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EntryKind {
    /// A regular file of this many bytes.
    File {
        size: u64,
    },
    Dir,
    /// A symbolic link to this target, as written.
    Symlink {
        target: PathBuf,
    },
    Other,
}

/// One entry of a directory.
#[derive(Debug)]
pub(super) struct Entry {
    pub(super) name: std::ffi::OsString,
    pub(super) kind: EntryKind,
}

/// Opens `path` as a directory to list, following symlinks, for a call nothing confines.
pub(super) fn open_dir_following(path: &Path) -> io::Result<Dir> {
    #[cfg(unix)]
    {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        Ok(Dir(rustix::fs::openat(CWD, path, flags, Mode::empty())?))
    }
    #[cfg(not(unix))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "`{}` not listed: this OS has no directory handle to list it through",
                path.display()
            ),
        ))
    }
}

/// Returns up to `cap` entries of `dir`, in the order it yields them, and whether there were
/// more. Each is looked at through `dir` without following it; one removed meanwhile is left out.
pub(super) fn entries(dir: &Dir, cap: usize) -> io::Result<(Vec<Entry>, bool)> {
    #[cfg(unix)]
    {
        let readable = rustix::fs::openat(
            &dir.0,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let mut out = Vec::new();
        for entry in rustix::fs::Dir::new(readable)? {
            let entry = entry?;
            let name = entry.file_name();
            if [&b"."[..], b".."].contains(&name.to_bytes()) {
                continue;
            }
            if out.len() >= cap {
                return Ok((out, true));
            }
            let at = match rustix::fs::statat(&dir.0, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(at) => at,
                Err(rustix::io::Errno::NOENT) => continue,
                Err(e) => return Err(e.into()),
            };
            let kind = match FileType::from_raw_mode(at.st_mode) {
                FileType::RegularFile => EntryKind::File {
                    size: u64::try_from(at.st_size).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("the filesystem gave a size of {} bytes", at.st_size),
                        )
                    })?,
                },
                FileType::Directory => EntryKind::Dir,
                FileType::Symlink => match rustix::fs::readlinkat(&dir.0, name, Vec::new()) {
                    Ok(t) => EntryKind::Symlink {
                        target: PathBuf::from(
                            <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(
                                t.into_bytes(),
                            ),
                        ),
                    },
                    Err(rustix::io::Errno::NOENT) => continue,
                    Err(e) => return Err(e.into()),
                },
                _ => EntryKind::Other,
            };
            out.push(Entry {
                name: <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(
                    name.to_bytes(),
                )
                .to_os_string(),
                kind,
            });
        }
        Ok((out, false))
    }
    #[cfg(not(unix))]
    {
        let _ = cap;
        match *dir {}
    }
}

fn not_canonical(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("`{}` is not a canonical path", path.display()),
    )
}

#[cfg(not(unix))]
fn unsupported(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "`{}` not opened: this OS cannot open a path without following symlinks, so a \
             confined call is refused",
            path.display()
        ),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn canonical_tempdir() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    #[test]
    fn a_canonical_path_opens() {
        let (_d, root) = canonical_tempdir();
        std::fs::create_dir(root.join("a")).unwrap();
        std::fs::write(root.join("a/f"), "x").unwrap();
        assert!(open_canonical_read(&root.join("a/f")).is_ok());
    }

    #[test]
    fn a_symlinked_directory_on_the_path_is_not_followed() {
        let (_d, root) = canonical_tempdir();
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/f"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let err = open_canonical_read(&root.join("link/f")).unwrap_err();
        assert!(is_symlink_refusal(&err), "{err}");
    }

    #[test]
    fn a_file_on_the_path_is_not_a_directory_not_a_symlink() {
        let (_d, root) = canonical_tempdir();
        std::fs::write(root.join("f"), "x").unwrap();
        let err = open_canonical_read(&root.join("f/g")).unwrap_err();
        assert!(!is_symlink_refusal(&err), "{err}");
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
    }

    #[test]
    fn the_root_directory_opens_as_a_directory() {
        let file = open_canonical_read(Path::new("/")).unwrap();
        assert!(file.metadata().unwrap().is_dir());
    }

    #[test]
    fn a_socket_is_a_special_file() {
        let (_d, root) = canonical_tempdir();
        let _l = std::os::unix::net::UnixListener::bind(root.join("s")).unwrap();
        let err = open_canonical_read(&root.join("s")).unwrap_err();
        assert!(is_special_file(&err), "{err}");
    }

    #[test]
    fn a_directory_that_may_only_be_searched_is_passed_through() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, root) = canonical_tempdir();
        std::fs::create_dir(root.join("x")).unwrap();
        std::fs::write(root.join("x/f"), "x").unwrap();
        std::fs::set_permissions(root.join("x"), std::fs::Permissions::from_mode(0o311)).unwrap();
        let opened = open_canonical_read(&root.join("x/f"));
        std::fs::set_permissions(root.join("x"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(opened.is_ok(), "{:?}", opened.err());
    }

    #[test]
    fn a_walk_opens_below_its_root_without_following_a_symlink() {
        let (_d, root) = canonical_tempdir();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("f"), "SECRET").unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/f"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        let beneath = WalkRoot::open(&root).unwrap();
        assert!(beneath.open_file(&root.join("real/f")).is_ok());
        assert!(beneath.is_file(&root.join("real/f")).unwrap());
        let err = beneath.open_file(&root.join("link/f")).unwrap_err();
        assert!(is_symlink_refusal(&err), "{err}");
        assert!(beneath.is_file(&root.join("link/f")).is_err());
    }

    #[test]
    fn a_walk_rooted_at_a_file_opens_that_file() {
        let (_d, root) = canonical_tempdir();
        std::fs::write(root.join("f"), "x").unwrap();
        let beneath = WalkRoot::open(&root.join("f")).unwrap();
        assert!(beneath.open_file(&root.join("f")).is_ok());
    }

    #[test]
    fn entries_are_read_without_following_them() {
        let (_d, root) = canonical_tempdir();
        std::fs::write(root.join("f"), "abc").unwrap();
        std::fs::create_dir(root.join("d")).unwrap();
        std::os::unix::fs::symlink("d", root.join("l")).unwrap();
        let (mut got, truncated) = entries(&open_canonical_dir(&root).unwrap(), 10).unwrap();
        got.sort_by(|a, b| a.name.cmp(&b.name));
        assert!(!truncated);
        let kinds: Vec<_> = got
            .iter()
            .map(|e| (e.name.to_str().unwrap(), &e.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("d", &EntryKind::Dir),
                ("f", &EntryKind::File { size: 3 }),
                (
                    "l",
                    &EntryKind::Symlink {
                        target: PathBuf::from("d")
                    }
                ),
            ]
        );
    }

    #[test]
    fn entries_past_the_cap_are_cut() {
        let (_d, root) = canonical_tempdir();
        for n in ["a", "b", "c"] {
            std::fs::write(root.join(n), "").unwrap();
        }
        let (got, truncated) = entries(&open_canonical_dir(&root).unwrap(), 2).unwrap();
        assert_eq!(got.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn a_symlink_at_the_leaf_is_refused_as_one() {
        let (_d, root) = canonical_tempdir();
        std::fs::write(root.join("real"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let err = open_canonical_read(&root.join("link")).unwrap_err();
        assert!(is_symlink_refusal(&err), "{err}");
    }

    #[test]
    fn a_relative_path_is_not_canonical() {
        let err = open_canonical_dir(Path::new("a/b")).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn missing_directories_are_made_below_the_one_opened() {
        let (_d, root) = canonical_tempdir();
        let dir = open_canonical_dir(&root).unwrap();
        let made = make_dirs(dir, Path::new("a/b")).unwrap();
        assert!(open_leaf(&made, OsStr::new("f"), Access::Create).is_ok());
        assert!(root.join("a/b/f").is_file());
    }

    #[test]
    fn a_symlink_where_a_directory_is_to_be_made_is_not_followed() {
        let (_d, root) = canonical_tempdir();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("a")).unwrap();
        let dir = open_canonical_dir(&root).unwrap();
        assert!(make_dirs(dir, Path::new("a/b")).is_err());
        assert!(!outside.path().join("b").exists());
    }
}
