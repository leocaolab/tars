//! Preconfigures the common providers so a user's TOML can stay small.
//!
//! The user's providers extend this set, and a user entry replaces the built-in of the same id
//! whole (see [`merge_builtin_with_user`]). Built-ins are not locked, because a user may need to
//! change one, such as its `base_url` behind a proxy. They say how to reach each provider; which
//! models exist is the model knowledge base's.

use std::collections::HashMap;

use tars_types::{Auth, HttpOverrides, ProviderId};

use crate::model_kb::MODEL_KB;
use crate::providers::ProviderConfig;

/// Returns the default model spec `data/provider.toml` gives a hosted provider, possibly a
/// `<series>@latest` spec that the provider registry resolves when it binds.
///
/// # Panics
///
/// Panics when the provider has no default in `data/provider.toml`, an authoring bug, rather
/// than shipping an empty `default_model`.
fn kb_default(provider: &str) -> String {
    MODEL_KB
        .default_model(provider)
        .unwrap_or_else(|| {
            panic!(
                "built-in provider `{provider}` has no default in data/provider.toml \
                 (defaults are DATA — add a `[providers.{provider}] default = ...`)"
            )
        })
        .to_string()
}

/// Returns the built-in provider configs. Keys come from environment variables, and an API
/// provider's `default_model` comes from `data/provider.toml`, so changing a default is a data
/// edit.
pub fn built_in_provider_defaults() -> HashMap<ProviderId, ProviderConfig> {
    [
        (ProviderId::new("openai"), default_openai()),
        (ProviderId::new("anthropic"), default_anthropic()),
        (ProviderId::new("gemini"), default_gemini()),
        (ProviderId::new("deepseek"), default_deepseek()),
        (ProviderId::new("claude_cli"), default_claude_cli()),
        (ProviderId::new("mlx"), default_mlx()),
        (ProviderId::new("llamacpp"), default_llamacpp()),
        (ProviderId::new("vllm"), default_vllm()),
    ]
    .into_iter()
    .collect()
}

/// Returns the ids of the built-in providers, sorted: the ones every config has without
/// declaring them.
pub fn built_in_provider_ids() -> Vec<ProviderId> {
    let mut ids: Vec<ProviderId> = built_in_provider_defaults().into_keys().collect();
    ids.sort();
    ids
}

/// Returns OpenAI keyed by `OPENAI_API_KEY`, sending the `OpenAI-Organization` and
/// `OpenAI-Project` headers when `OPENAI_ORGANIZATION` and `OPENAI_PROJECT` are set.
pub fn default_openai() -> ProviderConfig {
    ProviderConfig::Openai {
        base_url: None,
        auth: Auth::env("OPENAI_API_KEY"),
        default_model: kb_default("openai"),
        extras: HttpOverrides {
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

/// Returns Anthropic keyed by `ANTHROPIC_API_KEY`; the adapter supplies the `anthropic-version`
/// header.
pub fn default_anthropic() -> ProviderConfig {
    ProviderConfig::Anthropic {
        base_url: None,
        api_version: None,
        auth: Auth::env("ANTHROPIC_API_KEY"),
        default_model: kb_default("anthropic"),
        extras: HttpOverrides::default(),
    }
}

/// Returns Gemini keyed by `GEMINI_API_KEY`.
pub fn default_gemini() -> ProviderConfig {
    ProviderConfig::Gemini {
        base_url: None,
        auth: Auth::env("GEMINI_API_KEY"),
        default_model: kb_default("gemini"),
        extras: HttpOverrides::default(),
    }
}

/// Returns DeepSeek keyed by `DEEPSEEK_API_KEY`, as `openai_compat` on `api.deepseek.com`: its
/// API is OpenAI-compatible, and the OpenAI adapter turns its `delta.reasoning_content` into
/// [`tars_types::ChatEvent::ThinkingDelta`].
pub fn default_deepseek() -> ProviderConfig {
    ProviderConfig::OpenaiCompat {
        base_url: "https://api.deepseek.com".into(),
        auth: Auth::env("DEEPSEEK_API_KEY"),
        default_model: kb_default("deepseek"),
        gen_ai_provider_name: "deepseek".into(),
        extras: HttpOverrides::default(),
        capabilities: Default::default(),
    }
}

/// Returns the Claude Code CLI: the `claude` binary with a 300 s timeout.
///
/// `tools = disabled` turns off the CLI's own agent loop, which otherwise runs for minutes on a
/// single call. `bare = false` keeps the OAuth and keychain login of `claude login`, which
/// `--bare` replaces with `ANTHROPIC_API_KEY` alone. `exclude_dynamic_sections = true` drops the
/// per-machine sections of the system prompt so the prompt cache is shared across tenants.
/// `docs/providers/claude-cli.md` has the measurements.
pub fn default_claude_cli() -> ProviderConfig {
    ProviderConfig::ClaudeCli {
        executable: "claude".into(),
        timeout_secs: 300,
        default_model: kb_default("claude_cli"),
        tools: crate::providers::ClaudeCliToolsConfig::default(),
        bare: false,
        effort: None,
        exclude_dynamic_sections: true,
        extra_args: Vec::new(),
    }
}

/// Returns `mlx_lm.server` on the adapter's default address with no auth; the user overrides
/// `default_model` with the model actually loaded.
pub fn default_mlx() -> ProviderConfig {
    ProviderConfig::Mlx {
        base_url: None,
        auth: Auth::None,
        default_model: "mlx-community/Qwen2.5-Coder-32B-Instruct-4bit".into(),
        extras: HttpOverrides::default(),
        capabilities: Default::default(),
    }
}

/// Returns vLLM on the adapter's default address with no auth; the user overrides
/// `default_model` with the model served by `--model`. Its API is OpenAI-compatible, and the
/// separate variant tells vLLM apart from other compatible servers in logs and capability
/// defaults.
pub fn default_vllm() -> ProviderConfig {
    ProviderConfig::Vllm {
        base_url: None,
        auth: Auth::None,
        default_model: "Qwen/Qwen2.5-Coder-32B-Instruct".into(),
        extras: HttpOverrides::default(),
        capabilities: Default::default(),
    }
}

/// Returns `llama-server` on the adapter's default address with no auth; the user overrides
/// `default_model` with the file loaded by `-m`.
pub fn default_llamacpp() -> ProviderConfig {
    ProviderConfig::Llamacpp {
        base_url: None,
        auth: Auth::None,
        default_model: "Qwen2.5-Coder-7B-Q5_K_M".into(),
        extras: HttpOverrides::default(),
        capabilities: Default::default(),
    }
}

/// Returns the built-ins with the user's providers laid over them. A user entry replaces the
/// built-in of the same id whole, without a field-by-field merge. A built-in cannot be removed;
/// a user who does not want one gives their own providers other ids (`openai_main`).
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

    /// The provider id is the `data/provider.toml` key, so deepseek's `OpenaiCompat` is checked
    /// against the "deepseek" block.
    #[test]
    fn toml_interface_matches_config_interface_for_builtins() {
        let db = &*MODEL_KB;
        for (id, cfg) in built_in_provider_defaults() {
            if let Some(def) = db.providers.get(id.as_str()) {
                assert_eq!(
                    def.interface,
                    cfg.kind().interface(),
                    "provider `{id}`: data/provider.toml interface {:?} disagrees with \
                     ProviderKind::interface() {:?}",
                    def.interface,
                    cfg.kind().interface(),
                );
            }
        }
    }

    #[test]
    fn defaults_set_includes_all_well_known_providers() {
        let defs = built_in_provider_defaults();
        for id in [
            "openai",
            "anthropic",
            "gemini",
            "deepseek",
            "claude_cli",
            "mlx",
            "llamacpp",
            "vllm",
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
                extras: HttpOverrides::default(),
            },
        )]
        .into_iter()
        .collect();
        let merged = merge_builtin_with_user(user);
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
                gen_ai_provider_name: "local".into(),
                extras: HttpOverrides::default(),
                capabilities: Default::default(),
            },
        )]
        .into_iter()
        .collect();
        let merged = merge_builtin_with_user(user);
        // Eight built-ins and the user's one.
        assert_eq!(merged.len(), 9);
        assert!(merged.contains_key(&ProviderId::new("local_qwen")));
    }

    #[test]
    fn deepseek_default_is_openai_compat_pointed_at_deepseek() {
        match default_deepseek() {
            ProviderConfig::OpenaiCompat {
                base_url,
                auth,
                default_model,
                ..
            } => {
                assert_eq!(base_url, "https://api.deepseek.com");
                assert!(default_model.starts_with("deepseek-"));
                assert!(matches!(auth, Auth::Secret { .. }));
            }
            other => panic!("expected OpenaiCompat variant, got: {other:?}"),
        }
    }

    #[test]
    fn the_claude_cli_default_model_is_the_one_the_data_file_names() {
        assert_eq!(
            MODEL_KB.default_model("claude_cli"),
            Some("claude-opus-4-7")
        );
        assert_eq!(default_claude_cli().default_model(), "claude-opus-4-7");
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
                assert!(default_model.contains("Q5_K_M") || default_model.contains("Q4"));
            }
            other => panic!("expected Llamacpp variant, got: {other:?}"),
        }
    }
}
