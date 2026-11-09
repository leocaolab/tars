//! Builds the command line that runs a program under an OS write-jail: macOS Seatbelt
//! (`sandbox-exec`) or Linux bubblewrap. Reads stay broad and network egress can be cut. Under
//! `WorkspaceWrite` the program writes only under the writable roots, and never under a `.git`
//! from the workdir or a writable root up to `/`, nor where one points (see [`protected_dirs`]);
//! under `ReadOnly` it writes under no root. Either way it can still write the terminal devices
//! (`/dev/null`, `/dev/stdout`, `/dev/stderr`, `/dev/tty*`), and on Linux a private `/tmp` that
//! vanishes with it, unless `/tmp` is a writable root, whose bind then covers the private one. On
//! Linux it runs in a session of its own, so it cannot push input into the caller's terminal.
//!
//! It does not spawn anything and does not know which tool or provider is running the command:
//! `tars-tools` (BashTool) and `tars-llm` (claude_cli) both spawn what [`SandboxPolicy::wrap`]
//! returns, which is why it sits below both. Reads are not jailed because a macOS profile that
//! denies too many reads aborts the process at start.
//!
//! A policy that confines but cannot be built is an error, never an unconfined command.

#![deny(clippy::print_stdout, clippy::print_stderr)]

#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Names which kind of [`SandboxPolicy`] a policy is, the word a flag or a config file spells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    #[default]
    DangerFullAccess,
}

/// Says where a confined program may write and whether it may reach the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// Allows no writes under any root.
    ReadOnly { network: bool },
    /// Allows writes only under `writable_roots`, except under the protected directories of the
    /// workdir and the roots (see [`protected_dirs`]); with no roots it allows none.
    WorkspaceWrite {
        writable_roots: Vec<PathBuf>,
        network: bool,
    },
    /// Runs the program unconfined. `network` is kept so that a caller that confines the
    /// program anyway (a CLI delegate is always jailed) carries the caller's choice over.
    DangerFullAccess { network: bool },
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::DangerFullAccess { network: true }
    }
}

/// Says that a root could not be canonicalized, usually because it does not exist. The caller
/// decides what that costs: a tool with such a root is not built, a writable or readable root is
/// dropped and named.
#[derive(Debug)]
pub struct ResolveRootError {
    pub root: PathBuf,
    pub source: io::Error,
}

impl fmt::Display for ResolveRootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot resolve root `{}`", self.root.display())
    }
}

impl std::error::Error for ResolveRootError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Returns `root` canonicalized, so that a jail compares real paths: Seatbelt matches the real
/// path (`/tmp` is `/private/tmp`), and a symlink to a root behaves as the root.
///
/// # Errors
///
/// Fails when `root` cannot be canonicalized.
pub fn canonical_root(root: &Path) -> Result<PathBuf, ResolveRootError> {
    std::fs::canonicalize(root).map_err(|source| ResolveRootError {
        root: root.to_path_buf(),
        source,
    })
}

/// Says that what stands at a `.git`, or at the place it points to, could not be read for a
/// reason other than its absence; a jail that cannot tell what to protect is not built.
#[derive(Debug)]
pub struct FindGitDirError {
    pub path: PathBuf,
    pub source: io::Error,
}

impl fmt::Display for FindGitDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot read `{}` to find the git directory",
            self.path.display()
        )
    }
}

impl std::error::Error for FindGitDirError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn unreadable(path: &Path) -> impl FnOnce(io::Error) -> FindGitDirError {
    let path = path.to_path_buf();
    move |source| FindGitDirError { path, source }
}

fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Returns the path a `.git` file points at, the text after `gitdir:` on its first line that
/// has one.
fn gitdir_line(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|p| !p.is_empty())
}

/// Returns `paths` with each repeat after its first left out.
fn unique(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Returns `<d>/.git` for every directory `d` from each anchor up to `/`, whether or not it
/// exists: git may use any of them, and one made later must not take over from one there now.
fn dot_gits(anchors: &[PathBuf]) -> Vec<PathBuf> {
    unique(
        anchors
            .iter()
            .flat_map(|a| a.ancestors())
            .map(|d| d.join(".git")),
    )
}

/// Returns `path` canonicalized, or, when nothing is there yet, its deepest existing ancestor
/// canonicalized with the rest joined on and `..` and `.` resolved: a place a `.git` names is
/// protected whether or not it exists, and a jail matches real paths, so `wt/../gd` would never
/// match the `gd` a program then makes.
fn real_or_named(path: PathBuf) -> Result<PathBuf, FindGitDirError> {
    for base in path.ancestors() {
        match std::fs::canonicalize(base) {
            Ok(real) => {
                let rest: PathBuf = path.components().skip(base.components().count()).collect();
                return Ok(lexical_join(real, &rest));
            }
            Err(e) if is_absent(&e) => continue,
            Err(source) => {
                return Err(FindGitDirError {
                    path: base.to_path_buf(),
                    source,
                });
            }
        }
    }
    Ok(path)
}

/// Returns `base` with `rest` joined on, each `..` taking off the last component and each `.`
/// dropped. It is only right when nothing in `rest` exists, so no symlink can stand in it.
fn lexical_join(base: PathBuf, rest: &Path) -> PathBuf {
    use std::path::Component;
    rest.components().fold(base, |mut out, c| {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
        out
    })
}

/// Returns where the `.git` at `dot` points: the target of a symlink or the `gitdir:` of a file,
/// and that git directory's common dir (the main repository's, for a linked worktree). Nothing
/// is there to add when `dot` is absent, a directory with no `commondir`, or a file without a
/// `gitdir:` line, which git refuses to use.
fn pointed_at(dot: &Path) -> Result<Vec<PathBuf>, FindGitDirError> {
    let meta = match std::fs::symlink_metadata(dot) {
        Ok(meta) => meta,
        Err(e) if is_absent(&e) => return Ok(Vec::new()),
        Err(source) => return Err(unreadable(dot)(source)),
    };
    let target = if meta.file_type().is_symlink() {
        let link = std::fs::read_link(dot).map_err(unreadable(dot))?;
        real_or_named(dot.with_file_name(link))?
    } else {
        dot.to_path_buf()
    };
    let git_dir = match std::fs::metadata(&target) {
        Ok(m) if m.is_file() => {
            let text = std::fs::read_to_string(&target).map_err(unreadable(&target))?;
            let Some(named) = gitdir_line(&text) else {
                return Ok(vec![target]);
            };
            real_or_named(target.with_file_name(named))?
        }
        Ok(_) => target.clone(),
        Err(e) if is_absent(&e) => return Ok(vec![target]),
        Err(source) => return Err(unreadable(&target)(source)),
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => Some(real_or_named(git_dir.join(text.trim()))?),
        Err(e) if is_absent(&e) => None,
        Err(source) => return Err(unreadable(&git_dir.join("commondir"))(source)),
    };
    Ok(unique(
        std::iter::once(target).chain([git_dir]).chain(common),
    ))
}

/// Returns the directories a `WorkspaceWrite` jail never lets a write reach, even when a
/// writable root covers them, from the canonical `anchors` (the workdir and each writable root):
/// hooks and config in a git directory run code on the host the next time anyone runs git
/// there. They are `<d>/.git` for every `d` from each anchor up to `/`, and for each that exists
/// the place it points to and its common dir.
///
/// It does not decide which repository git would use, since that turns on what a confined
/// program can make on disk. What it reads cannot be changed from inside the jail either: each
/// path is denied and its ancestors pinned (see [`seatbelt_profile`], [`bwrap_argv`]), so working
/// it out again on the next call gives the same list, longer only by a `.git` a Linux jail could
/// not keep from being made.
///
/// # Errors
///
/// Fails when what stands at a `.git`, or where it points, cannot be read for a reason other
/// than its absence.
pub fn protected_dirs(anchors: &[PathBuf]) -> Result<Vec<PathBuf>, FindGitDirError> {
    let mut dirs = Vec::new();
    for dot in dot_gits(anchors) {
        let pointed = pointed_at(&dot)?;
        dirs.push(dot);
        dirs.extend(pointed);
    }
    Ok(unique(dirs))
}

/// Returns the directories a jail must keep from being renamed or removed: each ancestor of a
/// protected directory that sits under a writable root and not under a protected directory.
/// Renaming one would carry a protected directory out from under the path its rule names.
fn pinned_dirs(protected: &[PathBuf], writable: &[PathBuf]) -> Vec<PathBuf> {
    unique(
        protected
            .iter()
            .flat_map(|p| p.ancestors().skip(1))
            .filter(|a| writable.iter().any(|w| a.starts_with(w)))
            .filter(|a| !protected.iter().any(|p| a.starts_with(p)))
            .map(Path::to_path_buf),
    )
}

/// Says why a confining policy could not be built. A caller must refuse to run the command
/// rather than run it unconfined.
#[derive(Debug)]
pub enum SandboxError {
    /// The workdir could not be canonicalized, usually because it does not exist.
    UnresolvableWorkdir(ResolveRootError),
    /// The git directories to protect could not be found.
    GitDirUnreadable(FindGitDirError),
    /// The OS has no jail this crate knows how to build.
    Unsupported { os: &'static str },
    /// The jail's program is not installed, so the command could not be spawned under it.
    WrapperMissing { program: &'static str },
    /// A path the jail must name is not UTF-8, and the jail's rules are text (a Seatbelt
    /// profile), so the rule could not be written.
    NonUtf8Path { path: PathBuf },
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnresolvableWorkdir(_) => write!(f, "sandbox workdir"),
            Self::GitDirUnreadable(_) => write!(f, "sandbox"),
            Self::Unsupported { os } => {
                write!(f, "sandbox unavailable on {os}; refusing to run unconfined")
            }
            Self::WrapperMissing { program } => write!(
                f,
                "sandbox unavailable: `{program}` is not installed; refusing to run unconfined"
            ),
            Self::NonUtf8Path { path } => write!(
                f,
                "sandbox profile cannot name the non-UTF-8 path `{}`; refusing to run unconfined",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SandboxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnresolvableWorkdir(e) => Some(e),
            Self::GitDirUnreadable(e) => Some(e),
            Self::Unsupported { .. } | Self::WrapperMissing { .. } | Self::NonUtf8Path { .. } => {
                None
            }
        }
    }
}

/// A command ready to spawn, and the writable roots left out of its jail because they could not
/// be resolved; the caller says so to whoever can act on it.
#[must_use]
#[derive(Debug)]
pub struct CommandLine {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub dropped: Vec<ResolveRootError>,
}

impl SandboxPolicy {
    pub fn workspace_write(workdir: &Path) -> Self {
        Self::WorkspaceWrite {
            writable_roots: vec![workdir.to_path_buf()],
            network: true,
        }
    }

    pub fn read_only(network: bool) -> Self {
        Self::ReadOnly { network }
    }

    pub fn mode(&self) -> SandboxMode {
        match self {
            Self::ReadOnly { .. } => SandboxMode::ReadOnly,
            Self::WorkspaceWrite { .. } => SandboxMode::WorkspaceWrite,
            Self::DangerFullAccess { .. } => SandboxMode::DangerFullAccess,
        }
    }

    pub fn network(&self) -> bool {
        match self {
            Self::ReadOnly { network }
            | Self::WorkspaceWrite { network, .. }
            | Self::DangerFullAccess { network } => *network,
        }
    }

    /// Returns the roots the jail lets the program write under, or `None` when it runs
    /// unconfined.
    fn jail_roots(&self) -> Option<&[PathBuf]> {
        match self {
            Self::DangerFullAccess { .. } => None,
            Self::ReadOnly { .. } => Some(&[]),
            Self::WorkspaceWrite { writable_roots, .. } => Some(writable_roots),
        }
    }

    /// Returns the command that runs `program args` in `workdir` under this policy;
    /// `DangerFullAccess` returns it unchanged. A writable root that cannot be resolved is left
    /// out of the jail and returned in [`CommandLine::dropped`]. The jail's own program is looked
    /// up now, so a missing one fails here rather than at spawn.
    ///
    /// # Errors
    ///
    /// [`SandboxError::UnresolvableWorkdir`] when `workdir` cannot be canonicalized,
    /// [`SandboxError::GitDirUnreadable`] when its git directories cannot be found,
    /// [`SandboxError::WrapperMissing`] when `sandbox-exec` or `bwrap` is not installed,
    /// [`SandboxError::NonUtf8Path`] on macOS when a path the profile names is not UTF-8, and
    /// [`SandboxError::Unsupported`] on an OS other than macOS and Linux.
    pub fn wrap(
        &self,
        program: &str,
        args: &[String],
        workdir: &Path,
    ) -> Result<CommandLine, SandboxError> {
        let Some(roots) = self.jail_roots() else {
            return Ok(CommandLine {
                program: PathBuf::from(program),
                args: args.iter().map(OsString::from).collect(),
                dropped: Vec::new(),
            });
        };

        let work = canonical_root(workdir).map_err(SandboxError::UnresolvableWorkdir)?;
        let (mut writable, mut dropped) = (Vec::new(), Vec::new());
        for root in roots {
            match canonical_root(root) {
                Ok(p) => writable.push(p),
                Err(e) => dropped.push(e),
            }
        }
        // With nothing writable there is nothing to keep a write from, and a `.git` that cannot
        // be read must not refuse a read-only run.
        let protected = if writable.is_empty() {
            Vec::new()
        } else {
            let anchors: Vec<PathBuf> = std::iter::once(work.clone())
                .chain(writable.iter().cloned())
                .collect();
            protected_dirs(&anchors).map_err(SandboxError::GitDirUnreadable)?
        };

        #[cfg(target_os = "macos")]
        {
            let wrapper = installed(SEATBELT, std::env::var_os("PATH").as_deref())?;
            let profile = seatbelt_profile(&writable, self.network(), &protected)?;
            let argv = [String::from("-p"), profile, program.to_string()]
                .into_iter()
                .chain(args.iter().cloned())
                .map(OsString::from)
                .collect();
            Ok(CommandLine {
                program: wrapper,
                args: argv,
                dropped,
            })
        }
        #[cfg(target_os = "linux")]
        {
            let wrapper = installed(BWRAP, std::env::var_os("PATH").as_deref())?;
            Ok(CommandLine {
                program: wrapper,
                args: bwrap_argv(program, args, &writable, &protected, &work, self.network()),
                dropped,
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (program, args, &writable, &protected, dropped);
            Err(SandboxError::Unsupported {
                os: std::env::consts::OS,
            })
        }
    }
}

#[cfg(target_os = "macos")]
const SEATBELT: &str = "/usr/bin/sandbox-exec";
#[cfg(target_os = "linux")]
const BWRAP: &str = "bwrap";

/// Returns the path `program` would run from: itself when it is a path, else the first `PATH`
/// entry holding a file of that name.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn installed(program: &'static str, path_var: Option<&OsStr>) -> Result<PathBuf, SandboxError> {
    candidates(program, path_var)
        .into_iter()
        .find(|c| c.is_file())
        .ok_or(SandboxError::WrapperMissing { program })
}

/// Returns where `program` may live, in lookup order: only itself when it names a path, else
/// `<dir>/<program>` for each `PATH` entry.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn candidates(program: &str, path_var: Option<&OsStr>) -> Vec<PathBuf> {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return vec![p.to_path_buf()];
    }
    path_var
        .map(|v| std::env::split_paths(v).map(|d| d.join(program)).collect())
        .unwrap_or_default()
}

/// Returns `$TMPDIR` and `/tmp`, the temp directories a delegated CLI writes to, keeping only the
/// ones that exist as directories so that appending them to `writable_roots` cannot make
/// [`SandboxPolicy::wrap`] fail.
pub fn default_tmp_writable_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(tmpdir) = std::env::var_os("TMPDIR")
        && !tmpdir.is_empty()
    {
        let p = PathBuf::from(tmpdir);
        if p.is_dir() {
            roots.push(p);
        }
    }
    let slash_tmp = PathBuf::from("/tmp");
    if slash_tmp.is_dir() {
        roots.push(slash_tmp);
    }
    roots
}

/// Renders a Seatbelt profile that allows writes only under `writable` and never under
/// `protected`, so an agent cannot rewrite git hooks or config to run code on the host. Each
/// ancestor of a protected directory inside a writable root is denied as a literal path: Seatbelt
/// matches paths, so renaming one would move a protected directory out from under its rule, while
/// making and removing entries inside it stays allowed. The denies come last because Seatbelt's
/// last matching rule wins.
///
/// # Errors
///
/// [`SandboxError::NonUtf8Path`] when a path it would name is not UTF-8: the profile is text, and
/// a lossy rendering would name some other path.
#[cfg(any(target_os = "macos", test))]
pub fn seatbelt_profile(
    writable: &[PathBuf],
    network: bool,
    protected: &[PathBuf],
) -> Result<String, SandboxError> {
    let mut p = String::from("(version 1)\n(allow default)\n");
    if !network {
        p.push_str("(deny network*)\n");
    }
    p.push_str("(deny file-write*)\n(allow file-write*\n");
    for w in writable {
        p.push_str(&format!("  (subpath {})\n", sbpl_string(w)?));
    }
    p.push_str("  (literal \"/dev/null\") (literal \"/dev/stdout\") (literal \"/dev/stderr\")\n");
    p.push_str("  (regex #\"^/dev/tty\"))\n");
    for d in protected {
        p.push_str(&format!(
            "(deny file-write* (subpath {}))\n",
            sbpl_string(d)?
        ));
    }
    for d in pinned_dirs(protected, writable) {
        p.push_str(&format!(
            "(deny file-write* (literal {}))\n",
            sbpl_string(&d)?
        ));
    }
    Ok(p)
}

/// Returns `path` as a quoted Seatbelt string, with `\\` and `"` escaped, so a path holding a
/// quote cannot end the string and add rules of its own.
#[cfg(any(target_os = "macos", test))]
fn sbpl_string(path: &Path) -> Result<String, SandboxError> {
    let text = path.to_str().ok_or_else(|| SandboxError::NonUtf8Path {
        path: path.to_path_buf(),
    })?;
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

/// Returns bubblewrap's arguments for running `program args` in `workdir` under the jail. Paths
/// are passed as the bytes they are, so a non-UTF-8 path names itself, not a lossy copy.
#[cfg(any(target_os = "linux", test))]
pub fn bwrap_argv(
    program: &str,
    args: &[String],
    writable: &[PathBuf],
    protected: &[PathBuf],
    workdir: &Path,
    network: bool,
) -> Vec<OsString> {
    let mut a: Vec<OsString> = [
        "--die-with-parent",
        // A program sharing the caller's session could push input into its terminal (TIOCSTI).
        "--new-session",
        "--unshare-user",
        "--unshare-pid",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if !network {
        a.push("--unshare-net".into());
    }
    for w in writable {
        a.extend(["--bind".into(), w.into(), w.into()]);
    }
    // Each ancestor of a protected directory inside a writable root is bound onto itself, so it
    // is a mount point, which cannot be renamed or removed: renaming it would carry a protected
    // directory out from under its mount. The protected directories are then mounted read-only
    // over the writable binds so an agent cannot rewrite git hooks or config to run code on the
    // host. A bind needs its source to exist, so `--ro-bind-try` skips a `.git` that is not there,
    // and on Linux the program can make one there; the `.git` levels that do exist stay protected,
    // so it cannot take over from them, but a git run later in that directory would use it.
    for d in pinned_dirs(protected, writable) {
        a.extend(["--bind-try".into(), (&d).into(), d.into()]);
    }
    for d in protected {
        a.extend(["--ro-bind-try".into(), d.into(), d.into()]);
    }
    a.extend([
        "--chdir".into(),
        workdir.into(),
        "--".into(),
        program.into(),
    ]);
    a.extend(args.iter().map(OsString::from));
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn danger_full_access_is_passthrough() {
        let wrapped = SandboxPolicy::default()
            .wrap("claude", &["-p".into()], Path::new("/"))
            .unwrap();
        assert_eq!(wrapped.program, Path::new("claude"));
        assert_eq!(wrapped.args, vec!["-p"]);
    }

    #[test]
    fn seatbelt_write_jail_shape() {
        let prof =
            seatbelt_profile(&[PathBuf::from("/wt")], true, &[PathBuf::from("/wt/.git")]).unwrap();
        assert!(prof.contains("(allow default)"));
        assert!(prof.contains("(deny file-write*)"));
        assert!(prof.contains("(subpath \"/wt\")"));
        assert!(!prof.contains("(deny network*)"));
        assert!(!prof.contains("/private/tmp"));
        assert!(!prof.contains("/var/folders"));
        assert!(prof.contains("(deny file-write* (subpath \"/wt/.git\"))"));
        assert!(prof.find("(allow file-write*").unwrap() < prof.find("/wt/.git").unwrap());
    }

    #[test]
    fn seatbelt_renders_extra_writable_roots() {
        let prof = seatbelt_profile(
            &[
                PathBuf::from("/wt"),
                PathBuf::from("/private/tmp"),
                PathBuf::from("/home/u/.codex"),
            ],
            true,
            &[],
        )
        .unwrap();
        assert!(prof.contains("(subpath \"/private/tmp\")"));
        assert!(prof.contains("(subpath \"/home/u/.codex\")"));
    }

    #[test]
    fn a_quote_in_a_path_cannot_end_the_seatbelt_string() {
        let prof = seatbelt_profile(
            &[PathBuf::from("/wt/a\") (allow file-write* (subpath \"/")],
            true,
            &[],
        )
        .unwrap();
        assert!(
            prof.contains(r#"(subpath "/wt/a\") (allow file-write* (subpath \"/")"#),
            "{prof}"
        );
    }

    #[test]
    fn a_backslash_in_a_path_is_escaped() {
        assert_eq!(sbpl_string(Path::new(r"/wt/a\b")).unwrap(), r#""/wt/a\\b""#);
    }

    #[test]
    fn seatbelt_network_off() {
        assert!(
            seatbelt_profile(&[PathBuf::from("/wt")], false, &[])
                .unwrap()
                .contains("(deny network*)")
        );
    }

    #[test]
    fn bwrap_ro_root_rw_workspace() {
        let argv = bwrap_argv(
            "c",
            &["-p".into()],
            &[PathBuf::from("/wt")],
            &[PathBuf::from("/wt/.git")],
            Path::new("/wt"),
            true,
        );
        let j = joined(&argv);
        assert!(j.contains("--ro-bind / /"));
        assert!(j.contains("--bind /wt /wt"));
        assert!(j.contains("--chdir /wt"));
        assert!(!j.contains("--unshare-net"));
        assert!(j.contains("--ro-bind-try /wt/.git /wt/.git"));
        assert!(j.find("--bind /wt /wt").unwrap() < j.find("--ro-bind-try /wt/.git").unwrap());
    }

    #[test]
    fn default_tmp_writable_roots_are_existing_dirs() {
        let roots = default_tmp_writable_roots();
        assert!(
            roots.iter().all(|p| p.is_dir()),
            "all tmp roots must exist: {roots:?}"
        );
        assert!(
            roots.iter().any(|p| p == Path::new("/tmp")),
            "expected /tmp: {roots:?}"
        );
    }

    #[test]
    fn read_only_writes_under_no_root() {
        assert_eq!(SandboxPolicy::read_only(true).jail_roots(), Some(&[][..]));
    }

    #[test]
    fn workspace_write_writes_under_its_roots() {
        let ww = SandboxPolicy::workspace_write(Path::new("/repo/wt"));
        assert_eq!(ww.jail_roots(), Some(&[PathBuf::from("/repo/wt")][..]));
    }

    #[test]
    fn danger_full_access_has_no_jail() {
        assert_eq!(SandboxPolicy::default().jail_roots(), None);
    }

    #[test]
    fn a_wrapper_given_as_a_path_is_looked_up_only_there() {
        let path = std::ffi::OsString::from("/bin:/usr/bin");
        assert_eq!(
            candidates("/usr/bin/sandbox-exec", Some(&path)),
            vec![PathBuf::from("/usr/bin/sandbox-exec")]
        );
    }

    #[test]
    fn a_bare_wrapper_name_is_looked_up_on_each_path_entry() {
        let path = std::ffi::OsString::from("/opt/bin:/usr/bin");
        assert_eq!(
            candidates("bwrap", Some(&path)),
            vec![
                PathBuf::from("/opt/bin/bwrap"),
                PathBuf::from("/usr/bin/bwrap")
            ]
        );
    }

    #[test]
    fn a_bare_wrapper_name_without_a_path_has_nowhere_to_be() {
        assert!(candidates("bwrap", None).is_empty());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_wrapper_that_is_not_installed_is_refused_at_wrap() {
        let empty = empty_dir();
        let path = std::ffi::OsString::from(&empty);
        assert!(matches!(
            installed("tars-no-such-jail", Some(&path)),
            Err(SandboxError::WrapperMissing {
                program: "tars-no-such-jail"
            })
        ));
    }

    /// Returns a directory that exists and holds no file named like a jail program.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn empty_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("tars-sandbox-empty-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_git_file_names_its_directory_after_gitdir() {
        assert_eq!(
            gitdir_line("gitdir: /repo/.git/worktrees/wt\n"),
            Some("/repo/.git/worktrees/wt")
        );
        assert_eq!(gitdir_line("gitdir:   \n"), None);
        assert_eq!(gitdir_line("[core]\n"), None);
    }

    #[test]
    fn every_protected_directory_is_denied_last() {
        let prof = seatbelt_profile(
            &[PathBuf::from("/repo")],
            true,
            &[PathBuf::from("/repo/pkg/.git"), PathBuf::from("/repo/.git")],
        )
        .unwrap();
        let allow = prof.find("(allow file-write*").unwrap();
        assert!(
            prof.find("(deny file-write* (subpath \"/repo/pkg/.git\"))")
                .unwrap()
                > allow
        );
        assert!(
            prof.find("(deny file-write* (subpath \"/repo/.git\"))")
                .unwrap()
                > allow
        );
    }

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    fn joined(argv: &[OsString]) -> String {
        argv.join(OsStr::new(" ")).into_string().unwrap()
    }

    fn paths(ps: &[&str]) -> Vec<PathBuf> {
        ps.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn every_level_of_every_anchor_has_its_git_protected_once() {
        assert_eq!(
            dot_gits(&paths(&["/repo/pkg", "/repo/sub"])),
            paths(&["/repo/pkg/.git", "/repo/.git", "/.git", "/repo/sub/.git"])
        );
    }

    #[test]
    fn an_ancestor_inside_a_writable_root_is_pinned() {
        let pinned = pinned_dirs(&paths(&["/repo/pkg/.git"]), &paths(&["/repo"]));
        assert_eq!(pinned, paths(&["/repo/pkg", "/repo"]));
    }

    #[test]
    fn an_ancestor_outside_every_writable_root_is_not_pinned() {
        let pinned = pinned_dirs(&paths(&["/repo/pkg/.git"]), &paths(&["/repo/pkg"]));
        assert_eq!(pinned, paths(&["/repo/pkg"]));
    }

    #[test]
    fn an_ancestor_inside_a_protected_directory_is_not_pinned() {
        let pinned = pinned_dirs(
            &paths(&["/repo/.git", "/repo/.git/worktrees/wt"]),
            &paths(&["/repo"]),
        );
        assert_eq!(pinned, paths(&["/repo"]));
    }

    #[test]
    fn a_pinned_ancestor_is_denied_as_a_literal_path() {
        let prof = seatbelt_profile(&paths(&["/repo"]), true, &paths(&["/repo/pkg/.git"])).unwrap();
        assert!(
            prof.contains("(deny file-write* (literal \"/repo/pkg\"))"),
            "{prof}"
        );
        assert!(!prof.contains("(literal \"/\")"), "{prof}");
    }

    #[test]
    fn a_pinned_ancestor_is_bound_onto_itself_before_the_protected_mounts() {
        let j = joined(&bwrap_argv(
            "c",
            &[],
            &paths(&["/repo"]),
            &paths(&["/repo/pkg/.git"]),
            Path::new("/repo/pkg"),
            true,
        ));
        let pin = j.find("--bind-try /repo/pkg /repo/pkg").unwrap();
        assert!(pin < j.find("--ro-bind-try /repo/pkg/.git").unwrap(), "{j}");
    }

    #[test]
    fn an_empty_git_below_the_repository_does_not_unprotect_it() {
        let (_d, repo) = canonical_tempdir();
        std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(repo.join("pkg/.git")).unwrap();
        let dirs = protected_dirs(&[repo.join("pkg")]).unwrap();
        assert!(dirs.contains(&repo.join("pkg/.git")), "{dirs:?}");
        assert!(dirs.contains(&repo.join(".git")), "{dirs:?}");
    }

    #[test]
    fn a_submodules_superproject_git_is_protected_from_the_writable_root() {
        let (_d, repo) = canonical_tempdir();
        std::fs::create_dir_all(repo.join(".git/modules/sub")).unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        let dirs = protected_dirs(&[repo.join("sub"), repo.clone()]).unwrap();
        assert!(dirs.contains(&repo.join(".git/modules/sub")), "{dirs:?}");
        assert!(dirs.contains(&repo.join(".git")), "{dirs:?}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_writable_root_protects_its_git_whatever_the_workdir() {
        let (_d, base) = canonical_tempdir();
        let wt = base.join("wt");
        std::fs::create_dir_all(wt.join(".git/hooks")).unwrap();
        let other = base.join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        let wrapped = match SandboxPolicy::workspace_write(&wt).wrap("/bin/sh", &[], &other) {
            Err(SandboxError::WrapperMissing { .. }) => return,
            other => other.unwrap(),
        };
        let deny = format!(
            "(deny file-write* (subpath \"{}\"))",
            wt.join(".git").display()
        );
        let profile = wrapped.args[1].to_str().unwrap();
        assert!(profile.contains(&deny), "{profile}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_git_protects_where_it_points() {
        let (_d, wt) = canonical_tempdir();
        std::fs::create_dir_all(wt.join("gitdir/hooks")).unwrap();
        std::os::unix::fs::symlink(wt.join("gitdir"), wt.join(".git")).unwrap();
        let dirs = protected_dirs(std::slice::from_ref(&wt)).unwrap();
        assert_eq!(dirs[..2], [wt.join(".git"), wt.join("gitdir")]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_git_to_nothing_protects_the_place_it_names() {
        let (_d, wt) = canonical_tempdir();
        std::os::unix::fs::symlink(wt.join("later"), wt.join(".git")).unwrap();
        let dirs = protected_dirs(std::slice::from_ref(&wt)).unwrap();
        assert_eq!(dirs[..2], [wt.join(".git"), wt.join("later")]);
    }

    #[test]
    fn a_linked_worktree_protects_its_git_directory_and_the_common_one() {
        let (_d, base) = canonical_tempdir();
        let common = base.join("repo/.git");
        let own = common.join("worktrees/wt");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("commondir"), "../..\n").unwrap();
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", own.display())).unwrap();
        let dirs = protected_dirs(std::slice::from_ref(&wt)).unwrap();
        assert_eq!(dirs[..3], [wt.join(".git"), own, common]);
    }

    #[test]
    fn a_relative_gitdir_is_read_from_the_files_directory() {
        let (_d, wt) = canonical_tempdir();
        std::fs::create_dir_all(wt.join("real-git")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: real-git\n").unwrap();
        let dirs = protected_dirs(std::slice::from_ref(&wt)).unwrap();
        assert_eq!(dirs[..2], [wt.join(".git"), wt.join("real-git")]);
    }

    #[test]
    fn a_gitdir_that_is_gone_protects_the_place_it_names() {
        let (_d, wt) = canonical_tempdir();
        let gone = wt.join("pruned/.git/worktrees/wt");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gone.display())).unwrap();
        let dirs = protected_dirs(std::slice::from_ref(&wt)).unwrap();
        assert_eq!(dirs[..2], [wt.join(".git"), gone]);
    }

    #[test]
    fn a_git_file_without_gitdir_protects_only_itself() {
        let (_d, wt) = canonical_tempdir();
        std::fs::write(wt.join(".git"), "not a pointer\n").unwrap();
        let dirs = protected_dirs(std::slice::from_ref(&wt)).unwrap();
        assert_eq!(dirs, dot_gits(std::slice::from_ref(&wt)));
    }

    #[test]
    fn outside_any_repository_every_git_discovery_would_try_is_protected() {
        let (_d, wt) = canonical_tempdir();
        let dirs = protected_dirs(std::slice::from_ref(&wt)).unwrap();
        assert_eq!(dirs.first(), Some(&wt.join(".git")));
        assert_eq!(dirs.last(), Some(&PathBuf::from("/.git")));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_writable_root_that_cannot_be_resolved_is_dropped_and_returned() {
        let (_d, wt) = canonical_tempdir();
        let gone = wt.join("not-made-yet");
        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![wt.clone(), gone.clone()],
            network: true,
        };
        let wrapped = match policy.wrap("/bin/sh", &[], &wt) {
            Err(SandboxError::WrapperMissing { .. }) => return,
            other => other.unwrap(),
        };
        assert_eq!(wrapped.dropped.len(), 1);
        assert_eq!(wrapped.dropped[0].root, gone);
        assert!(!joined(&wrapped.args).contains("not-made-yet"));
    }

    #[test]
    fn workspace_write_and_read_only_constructors() {
        let ww = SandboxPolicy::workspace_write(Path::new("/repo/wt"));
        assert_eq!(ww.mode(), SandboxMode::WorkspaceWrite);
        assert_eq!(ww.jail_roots(), Some(&[PathBuf::from("/repo/wt")][..]));
        let ro = SandboxPolicy::read_only(false);
        assert_eq!(ro.mode(), SandboxMode::ReadOnly);
        assert!(!ro.network());
    }

    #[test]
    fn a_gitdir_that_does_not_exist_yet_is_named_without_dotdot() {
        let (_d, root) = canonical_tempdir();
        std::fs::create_dir_all(root.join("wt")).unwrap();
        let named = real_or_named(root.join("wt/../gd/./worktrees/x")).unwrap();
        assert_eq!(named, root.join("gd/worktrees/x"));
    }

    #[test]
    fn a_dot_git_file_naming_a_gitdir_not_made_yet_protects_its_real_path() {
        let (_d, root) = canonical_tempdir();
        std::fs::create_dir_all(root.join("wt")).unwrap();
        std::fs::write(root.join("wt/.git"), "gitdir: ../gd/worktrees/x\n").unwrap();
        let dirs = protected_dirs(&[root.join("wt")]).unwrap();
        assert!(dirs.contains(&root.join("gd/worktrees/x")), "{dirs:?}");
    }

    #[test]
    fn a_bwrap_jail_runs_in_a_session_of_its_own() {
        let argv = bwrap_argv("c", &[], &paths(&["/wt"]), &[], Path::new("/wt"), true);
        assert!(argv.iter().any(|a| a == "--new-session"), "{argv:?}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_program_cannot_make_the_gitdir_a_dot_git_file_names() {
        let (_d, root) = canonical_tempdir();
        std::fs::create_dir_all(root.join("wt")).unwrap();
        let wt = root.join("wt");
        std::fs::write(wt.join(".git"), "gitdir: ../gd/worktrees/x\n").unwrap();
        let w = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![root.clone()],
            network: true,
        }
        .wrap(
            "/bin/sh",
            &[
                "-c".into(),
                "mkdir -p ../gd/worktrees/x && echo pwned > ../gd/worktrees/x/HEAD".into(),
            ],
            &wt,
        )
        .unwrap();
        std::process::Command::new(w.program)
            .args(w.args)
            .current_dir(&wt)
            .output()
            .unwrap();
        assert!(!root.join("gd/worktrees/x/HEAD").exists());
    }

    #[cfg(unix)]
    fn non_utf8(bytes: &[u8]) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(bytes))
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_reaches_bwrap_as_its_bytes() {
        let root = non_utf8(b"/wt/caf\xe9");
        let argv = bwrap_argv("c", &[], std::slice::from_ref(&root), &[], &root, true);
        let bind = argv.iter().position(|a| a == "--bind").unwrap();
        assert_eq!(argv[bind + 1], root.as_os_str());
        assert_eq!(argv[bind + 2], root.as_os_str());
        let chdir = argv.iter().position(|a| a == "--chdir").unwrap();
        assert_eq!(argv[chdir + 1], root.as_os_str());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_is_refused_by_the_seatbelt_profile() {
        let root = non_utf8(b"/wt/caf\xe9");
        match seatbelt_profile(std::slice::from_ref(&root), true, &[]) {
            Err(SandboxError::NonUtf8Path { path }) => assert_eq!(path, root),
            other => panic!("expected NonUtf8Path, got {other:?}"),
        }
    }
}
