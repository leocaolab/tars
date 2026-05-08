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
#[cfg(not(test))]
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use thiserror::Error;

/// Hard cap on credential payload size (env var or file). 64 KiB is far
/// larger than any real API key / OAuth token, but small enough that a
/// misconfigured `/dev/zero` or runaway file can't OOM the process.
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;

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
    async fn resolve(&self, auth: &Auth, ctx: &RequestContext) -> Result<ResolvedAuth, AuthError>;
}

/// Handles `Auth::None`, `Auth::Delegate`, and the basic [`SecretRef`]
/// variants (`Inline` / `Env` / `File`). Suitable for tests and Personal
/// mode. Production deployments swap in a Vault-aware resolver from
/// `tars-security` (when that crate exists).
#[derive(Default)]
pub struct BasicAuthResolver;

#[async_trait]
impl AuthResolver for BasicAuthResolver {
    async fn resolve(&self, auth: &Auth, _ctx: &RequestContext) -> Result<ResolvedAuth, AuthError> {
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
                    {
                        static WARNED: AtomicBool = AtomicBool::new(false);
                        if WARNED
                            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                        {
                            tracing::warn!(
                                "auth: resolving SecretRef::Inline credential — intended for test/dev use only; \
                                 use SecretRef::Env or SecretRef::File in production",
                            );
                        }
                    }
                    let raw = value.expose();
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        return Err(AuthError::Missing("inline credential is empty".into()));
                    }
                    Ok(ResolvedAuth::ApiKey(trimmed.to_string()))
                }
                SecretRef::Env { var } => match std::env::var(var) {
                    Ok(v) => {
                        if v.len() > MAX_CREDENTIAL_BYTES {
                            return Err(AuthError::Internal(format!(
                                "env var `{var}` exceeds credential size cap ({} bytes > {MAX_CREDENTIAL_BYTES})",
                                v.len()
                            )));
                        }
                        // An empty / whitespace-only env var produces
                        // mysterious 401s downstream — surface it as a
                        // missing credential here.
                        let trimmed = v.trim();
                        if trimmed.is_empty() {
                            return Err(AuthError::Missing(format!(
                                "env var `{var}` is set but empty"
                            )));
                        }
                        Ok(ResolvedAuth::ApiKey(trimmed.to_string()))
                    }
                    // Audit `tars-provider-src-auth-1`: VarError has
                    // two distinct cases — surfacing them separately
                    // turns "auth doesn't work" into actionable
                    // diagnostics ("set the env var" vs "the env
                    // var contains a NUL or non-UTF-8 byte").
                    Err(std::env::VarError::NotPresent) => {
                        Err(AuthError::Missing(format!("env var `{var}` is not set")))
                    }
                    Err(std::env::VarError::NotUnicode(_)) => Err(AuthError::Internal(format!(
                        "env var `{var}` contains non-UTF-8 bytes"
                    ))),
                },
                SecretRef::File { path } => {
                    // Cap the read size: a misconfigured path pointing
                    // at /dev/zero or a multi-GB file must not OOM the
                    // process. Read up to MAX+1 bytes; if we hit the
                    // limit, reject the file.
                    use tokio::io::AsyncReadExt;
                    let mut file = tokio::fs::File::open(path)
                        .await
                        .map_err(|e| AuthError::Io(format!("opening {}: {e}", path.display())))?;
                    let mut buf = Vec::with_capacity(256);
                    let mut limited = (&mut file).take((MAX_CREDENTIAL_BYTES as u64) + 1);
                    limited
                        .read_to_end(&mut buf)
                        .await
                        .map_err(|e| AuthError::Io(format!("reading {}: {e}", path.display())))?;
                    if buf.len() > MAX_CREDENTIAL_BYTES {
                        return Err(AuthError::Internal(format!(
                            "credential file `{}` exceeds size cap ({MAX_CREDENTIAL_BYTES} bytes)",
                            path.display()
                        )));
                    }
                    let raw = String::from_utf8(buf).map_err(|e| {
                        AuthError::Internal(format!(
                            "credential file `{}` is not valid UTF-8: {e}",
                            path.display()
                        ))
                    })?;
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        return Err(AuthError::Missing(format!(
                            "credential file `{}` is empty",
                            path.display()
                        )));
                    }
                    Ok(ResolvedAuth::ApiKey(trimmed.to_string()))
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
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn file_strips_surrounding_whitespace() {
        // Audit `tars-provider-src-auth-7`: a file like "  secret  \n"
        // must not leak whitespace into the API key.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tars-auth-ws-test-{}-{}", std::process::id(), "ws"));
        std::fs::write(&path, "  secret-key  \n").unwrap();
        let r = BasicAuthResolver;
        let v = r
            .resolve(&Auth::file(&path), &RequestContext::test_default())
            .await
            .unwrap();
        match v {
            ResolvedAuth::ApiKey(k) => assert_eq!(k, "secret-key"),
            _ => panic!("expected ApiKey"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn file_empty_returns_missing() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tars-auth-empty-{}", std::process::id()));
        std::fs::write(&path, "   \n\r\n").unwrap();
        let r = BasicAuthResolver;
        let err = r
            .resolve(&Auth::file(&path), &RequestContext::test_default())
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Missing(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn file_missing_returns_io_error() {
        let path = std::env::temp_dir().join(format!(
            "tars-auth-missing-{}-{}",
            std::process::id(),
            "nope"
        ));
        // Make sure it isn't there.
        let _ = std::fs::remove_file(&path);
        let r = BasicAuthResolver;
        let err = r
            .resolve(&Auth::file(&path), &RequestContext::test_default())
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Io(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn file_oversized_returns_internal() {
        // Audit `tars-provider-src-auth-8`: a credential file larger
        // than the cap must be rejected, not loaded into memory.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tars-auth-big-{}", std::process::id()));
        let big = vec![b'a'; 64 * 1024 + 16];
        std::fs::write(&path, &big).unwrap();
        let r = BasicAuthResolver;
        let err = r
            .resolve(&Auth::file(&path), &RequestContext::test_default())
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Internal(_)), "got {err:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn inline_empty_returns_missing() {
        // Audit `tars-provider-src-auth-5`: an empty inline credential
        // must be rejected at resolution time, not produce a 401 later.
        let r = BasicAuthResolver;
        let err = r
            .resolve(&Auth::inline("   "), &RequestContext::test_default())
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Missing(_)));
    }

    #[tokio::test]
    async fn inline_strips_surrounding_whitespace() {
        let r = BasicAuthResolver;
        let v = r
            .resolve(&Auth::inline("  sk-x  "), &RequestContext::test_default())
            .await
            .unwrap();
        match v {
            ResolvedAuth::ApiKey(k) => assert_eq!(k, "sk-x"),
            _ => panic!("expected ApiKey"),
        }
    }
}
