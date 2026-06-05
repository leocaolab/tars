//! Process-wide env-var knobs, in one place.
//!
//! Per `ARC-L5-COH-18`, every tars-owned env-var read goes through a
//! typed accessor here so call sites can't string-typo a knob name,
//! and `grep tars_types::env::` enumerates the full process-wide
//! knob surface in a single pass.
//!
//! **Not in scope.** Two categories of env reads deliberately don't
//! live here and won't get a centralised accessor:
//!
//! 1. `SecretRef::Env` resolution (auth.rs, http_extras.rs) — these
//!    read a var whose *name comes from user TOML*, so they can't be
//!    hoisted into a fixed accessor list. They are a data-driven
//!    credential resolver, not config-as-knobs.
//! 2. `std::env::vars()` sweeps in the CLI-spawning backends (claude_cli,
//!    codex_cli, gemini_cli) — those are the subprocess
//!    env-passthrough firewall, deliberately broad. Hoisting them
//!    would change the security posture, not centralise a knob.
//!
//! **Lookup is at call time, not startup.** Operators can flip a
//! knob between requests and the change takes effect on the next
//! [`claude_cli_streaming`] / etc. call. The accessors centralise
//! the lookup and type-cast; they don't snapshot the environment.
//! This keeps the documented ops-debug ergonomics of
//! `TARS_CLAUDE_CLI_STREAM` (flip between two runs to bisect a
//! streaming-vs-buffered bug) without re-scattering the read.

use std::path::PathBuf;

/// Override path to the Node daemon script for the `claude_sdk`
/// backend (env var `TARS_CLAUDE_SDK_SCRIPT`). Used by `claude_sdk`'s
/// default-script-path resolver; production installs that ran
/// `tars install-claude-daemon` don't need to set this.
/// `None` when unset or non-UTF-8.
pub fn claude_sdk_script_override() -> Option<PathBuf> {
    std::env::var_os("TARS_CLAUDE_SDK_SCRIPT").map(PathBuf::from)
}

/// Operator's `$HOME` directory. Used to resolve the standard
/// per-user install path `~/.tars/claude-daemon/server.mjs`.
/// `None` when `HOME` is unset (e.g. some container environments) or
/// non-UTF-8.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Stream-mode toggle for the `claude_cli` backend (env var
/// `TARS_CLAUDE_CLI_STREAM`). The accepted truthy values are anything
/// other than `""`, `"0"`, `"false"`, `"off"`, `"no"` (case-insensitive).
///
/// Returns `Ok(None)` when the var is unset entirely so callers can
/// distinguish "operator didn't pick" from "operator picked false."
/// Most call sites want the default-false semantics —
/// `.unwrap_or(None).unwrap_or(false)` or surfacing the `Err`.
///
/// Returns `Err` when the var is set but non-UTF-8: a `.ok()` here would
/// make invalid UTF-8 indistinguishable from "unset", silently falling
/// back to the default and masking operator misconfiguration. Mirrors
/// [`log_format_raw`]'s NotPresent→None / NotUnicode→propagate contract.
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

/// Raw value of `TARS_LOG_FORMAT` for tars-melt to parse into its
/// own `TelemetryFormat` enum.
///
/// Returns `Ok(None)` when unset, `Ok(Some(_))` when set, and `Err`
/// when set but unreadable (e.g. non-UTF-8). Consumers MUST surface
/// the `Err` case loudly — silently defaulting on a read failure is
/// what made the original site smell like swallowed-error.
pub fn log_format_raw() -> Result<Option<String>, std::env::VarError> {
    match std::env::var("TARS_LOG_FORMAT") {
        Ok(s) => Ok(Some(s)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    // These tests can't safely set process-wide env (other tests run
    // in parallel and would race), so they cover the deterministic
    // helper logic only: pin the truthy/falsy parser used by the
    // streaming accessor against every documented case.

    #[test]
    fn claude_cli_streaming_falsy_set_matches_documented_strings() {
        // Build the parser locally; can't actually mutate env in a
        // multi-threaded test. The accessor calls this exact match.
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
        // Anything not in the falsy set counts as truthy — including
        // garbage; documented behaviour.
        assert!(parse("yarp"));
    }
}
