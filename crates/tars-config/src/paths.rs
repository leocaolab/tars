//! Standard paths used by tars consumers.
//!
//! Returns the platform-independent location where tars looks for the
//! user-level config by default. We deliberately *don't* follow XDG /
//! Apple's Application Support conventions here, despite the `dirs`
//! crate's `config_dir()` being right there. Reasoning:
//!
//! - tars is a developer tool, not a desktop application. Developer
//!   tools by overwhelming convention live under `~/.<tool>/`: git
//!   (`~/.gitconfig`), cargo (`~/.cargo/`), npm (`~/.npmrc`),
//!   aws (`~/.aws/`), docker (`~/.docker/`), ssh (`~/.ssh/`),
//!   claude code (`~/.claude/`).
//! - `~/Library/Application Support/tars/` (macOS via `dirs`) is verbose
//!   to type and contains a space — annoying in shells/scripts.
//! - Identical layout across macOS / Linux / Windows means the same
//!   `Pipeline.from_default()` works everywhere with no per-platform
//!   branching for the config path.

use std::path::PathBuf;

/// Returns `$HOME/.tars/config.toml` (or `%USERPROFILE%\.tars\config.toml`
/// on Windows). Returns `None` only if the home directory cannot be
/// determined — this is rare enough that callers usually treat it as an
/// error path.
pub fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".tars").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_some_under_home() {
        let p = default_config_path().expect("home dir resolvable in tests");
        assert!(p.ends_with(".tars/config.toml"));
        // Sanity: the parent exists under home (it might not exist on
        // disk, but the *path* should be `<something>/.tars/config.toml`).
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("config.toml"));
        assert_eq!(
            p.parent()
                .and_then(|d| d.file_name())
                .and_then(|s| s.to_str()),
            Some(".tars")
        );
    }
}
