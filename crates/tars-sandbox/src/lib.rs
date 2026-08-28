//! `tars-sandbox` — the OS exec-confinement mechanism.
//!
//! **Where it lives and why.** `tars-tools` (BashTool) and `tars-provider`
//! (claude_cli subprocess) are siblings — both depend only on `tars-types`,
//! neither on the other. The confinement they both need therefore lives in a
//! crate *below* both: this one. `tars-provider` is the LLM-inference layer; the
//! sandbox is NOT an LLM concern, so it does not live there.
//!
//! **Model (codex-consistent, per the user).** A *write-jail*: read broadly,
//! write only to the workspace roots — enforced by macOS Seatbelt
//! (`sandbox-exec`) or Linux bubblewrap, with a network toggle. A deny-default
//! *read*-jail is intentionally not the default: on macOS a too-tight read
//! profile aborts the process (validated), so containment is on writes + egress,
//! matching codex's `SandboxPolicy::WorkspaceWrite`.
//!
//! **Pure mechanism.** [`SandboxPolicy::wrap`] builds `(program, argv)` — it
//! never spawns. The caller (BashTool / claude_cli) spawns. Fail-closed: if a
//! sandbox is requested but can't be built, `wrap` errors and the caller must
//! refuse to run rather than spawn unconfined.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxMode {
    /// No writes anywhere (reviewer / read-only agents, e.g. a deepseek review).
    ReadOnly,
    /// Write only under `writable_roots` (the worktree). Read broad. The safe
    /// default for a fixer/merge.
    WorkspaceWrite,
    /// Explicit escape hatch.
    #[default]
    DangerFullAccess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub writable_roots: Vec<PathBuf>,
    /// The delegate LLM CLI needs its API.
    pub network: bool,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            mode: SandboxMode::DangerFullAccess,
            writable_roots: Vec::new(),
            network: true,
        }
    }
}

/// Callers MUST fail-closed on it.
#[derive(Debug)]
pub enum SandboxError {
    /// A path could not be canonicalized (missing root, non-UTF8, …).
    Path(String),
    /// The requested confinement has no implementation on this platform.
    Unsupported(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(m) => write!(f, "sandbox path error: {m}"),
            Self::Unsupported(m) => write!(f, "sandbox unavailable: {m}"),
        }
    }
}
impl std::error::Error for SandboxError {}

impl SandboxPolicy {
    pub fn workspace_write(workdir: &Path) -> Self {
        Self {
            mode: SandboxMode::WorkspaceWrite,
            writable_roots: vec![workdir.to_path_buf()],
            network: true,
        }
    }

    pub fn read_only(network: bool) -> Self {
        Self {
            mode: SandboxMode::ReadOnly,
            writable_roots: Vec::new(),
            network,
        }
    }

    /// Fail-closed on a confining mode that has no platform impl.
    ///
    /// [`DangerFullAccess`]: SandboxMode::DangerFullAccess
    pub fn wrap(
        &self,
        program: &str,
        args: &[String],
        workdir: &Path,
    ) -> Result<(String, Vec<String>), SandboxError> {
        if self.mode == SandboxMode::DangerFullAccess {
            let mut argv = Vec::with_capacity(args.len());
            argv.extend(args.iter().cloned());
            return Ok((program.to_string(), argv));
        }

        // Canonicalize workdir + writable roots — macOS Seatbelt matches the
        // REAL path (`/tmp`→`/private/tmp`); a symlinked root would match
        // nothing (silent over-deny).
        let work = canon(workdir)?;
        let writable: Vec<PathBuf> = self
            .writable_roots
            .iter()
            .map(|p| canon(p))
            .collect::<Result<_, _>>()?;

        #[cfg(target_os = "macos")]
        {
            let profile = seatbelt_profile(&writable, self.network, &work);
            let mut argv = vec!["-p".to_string(), profile, program.to_string()];
            argv.extend(args.iter().cloned());
            Ok(("/usr/bin/sandbox-exec".to_string(), argv))
        }
        #[cfg(target_os = "linux")]
        {
            Ok((
                "bwrap".to_string(),
                bwrap_argv(program, args, &writable, &work, self.network),
            ))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (program, args, &work, &writable);
            Err(SandboxError::Unsupported(format!(
                "{} — refusing to run unconfined",
                std::env::consts::OS
            )))
        }
    }
}

fn canon(p: &Path) -> Result<PathBuf, SandboxError> {
    std::fs::canonicalize(p).map_err(|e| SandboxError::Path(format!("{}: {e}", p.display())))
}

/// A CLI delegate needs the real per-user `$TMPDIR` and `/tmp`.
/// Each entry is included only when it exists as a directory, so a caller can
/// append the result to `writable_roots` and hand it straight to
/// [`SandboxPolicy::wrap`] without a canonicalize failure.
///
/// [`get_writable_roots_with_cwd`]: https://developers.openai.com/codex/config-reference
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
    // /tmp on Unix (often a symlink to /private/tmp on macOS — `wrap` canon's it).
    let slash_tmp = PathBuf::from("/tmp");
    if slash_tmp.is_dir() {
        roots.push(slash_tmp);
    }
    roots
}

/// **`.git` write-protection** (codex + claude both do this): even though `.git`
/// lives under the writable worktree, an agent must not be able to rewrite git
/// hooks/config to gain host execution. The final `(deny file-write* (subpath
/// "<workdir>/.git"))` wins under Seatbelt's last-match-wins ordering, so `.git`
/// is read-only while the rest of the worktree is writable.
#[cfg(any(target_os = "macos", test))]
pub fn seatbelt_profile(writable: &[PathBuf], network: bool, workdir: &Path) -> String {
    let mut p = String::from("(version 1)\n(allow default)\n");
    if !network {
        p.push_str("(deny network*)\n");
    }
    p.push_str("(deny file-write*)\n(allow file-write*\n");
    for w in writable {
        p.push_str(&format!("  (subpath \"{}\")\n", w.display()));
    }
    p.push_str("  (literal \"/dev/null\") (literal \"/dev/stdout\") (literal \"/dev/stderr\")\n");
    p.push_str("  (regex #\"^/dev/tty\"))\n");
    let git = workdir.join(".git");
    p.push_str(&format!(
        "(deny file-write* (subpath \"{}\"))\n",
        git.display()
    ));
    p
}

#[cfg(any(target_os = "linux", test))]
pub fn bwrap_argv(
    program: &str,
    args: &[String],
    writable: &[PathBuf],
    workdir: &Path,
    network: bool,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--die-with-parent".into(),
        "--unshare-user".into(),
        "--unshare-pid".into(),
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--tmpfs".into(),
        "/tmp".into(),
    ];
    if !network {
        a.push("--unshare-net".into());
    }
    for w in writable {
        let s = w.display().to_string();
        a.push("--bind".into());
        a.push(s.clone());
        a.push(s);
    }
    // Re-mount `<workdir>/.git` read-only ON TOP of the writable worktree bind
    // so an agent can't rewrite git hooks/config for host execution (matches the
    // Seatbelt `.git` deny). `--ro-bind-try` is a no-op when there is no `.git`.
    let git = workdir.join(".git").display().to_string();
    a.push("--ro-bind-try".into());
    a.push(git.clone());
    a.push(git);
    a.push("--chdir".into());
    a.push(workdir.display().to_string());
    a.push("--".into());
    a.push(program.to_string());
    a.extend(args.iter().cloned());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn danger_full_access_is_passthrough() {
        let (prog, argv) = SandboxPolicy::default()
            .wrap("claude", &["-p".into()], Path::new("/"))
            .unwrap();
        assert_eq!(prog, "claude");
        assert_eq!(argv, vec!["-p"]);
    }

    #[test]
    fn seatbelt_write_jail_shape() {
        let prof = seatbelt_profile(&[PathBuf::from("/wt")], true, Path::new("/wt"));
        assert!(prof.contains("(allow default)"));
        assert!(prof.contains("(deny file-write*)"));
        assert!(prof.contains("(subpath \"/wt\")"));
        assert!(!prof.contains("(deny network*)"));
        assert!(!prof.contains("/private/tmp"));
        assert!(!prof.contains("/var/folders"));
        assert!(prof.contains("(deny file-write* (subpath \"/wt/.git\"))"));
        // The `.git` deny is AFTER the allow block so Seatbelt's last-match-wins
        // ordering makes it override the worktree allow.
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
            Path::new("/wt"),
        );
        assert!(prof.contains("(subpath \"/private/tmp\")"));
        assert!(prof.contains("(subpath \"/home/u/.codex\")"));
    }

    #[test]
    fn seatbelt_network_off() {
        assert!(
            seatbelt_profile(&[PathBuf::from("/wt")], false, Path::new("/wt"))
                .contains("(deny network*)")
        );
    }

    #[test]
    fn bwrap_ro_root_rw_workspace() {
        let argv = bwrap_argv(
            "c",
            &["-p".into()],
            &[PathBuf::from("/wt")],
            Path::new("/wt"),
            true,
        );
        let j = argv.join(" ");
        assert!(j.contains("--ro-bind / /"));
        assert!(j.contains("--bind /wt /wt"));
        assert!(j.contains("--chdir /wt"));
        assert!(!j.contains("--unshare-net"));
        // `.git` re-mounted read-only on top of the writable worktree bind, AFTER
        // the `--bind /wt /wt` so it wins.
        assert!(j.contains("--ro-bind-try /wt/.git /wt/.git"));
        assert!(j.find("--bind /wt /wt").unwrap() < j.find("--ro-bind-try /wt/.git").unwrap());
    }

    #[test]
    fn default_tmp_writable_roots_are_existing_dirs() {
        // /tmp exists on any Unix CI box; every returned root must be a real dir
        // (so `wrap`'s canonicalize can't fail on them).
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
    fn workspace_write_and_read_only_constructors() {
        let ww = SandboxPolicy::workspace_write(Path::new("/repo/wt"));
        assert_eq!(ww.mode, SandboxMode::WorkspaceWrite);
        assert_eq!(ww.writable_roots, vec![PathBuf::from("/repo/wt")]);
        let ro = SandboxPolicy::read_only(false);
        assert_eq!(ro.mode, SandboxMode::ReadOnly);
        assert!(ro.writable_roots.is_empty());
    }
}
