//! Built-in provider defaults — common providers preconfigured so user
//! TOML can stay minimal.
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
        (ProviderId::new("mlx"), default_mlx()),
        (ProviderId::new("llamacpp"), default_llamacpp()),
        (ProviderId::new("vllm"), default_vllm()),
    ]
    .into_iter()
    .collect()
}

/// Default OpenAI: `OPENAI_API_KEY`, `gpt-4o`, plus the standard
/// `OpenAI-Organization` / `OpenAI-Project` env-headers (set if
/// exported, ignored otherwise).
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

/// Default MLX: `mlx_lm.server` on `localhost:8080`, no auth, a Qwen
/// 32B 4-bit MLX-converted Coder model. Override `default_model` to
/// match whatever you've actually loaded into mlx-lm.server.
pub fn default_mlx() -> ProviderConfig {
    ProviderConfig::Mlx {
        base_url: None,
        auth: Auth::None,
        default_model: "mlx-community/Qwen2.5-Coder-32B-Instruct-4bit".into(),
        extras: HttpProviderExtras::default(),
        capabilities: Default::default(),
    }
}

/// Default vLLM: `localhost:8000`, no auth, a 32B coder. vLLM ships an
/// OpenAI-compatible server, but the dedicated `Vllm` variant lets us
/// distinguish "running on a vLLM cluster" from generic openai-compat
/// in logs / capability defaults. Override `default_model` to match
/// whatever you served with `--model`.
pub fn default_vllm() -> ProviderConfig {
    ProviderConfig::Vllm {
        base_url: None,
        auth: Auth::None,
        default_model: "Qwen/Qwen2.5-Coder-32B-Instruct".into(),
        extras: HttpProviderExtras::default(),
        capabilities: Default::default(),
    }
}

/// Default llama.cpp: `llama-server` on `localhost:8080`, no auth, a
/// 7B coder GGUF. Override `default_model` to match the file you
/// loaded with `-m`.
pub fn default_llamacpp() -> ProviderConfig {
    ProviderConfig::Llamacpp {
        base_url: None,
        auth: Auth::None,
        default_model: "Qwen2.5-Coder-7B-Q5_K_M".into(),
        extras: HttpProviderExtras::default(),
        capabilities: Default::default(),
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
        for id in [
            "openai",
            "anthropic",
            "gemini",
            "claude_cli",
            "gemini_cli",
            "mlx",
            "llamacpp",
        ] {
            assert!(
                defs.contains_key(&ProviderId::new(id)),
                "missing default: {id}"
            );
        }
    }

    #[test]
    fn openai_default_includes_org_and_project_env_headers() {
        let cfg = default_openai();
        match &cfg {
            ProviderConfig::Openai { extras, .. } => {
                assert_eq!(
                    extras
                        .env_http_headers
                        .get("OpenAI-Organization")
                        .expect("OpenAI-Organization env header missing"),
                    "OPENAI_ORGANIZATION"
                );
                assert_eq!(
                    extras
                        .env_http_headers
                        .get("OpenAI-Project")
                        .expect("OpenAI-Project env header missing"),
                    "OPENAI_PROJECT"
                );
            }
            other => panic!("expected Openai variant, got: {other:?}"),
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
        let cfg = merged
            .get(&ProviderId::new("openai"))
            .expect("merged config should contain 'openai' provider");
        match cfg {
            ProviderConfig::Openai {
                base_url,
                default_model,
                ..
            } => {
                assert_eq!(base_url.as_deref(), Some("https://my.proxy/v1"));
                assert_eq!(default_model, "gpt-4o-mini");
            }
            other => panic!("expected Openai variant after override, got: {other:?}"),
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
                capabilities: Default::default(),
            },
        )]
        .into_iter()
        .collect();
        let merged = merge_builtin_with_user(user);
        // 8 built-ins (openai, anthropic, gemini, claude_cli, gemini_cli,
        // mlx, llamacpp, vllm) + 1 user-added.
        assert_eq!(merged.len(), 9);
        assert!(merged.contains_key(&ProviderId::new("local_qwen")));
    }

    #[test]
    fn mlx_default_targets_mlx_community_qwen() {
        let cfg = default_mlx();
        match &cfg {
            ProviderConfig::Mlx {
                default_model,
                base_url,
                auth,
                ..
            } => {
                assert!(default_model.starts_with("mlx-community/"));
                assert!(base_url.is_none()); // adapter falls back to localhost:8080
                assert!(matches!(auth, Auth::None));
            }
            other => panic!("expected Mlx variant, got: {other:?}"),
        }
    }

    #[test]
    fn llamacpp_default_targets_gguf_filename() {
        let cfg = default_llamacpp();
        match &cfg {
            ProviderConfig::Llamacpp { default_model, .. } => {
                // GGUF naming convention: <model>-<size>-<quant>
                assert!(default_model.contains("Q5_K_M") || default_model.contains("Q4"));
            }
            other => panic!("expected Llamacpp variant, got: {other:?}"),
        }
    }
}
