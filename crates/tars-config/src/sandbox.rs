//! Turns the `[sandbox]` table and a `--sandbox <mode>` flag into a
//! [`tars_sandbox::SandboxPolicy`].
//!
//! With no `[sandbox]` and no flag the policy is [`SandboxMode::ReadOnly`] with network: most
//! users never write the table, and a model call that writes files is the one they opt into.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tars_sandbox::{SandboxMode, SandboxPolicy};

/// The `[sandbox]` TOML table, one field per [`SandboxPolicy`] field.
///
/// ```toml
/// [sandbox]
/// mode = "workspace-write"      # read-only | workspace-write | danger-full-access
/// network = true                 # egress toggle
/// writable_roots = ["/repo/wt"]  # optional
/// ```
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Defaults to `workspace-write`, not `danger-full-access`: writing `[sandbox]` at all opts
    /// into confinement.
    #[serde(default)]
    pub mode: SandboxModeConfig,
    /// Permits network egress; defaults to `true` because a delegate LLM CLI must reach its API.
    #[serde(default = "default_true")]
    pub network: bool,
    /// Lists the directories `workspace-write` allows writes under; the other modes ignore it,
    /// with a warning, since a mode that writes under no root has nowhere to put them. When it is
    /// empty, `tars-llm`'s CLI-delegate spawn fills in the worktree, while the `write_file`
    /// tool allows no write at all.
    #[serde(default)]
    pub writable_roots: Vec<PathBuf>,
}

fn default_true() -> bool {
    true
}

impl SandboxConfig {
    /// Returns the policy this section describes; under `workspace-write` the roots are kept as
    /// written, even when empty.
    pub fn to_policy(&self) -> SandboxPolicy {
        policy(self.mode.into(), &self.writable_roots, self.network)
    }
}

/// Returns whether `mode` leaves out roots it was given.
fn ignores_roots(mode: SandboxMode, writable_roots: &[PathBuf]) -> bool {
    mode != SandboxMode::WorkspaceWrite && !writable_roots.is_empty()
}

/// Returns the policy a mode, its roots and the network toggle spell. Only `WorkspaceWrite`
/// writes under roots; any other mode given roots drops them and warns, since a config that
/// lists them expected them to count.
fn policy(mode: SandboxMode, writable_roots: &[PathBuf], network: bool) -> SandboxPolicy {
    if ignores_roots(mode, writable_roots) {
        tracing::warn!(
            ?mode,
            ?writable_roots,
            "[sandbox] writable_roots are ignored: only workspace-write writes under roots"
        );
    }
    match mode {
        SandboxMode::ReadOnly => SandboxPolicy::ReadOnly { network },
        SandboxMode::WorkspaceWrite => SandboxPolicy::WorkspaceWrite {
            writable_roots: writable_roots.to_vec(),
            network,
        },
        SandboxMode::DangerFullAccess => SandboxPolicy::DangerFullAccess { network },
    }
}

/// The TOML spelling of [`tars_sandbox::SandboxMode`], kebab-case so the words match codex's
/// `--sandbox` values and the `[providers.*] sandbox` field.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxModeConfig {
    ReadOnly,
    #[default]
    WorkspaceWrite,
    DangerFullAccess,
}

impl From<SandboxModeConfig> for SandboxMode {
    fn from(m: SandboxModeConfig) -> Self {
        match m {
            SandboxModeConfig::ReadOnly => SandboxMode::ReadOnly,
            SandboxModeConfig::WorkspaceWrite => SandboxMode::WorkspaceWrite,
            SandboxModeConfig::DangerFullAccess => SandboxMode::DangerFullAccess,
        }
    }
}

/// Returns the policy the `[sandbox]` table and the `--sandbox` flag give together:
///
/// | `[sandbox]` | `--sandbox` | result |
/// |-------------|-------------|--------|
/// | absent      | absent      | `ReadOnly`, `network = true` |
/// | present     | absent      | the TOML section verbatim |
/// | absent      | present     | flag `mode`, `network = true`, no writable_roots |
/// | present     | present     | flag `mode` OVERRIDES; TOML `network`/`writable_roots` kept |
///
/// The flag overrides only `mode`, the knob a user reaches for on the command line;
/// `network` and `writable_roots` come from the TOML, the roots only when the mode writes under
/// them.
pub fn resolve_policy(
    cfg: Option<&SandboxConfig>,
    flag_mode: Option<SandboxMode>,
) -> SandboxPolicy {
    match (cfg, flag_mode) {
        (None, None) => SandboxPolicy::read_only(true),
        (Some(c), None) => c.to_policy(),
        (None, Some(mode)) => policy(mode, &[], true),
        (Some(c), Some(mode)) => policy(mode, &c.writable_roots, c.network),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn a_config_without_a_sandbox_table_runs_read_only() {
        let cfg = Config::from_toml("[providers]\n").unwrap();
        assert!(cfg.sandbox().is_none());
        let pol = resolve_policy(cfg.sandbox(), None);
        assert_eq!(pol, SandboxPolicy::read_only(true));
    }

    #[test]
    fn read_only_mode_parses() {
        let cfg = Config::from_toml("[sandbox]\nmode = \"read-only\"\n").unwrap();
        let sb = cfg.sandbox().unwrap();
        assert_eq!(sb.mode, SandboxModeConfig::ReadOnly);
        assert_eq!(sb.to_policy(), SandboxPolicy::read_only(true));
    }

    #[test]
    fn workspace_write_mode_with_roots_and_network() {
        let toml = r#"
            [sandbox]
            mode = "workspace-write"
            network = false
            writable_roots = ["/repo/wt", "/tmp/scratch"]
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let pol = cfg.sandbox().unwrap().to_policy();
        assert_eq!(
            pol,
            SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![PathBuf::from("/repo/wt"), PathBuf::from("/tmp/scratch")],
                network: false,
            }
        );
    }

    #[test]
    fn danger_full_access_mode_parses() {
        let cfg = Config::from_toml("[sandbox]\nmode = \"danger-full-access\"\n").unwrap();
        assert_eq!(
            cfg.sandbox().unwrap().to_policy().mode(),
            SandboxMode::DangerFullAccess
        );
    }

    #[test]
    fn present_section_defaults_mode_to_workspace_write() {
        let cfg = Config::from_toml("[sandbox]\nnetwork = true\n").unwrap();
        assert_eq!(
            cfg.sandbox().unwrap().mode,
            SandboxModeConfig::WorkspaceWrite
        );
    }

    #[test]
    fn unknown_field_in_sandbox_is_rejected() {
        let err = Config::from_toml("[sandbox]\nmoed = \"read-only\"\n");
        assert!(
            err.is_err(),
            "typo'd key must be caught by deny_unknown_fields"
        );
    }

    #[test]
    fn flag_overrides_toml_mode_but_keeps_network() {
        let toml = r#"
            [sandbox]
            mode = "workspace-write"
            network = false
            writable_roots = ["/repo/wt"]
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let pol = resolve_policy(cfg.sandbox(), Some(SandboxMode::ReadOnly));
        assert_eq!(pol, SandboxPolicy::read_only(false));
    }

    #[test]
    fn a_workspace_write_flag_keeps_the_toml_roots() {
        let toml = r#"
            [sandbox]
            mode = "read-only"
            network = false
            writable_roots = ["/repo/wt"]
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        let pol = resolve_policy(cfg.sandbox(), Some(SandboxMode::WorkspaceWrite));
        assert_eq!(
            pol,
            SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![PathBuf::from("/repo/wt")],
                network: false,
            }
        );
    }

    #[test]
    fn flag_only_no_toml_builds_policy_from_flag() {
        let pol = resolve_policy(None, Some(SandboxMode::WorkspaceWrite));
        assert_eq!(
            pol,
            SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network: true,
            }
        );
    }

    #[test]
    fn a_mode_that_writes_under_no_root_ignores_the_roots_it_was_given() {
        let roots = [PathBuf::from("/repo/wt")];
        assert!(ignores_roots(SandboxMode::ReadOnly, &roots));
        assert!(ignores_roots(SandboxMode::DangerFullAccess, &roots));
        assert!(!ignores_roots(SandboxMode::WorkspaceWrite, &roots));
        assert!(!ignores_roots(SandboxMode::ReadOnly, &[]));
    }
}
