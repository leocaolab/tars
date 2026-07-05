//! `[sandbox]` config section (D6) → [`tars_sandbox::SandboxPolicy`].
//!
//! The user's security config flows in from two places, codex-consistent:
//! the TOML `[sandbox]` table (this module) and the `--sandbox <mode>` CLI
//! flag (tars-cli). [`resolve_policy`] combines them with the flag winning
//! on `mode`.
//!
//! **Default (backward-compatible).** The section is OPTIONAL — `Config.sandbox`
//! is `Option<SandboxConfig>`. Absent `[sandbox]` AND no `--sandbox` flag ⇒
//! [`SandboxPolicy::default`] = [`SandboxMode::DangerFullAccess`] = today's
//! unconfined behaviour. Existing configs keep working untouched; confinement
//! is strictly opt-in (guardrail: "don't abruptly confine existing users").

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tars_sandbox::{SandboxMode, SandboxPolicy};

/// The `[sandbox]` TOML table. Maps 1:1 onto [`SandboxPolicy`].
///
/// ```toml
/// [sandbox]
/// mode = "workspace-write"      # read-only | workspace-write | danger-full-access
/// network = true                 # egress toggle
/// writable_roots = ["/repo/wt"]  # optional; empty + workspace-write ⇒ runtime fills [cwd]
/// ```
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Confinement mode. Present-but-omitted defaults to `workspace-write`
    /// (the proven safe confined mode, D1): writing `[sandbox]` at all is an
    /// explicit opt-in to confinement, so the default here is confining, not
    /// `danger-full-access`. Absence of the whole section is what preserves
    /// today's unconfined behaviour (see module docs).
    #[serde(default)]
    pub mode: SandboxModeConfig,
    /// Whether network egress is permitted. Default `true` — a delegate LLM
    /// CLI needs to reach its API; matches [`SandboxPolicy::workspace_write`].
    #[serde(default = "default_true")]
    pub network: bool,
    /// Directories writes are allowed under (`workspace-write`). Empty +
    /// `workspace-write` ⇒ the runtime defaults it to `[cwd]` (the worktree)
    /// from `RequestContext.cwd` — see the ToolContext build site in
    /// `tars-runtime`. Ignored for `read-only` / `danger-full-access`.
    #[serde(default)]
    pub writable_roots: Vec<PathBuf>,
}

fn default_true() -> bool {
    true
}

impl SandboxConfig {
    /// Map this section onto the runtime [`SandboxPolicy`]. `writable_roots`
    /// is carried through as-is (possibly empty); the runtime fills `[cwd]`
    /// for `workspace-write` when it's empty.
    pub fn to_policy(&self) -> SandboxPolicy {
        SandboxPolicy {
            mode: self.mode.into(),
            writable_roots: self.writable_roots.clone(),
            network: self.network,
        }
    }
}

/// TOML-friendly mirror of [`tars_sandbox::SandboxMode`]. kebab-case on the
/// wire so the names match codex (`read-only` / `workspace-write` /
/// `danger-full-access`) and the existing `[providers.*] sandbox` field.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxModeConfig {
    /// No writes anywhere (reviewer / read-only agents).
    ReadOnly,
    /// Write only under `writable_roots` (the worktree). The safe default when
    /// `[sandbox]` is present.
    #[default]
    WorkspaceWrite,
    /// No confinement — today's behaviour, explicit escape hatch.
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

/// Resolve the effective [`SandboxPolicy`] from the two config surfaces (D6),
/// flag-over-TOML precedence:
///
/// | `[sandbox]` | `--sandbox` | result |
/// |-------------|-------------|--------|
/// | absent      | absent      | `DangerFullAccess` (unconfined — today's default) |
/// | present     | absent      | the TOML section verbatim |
/// | absent      | present     | flag `mode`, `network = true`, no writable_roots |
/// | present     | present     | flag `mode` OVERRIDES; TOML `network`/`writable_roots` kept |
///
/// The flag intentionally overrides only `mode` (the security-critical knob a
/// user reaches for on the command line); the finer `network` / `writable_roots`
/// stay TOML-driven.
pub fn resolve_policy(cfg: Option<&SandboxConfig>, flag_mode: Option<SandboxMode>) -> SandboxPolicy {
    match (cfg, flag_mode) {
        (None, None) => SandboxPolicy::default(),
        (Some(c), None) => c.to_policy(),
        (None, Some(mode)) => SandboxPolicy { mode, writable_roots: Vec::new(), network: true },
        (Some(c), Some(mode)) => SandboxPolicy { mode, ..c.to_policy() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConfigManager;

    #[test]
    fn absent_section_means_none_on_config() {
        let cfg = ConfigManager::load_from_str("[providers]\n").unwrap();
        assert!(cfg.sandbox.is_none(), "no [sandbox] ⇒ None ⇒ unconfined default");
        // And the resolver turns None + no flag into today's behaviour.
        let pol = resolve_policy(cfg.sandbox.as_ref(), None);
        assert_eq!(pol.mode, SandboxMode::DangerFullAccess);
    }

    #[test]
    fn read_only_mode_parses() {
        let cfg = ConfigManager::load_from_str("[sandbox]\nmode = \"read-only\"\n").unwrap();
        let sb = cfg.sandbox.as_ref().unwrap();
        assert_eq!(sb.mode, SandboxModeConfig::ReadOnly);
        let pol = sb.to_policy();
        assert_eq!(pol.mode, SandboxMode::ReadOnly);
        assert!(pol.writable_roots.is_empty());
    }

    #[test]
    fn workspace_write_mode_with_roots_and_network() {
        let toml = r#"
            [sandbox]
            mode = "workspace-write"
            network = false
            writable_roots = ["/repo/wt", "/tmp/scratch"]
        "#;
        let cfg = ConfigManager::load_from_str(toml).unwrap();
        let pol = cfg.sandbox.as_ref().unwrap().to_policy();
        assert_eq!(pol.mode, SandboxMode::WorkspaceWrite);
        assert!(!pol.network);
        assert_eq!(pol.writable_roots.len(), 2);
        assert!(pol.writable_roots.contains(&PathBuf::from("/repo/wt")));
    }

    #[test]
    fn danger_full_access_mode_parses() {
        let cfg =
            ConfigManager::load_from_str("[sandbox]\nmode = \"danger-full-access\"\n").unwrap();
        assert_eq!(cfg.sandbox.unwrap().to_policy().mode, SandboxMode::DangerFullAccess);
    }

    #[test]
    fn present_section_defaults_mode_to_workspace_write() {
        // `[sandbox]` present but no `mode` ⇒ opt-in to confinement ⇒
        // workspace-write, NOT danger-full-access.
        let cfg = ConfigManager::load_from_str("[sandbox]\nnetwork = true\n").unwrap();
        assert_eq!(cfg.sandbox.unwrap().mode, SandboxModeConfig::WorkspaceWrite);
    }

    #[test]
    fn unknown_field_in_sandbox_is_rejected() {
        let err = ConfigManager::load_from_str("[sandbox]\nmoed = \"read-only\"\n");
        assert!(err.is_err(), "typo'd key must be caught by deny_unknown_fields");
    }

    #[test]
    fn flag_overrides_toml_mode_but_keeps_network_and_roots() {
        let toml = r#"
            [sandbox]
            mode = "workspace-write"
            network = false
            writable_roots = ["/repo/wt"]
        "#;
        let cfg = ConfigManager::load_from_str(toml).unwrap();
        // Flag says read-only; TOML said workspace-write.
        let pol = resolve_policy(cfg.sandbox.as_ref(), Some(SandboxMode::ReadOnly));
        assert_eq!(pol.mode, SandboxMode::ReadOnly, "flag mode wins");
        assert!(!pol.network, "TOML network kept");
        assert_eq!(pol.writable_roots, vec![PathBuf::from("/repo/wt")], "TOML roots kept");
    }

    #[test]
    fn flag_only_no_toml_builds_policy_from_flag() {
        let pol = resolve_policy(None, Some(SandboxMode::WorkspaceWrite));
        assert_eq!(pol.mode, SandboxMode::WorkspaceWrite);
        assert!(pol.network, "flag-only defaults network on");
        assert!(pol.writable_roots.is_empty());
    }
}
