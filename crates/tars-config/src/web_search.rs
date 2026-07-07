//! `[web_search]` — web-search backend config + API-key resolution.
//!
//! The **schema is owned by sisurf** ([`sisurf_core::SearchConfig`]): sisurf
//! decides which backends exist (`ddg` / `google_cse` / `brave`) and what each
//! needs. tars deserializes the `[web_search]` TOML section straight into that
//! type (it does NOT redeclare it), then does the one thing sisurf refuses to
//! do as a library: **resolve the API key from the environment and inject it**.
//!
//! ## Why sisurf never reads the env
//!
//! sisurf is a pure library — it must not reach into `std::env` or the
//! filesystem for secrets (untestable, non-hermetic, surprising). So its
//! `GoogleCseConfig` / `BraveConfig` carry a plain `api_key: String` that
//! defaults empty, and the committed `[web_search]` TOML omits the secret. tars
//! (the application) resolves the key from a conventional env var — the same
//! posture as provider `api_key_env` (a `SecretRef::Env`) — and writes it into
//! the sub-config via [`inject_search_keys`]. The key never lives on disk.
//!
//! A missing/empty env var is **not** patched over here: the value stays empty,
//! and [`sisurf_core::SearchConfig::build`] then typed-fails with
//! [`sisurf_core::WebError::MissingApiKey`] at search time, which the
//! `web.search` tool surfaces legibly. Truth over cover-up.

use sisurf_core::{BackendKind, SearchConfig};

/// Env var tars resolves the **Google CSE** API key from. sisurf never reads
/// this — tars reads it and injects the value into `google_cse.api_key`.
pub const GOOGLE_CSE_API_KEY_ENV: &str = "GOOGLE_CSE_KEY";

/// Env var tars resolves the **Brave Search** API key from.
pub const BRAVE_API_KEY_ENV: &str = "BRAVE_API_KEY";

/// Inject the env-resolved API key into a `[web_search]` [`SearchConfig`].
///
/// Reads the conventional env var for the selected backend (via
/// `std::env::var`) and writes it into the matching sub-config's `api_key`.
/// A missing env var leaves `api_key` empty on purpose — `build()` reports it
/// as a typed `MissingApiKey` at call time rather than us silently falling back
/// to a different backend. `ddg` needs no key and is untouched.
pub fn inject_search_keys(cfg: SearchConfig) -> SearchConfig {
    inject_search_keys_with(cfg, env_key)
}

/// Testable core of [`inject_search_keys`]: the env lookup is a parameter so
/// tests exercise the injection logic hermetically, without mutating process
/// env (which is `unsafe` under edition 2024 and forbidden by our lints).
fn inject_search_keys_with(
    mut cfg: SearchConfig,
    resolve: impl Fn(&str) -> Option<String>,
) -> SearchConfig {
    match cfg.backend {
        BackendKind::Ddg => {}
        BackendKind::GoogleCse => {
            if let Some(key) = resolve(GOOGLE_CSE_API_KEY_ENV) {
                cfg.google_cse.get_or_insert_with(Default::default).api_key = key;
            }
        }
        BackendKind::Brave => {
            if let Some(key) = resolve(BRAVE_API_KEY_ENV) {
                cfg.brave.get_or_insert_with(Default::default).api_key = key;
            }
        }
    }
    cfg
}

/// Read an env var, treating a present-but-blank value as absent (so a
/// `FOO=` in the environment doesn't inject an empty "key" that would only
/// fail later with a more confusing error than "missing").
fn env_key(var: &str) -> Option<String> {
    std::env::var(var).ok().and_then(non_blank)
}

/// Blank-guard: a present-but-whitespace value is treated as absent.
fn non_blank(v: String) -> Option<String> {
    if v.trim().is_empty() {
        None
    } else {
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use sisurf_core::{BraveConfig, GoogleCseConfig};

    /// A fake env resolver — maps var name → value, so injection is tested
    /// hermetically without touching (unsafe-to-mutate) process env.
    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |var: &str| map.get(var).cloned()
    }

    #[test]
    fn web_search_section_deserializes_into_sisurf_schema() {
        // The whole point: tars parses the TOML straight into sisurf's owned
        // schema. Secrets are NOT in the file — only backend + cx.
        let toml_str = r#"
            backend = "google_cse"
            google_cse = { cx = "abc123" }
        "#;
        let cfg: SearchConfig = toml::from_str(toml_str).expect("parse into sisurf SearchConfig");
        assert_eq!(cfg.backend, BackendKind::GoogleCse);
        let g = cfg.google_cse.expect("google_cse section");
        assert_eq!(g.cx, "abc123");
        assert!(g.api_key.is_empty(), "secret must not be in the committed TOML");
    }

    #[test]
    fn default_backend_is_ddg_and_needs_no_key() {
        let cfg = SearchConfig::default();
        assert_eq!(cfg.backend, BackendKind::Ddg);
        // DDG builds without any key (real env resolver — ddg reads none).
        let injected = inject_search_keys(cfg);
        assert!(injected.build().is_ok());
    }

    #[test]
    fn injects_google_cse_key_from_env() {
        let cfg = SearchConfig {
            backend: BackendKind::GoogleCse,
            google_cse: Some(GoogleCseConfig {
                cx: "cx1".into(),
                api_key: String::new(),
            }),
            brave: None,
        };
        let injected = inject_search_keys_with(
            cfg,
            fake_env(&[(GOOGLE_CSE_API_KEY_ENV, "resolved-secret")]),
        );
        assert_eq!(injected.google_cse.as_ref().unwrap().api_key, "resolved-secret");
        // And it now builds into a runnable backend.
        assert!(injected.build().is_ok());
    }

    #[test]
    fn injects_brave_key_from_env() {
        let cfg = SearchConfig {
            backend: BackendKind::Brave,
            google_cse: None,
            brave: Some(BraveConfig {
                api_key: String::new(),
            }),
        };
        let injected =
            inject_search_keys_with(cfg, fake_env(&[(BRAVE_API_KEY_ENV, "brave-secret")]));
        assert_eq!(injected.brave.as_ref().unwrap().api_key, "brave-secret");
        assert!(injected.build().is_ok());
    }

    #[test]
    fn missing_env_leaves_key_empty_and_build_typed_fails() {
        // Truth over cover: no env var ⇒ empty key ⇒ build() reports the typed
        // MissingApiKey, not a silent fallback.
        let cfg = SearchConfig {
            backend: BackendKind::GoogleCse,
            google_cse: Some(GoogleCseConfig {
                cx: "cx1".into(),
                api_key: String::new(),
            }),
            brave: None,
        };
        let injected = inject_search_keys_with(cfg, fake_env(&[]));
        assert!(injected.google_cse.as_ref().unwrap().api_key.is_empty());
        assert!(matches!(
            injected.build(),
            Err(sisurf_core::WebError::MissingApiKey(_))
        ));
    }

    #[test]
    fn blank_env_value_treated_as_absent() {
        // A present-but-whitespace env value must not inject an empty-ish key.
        assert!(super::non_blank("   ".to_string()).is_none());
        assert!(super::non_blank(String::new()).is_none());
        assert_eq!(super::non_blank("sk-x".to_string()), Some("sk-x".to_string()));
    }
}
