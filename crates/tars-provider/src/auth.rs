//! Provider authentication runtime.
//!
//! The *spec* for auth lives in `tars_types::Auth` (serializable, what
//! config files reference). This module owns the *runtime* side:
//! [`AuthResolver`] turns an [`Auth`] spec into a [`ResolvedAuth`]
//! the adapter can stuff into headers.
//!
//! When `tars-security` lands (Doc 14 §M0), the resolver implementations
//! that talk to Vault / KMS / etc. will live there. For now the basic
//! Inline / Env / File / None / Delegate resolutions live here so the
//! Provider layer is self-contained.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

pub use tars_types::Auth;
use tars_types::{ProviderError, RequestContext, SecretRef};

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("missing credential: {0}")]
    Missing(String),
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("permission denied (cross-tenant access)")]
    PermissionDenied,
    #[error("io: {0}")]
    Io(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl From<AuthError> for ProviderError {
    fn from(value: AuthError) -> Self {
        ProviderError::Auth(value.to_string())
    }
}

/// What the resolver hands back. The Adapter decides how to apply it
/// (Bearer header, x-api-key header, query string …).
///
/// `Debug` is implemented manually to redact credential bodies. Audit
/// finding `tars-provider-src-auth-2`: a `tracing::error!(auth = ?auth)`
/// on this type would otherwise dump the bearer/api-key plaintext.
#[derive(Clone)]
pub enum ResolvedAuth {
    /// Plaintext bearer-style credential string.
    Bearer(String),
    /// API-key style — adapter chooses the header name.
    ApiKey(String),
    /// Nothing to inject (Auth::None / Delegate fall here).
    None,
}

impl std::fmt::Debug for ResolvedAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bearer(s) => write!(f, "Bearer(<redacted:{}>)", s.len()),
            Self::ApiKey(s) => write!(f, "ApiKey(<redacted:{}>)", s.len()),
            Self::None => write!(f, "None"),
        }
    }
}

#[async_trait]
pub trait AuthResolver: Send + Sync {
    /// Resolve `auth` in the context of `ctx`. The context carries
    /// tenant/principal so production resolvers can do per-tenant
    /// secret namespacing (Doc 06 §5.3).
    async fn resolve(
        &self,
        auth: &Auth,
        ctx: &RequestContext,
    ) -> Result<ResolvedAuth, AuthError>;
}

/// Handles `Auth::None`, `Auth::Delegate`, and the basic [`SecretRef`]
/// variants (`Inline` / `Env` / `File`). Suitable for tests and Personal
/// mode. Production deployments swap in a Vault-aware resolver from
/// `tars-security` (when that crate exists).
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
            Auth::None | Auth::Delegate => Ok(ResolvedAuth::None),
            Auth::Secret { secret } => match secret {
                SecretRef::Inline { value } => {
                    // Audit `tars-provider-src-auth-8`: `SecretRef::Inline`
                    // is documented as test/dev-only but nothing in the
                    // resolver enforced that. We can't safely refuse it
                    // (existing tests + Personal-mode workflows pass
                    // inline keys), but emit a loud one-time warning so
                    // production deployments notice if it slips through
                    // a config review. Suppress in test builds where
                    // it's the expected path.
                    #[cfg(not(test))]
                    tracing::warn!(
                        "auth: resolving SecretRef::Inline credential — intended for test/dev use only; \
                         use SecretRef::Env or SecretRef::File in production",
                    );
                    Ok(ResolvedAuth::ApiKey(value.expose().to_string()))
                }
                SecretRef::Env { var } => match std::env::var(var) {
                    Ok(v) => {
                        // An empty / whitespace-only env var produces
                        // mysterious 401s downstream — surface it as a
                        // missing credential here.
                        if v.trim().is_empty() {
                            return Err(AuthError::Missing(format!(
                                "env var `{var}` is set but empty"
                            )));
                        }
                        Ok(ResolvedAuth::ApiKey(v))
                    }
                    // Audit `tars-provider-src-auth-1`: VarError has
                    // two distinct cases — surfacing them separately
                    // turns "auth doesn't work" into actionable
                    // diagnostics ("set the env var" vs "the env
                    // var contains a NUL or non-UTF-8 byte").
                    Err(std::env::VarError::NotPresent) => {
                        Err(AuthError::Missing(format!("env var `{var}` is not set")))
                    }
                    Err(std::env::VarError::NotUnicode(_)) => Err(AuthError::Internal(
                        format!("env var `{var}` contains non-UTF-8 bytes"),
                    )),
                },
                SecretRef::File { path } => {
                    let raw = std::fs::read_to_string(path).map_err(|e| {
                        AuthError::Io(format!("reading {}: {e}", path.display()))
                    })?;
                    let trimmed = raw.trim_end_matches(['\n', '\r']).to_string();
                    if trimmed.trim().is_empty() {
                        return Err(AuthError::Missing(format!(
                            "credential file `{}` is empty",
                            path.display()
                        )));
                    }
                    Ok(ResolvedAuth::ApiKey(trimmed))
                }
            },
        }
    }
}

/// Convenience: `Arc<dyn AuthResolver>` wrapping a [`BasicAuthResolver`].
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
            .resolve(&Auth::inline("sk-x"), &RequestContext::test_default())
            .await
            .unwrap();
        match v {
            ResolvedAuth::ApiKey(k) => assert_eq!(k, "sk-x"),
            _ => panic!("expected ApiKey"),
        }
    }

    #[tokio::test]
    async fn env_missing_returns_typed_error() {
        // env mutation is unsafe in Rust 2024 + workspace forbids unsafe;
        // verify the error path with a guaranteed-missing key.
        let r = BasicAuthResolver;
        let key = "TARS_TEST_AUTH_KEY_THAT_NEVER_EXISTS_42";
        let err = r
            .resolve(&Auth::env(key), &RequestContext::test_default())
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

    #[test]
    fn resolved_auth_debug_redacts_credentials() {
        // Audit `tars-provider-src-auth-2`: a `tracing::error!(auth = ?a)`
        // would dump the bearer plaintext if Debug were derived.
        let a = ResolvedAuth::Bearer("super-secret-token".into());
        let s = format!("{a:?}");
        assert!(!s.contains("super-secret-token"));
        assert!(s.contains("redacted"));

        let b = ResolvedAuth::ApiKey("sk-proj-abcdef".into());
        let s = format!("{b:?}");
        assert!(!s.contains("sk-proj-abcdef"));
        assert!(s.contains("redacted"));
    }

    #[tokio::test]
    async fn delegate_resolves_to_none() {
        let r = BasicAuthResolver;
        let v = r
            .resolve(&Auth::Delegate, &RequestContext::test_default())
            .await
            .unwrap();
        assert!(matches!(v, ResolvedAuth::None));
    }

    #[tokio::test]
    async fn file_resolves_and_strips_trailing_newline() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tars-auth-test-{}", std::process::id()));
        std::fs::write(&path, "secret-from-file\n").unwrap();
        let r = BasicAuthResolver;
        let v = r
            .resolve(&Auth::file(&path), &RequestContext::test_default())
            .await
            .unwrap();
        match v {
            ResolvedAuth::ApiKey(k) => assert_eq!(k, "secret-from-file"),
            _ => panic!("expected ApiKey"),
        }
        std::fs::remove_file(&path).ok();
    }
}
