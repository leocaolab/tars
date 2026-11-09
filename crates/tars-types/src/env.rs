//! Reads the process-wide env-var knobs tars owns, through one typed accessor each, so a knob
//! name cannot be misspelled at a call site and this module lists them all.
//!
//! Two kinds of env reads stay out: `SecretRef::Env`, whose variable name comes from user TOML,
//! and the CLI backends' `std::env::vars()` sweep, which is the subprocess env-passthrough filter
//! and is broad on purpose.
//!
//! Each accessor reads the environment when called and keeps no snapshot, so flipping
//! `TARS_CLAUDE_CLI_STREAM` between two runs takes effect on the next call. One accessor,
//! [`cassette_force_record`], is handed the lookup instead of reading the process: its reader
//! builds over an environment of its own (`$TARS_HOME/.env` laid over the process, or a test's).

use std::path::PathBuf;

/// Returns `$HOME`; `None` when it is unset, as in some containers. A non-UTF-8 value is
/// returned as is.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Returns whether `TARS_CLAUDE_CLI_STREAM` turns on the `claude_cli` backend's stream mode:
/// any value other than `""`, `"0"`, `"false"`, `"off"` or `"no"` (trimmed, case-insensitive) is
/// true, and `Ok(None)` means the operator did not choose.
///
/// # Errors
///
/// Returns the `VarError` when the variable is set but not UTF-8, so a misconfiguration is not
/// read as unset.
pub fn claude_cli_streaming() -> Result<Option<bool>, std::env::VarError> {
    match std::env::var("TARS_CLAUDE_CLI_STREAM") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            Ok(Some(!matches!(
                v.as_str(),
                "" | "0" | "false" | "off" | "no"
            )))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Returns whether `TARS_CASSETTE_RECORD`, as `var` reads it, makes every cassette record instead
/// of replaying: `1` or `true` (any case) does; any other value, or none, does not.
///
/// # Errors
///
/// Returns the `VarError` when the variable is set but not UTF-8, so a misconfiguration is not
/// read as unset.
pub fn cassette_force_record(
    var: impl FnOnce(&str) -> Result<String, std::env::VarError>,
) -> Result<bool, std::env::VarError> {
    match var("TARS_CASSETTE_RECORD") {
        Ok(v) => Ok(v == "1" || v.eq_ignore_ascii_case("true")),
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Returns the raw `TARS_LOG_FORMAT` for tars-melt to parse, in `std::env::var`'s shape, as
/// tars-melt reads its other variables.
///
/// # Errors
///
/// `VarError::NotPresent` when it is unset; `VarError::NotUnicode` when it is set but not UTF-8,
/// which the caller reports rather than reading it as unset.
pub fn log_format_raw() -> Result<String, std::env::VarError> {
    std::env::var("TARS_LOG_FORMAT")
}

#[cfg(test)]
mod tests {
    // Setting process-wide env would race the tests running in parallel, so these test a copy
    // of the accessor's match, not the accessor.

    #[test]
    fn cassette_recording_is_forced_by_tars_cassette_record_alone() {
        let only = |name: &'static str, value: &'static str| {
            move |asked: &str| {
                if asked == name {
                    Ok(value.to_string())
                } else {
                    Err(std::env::VarError::NotPresent)
                }
            }
        };

        assert_eq!(
            super::cassette_force_record(only("TARS_CASSETTE_RECORD", "1")),
            Ok(true)
        );
        assert_eq!(
            super::cassette_force_record(only("TARS_CASSETTE_RECORD", "True")),
            Ok(true)
        );
        assert_eq!(
            super::cassette_force_record(only("TARS_CASSETTE_RECORD", "0")),
            Ok(false)
        );
        assert_eq!(
            super::cassette_force_record(only("TARS_RECORD", "1")),
            Ok(false)
        );
    }

    #[test]
    fn a_cassette_switch_that_is_not_utf8_is_unreadable_not_unset() {
        let not_utf8 = |_: &str| {
            Err(std::env::VarError::NotUnicode(std::ffi::OsString::from(
                "x",
            )))
        };
        assert!(super::cassette_force_record(not_utf8).is_err());
    }

    #[test]
    fn claude_cli_streaming_falsy_set_matches_documented_strings() {
        let parse = |v: &str| {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "" | "0" | "false" | "off" | "no")
        };
        assert!(!parse(""));
        assert!(!parse("0"));
        assert!(!parse("false"));
        assert!(!parse("FALSE"));
        assert!(!parse(" false "));
        assert!(!parse("off"));
        assert!(!parse("no"));
    }

    #[test]
    fn claude_cli_streaming_truthy_set_matches_anything_else() {
        let parse = |v: &str| {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "" | "0" | "false" | "off" | "no")
        };
        assert!(parse("1"));
        assert!(parse("true"));
        assert!(parse("TRUE"));
        assert!(parse("yes"));
        assert!(parse("on"));
        assert!(parse("yarp"));
    }
}
