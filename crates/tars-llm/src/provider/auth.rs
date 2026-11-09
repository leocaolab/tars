//! Turns an [`Auth`] from config into the credential an adapter puts on the wire. The basic
//! resolver reads inline values, environment variables and files; it knows no secret store.

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use thiserror::Error;

/// Bounds a credential's size: far above any real API key or OAuth token, small enough that a
/// credential path pointing at `/dev/zero` cannot exhaust memory.
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;

use crate::ProviderError;
use crate::provider::error::CredentialError;
pub use tars_types::Auth;
use tars_types::{Caller, SecretRef};

/// Where a credential came from, named in error messages so they point at the config to fix. Its
/// `Display` keeps a stable wording, which operators grep for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    Inline,
    Env { var: String },
    File { path: PathBuf },
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialSource::Inline => write!(f, "inline credential"),
            CredentialSource::Env { var } => write!(f, "env var `{var}`"),
            CredentialSource::File { path } => write!(f, "credential file `{}`", path.display()),
        }
    }
}

/// Returns the credential trimmed, refusing one over the size cap or empty after trimming.
fn validate_credential(raw: &str, source: CredentialSource) -> Result<String, AuthError> {
    if raw.len() > MAX_CREDENTIAL_BYTES {
        return Err(AuthError::TooLarge {
            from: source,
            len: raw.len(),
        });
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AuthError::Empty { from: source });
    }
    Ok(trimmed.to_string())
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("missing credential: {from} is empty")]
    Empty { from: CredentialSource },

    #[error("{from} exceeds credential size cap ({len} bytes > {MAX_CREDENTIAL_BYTES})")]
    TooLarge { from: CredentialSource, len: usize },

    #[error("missing credential: env var `{var}` is not set")]
    EnvUnset { var: String },

    #[error("env var `{var}` contains non-UTF-8 bytes")]
    EnvNotUnicode { var: String },

    #[error("opening credential file `{}`", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("reading credential file `{}`", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The cause is the UTF-8 error alone, without the bytes, which are the credential.
    #[error("credential file `{}` is not valid UTF-8", path.display())]
    NotUtf8 {
        path: PathBuf,
        #[source]
        source: std::str::Utf8Error,
    },

    /// A resolver's secret store could not be reached; the basic resolver has none.
    #[error("backend unavailable")]
    BackendUnavailable(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("permission denied (cross-tenant access)")]
    PermissionDenied,
}

impl From<AuthError> for ProviderError {
    fn from(value: AuthError) -> Self {
        ProviderError::Auth(CredentialError::Resolve(value))
    }
}

/// A resolved credential; the adapter decides where it goes (a Bearer header, `x-api-key`, a
/// query string). Its `Debug` prints lengths, never the credential.
#[derive(Clone)]
pub enum ResolvedAuth {
    Bearer(String),
    /// A key, placed where the adapter's API expects one.
    ApiKey(String),
    /// Means nothing is injected, for `Auth::None` and `Auth::Delegate`.
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
    /// Resolves `auth` for `ctx`, whose tenant lets a resolver keep each tenant's secrets apart.
    async fn resolve(&self, auth: &Auth, ctx: &Caller) -> Result<ResolvedAuth, AuthError>;
}

/// The environment a resolver reads `SecretRef::Env` variables from.
pub trait EnvSource: Send + Sync {
    /// Returns the value of `name`, with [`std::env::var`]'s errors.
    fn var(&self, name: &str) -> Result<String, std::env::VarError>;
}

/// The process environment.
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn var(&self, name: &str) -> Result<String, std::env::VarError> {
        std::env::var(name)
    }
}

/// Returns the credential env var `var` of `env` holds, trimmed.
///
/// # Errors
///
/// Fails when the variable is unset, empty after trimming, over the size cap, or not UTF-8.
pub fn env_credential(env: &dyn EnvSource, var: &str) -> Result<String, AuthError> {
    match env.var(var) {
        Ok(v) => validate_credential(
            &v,
            CredentialSource::Env {
                var: var.to_owned(),
            },
        ),
        Err(std::env::VarError::NotPresent) => Err(AuthError::EnvUnset {
            var: var.to_owned(),
        }),
        Err(std::env::VarError::NotUnicode(_)) => Err(AuthError::EnvNotUnicode {
            var: var.to_owned(),
        }),
    }
}

/// Resolves `Auth::None`, `Auth::Delegate` and the inline, env and file [`SecretRef`]s, every
/// secret as [`ResolvedAuth::ApiKey`], reading env vars from its [`EnvSource`]. It ignores the
/// tenant.
pub struct EnvFileAuthResolver {
    env: Arc<dyn EnvSource>,
}

impl Default for EnvFileAuthResolver {
    fn default() -> Self {
        Self::new(Arc::new(ProcessEnv))
    }
}

#[async_trait]
impl AuthResolver for EnvFileAuthResolver {
    async fn resolve(&self, auth: &Auth, _ctx: &Caller) -> Result<ResolvedAuth, AuthError> {
        self.resolve_untenanted(auth).await
    }
}

impl EnvFileAuthResolver {
    pub fn new(env: Arc<dyn EnvSource>) -> Self {
        Self { env }
    }

    /// Resolves `auth` as [`AuthResolver::resolve`] does; this resolver keeps no tenant's
    /// secrets apart, so a caller with no tenant (listing a provider's models) needs none.
    pub async fn resolve_untenanted(&self, auth: &Auth) -> Result<ResolvedAuth, AuthError> {
        match auth {
            // `Delegate` means the spawned tool/CLI carries its own credential
            // (e.g. `claude login`); tars injects nothing.
            Auth::None | Auth::Delegate => Ok(ResolvedAuth::None),
            Auth::Secret { secret } => match secret {
                SecretRef::Inline { value } => {
                    // An inline secret is meant for tests and development, but personal setups
                    // use it, so it warns once instead of refusing; test builds stay quiet.
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
                    let trimmed = validate_credential(raw, CredentialSource::Inline)?;
                    Ok(ResolvedAuth::ApiKey(trimmed))
                }
                SecretRef::Env { var } => {
                    env_credential(self.env.as_ref(), var).map(ResolvedAuth::ApiKey)
                }
                SecretRef::File { path } => {
                    // Reads at most one byte past the cap, so an oversize file is refused without
                    // being loaded.
                    use tokio::io::AsyncReadExt;
                    let mut file =
                        tokio::fs::File::open(path)
                            .await
                            .map_err(|source| AuthError::Open {
                                path: path.clone(),
                                source,
                            })?;
                    let mut buf = Vec::with_capacity(256);
                    let mut limited = (&mut file).take((MAX_CREDENTIAL_BYTES as u64) + 1);
                    limited
                        .read_to_end(&mut buf)
                        .await
                        .map_err(|source| AuthError::Read {
                            path: path.clone(),
                            source,
                        })?;
                    let raw = String::from_utf8(buf).map_err(|e| AuthError::NotUtf8 {
                        path: path.clone(),
                        source: e.utf8_error(),
                    })?;
                    let trimmed =
                        validate_credential(&raw, CredentialSource::File { path: path.clone() })?;
                    Ok(ResolvedAuth::ApiKey(trimmed))
                }
            },
        }
    }
}

pub fn basic() -> Arc<dyn AuthResolver> {
    Arc::new(EnvFileAuthResolver::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inline_resolves_to_api_key() {
        let r = EnvFileAuthResolver::default();
        let v = r
            .resolve(&Auth::inline("sk-x"), &Caller::test_default())
            .await
            .unwrap();
        match v {
            ResolvedAuth::ApiKey(k) => assert_eq!(k, "sk-x"),
            _ => panic!("expected ApiKey"),
        }
    }

    struct OneVar(&'static str, &'static str);

    impl EnvSource for OneVar {
        fn var(&self, name: &str) -> Result<String, std::env::VarError> {
            if name == self.0 {
                Ok(self.1.to_string())
            } else {
                Err(std::env::VarError::NotPresent)
            }
        }
    }

    #[tokio::test]
    async fn env_reads_the_resolvers_source_and_trims() {
        let r = EnvFileAuthResolver::new(Arc::new(OneVar("TARS_K", "  sk-env \n")));
        match r.resolve_untenanted(&Auth::env("TARS_K")).await.unwrap() {
            ResolvedAuth::ApiKey(k) => assert_eq!(k, "sk-env"),
            other => panic!("expected ApiKey, got {other:?}"),
        }
        let blank = OneVar("TARS_K", "   ");
        assert!(matches!(
            env_credential(&blank, "TARS_K"),
            Err(AuthError::Empty { .. })
        ));
    }

    #[tokio::test]
    async fn env_missing_returns_typed_error() {
        // Setting an env var is unsafe in Rust 2024 and the workspace forbids unsafe, so only a
        // missing var is tested.
        let r = EnvFileAuthResolver::default();
        let key = "TARS_TEST_AUTH_KEY_THAT_NEVER_EXISTS_42";
        let err = r
            .resolve(&Auth::env(key), &Caller::test_default())
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::EnvUnset { .. }));
    }

    #[tokio::test]
    async fn none_resolves_to_none() {
        let r = EnvFileAuthResolver::default();
        let v = r
            .resolve(&Auth::None, &Caller::test_default())
            .await
            .unwrap();
        assert!(matches!(v, ResolvedAuth::None));
    }

    #[test]
    fn resolved_auth_debug_redacts_credentials() {
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
        let r = EnvFileAuthResolver::default();
        let v = r
            .resolve(&Auth::Delegate, &Caller::test_default())
            .await
            .unwrap();
        assert!(matches!(v, ResolvedAuth::None));
    }

    #[tokio::test]
    async fn file_resolves_and_strips_trailing_newline() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tars-auth-test-{}", std::process::id()));
        std::fs::write(&path, "secret-from-file\n").unwrap();
        let r = EnvFileAuthResolver::default();
        let v = r
            .resolve(&Auth::file(&path), &Caller::test_default())
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
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tars-auth-ws-test-{}-{}", std::process::id(), "ws"));
        std::fs::write(&path, "  secret-key  \n").unwrap();
        let r = EnvFileAuthResolver::default();
        let v = r
            .resolve(&Auth::file(&path), &Caller::test_default())
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
        let r = EnvFileAuthResolver::default();
        let err = r
            .resolve(&Auth::file(&path), &Caller::test_default())
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Empty { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn file_missing_returns_io_error() {
        let path = std::env::temp_dir().join(format!(
            "tars-auth-missing-{}-{}",
            std::process::id(),
            "nope"
        ));
        let _ = std::fs::remove_file(&path);
        let r = EnvFileAuthResolver::default();
        let err = r
            .resolve(&Auth::file(&path), &Caller::test_default())
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Open { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn file_oversized_returns_too_large() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tars-auth-big-{}", std::process::id()));
        let big = vec![b'a'; 64 * 1024 + 16];
        std::fs::write(&path, &big).unwrap();
        let r = EnvFileAuthResolver::default();
        let err = r
            .resolve(&Auth::file(&path), &Caller::test_default())
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::TooLarge { .. }), "got {err:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn inline_empty_returns_missing() {
        // Refused here rather than sent and answered with a 401.
        let r = EnvFileAuthResolver::default();
        let err = r
            .resolve(&Auth::inline("   "), &Caller::test_default())
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Empty { .. }));
    }

    #[tokio::test]
    async fn inline_strips_surrounding_whitespace() {
        let r = EnvFileAuthResolver::default();
        let v = r
            .resolve(&Auth::inline("  sk-x  "), &Caller::test_default())
            .await
            .unwrap();
        match v {
            ResolvedAuth::ApiKey(k) => assert_eq!(k, "sk-x"),
            _ => panic!("expected ApiKey"),
        }
    }
}
