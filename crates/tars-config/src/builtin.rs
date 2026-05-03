//! Built-in provider defaults — common providers preconfigured so user
//! TOML can stay minimal. Pattern adapted from
//! `codex-rs/model-provider-info::built_in_model_providers`.
//!
//! User config **extends** the built-in set rather than replacing it.
//! See [`merge_builtin_with_user`] for the merge semantics: by default
//! a user-declared provider overrides a built-in of the same id. (We
//! deliberately don't lock built-ins; each provider has only one
//! sensible default config, and users may need to adjust e.g. `base_url`
//! for proxied environments.)
//!
//! This is **not** a model catalog — only "how to talk to the provider"
//! defaults. Model picking belongs to the routing layer (Doc 02 §4.6).

use std::collections::HashMap;

use tars_types::{Auth, HttpProviderExtras, ProviderId};

use crate::providers::ProviderConfig;

/// Returns provider configs for the well-known LLM backends. Each
/// entry uses an env-var auth reference so users only need to export
/// the appropriate env var (no inline secrets in defaults).
///
/// `default_model` is set to the most useful model in each family at
/// time of writing. Users override per-provider in their config.
pub fn built_in_provider_defaults() -> HashMap<ProviderId, ProviderConfig> {
    [
        (ProviderId::new("openai"), default_openai()),
        (ProviderId::new("anthropic"), default_anthropic()),
        (ProviderId::new("gemini"), default_gemini()),
        (ProviderId::new("claude_cli"), default_claude_cli()),
        (ProviderId::new("gemini_cli"), default_gemini_cli()),
    ]
    .into_iter()
    .collect()
}

/// Default OpenAI: `OPENAI_API_KEY`, `gpt-4o`, plus the standard
/// `OpenAI-Organization` / `OpenAI-Project` env-headers (mirrors
/// codex-rs's defaults — they're set if exported, ignored otherwise).
pub fn default_openai() -> ProviderConfig {
    ProviderConfig::Openai {
        base_url: None,
        auth: Auth::env("OPENAI_API_KEY"),
        default_model: "gpt-4o".into(),
        extras: HttpProviderExtras {
            env_http_headers: [
                ("OpenAI-Organization".into(), "OPENAI_ORGANIZATION".into()),
                ("OpenAI-Project".into(), "OPENAI_PROJECT".into()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        },
    }
}

/// Default Anthropic: `ANTHROPIC_API_KEY`, claude-opus-4-7, default
/// API version. `anthropic-version` header is supplied by the adapter.
pub fn default_anthropic() -> ProviderConfig {
    ProviderConfig::Anthropic {
        base_url: None,
        api_version: None,
        auth: Auth::env("ANTHROPIC_API_KEY"),
        default_model: "claude-opus-4-7".into(),
        extras: HttpProviderExtras::default(),
    }
}

/// Default Gemini: `GEMINI_API_KEY`, gemini-2.5-pro.
pub fn default_gemini() -> ProviderConfig {
    ProviderConfig::Gemini {
        base_url: None,
        auth: Auth::env("GEMINI_API_KEY"),
        default_model: "gemini-2.5-pro".into(),
        extras: HttpProviderExtras::default(),
    }
}

/// Default Claude Code CLI: `claude` binary, 5-min timeout, opus model.
pub fn default_claude_cli() -> ProviderConfig {
    ProviderConfig::ClaudeCli {
        executable: "claude".into(),
        timeout_secs: 300,
        default_model: "claude-opus-4-7".into(),
    }
}

/// Default Gemini CLI: `gemini` binary, 5-min timeout, flash model
/// (cheaper than pro for the typical CLI use case).
pub fn default_gemini_cli() -> ProviderConfig {
    ProviderConfig::GeminiCli {
        executable: "gemini".into(),
        timeout_secs: 300,
        default_model: "gemini-2.5-flash".into(),
    }
}

/// Merge user-declared providers on top of built-in defaults.
///
/// Semantics:
/// - Built-ins start as base
/// - User entry with a known id **overrides** the built-in entirely
///   (we don't try to deep-merge — too magical, surprising in production)
/// - User entries with new ids extend the set
///
/// To exclude a built-in from the effective config, the user can set
/// e.g. `[providers.gemini_cli]` with no body — but that violates serde,
/// so the practical pattern is to use distinct ids for everything you
/// configure (e.g. `openai_main` instead of `openai`) and ignore the
/// built-ins you don't need.
pub fn merge_builtin_with_user(
    user: HashMap<ProviderId, ProviderConfig>,
) -> HashMap<ProviderId, ProviderConfig> {
    let mut merged = built_in_provider_defaults();
    for (id, cfg) in user {
        merged.insert(id, cfg);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_set_includes_all_well_known_providers() {
        let defs = built_in_provider_defaults();
        for id in ["openai", "anthropic", "gemini", "claude_cli", "gemini_cli"] {
            assert!(
                defs.contains_key(&ProviderId::new(id)),
                "missing default: {id}"
            );
        }
    }

    #[test]
    fn openai_default_includes_org_and_project_env_headers() {
        let cfg = default_openai();
        match cfg {
            ProviderConfig::Openai { extras, .. } => {
                assert_eq!(
                    extras.env_http_headers.get("OpenAI-Organization").unwrap(),
                    "OPENAI_ORGANIZATION"
                );
                assert_eq!(
                    extras.env_http_headers.get("OpenAI-Project").unwrap(),
                    "OPENAI_PROJECT"
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn user_overrides_builtin_with_same_id() {
        let user = [(
            ProviderId::new("openai"),
            ProviderConfig::Openai {
                base_url: Some("https://my.proxy/v1".into()),
                auth: Auth::env("MY_OPENAI_KEY"),
                default_model: "gpt-4o-mini".into(),
                extras: HttpProviderExtras::default(),
            },
        )]
        .into_iter()
        .collect();
        let merged = merge_builtin_with_user(user);
        // Now `openai` is the user version, not the default.
        match merged.get(&ProviderId::new("openai")).unwrap() {
            ProviderConfig::Openai { base_url, default_model, .. } => {
                assert_eq!(base_url.as_deref(), Some("https://my.proxy/v1"));
                assert_eq!(default_model, "gpt-4o-mini");
            }
            _ => panic!(),
        }
        // Other built-ins unchanged.
        assert!(merged.contains_key(&ProviderId::new("anthropic")));
    }

    #[test]
    fn user_can_add_new_provider_alongside_builtins() {
        let user = [(
            ProviderId::new("local_qwen"),
            ProviderConfig::OpenaiCompat {
                base_url: "http://localhost:8000/v1".into(),
                auth: Auth::None,
                default_model: "Qwen/Qwen2.5-Coder-32B-Instruct".into(),
                extras: HttpProviderExtras::default(),
            },
        )]
        .into_iter()
        .collect();
        let merged = merge_builtin_with_user(user);
        // 5 built-ins + 1 new
        assert_eq!(merged.len(), 6);
        assert!(merged.contains_key(&ProviderId::new("local_qwen")));
    }
}
