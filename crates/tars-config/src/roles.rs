//! Binds each role name application code uses ("critic", "fixer") to the provider and model that
//! serve it.
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
//! A role is looked up by name and a miss is an error. There is no fallback to a tier, a
//! provider id or "the only provider": a role the operator did not configure says so instead of
//! reaching some other model.

use serde::{Deserialize, Serialize};

use tars_types::ProviderId;

/// One `[roles.<name>]` entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleConfig {
    pub provider: ProviderId,
    /// Is required rather than falling back to the provider's `default_model`, so two roles on
    /// one provider can call different models.
    pub model: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn two_roles_on_one_provider_can_bind_different_models() {
        let toml_str = r#"
            [providers.deepseek]
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "http://localhost:8000/v1"
            default_model = "deepseek-chat"

            [roles.critic]
            provider = "deepseek"
            model    = "deepseek-chat"

            [roles.critic_l5]
            provider = "deepseek"
            model    = "deepseek-reasoner"
        "#;
        let cfg = Config::from_toml(toml_str).expect("[roles.<name>] must load");
        assert_eq!(cfg.roles()["critic"].model, "deepseek-chat");
        assert_eq!(cfg.roles()["critic_l5"].model, "deepseek-reasoner");
        assert_eq!(cfg.roles()["critic"].provider, ProviderId::new("deepseek"));
    }

    #[test]
    fn role_missing_model_is_a_parse_error_not_a_fallback() {
        let toml_str = r#"
            [providers.deepseek]
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "http://localhost:8000/v1"
            default_model = "deepseek-chat"

            [roles.critic]
            provider = "deepseek"
        "#;
        let err = Config::from_toml(toml_str)
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
            Config::from_toml(toml_str).is_err(),
            "deny_unknown_fields must catch a misspelled role key"
        );
    }
}
