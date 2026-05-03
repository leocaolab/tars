//! Provider authentication.
//!
//! Doc 06 §5 calls for `SecretRef` references resolved by an external
//! `SecretResolver`. At the Provider layer we don't want a direct
//! dependency on the (forthcoming) `tars-security` crate, so we define
//! a minimal local trait. `tars-security` will impl this for us later.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use tars_types::{ProviderError, RequestContext};

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("missing credential: {0}")]
    Missing(String),
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("permission denied (cross-tenant access)")]
    PermissionDenied,
    #[error("internal: {0}")]
    Internal(String),
}

impl From<AuthError> for ProviderError {
    fn from(value: AuthError) -> Self {
        ProviderError::Auth(value.to_string())
    }
}

/// Caller-facing description of where to fetch a credential.
#[derive(Clone, Debug)]
pub enum Auth {
    /// Inline value. Test/dev only — production should use Env / SecretManager.
    Inline(String),
    /// `$VARNAME` from process environment at resolve time.
    Env { var: String },
    /// No credential needed (local OpenAI-compatible servers, mock).
    None,
    /// Delegate to a downstream tool (Claude CLI, gemini CLI). Adapter
    /// understands what to do; no value is returned here.
    Delegate,
}

/// What the resolver hands back. The Adapter decides how to apply it
/// (Bearer header, x-api-key header, query string …).
#[derive(Clone, Debug)]
pub enum ResolvedAuth {
    /// Plaintext credential string.
    Bearer(String),
    /// API-key style — adapter chooses the header name.
    ApiKey(String),
    /// Nothing to inject (Auth::None / Delegate fall here).
    None,
}

#[async_trait]
pub trait AuthResolver: Send + Sync {
    /// Resolve `auth` in the context of `ctx`. The context carries
    /// tenant/principal so the resolver can do per-tenant secret
    /// namespacing (Doc 06 §5.3) — not required by the trait, but
    /// honored by all production resolvers.
    async fn resolve(
        &self,
        auth: &Auth,
        ctx: &RequestContext,
    ) -> Result<ResolvedAuth, AuthError>;
}

/// The simplest possible resolver: handles `Inline`, `Env`, `None`,
/// `Delegate`. Suitable for tests and Personal mode.
#[derive(Default)]
pub struct BasicAuthResolver;

#[async_trait]
impl AuthResolver for BasicAuthResolver {
    async fn resolve(
        &self,
        auth: &Auth,
        _ctx: &RequestContext,
    ) -> Result<ResolvedAuth, AuthError> {
        match auth {
            Auth::Inline(s) => Ok(ResolvedAuth::ApiKey(s.clone())),
            Auth::Env { var } => std::env::var(var)
                .map(ResolvedAuth::ApiKey)
                .map_err(|_| AuthError::Missing(format!("env {var}"))),
            Auth::None | Auth::Delegate => Ok(ResolvedAuth::None),
        }
    }
}

/// Convenience — wrap in `Arc<dyn AuthResolver>` for storage.
pub fn basic() -> Arc<dyn AuthResolver> {
    Arc::new(BasicAuthResolver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inline_resolves_to_api_key() {
        let r = BasicAuthResolver;
        let v = r
            .resolve(&Auth::Inline("sk-x".into()), &RequestContext::test_default())
            .await
            .unwrap();
        match v {
            ResolvedAuth::ApiKey(k) => assert_eq!(k, "sk-x"),
            _ => panic!("expected ApiKey"),
        }
    }

    #[tokio::test]
    async fn env_missing_returns_typed_error() {
        // Rust 2024 marks std::env::set_var as unsafe (process-wide
        // shared state, racy under threading). Workspace forbids
        // `unsafe_code`, so we don't mutate env in tests. Instead we
        // use a guaranteed-missing key and verify the error path —
        // the "happy path" is exercised by integration tests that
        // wrap the binary process where the harness sets env.
        let r = BasicAuthResolver;
        let key = "TARS_TEST_AUTH_KEY_THAT_NEVER_EXISTS_42";
        let err = r
            .resolve(
                &Auth::Env { var: key.into() },
                &RequestContext::test_default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Missing(_)));
    }

    #[tokio::test]
    async fn none_resolves_to_none() {
        let r = BasicAuthResolver;
        let v = r
            .resolve(&Auth::None, &RequestContext::test_default())
            .await
            .unwrap();
        assert!(matches!(v, ResolvedAuth::None));
    }
}
