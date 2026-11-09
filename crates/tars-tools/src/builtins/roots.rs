//! Holds the roots a jail admits paths under. A root that cannot be canonicalized (a dependency
//! cache that was cleared, a scratch directory not made yet) admits nothing: it is dropped from
//! the roots that admit and kept aside, so a refusal can name it, which is the only way anyone
//! learns of it. With no root left, nothing is admitted. It does not know whether the roots bound
//! reads or writes.

use std::fmt;
use std::path::{Path, PathBuf};

use tars_sandbox::ResolveRootError;

/// The roots one call admits paths under; every one in `canonical` is canonical.
pub(super) struct RootSet {
    canonical: Vec<PathBuf>,
    dropped: Vec<ResolveRootError>,
}

/// Returns whether `canonical` sits under one of `roots`; no roots admit nothing.
fn under_any(canonical: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| canonical.starts_with(r))
}

impl FromIterator<Result<PathBuf, ResolveRootError>> for RootSet {
    fn from_iter<I: IntoIterator<Item = Result<PathBuf, ResolveRootError>>>(roots: I) -> Self {
        let (mut canonical, mut dropped) = (Vec::new(), Vec::new());
        for r in roots {
            match r {
                Ok(p) => canonical.push(p),
                Err(u) => dropped.push(u),
            }
        }
        Self { canonical, dropped }
    }
}

impl RootSet {
    /// Returns whether the canonical `path` sits under one of the roots that resolved.
    pub(super) fn admits(&self, path: &Path) -> bool {
        under_any(path, &self.canonical)
    }

    /// Returns the roots that resolved.
    pub(super) fn canonical(&self) -> &[PathBuf] {
        &self.canonical
    }

    /// Returns whether the set was given no root at all, resolved or not.
    pub(super) fn is_unbounded(&self) -> bool {
        self.canonical.is_empty() && self.dropped.is_empty()
    }
}

/// Lists the roots that resolved, or `none`, then names each dropped root and why.
impl fmt::Display for RootSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.canonical.is_empty() {
            f.write_str("none")?;
        } else {
            let shown: Vec<String> = self
                .canonical
                .iter()
                .map(|r| format!("`{}`", r.display()))
                .collect();
            f.write_str(&shown.join(", "))?;
        }
        for u in &self.dropped {
            write!(
                f,
                "; root `{}` cannot be resolved and was dropped: {}",
                u.root.display(),
                u.source
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_sandbox::canonical_root;

    #[test]
    fn a_path_under_a_root_is_admitted() {
        assert!(under_any(Path::new("/wt/src"), &[PathBuf::from("/wt")]));
    }

    #[test]
    fn a_path_outside_every_root_is_not() {
        let roots = [PathBuf::from("/wt"), PathBuf::from("/deps")];
        assert!(!under_any(Path::new("/etc"), &roots));
    }

    #[test]
    fn no_roots_admit_nothing() {
        assert!(!under_any(Path::new("/wt/a.rs"), &[]));
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_under_the_root() {
        assert!(!under_any(Path::new("/wt2/a.rs"), &[PathBuf::from("/wt")]));
    }

    #[test]
    fn an_unresolvable_root_is_dropped_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("not-made-yet");
        let set: RootSet = [canonical_root(dir.path()), canonical_root(&gone)]
            .into_iter()
            .collect();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        assert!(set.admits(&canonical.join("a")));
        let text = set.to_string();
        assert!(text.contains(&gone.display().to_string()), "{text}");
        assert!(text.contains("cannot be resolved"), "{text}");
    }
}
