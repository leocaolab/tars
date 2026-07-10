//! `[roles]` — the business-facing name → (provider, model) binding.
//!
//! A role is what application code names ("critic", "fixer"); the config says
//! which provider serves it and with which model. Both are required: a role
//! that names only a provider cannot be distinguished from another role on the
//! same provider, which is the whole point of having roles.
//!
//! ```toml
//! [roles.critic]
//! provider = "deepseek"
//! model    = "deepseek-chat"
//!
//! [roles.fixer]
//! provider = "claude_cli"
//! model    = "claude-opus-4-8"
//! ```
//!
//! Resolution is a single lookup — `cfg.roles.get(name)` — and a miss is an
//! error, not a cue to guess. There is deliberately no fallback to a tier, to
//! a literal provider id, or to "the only provider": a role the operator did
//! not configure must say so, not silently resolve to some other model.

use serde::{Deserialize, Serialize};

use tars_types::ProviderId;

/// One `[roles.<name>]` entry: which provider serves this role, and the exact
/// model it is bound to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleConfig {
    /// A provider id declared in `[providers.*]` (checked by
    /// [`Config::validate`](crate::Config::validate)).
    pub provider: ProviderId,
    /// The concrete model this role calls. Required — the provider's
    /// `default_model` is the CLI's convenience, not a role's fallback: two
    /// roles on one provider must be able to differ.
    pub model: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::ConfigManager;

    #[test]
    fn two_roles_on_one_provider_can_bind_different_models() {
        // The shape the old `name = "provider_id"` map could not express: the
        // model was pinned per-provider, so `critic` and `critic_l5` were
        // forced onto the same model.
        let toml_str = r#"
            [providers.deepseek]
            type = "openai_compat"
            base_url = "http://localhost:8000/v1"
            default_model = "deepseek-chat"

            [roles.critic]
            provider = "deepseek"
            model    = "deepseek-chat"

            [roles.critic_l5]
            provider = "deepseek"
            model    = "deepseek-reasoner"
        "#;
        let cfg = ConfigManager::load_from_str(toml_str).expect("[roles.<name>] must load");
        assert_eq!(cfg.roles["critic"].model, "deepseek-chat");
        assert_eq!(cfg.roles["critic_l5"].model, "deepseek-reasoner");
        assert_eq!(cfg.roles["critic"].provider, ProviderId::new("deepseek"));
    }

    #[test]
    fn role_missing_model_is_a_parse_error_not_a_fallback() {
        let toml_str = r#"
            [providers.deepseek]
            type = "openai_compat"
            base_url = "http://localhost:8000/v1"
            default_model = "deepseek-chat"

            [roles.critic]
            provider = "deepseek"
        "#;
        let err = ConfigManager::load_from_str(toml_str)
            .expect_err("a role without `model` must not fall back to default_model");
        let msg = err.to_string();
        assert!(msg.contains("parse"), "expected a parse error, got: {msg}");
    }

    #[test]
    fn typo_in_role_field_is_caught() {
        let toml_str = r#"
            [roles.critic]
            provider = "deepseek"
            modle    = "deepseek-chat"
        "#;
        assert!(
            ConfigManager::load_from_str(toml_str).is_err(),
            "deny_unknown_fields must catch a misspelled role key"
        );
    }
}
