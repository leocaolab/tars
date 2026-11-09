//! Locates the tars home and the config file in it.
//!
//! The default home is `~/.tars/` on every platform, not XDG or `~/Library/Application Support`:
//! tars is a developer tool, and those live under `~/.<tool>/` (git, cargo, aws, ssh); the macOS
//! path also has a space in it, which is awkward in shells.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// Returns the tars home: `flag`, else `$TARS_HOME`, else `~/.tars`; `None` when none of them
/// resolves. An empty or blank `flag` or `$TARS_HOME` counts as not given, so `TARS_HOME=` or a
/// binding's `home=""` does not make the current directory the home.
pub fn resolve_home(flag: Option<PathBuf>) -> Option<PathBuf> {
    home_from(flag, std::env::var_os("TARS_HOME"), dirs::home_dir())
}

/// Returns `config.toml` in the home [`resolve_home`] finds without a flag, or `None` when no home
/// resolves. It does not check that the file exists.
pub fn default_config_path() -> Option<PathBuf> {
    resolve_home(None).map(|home| home.join("config.toml"))
}

fn home_from(
    flag: Option<PathBuf>,
    tars_home: Option<OsString>,
    user_home: Option<PathBuf>,
) -> Option<PathBuf> {
    flag.map(PathBuf::into_os_string)
        .filter(|v| !is_blank(v))
        .or_else(|| tars_home.filter(|v| !is_blank(v)))
        .map(PathBuf::from)
        .or_else(|| user_home.map(|h| h.join(".tars")))
}

/// Returns whether `s` is empty or only whitespace; a value that is not Unicode is not blank.
fn is_blank(s: &OsStr) -> bool {
    s.to_str().is_some_and(|s| s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_beats_tars_home_which_beats_the_user_home() {
        let flag = Some(PathBuf::from("/flag"));
        let env = || Some(OsString::from("/env"));
        let user = || Some(PathBuf::from("/u"));
        assert_eq!(home_from(flag, env(), user()), Some(PathBuf::from("/flag")));
        assert_eq!(home_from(None, env(), user()), Some(PathBuf::from("/env")));
        assert_eq!(
            home_from(None, None, user()),
            Some(PathBuf::from("/u/.tars"))
        );
        assert_eq!(home_from(None, None, None), None);
    }

    #[test]
    fn a_blank_flag_or_tars_home_counts_as_not_given() {
        let user = || Some(PathBuf::from("/u"));
        for blank in ["", " ", "\t"] {
            assert_eq!(
                home_from(
                    Some(PathBuf::from(blank)),
                    Some(OsString::from("/env")),
                    user()
                ),
                Some(PathBuf::from("/env")),
                "flag {blank:?}"
            );
            assert_eq!(
                home_from(None, Some(OsString::from(blank)), user()),
                Some(PathBuf::from("/u/.tars")),
                "TARS_HOME {blank:?}"
            );
        }
        assert_eq!(
            home_from(Some(PathBuf::from(" h")), None, user()),
            Some(PathBuf::from(" h"))
        );
    }

    #[test]
    fn an_empty_tars_home_is_unset_and_a_one_character_one_is_a_home() {
        let user = || Some(PathBuf::from("/u"));
        assert_eq!(
            home_from(None, Some(OsString::new()), user()),
            Some(PathBuf::from("/u/.tars"))
        );
        assert_eq!(
            home_from(None, Some(OsString::from("h")), user()),
            Some(PathBuf::from("h"))
        );
    }
}
