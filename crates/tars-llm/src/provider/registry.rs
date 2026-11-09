//! Builds a provider for every entry of a [`tars_config::ProvidersConfig`] and holds them by id,
//! with each one's default model resolved to a concrete id. It is a flat factory, one match arm
//! per [`ProviderConfig`] variant, so a new backend adds an arm.
//!
//! It does not choose a provider for a request; routing does, through [`ProviderRegistry::get`]
//! and [`ProviderRegistry::resolve_model`].

use std::collections::HashMap;
use std::sync::Arc;
#[cfg(feature = "cli")]
use std::time::Duration;

use thiserror::Error;

use tars_config::{
    ModelCatalog, ModelSpec, ProviderConfig, ProvidersConfig, ResolveModelSpecError,
};
use tars_types::ProviderId;

use crate::pipeline::middleware::OutputReserve;
use crate::provider::LlmProvider;
use crate::provider::auth::{AuthResolver, EnvFileAuthResolver, EnvSource, ProcessEnv};
use crate::provider::backends::anthropic::AnthropicProviderBuilder;
use crate::provider::backends::cassette::{CassetteFileError, CassetteProvider};
#[cfg(feature = "cli")]
use crate::provider::backends::claude_cli::ClaudeCliProviderBuilder;
#[cfg(feature = "cli")]
use crate::provider::backends::claude_sdk::ClaudeSdkProviderBuilder;
#[cfg(feature = "cli")]
use crate::provider::backends::cli::{
    AgentCliBackend, AntigravityDialect, AntigravityEffort, OpenCodeDialect, SharedCliRunner,
};
#[cfg(feature = "cli")]
use crate::provider::backends::codex_cli::{CodexCliProviderBuilder, CodexSandbox};
use crate::provider::backends::gemini::GeminiProviderBuilder;
use crate::provider::backends::llamacpp::llamacpp_builder;
use crate::provider::backends::mlx::mlx_builder;
use crate::provider::backends::mock::{CannedResponse, MockProvider};
use crate::provider::backends::openai::{CompatServer, OpenAiProviderBuilder};
use crate::provider::backends::vllm::vllm_builder;
use crate::provider::http_base::{
    ExtraHeaderError, HttpClient, HttpClientConfig, HttpOverrides, static_headers,
};
use crate::provider::wire::WireRecorder;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("http base init")]
    HttpBaseInit(#[source] reqwest::Error),
    /// A cassette file is there but does not read. Replaying it would answer every request with
    /// a miss, and recording on top of it would flush over every recording it holds.
    #[error("cassette {path} does not read")]
    CassetteUnreadable {
        path: String,
        #[source]
        source: CassetteFileError,
    },
    /// A cassette with no file to replay names a `record_from` provider that is not configured.
    /// Replaying nothing instead would refuse every request with a miss and record nothing.
    #[error("cassette {cassette} records from {record_from}, which is not configured")]
    CassetteSourceMissing {
        cassette: ProviderId,
        record_from: ProviderId,
    },
    /// [`ProviderRegistry::get`] was asked for a provider whose backend is behind a cargo
    /// feature this build left out. The registry still holds it, since a built-in config names
    /// it whether or not anyone uses it.
    #[error(
        "provider {id}: backend requires the `{feature}` cargo feature — \
         rebuild tars with `--features tars-llm/{feature}`"
    )]
    FeatureDisabled {
        id: ProviderId,
        feature: &'static str,
    },
    /// A model spec, the provider's `default_model` or a caller's, resolved to no concrete model:
    /// a `<series>@latest` of an unknown series, or of one with no matching model.
    #[error("provider {id}")]
    Model {
        id: ProviderId,
        #[source]
        source: Box<ResolveModelSpecError>,
    },
    /// A lookup named a provider id this registry does not hold.
    #[error("provider {0} is not in the registry")]
    UnknownProvider(ProviderId),
    /// `resolve_model` was given no model for a provider that was not built from config, so it
    /// has no default.
    #[error("provider {0} has no default model (it was not built from config); name a model")]
    NoDefaultModel(ProviderId),
    /// A static header in the provider's `http_headers` does not make a valid one; sending
    /// without it would drop it from every call.
    #[error("provider {id}")]
    InvalidHeader {
        id: ProviderId,
        #[source]
        source: Box<ExtraHeaderError>,
    },
}

/// The built providers by id; cloning shares them.
#[derive(Clone)]
pub struct ProviderRegistry {
    entries: Arc<HashMap<ProviderId, RegisteredProvider>>,
}

/// One provider of the registry, with what its config says of its models when it was built from
/// one.
#[derive(Clone)]
struct RegisteredProvider {
    backend: Backend,
    models: Option<ConfiguredModels>,
}

/// What a provider's config became in this build.
#[derive(Clone)]
enum Backend {
    Built(Arc<dyn LlmProvider>),
    /// The backend is behind a cargo feature this build left out; asking for it is the error,
    /// not configuring it.
    #[cfg(not(feature = "cli"))]
    FeatureDisabled {
        feature: &'static str,
    },
}

/// What a provider's config block says of its models. It is kept here because `LlmProvider`
/// does not know its default.
#[derive(Clone)]
struct ConfiguredModels {
    /// The provider's block in `data/provider.toml` ([`ProviderConfig::catalog_name`]).
    catalog: String,
    /// The configured default model, resolved to a concrete id.
    default_model: String,
}

impl ProviderRegistry {
    /// Returns a registry with no providers.
    pub fn empty() -> Self {
        Self::from_providers(HashMap::new())
    }

    /// Returns a registry of providers built elsewhere (an embedder's own backends, a test's
    /// mock). With no config there are no default models and no catalog:
    /// [`Self::default_model`] is `None`, and [`Self::resolve_model`] passes a concrete id
    /// through and refuses a `@latest` spec or no spec.
    pub fn from_providers(providers: HashMap<ProviderId, Arc<dyn LlmProvider>>) -> Self {
        let entries = providers
            .into_iter()
            .map(|(id, provider)| {
                (
                    id,
                    RegisteredProvider {
                        backend: Backend::Built(provider),
                        models: None,
                    },
                )
            })
            .collect();
        Self {
            entries: Arc::new(entries),
        }
    }

    /// Returns [`Self::from_providers`] of one provider.
    pub fn single(id: ProviderId, provider: Arc<dyn LlmProvider>) -> Self {
        Self::from_providers(HashMap::from([(id, provider)]))
    }

    /// Builds every declared provider as [`Self::from_config_with_env`] does, over the process
    /// environment.
    pub fn from_config_default(
        cfg: &ProvidersConfig,
        model_catalog: &ModelCatalog<'_>,
        wire: Option<WireRecorder>,
    ) -> Result<Self, RegistryError> {
        Self::from_config_with_env(
            cfg,
            model_catalog,
            HttpClientConfig::default(),
            OutputReserve::default(),
            Arc::new(ProcessEnv),
            wire,
        )
    }

    /// Builds every declared provider with the HTTP settings `http`, the env-var credentials and
    /// the `env_http_headers` both read from `env`; a caller that needs another auth resolver uses
    /// [`Self::from_config`].
    pub fn from_config_with_env(
        cfg: &ProvidersConfig,
        model_catalog: &ModelCatalog<'_>,
        http: HttpClientConfig,
        output_reserve: OutputReserve,
        env: Arc<dyn EnvSource>,
        wire: Option<WireRecorder>,
    ) -> Result<Self, RegistryError> {
        let auth_resolver = Arc::new(EnvFileAuthResolver::new(env.clone()));
        let http = HttpClient::new(http, env).map_err(RegistryError::HttpBaseInit)?;
        Self::from_config(
            cfg,
            model_catalog,
            http,
            output_reserve,
            auth_resolver,
            wire,
        )
    }

    /// Builds every provider declared in `cfg`, the HTTP ones sharing `http` and `auth_resolver`.
    /// Default models resolve and capabilities come from `model_catalog`, which need not be the
    /// installed one: a composition root builds against the catalog it is about to install.
    ///
    /// `wire` is where each provider whose transport holds the raw bytes (the HTTP backends and
    /// the CLI delegates) records each attempt of a logged call. The Claude SDK,
    /// cassettes and mocks never record: they do not see the bytes.
    ///
    /// # Errors
    ///
    /// A default model that does not resolve, or a cassette file that is there but does not read.
    /// A backend behind a disabled feature does not fail the build: [`Self::get`] refuses it.
    pub fn from_config(
        cfg: &ProvidersConfig,
        model_catalog: &ModelCatalog<'_>,
        http: Arc<HttpClient>,
        output_reserve: OutputReserve,
        auth_resolver: Arc<dyn AuthResolver>,
        wire: Option<WireRecorder>,
    ) -> Result<Self, RegistryError> {
        let mut entries: HashMap<ProviderId, RegisteredProvider> = HashMap::new();
        // Read once here, from the client's environment, so `build_cassette` stays a function of
        // its arguments and an injected environment is the one read.
        let force_record = cassette_force_record(http.env.as_ref());
        // Cassettes come last: one that records wraps its `record_from` provider, which must be
        // built by then.
        let (cassettes, base): (Vec<_>, Vec<_>) = cfg
            .iter()
            .partition(|(_, e)| matches!(e, ProviderConfig::Cassette { .. }));
        for (id, entry) in base.into_iter().chain(cassettes) {
            // Resolved here, so the backend is sized for the model that will run and tier routing
            // hands out a concrete id.
            let catalog = entry.catalog_name(id.as_str()).to_string();
            let default_model = model_catalog
                .resolve(&catalog, entry.default_model())
                .map_err(|source| RegistryError::Model {
                    id: id.clone(),
                    source: Box::new(source),
                })?
                .id;
            let models = ConfiguredModels {
                catalog,
                default_model,
            };
            let backend = match entry {
                ProviderConfig::Cassette {
                    path, record_from, ..
                } => Backend::Built(build_cassette(
                    id.clone(),
                    path,
                    record_from.as_ref(),
                    force_record,
                    &entries,
                )?),
                _ => build_one(
                    id.clone(),
                    entry,
                    model_catalog,
                    &models,
                    RegistryInputs {
                        http: http.clone(),
                        auth_resolver: auth_resolver.clone(),
                        output_reserve,
                        wire: wire.clone(),
                    },
                )?,
            };
            entries.insert(
                id.clone(),
                RegisteredProvider {
                    backend,
                    models: Some(models),
                },
            );
        }
        Ok(Self {
            entries: Arc::new(entries),
        })
    }

    /// Returns a registry of every built provider passed through `f` (to wrap it in a circuit
    /// breaker, say), keeping the default models and catalogs. `f` runs once per provider, so a wrapper's
    /// state is one instance per id; calling this twice wraps twice, with two independent states.
    pub fn map_providers<F>(&self, mut f: F) -> Self
    where
        F: FnMut(&ProviderId, Arc<dyn LlmProvider>) -> Arc<dyn LlmProvider>,
    {
        let mapped = self
            .entries
            .iter()
            .map(|(id, e)| {
                (
                    id.clone(),
                    RegisteredProvider {
                        backend: match &e.backend {
                            Backend::Built(provider) => Backend::Built(f(id, provider.clone())),
                            #[cfg(not(feature = "cli"))]
                            disabled @ Backend::FeatureDisabled { .. } => disabled.clone(),
                        },
                        models: e.models.clone(),
                    },
                )
            })
            .collect();
        Self {
            entries: Arc::new(mapped),
        }
    }

    /// Returns the provider `id`.
    ///
    /// # Errors
    ///
    /// [`RegistryError::UnknownProvider`] for an id the registry does not hold;
    /// [`RegistryError::FeatureDisabled`] for one whose backend this build left out.
    pub fn get(&self, id: &ProviderId) -> Result<Arc<dyn LlmProvider>, RegistryError> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| RegistryError::UnknownProvider(id.clone()))?;
        match &entry.backend {
            Backend::Built(provider) => Ok(provider.clone()),
            #[cfg(not(feature = "cli"))]
            Backend::FeatureDisabled { feature } => Err(RegistryError::FeatureDisabled {
                id: id.clone(),
                feature,
            }),
        }
    }

    /// Returns the configured default model of `id`, resolved to a concrete id; `None` when the
    /// registry was not built from config.
    pub fn default_model(&self, id: &ProviderId) -> Option<&str> {
        self.entries
            .get(id)?
            .models
            .as_ref()
            .map(|m| m.default_model.as_str())
    }

    /// Returns the concrete model to bind to provider `id`: `spec` resolved against the catalog
    /// (a concrete id passes through), else the provider's default. A caller resolves before
    /// binding an `LlmService`, so events and cassette fingerprints name the model
    /// that ran. A provider not built from config has no catalog and no default: a concrete id
    /// passes through, and a `@latest` spec or no spec is refused.
    ///
    /// # Errors
    ///
    /// [`RegistryError::UnknownProvider`] for an id the registry does not hold;
    /// [`RegistryError::Model`] for a spec that does not resolve;
    /// [`RegistryError::NoDefaultModel`] for no spec on a provider not built from config.
    pub fn resolve_model(
        &self,
        id: &ProviderId,
        spec: Option<&str>,
    ) -> Result<String, RegistryError> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| RegistryError::UnknownProvider(id.clone()))?;
        let model_error = |source| RegistryError::Model {
            id: id.clone(),
            source: Box::new(source),
        };
        match (spec, &entry.models) {
            (Some(spec), Some(models)) => tars_config::catalog()
                .resolve(&models.catalog, spec)
                .map(|r| r.id)
                .map_err(model_error),
            (Some(spec), None) => {
                match ModelSpec::parse(spec).map_err(|e| model_error(e.into()))? {
                    ModelSpec::Concrete(model) => Ok(model.to_string()),
                    ModelSpec::Latest { series } => {
                        Err(model_error(ResolveModelSpecError::NoSeries {
                            provider: id.to_string(),
                            series: series.to_string(),
                        }))
                    }
                }
            }
            (None, Some(models)) => Ok(models.default_model.clone()),
            (None, None) => Err(RegistryError::NoDefaultModel(id.clone())),
        }
    }

    pub fn ids(&self) -> impl Iterator<Item = &ProviderId> {
        self.entries.keys()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Returns the extra headers and query parameters of a provider that speaks HTTP.
fn http_extras(cfg: &ProviderConfig) -> Option<&HttpOverrides> {
    match cfg {
        ProviderConfig::Openai { extras, .. }
        | ProviderConfig::OpenaiCompat { extras, .. }
        | ProviderConfig::Anthropic { extras, .. }
        | ProviderConfig::Gemini { extras, .. }
        | ProviderConfig::Vllm { extras, .. }
        | ProviderConfig::Mlx { extras, .. }
        | ProviderConfig::Llamacpp { extras, .. } => Some(extras),
        ProviderConfig::ClaudeCli { .. }
        | ProviderConfig::ClaudeSdk { .. }
        | ProviderConfig::CodexCli { .. }
        | ProviderConfig::Opencode { .. }
        | ProviderConfig::Antigravity { .. }
        | ProviderConfig::Mock { .. }
        | ProviderConfig::Cassette { .. } => None,
    }
}

/// What a registry gives every backend it builds alike.
struct RegistryInputs {
    http: Arc<HttpClient>,
    auth_resolver: Arc<dyn AuthResolver>,
    output_reserve: OutputReserve,
    wire: Option<WireRecorder>,
}

/// Builds the provider of one non-cassette [`ProviderConfig`].
fn build_one(
    id: ProviderId,
    cfg: &ProviderConfig,
    model_catalog: &ModelCatalog<'_>,
    models: &ConfiguredModels,
    inputs: RegistryInputs,
) -> Result<Backend, RegistryError> {
    let ConfiguredModels {
        catalog,
        default_model,
    } = models;
    let RegistryInputs {
        http,
        auth_resolver,
        output_reserve,
        wire,
    } = inputs;
    if let Some(extras) = http_extras(cfg) {
        static_headers(extras).map_err(|source| RegistryError::InvalidHeader {
            id: id.clone(),
            source: Box::new(source),
        })?;
    }
    let provider: Arc<dyn LlmProvider> = match cfg {
        ProviderConfig::Openai {
            base_url,
            auth,
            default_model: _,
            extras,
        } => {
            let mut builder = OpenAiProviderBuilder::new(id, auth.clone())
                .extras(extras.clone())
                .capabilities(model_catalog.capabilities_for(catalog, default_model));
            if let Some(url) = base_url {
                builder = builder.base_url(url.clone());
            }
            builder.wire(wire).build(http, auth_resolver)
        }

        ProviderConfig::OpenaiCompat {
            base_url,
            auth,
            default_model: _,
            gen_ai_provider_name,
            extras,
            capabilities,
        } => {
            // A provider the DB names (`deepseek`) gets its capabilities from the DB; one it does
            // not name is undescribed, with the chat template guessed from the URL. What the
            // user declares in the block wins over both.
            let server = CompatServer::at(base_url);
            let mut caps = model_catalog.capabilities_for(catalog, default_model);
            if !tars_config::MODEL_KB.knows(catalog) {
                caps.supports_chat_template_thinking = server.reads_chat_template();
            }
            caps.gen_ai_provider_name = Some(gen_ai_provider_name.clone());
            let caps = capabilities.apply_to(caps);
            let builder = OpenAiProviderBuilder::new(id, auth.clone())
                .base_url(base_url.clone())
                .dialect(server.dialect())
                .extras(extras.clone())
                .capabilities(caps)
                .output_limits(
                    tars_config::OutputLimitRule::catalog(catalog)
                        .configured(capabilities.max_output_tokens),
                );
            builder.wire(wire).build(http, auth_resolver)
        }

        ProviderConfig::Anthropic {
            base_url,
            api_version,
            auth,
            default_model: _,
            extras,
        } => {
            let mut builder = AnthropicProviderBuilder::new(id, auth.clone())
                .extras(extras.clone())
                .output_reserve(output_reserve)
                .capabilities(model_catalog.capabilities_for(catalog, default_model));
            if let Some(url) = base_url {
                builder = builder.base_url(url.clone());
            }
            if let Some(v) = api_version {
                builder = builder.api_version(v.clone());
            }
            builder.wire(wire).build(http, auth_resolver)
        }

        ProviderConfig::Gemini {
            base_url,
            auth,
            default_model: _,
            extras,
        } => {
            let mut builder = GeminiProviderBuilder::new(id, auth.clone())
                .extras(extras.clone())
                .capabilities(model_catalog.capabilities_for(catalog, default_model));
            if let Some(url) = base_url {
                builder = builder.base_url(url.clone());
            }
            builder.wire(wire).build(http, auth_resolver)
        }

        ProviderConfig::Vllm {
            base_url,
            auth,
            default_model: _,
            extras,
            capabilities,
        } => vllm_builder(
            id.clone(),
            base_url.clone(),
            auth.clone(),
            extras.clone(),
            capabilities.clone(),
            model_catalog,
        )
        .wire(wire)
        .build(http, auth_resolver),

        ProviderConfig::Mlx {
            base_url,
            auth,
            default_model: _,
            extras,
            capabilities,
        } => mlx_builder(
            id.clone(),
            base_url.clone(),
            auth.clone(),
            extras.clone(),
            capabilities.clone(),
            model_catalog,
        )
        .wire(wire)
        .build(http, auth_resolver),

        ProviderConfig::Llamacpp {
            base_url,
            auth,
            default_model: _,
            extras,
            capabilities,
        } => llamacpp_builder(
            id.clone(),
            base_url.clone(),
            auth.clone(),
            extras.clone(),
            capabilities.clone(),
            model_catalog,
        )
        .wire(wire)
        .build(http, auth_resolver),

        #[cfg(feature = "cli")]
        ProviderConfig::ClaudeCli {
            executable,
            timeout_secs,
            default_model: _,
            tools,
            bare,
            effort,
            exclude_dynamic_sections,
            extra_args,
        } => {
            use crate::provider::backends::claude_cli::{ClaudeCliEffort, ClaudeToolAccess};
            use tars_config::{ClaudeCliEffortConfig, ClaudeCliToolsConfig, ClaudeCliToolsKeyword};

            let tools_runtime = match tools {
                ClaudeCliToolsConfig::Named(ClaudeCliToolsKeyword::Disabled) => {
                    ClaudeToolAccess::Disabled
                }
                ClaudeCliToolsConfig::Named(ClaudeCliToolsKeyword::Default) => {
                    ClaudeToolAccess::Default
                }
                ClaudeCliToolsConfig::Allow(list) => ClaudeToolAccess::Allow(list.clone()),
            };

            let effort_runtime = effort.as_ref().map(|e| match e {
                ClaudeCliEffortConfig::Low => ClaudeCliEffort::Low,
                ClaudeCliEffortConfig::Medium => ClaudeCliEffort::Medium,
                ClaudeCliEffortConfig::High => ClaudeCliEffort::High,
                ClaudeCliEffortConfig::Xhigh => ClaudeCliEffort::Xhigh,
                ClaudeCliEffortConfig::Max => ClaudeCliEffort::Max,
            });

            ClaudeCliProviderBuilder::new(id)
                .executable(executable.clone())
                .timeout(Duration::from_secs(*timeout_secs))
                .tools(tools_runtime)
                .bare(*bare)
                .effort(effort_runtime)
                .exclude_dynamic_sections(*exclude_dynamic_sections)
                .extra_args(extra_args.clone())
                .capabilities(model_catalog.capabilities_for("claude_cli", ""))
                .wire(wire)
                .build()
        }

        #[cfg(feature = "cli")]
        ProviderConfig::CodexCli {
            executable,
            timeout_secs,
            sandbox,
            skip_git_repo_check,
            default_model: _,
        } => {
            use tars_config::CodexSandboxConfig;
            let runtime_sandbox = match sandbox {
                CodexSandboxConfig::ReadOnly => CodexSandbox::ReadOnly,
                CodexSandboxConfig::WorkspaceWrite => CodexSandbox::WorkspaceWrite,
                CodexSandboxConfig::DangerFullAccess => CodexSandbox::DangerFullAccess,
            };
            CodexCliProviderBuilder::new(id)
                .executable(executable.clone())
                .timeout(Duration::from_secs(*timeout_secs))
                .sandbox(runtime_sandbox)
                .skip_git_repo_check(*skip_git_repo_check)
                .capabilities(model_catalog.capabilities_for("codex_cli", ""))
                .wire(wire)
                .build()
        }

        #[cfg(feature = "cli")]
        ProviderConfig::Opencode {
            executable,
            timeout_secs,
            default_model: _,
        } => {
            let caps = model_catalog.capabilities_for(catalog, default_model);
            let dialect = Arc::new(OpenCodeDialect::new(
                executable.clone(),
                Duration::from_secs(*timeout_secs),
            ));
            let runner = Arc::new(SharedCliRunner::new(dialect.clone()));
            Arc::new(AgentCliBackend::new(id, caps, dialect, runner).wire(wire))
        }

        #[cfg(feature = "cli")]
        ProviderConfig::Antigravity {
            executable,
            timeout_secs,
            default_model: _,
            effort,
        } => {
            use tars_config::AntigravityEffortConfig;
            let effort_runtime = match effort {
                AntigravityEffortConfig::Low => AntigravityEffort::Low,
                AntigravityEffortConfig::High => AntigravityEffort::High,
            };
            let caps = model_catalog.capabilities_for(catalog, default_model);
            let dialect = Arc::new(AntigravityDialect::new(
                executable.clone(),
                Duration::from_secs(*timeout_secs),
                effort_runtime,
            ));
            let runner = Arc::new(SharedCliRunner::new(dialect.clone()));
            Arc::new(AgentCliBackend::new(id, caps, dialect, runner).wire(wire))
        }

        #[cfg(feature = "cli")]
        ProviderConfig::ClaudeSdk {
            executable,
            script_path,
            timeout_secs,
            default_model: _,
        } => {
            let mut b = ClaudeSdkProviderBuilder::new(id)
                .executable(executable.clone())
                .timeout(Duration::from_secs(*timeout_secs))
                .capabilities(model_catalog.capabilities_for("claude_sdk", ""));
            if let Some(sp) = script_path {
                b = b.script_path(sp.clone());
            }
            b.build()
        }

        #[cfg(not(feature = "cli"))]
        ProviderConfig::ClaudeCli { .. }
        | ProviderConfig::CodexCli { .. }
        | ProviderConfig::Opencode { .. }
        | ProviderConfig::Antigravity { .. }
        | ProviderConfig::ClaudeSdk { .. } => {
            return Ok(Backend::FeatureDisabled { feature: "cli" });
        }

        ProviderConfig::Mock { canned_response } => {
            MockProvider::new(id, CannedResponse::Text(canned_response.clone()))
        }

        // `from_config` builds cassettes with `build_cassette`.
        ProviderConfig::Cassette { .. } => {
            unreachable!("cassette providers are built via build_cassette")
        }
    };

    Ok(Backend::Built(provider))
}

/// Reads [`tars_types::env::cassette_force_record`] from `env`. Set but not UTF-8 is logged and
/// read as off, as `TARS_CLAUDE_CLI_STREAM` is.
fn cassette_force_record(env: &dyn EnvSource) -> bool {
    tars_types::env::cassette_force_record(|name| env.var(name)).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "TARS_CASSETTE_RECORD set but unreadable (non-UTF-8); not recording");
        false
    })
}

/// Builds a cassette provider, which replays recorded answers for tests.
///
/// An existing `path` is replayed unless `force_record`. Otherwise, when `record_from` names a
/// built provider, the cassette wraps it and records, writing `path` after each capture; a
/// `record_from` that is not configured, or whose backend this build left out, is refused. Without
/// a file and without `record_from`, it replays nothing, so every request is refused instead of
/// answered with a made-up reply.
///
/// `force_record` keeps a whole session recording: a later registry build in the same session
/// would otherwise find the file and replay, losing every request after the first.
fn build_cassette(
    id: ProviderId,
    path: &str,
    record_from: Option<&ProviderId>,
    force_record: bool,
    built: &HashMap<ProviderId, RegisteredProvider>,
) -> Result<Arc<dyn LlmProvider>, RegistryError> {
    let pb = std::path::PathBuf::from(path);
    if pb.exists() && !force_record {
        let replay = CassetteProvider::replay_from_file(id, &pb).map_err(|source| {
            RegistryError::CassetteUnreadable {
                path: path.to_string(),
                source,
            }
        })?;
        return Ok(replay);
    }
    if let Some(src) = record_from {
        return match built.get(src).map(|e| &e.backend) {
            Some(Backend::Built(inner)) => {
                // Seeded from the file, so a session of several builds adds to it; a file that does not
                // read refuses the recording, since an empty seed would overwrite it.
                let seed = if pb.exists() {
                    CassetteProvider::recordings_from_file(&pb).map_err(|source| {
                        RegistryError::CassetteUnreadable {
                            path: path.to_string(),
                            source,
                        }
                    })?
                } else {
                    HashMap::new()
                };
                tracing::info!(
                    path,
                    record_from = %src,
                    recorded = seed.len(),
                    "cassette recording"
                );
                Ok(CassetteProvider::record_seeded(
                    id,
                    inner.clone(),
                    Some(pb),
                    seed,
                ))
            }
            #[cfg(not(feature = "cli"))]
            Some(Backend::FeatureDisabled { feature }) => Err(RegistryError::FeatureDisabled {
                id: src.clone(),
                feature,
            }),
            None => Err(RegistryError::CassetteSourceMissing {
                cassette: id,
                record_from: src.clone(),
            }),
        };
    }
    Ok(CassetteProvider::replay(id, HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::auth::basic;
    use tars_config::Config;

    fn http() -> Arc<HttpClient> {
        HttpClient::default_arc().unwrap()
    }

    #[test]
    fn empty_registry_is_empty() {
        let r = ProviderRegistry::empty();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn map_providers_preserves_ids_and_count() {
        let cfg = Config::from_toml(
            r#"
            [providers.a]
            type = "mock"
            canned_response = "ok"

            [providers.b]
            type = "mock"
            canned_response = "ok"
            "#,
        )
        .unwrap();
        let reg = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();
        let mapped = reg.map_providers(|_id, p| p);
        assert!(mapped.get(&ProviderId::new("a")).is_ok());
        assert!(mapped.get(&ProviderId::new("b")).is_ok());
    }

    /// `AgentCliBackend` reads a delegate's whole stdout before emitting any event, so no delegate
    /// streams or can be cancelled mid-turn.
    #[cfg(feature = "cli")]
    #[test]
    fn every_cli_delegate_advertises_buffered_caps() {
        let cfg = Config::from_toml(
            r#"
            [providers.claude]
            type = "claude_cli"
            default_model = "opus"

            [providers.codex]
            type = "codex_cli"
            default_model = "gpt-5-codex"

            [providers.opencode]
            type = "opencode"
            default_model = "anthropic/claude-sonnet-4-5"

            [providers.antigravity]
            type = "antigravity"
            default_model = "gemini-2.5-pro"
            "#,
        )
        .unwrap();
        let reg = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();

        for id in ["claude", "codex", "opencode", "antigravity"] {
            let p = reg
                .get(&ProviderId::new(id))
                .unwrap_or_else(|e| panic!("delegate `{id}` should be built: {e}"));
            let caps = p.capabilities();
            assert!(
                !caps.streaming,
                "delegate `{id}` is buffered (emit-after-turn) but advertises streaming: true"
            );
            assert!(
                !caps.supports_cancel,
                "delegate `{id}` cannot be cancelled mid-turn but advertises supports_cancel: true"
            );
        }
    }

    #[tokio::test]
    async fn cassette_records_then_replays_via_config() {
        use crate::provider::LlmProvider;
        use futures::StreamExt;
        use tars_types::{Caller, ChatEvent, ChatRequest};

        let path =
            std::env::temp_dir().join(format!("tars_cassette_cfg_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg_str = format!(
            r#"
            [providers.real]
            type = "mock"
            canned_response = "RECORDED_FINDING"

            [providers.cass]
            type = "cassette"
            path = "{}"
            record_from = "real"
            "#,
            path.display()
        );

        async fn ask(p: Arc<dyn LlmProvider>, prompt: &str) -> String {
            p.stream(
                ChatRequest::user(prompt),
                "test-model",
                Caller::test_default(),
            )
            .await
            .unwrap()
            .filter_map(|e| async move {
                match e {
                    Ok(ChatEvent::Delta { text }) => Some(text),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .await
            .join("")
        }

        // No file yet: records through the mock.
        {
            let cfg = Config::from_toml(&cfg_str).unwrap();
            let reg = ProviderRegistry::from_config(
                cfg.providers(),
                tars_config::catalog(),
                http(),
                OutputReserve::default(),
                basic(),
                None,
            )
            .unwrap();
            let cass = reg.get(&ProviderId::new("cass")).unwrap();
            assert_eq!(ask(cass.clone(), "file-x").await, "RECORDED_FINDING");
            drop(cass);
            drop(reg);
        }
        assert!(path.exists(), "recording run must write the cassette file");

        // The file exists: replays.
        {
            let cfg = Config::from_toml(&cfg_str).unwrap();
            let reg = ProviderRegistry::from_config(
                cfg.providers(),
                tars_config::catalog(),
                http(),
                OutputReserve::default(),
                basic(),
                None,
            )
            .unwrap();
            let cass = reg.get(&ProviderId::new("cass")).unwrap();
            assert_eq!(ask(cass.clone(), "file-x").await, "RECORDED_FINDING");
        }
        let _ = std::fs::remove_file(&path);
    }

    /// A forced recording adds to the cassette it finds instead of flushing over it:
    /// two sessions (two reviewer processes), one file, both recordings replay. A file
    /// that is there but does not read refuses the recording and is left as it was.
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

    #[test]
    fn forced_recording_is_read_from_the_injected_environment() {
        assert!(cassette_force_record(&OneVar("TARS_CASSETTE_RECORD", "1")));
        assert!(cassette_force_record(&OneVar(
            "TARS_CASSETTE_RECORD",
            "TRUE"
        )));
        assert!(!cassette_force_record(&OneVar("TARS_CASSETTE_RECORD", "0")));
        assert!(!cassette_force_record(&OneVar("OTHER", "1")));
    }

    #[tokio::test]
    async fn a_registry_built_over_an_environment_that_forces_recording_records() {
        let dir = std::env::temp_dir().join(format!("tars_cassette_env_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cassette.json");
        std::fs::write(&path, r#"{"recordings": {}}"#).unwrap();
        let cfg = Config::from_toml(&format!(
            "[providers.real]\ntype = \"mock\"\ncanned_response = \"ANSWER\"\n\n\
             [providers.cass]\ntype = \"cassette\"\npath = \"{}\"\nrecord_from = \"real\"\n",
            path.display()
        ))
        .unwrap();
        let reg = ProviderRegistry::from_config_with_env(
            cfg.providers(),
            tars_config::catalog(),
            HttpClientConfig::default(),
            OutputReserve::default(),
            Arc::new(OneVar("TARS_CASSETTE_RECORD", "1")),
            None,
        )
        .unwrap();
        let cass = reg.get(&ProviderId::new("cass")).unwrap();
        let answered = cass
            .stream(
                tars_types::ChatRequest::user("q"),
                "test-model",
                tars_types::Caller::test_default(),
            )
            .await;
        assert!(answered.is_ok(), "a replay of the empty file would miss");
    }

    #[test]
    fn a_cassette_recording_from_an_unconfigured_provider_is_refused() {
        let reg = ProviderRegistry::empty();
        let refused = build_cassette(
            ProviderId::new("cass"),
            "/nonexistent/tars/cassette.json",
            Some(&ProviderId::new("missing")),
            false,
            &reg.entries,
        );
        assert!(
            matches!(refused, Err(RegistryError::CassetteSourceMissing { .. })),
            "{:?}",
            refused.err()
        );
    }

    #[tokio::test]
    async fn a_forced_recording_adds_to_the_cassette_it_finds() {
        use crate::provider::LlmProvider;
        use futures::StreamExt;
        use tars_types::{Caller, ChatEvent, ChatRequest};

        async fn ask(p: Arc<dyn LlmProvider>, prompt: &str) -> String {
            p.stream(
                ChatRequest::user(prompt),
                "test-model",
                Caller::test_default(),
            )
            .await
            .unwrap()
            .filter_map(|e| async move {
                match e {
                    Ok(ChatEvent::Delta { text }) => Some(text),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .await
            .join("")
        }

        let dir = std::env::temp_dir().join(format!("tars_cassette_seed_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cassette.json");
        let _ = std::fs::remove_file(&path);
        let cfg =
            Config::from_toml("[providers.real]\ntype = \"mock\"\ncanned_response = \"ANSWER\"\n")
                .unwrap();
        let reg = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();
        let built = &*reg.entries;
        let p = path.to_str().unwrap();

        for prompt in ["first session", "second session"] {
            let cass = build_cassette(
                ProviderId::new("cass"),
                p,
                Some(&ProviderId::new("real")),
                true,
                built,
            )
            .unwrap();
            assert_eq!(ask(cass, prompt).await, "ANSWER");
        }
        let replay = CassetteProvider::replay_from_file(ProviderId::new("cass"), &path).unwrap();
        for prompt in ["first session", "second session"] {
            assert_eq!(
                ask(replay.clone(), prompt).await,
                "ANSWER",
                "{prompt} was recorded"
            );
        }

        std::fs::write(&path, "not a cassette").unwrap();
        let refused = build_cassette(
            ProviderId::new("cass"),
            p,
            Some(&ProviderId::new("real")),
            true,
            built,
        );
        assert!(matches!(
            refused,
            Err(RegistryError::CassetteUnreadable { .. })
        ));
        let replay_refused = build_cassette(ProviderId::new("cass"), p, None, false, built);
        assert!(matches!(
            replay_refused,
            Err(RegistryError::CassetteUnreadable { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "not a cassette",
            "left as it was"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_static_header_that_makes_no_header_is_refused_at_build() {
        let cfg = Config::from_toml(
            r#"
            [providers.local]
            type = "vllm"
            default_model = "m"
            http_headers = { "bad name" = "v" }
            "#,
        )
        .unwrap();
        let built = ProviderRegistry::from_config_with_env(
            cfg.providers(),
            tars_config::catalog(),
            HttpClientConfig::default(),
            OutputReserve::default(),
            Arc::new(OneVar("", "")),
            None,
        );
        assert!(
            matches!(built, Err(RegistryError::InvalidHeader { ref id, .. }) if id.as_str() == "local"),
            "{:?}",
            built.err()
        );
    }

    #[tokio::test]
    async fn an_env_header_that_makes_no_header_fails_the_call() {
        let cfg = Config::from_toml(
            r#"
            [providers.local]
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "http://127.0.0.1:9/v1"
            default_model = "m"
            env_http_headers = { "X-Org" = "ORG" }
            "#,
        )
        .unwrap();
        let reg = ProviderRegistry::from_config_with_env(
            cfg.providers(),
            tars_config::catalog(),
            HttpClientConfig::default(),
            OutputReserve::default(),
            Arc::new(OneVar("ORG", "org\nsecret")),
            None,
        )
        .unwrap();
        let sent = reg
            .get(&ProviderId::new("local"))
            .unwrap()
            .stream(
                tars_types::ChatRequest::user("q"),
                "m",
                tars_types::Caller::test_default(),
            )
            .await;
        match sent {
            Err(e @ crate::ProviderError::ExtraHeader(_)) => {
                assert!(!tars_utils::display_chain(&e).contains("secret"));
            }
            Err(other) => panic!("expected ExtraHeader, got {other:?}"),
            Ok(_) => panic!("expected ExtraHeader, the call was sent"),
        }
    }

    #[test]
    fn builds_each_supported_variant() {
        let toml_str = r#"
            [providers.openai_main]
            type = "openai"
            auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
            default_model = "gpt-4o"

            [providers.openai_compat_local]
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "http://localhost:8000/v1"
            default_model = "Qwen/Qwen2.5-Coder-7B-Instruct"

            [providers.anthropic_main]
            type = "anthropic"
            auth = { kind = "secret", secret = { source = "env", var = "ANTHROPIC_API_KEY" } }
            default_model = "claude-opus-4-7"

            [providers.gemini_main]
            type = "gemini"
            auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
            default_model = "gemini-2.5-pro"

            [providers.vllm_local]
            type = "vllm"
            default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"

            [providers.mlx_local]
            type = "mlx"
            default_model = "mlx-community/Qwen2.5-Coder-32B-Instruct-4bit"

            [providers.llamacpp_local]
            type = "llamacpp"
            default_model = "Qwen2.5-Coder-7B-Q5_K_M"

            [providers.claude_cli]
            type = "claude_cli"
            default_model = "claude-opus-4-7"

            [providers.codex_cli]
            type = "codex_cli"
            default_model = "gpt-5"

            [providers.mock_test]
            type = "mock"
            canned_response = "hi"
        "#;
        let cfg = Config::from_toml(toml_str).unwrap();
        let reg = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();
        // 10 user-declared + 8 builtins, but one user-declared id
        // (`claude_cli`) collides with a builtin of the same name and
        // overrides it — net 10 + (8 - 1) = 17.
        assert_eq!(reg.len(), 17);
        assert!(reg.get(&ProviderId::new("openai_main")).is_ok());
        assert!(reg.get(&ProviderId::new("openai_compat_local")).is_ok());
        assert!(reg.get(&ProviderId::new("anthropic_main")).is_ok());
        assert!(reg.get(&ProviderId::new("gemini_main")).is_ok());
        assert!(reg.get(&ProviderId::new("vllm_local")).is_ok());
        assert!(reg.get(&ProviderId::new("mlx_local")).is_ok());
        assert!(reg.get(&ProviderId::new("llamacpp_local")).is_ok());
        #[cfg(feature = "cli")]
        assert!(reg.get(&ProviderId::new("claude_cli")).is_ok());
        #[cfg(feature = "cli")]
        assert!(reg.get(&ProviderId::new("codex_cli")).is_ok());
        assert!(reg.get(&ProviderId::new("mock_test")).is_ok());
    }

    #[tokio::test]
    async fn mock_provider_built_from_config_actually_responds() {
        use tars_types::{Caller, ChatRequest};

        let cfg = Config::from_toml(
            r#"
            [providers.greeter]
            type = "mock"
            canned_response = "hello from config"
        "#,
        )
        .unwrap();
        let reg = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();
        let provider = reg.get(&ProviderId::new("greeter")).unwrap();
        let resp = provider
            .complete(
                ChatRequest::user("ping"),
                "test-model",
                Caller::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "hello from config");
    }

    #[cfg(feature = "cli")]
    #[test]
    fn builds_opencode_and_antigravity_cli_delegates() {
        let cfg = Config::from_toml(
            r#"
            [providers.oc]
            type = "opencode"
            default_model = "anthropic/claude-sonnet-4-5"

            [providers.agy]
            type = "antigravity"
            default_model = "gemini-2.5-pro"
            "#,
        )
        .unwrap();
        let reg = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();
        assert!(reg.get(&ProviderId::new("oc")).is_ok());
        assert!(reg.get(&ProviderId::new("agy")).is_ok());
        assert_eq!(
            reg.default_model(&ProviderId::new("oc")),
            Some("anthropic/claude-sonnet-4-5")
        );
        assert_eq!(
            reg.default_model(&ProviderId::new("agy")),
            Some("gemini-2.5-pro")
        );
    }

    /// A `<series>@latest` default resolves at build, and a caller's spec
    /// resolves through `resolve_model`. The catalog is the process-global
    /// one — it may carry this machine's refreshed model list — so the
    /// assertions are about the shape of the answer, not a specific id.
    #[test]
    fn latest_specs_resolve_to_concrete_ids() {
        let cfg = Config::from_toml(
            r#"
            [providers.gemini_flash]
            type = "gemini"
            auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
            default_model = "flash@latest"

            [providers.local]
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "http://localhost:8000/v1"
            default_model = "qwen/qwen3-coder-30b"
            "#,
        )
        .unwrap();
        let reg = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();
        let flash = ProviderId::new("gemini_flash");
        let default = reg.default_model(&flash).unwrap();
        assert!(
            default.starts_with("gemini-") && default.ends_with("-flash"),
            "flash@latest must resolve to a concrete gemini flash id, got {default}"
        );
        assert_eq!(reg.resolve_model(&flash, None).unwrap(), default);
        let pro = reg.resolve_model(&flash, Some("pro@latest")).unwrap();
        assert!(pro.contains("-pro"), "got {pro}");
        assert_eq!(
            reg.resolve_model(&flash, Some("gemini-2.5-flash")).unwrap(),
            "gemini-2.5-flash"
        );
        assert!(matches!(
            reg.resolve_model(&flash, Some("flsh@latest")),
            Err(RegistryError::Model { .. })
        ));
        // A concrete local model passes through; a series spec there has
        // nothing to resolve against.
        let local = ProviderId::new("local");
        assert_eq!(reg.default_model(&local), Some("qwen/qwen3-coder-30b"));
        assert!(matches!(
            reg.resolve_model(&local, Some("flash@latest")),
            Err(RegistryError::Model { .. })
        ));
        assert!(matches!(
            reg.resolve_model(&ProviderId::new("nope"), None),
            Err(RegistryError::UnknownProvider(_))
        ));
    }

    #[test]
    fn a_provider_not_built_from_config_binds_a_concrete_model_and_refuses_the_rest() {
        let id = ProviderId::new("given");
        let reg = ProviderRegistry::single(
            id.clone(),
            MockProvider::new(ProviderId::new("given"), CannedResponse::text("ok")),
        );
        assert_eq!(reg.resolve_model(&id, Some("m")).unwrap(), "m");
        assert_eq!(reg.resolve_model(&id, Some(" m ")).unwrap(), "m");
        assert!(matches!(
            reg.resolve_model(&id, Some("flash@latest")),
            Err(RegistryError::Model { ref source, .. })
                if matches!(**source, ResolveModelSpecError::NoSeries { .. })
        ));
        assert!(matches!(
            reg.resolve_model(&id, Some(" ")),
            Err(RegistryError::Model { ref source, .. })
                if matches!(
                    **source,
                    ResolveModelSpecError::Parse(tars_config::ParseModelSpecError::Blank { .. })
                )
        ));
        assert!(matches!(
            reg.resolve_model(&id, None),
            Err(RegistryError::NoDefaultModel(ref got)) if *got == id
        ));
        assert!(matches!(
            reg.resolve_model(&ProviderId::new("nope"), Some("m")),
            Err(RegistryError::UnknownProvider(_))
        ));
    }

    fn gen_ai_provider_name_of(toml: &str, id: &str) -> Option<String> {
        let cfg = Config::from_toml(toml).unwrap();
        let registry = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();
        registry
            .get(&id.parse::<ProviderId>().unwrap())
            .unwrap()
            .capabilities()
            .gen_ai_provider_name
            .clone()
    }

    #[test]
    fn a_block_of_a_type_the_data_file_describes_serves_as_the_data_file_says() {
        let got = gen_ai_provider_name_of(
            r#"
            [providers.gateway]
            type = "anthropic"
            auth = { kind = "secret", secret = { source = "env", var = "ANTHROPIC_API_KEY" } }
            default_model = "claude-sonnet-4-5"
            "#,
            "gateway",
        );
        assert_eq!(got.as_deref(), Some("anthropic"));
    }

    #[test]
    fn an_openai_compat_block_serves_as_the_name_it_gives() {
        let got = gen_ai_provider_name_of(
            r#"
            [providers.fast]
            type = "openai_compat"
            gen_ai_provider_name = "groq"
            base_url = "https://api.groq.com/openai/v1"
            default_model = "llama-3.3-70b"
            "#,
            "fast",
        );
        assert_eq!(got.as_deref(), Some("groq"));
    }

    #[test]
    fn tool_use_declared_off_on_a_block_the_data_file_says_calls_in_parallel_builds_without_either()
    {
        let cfg = Config::from_toml(
            r#"
            [providers.xai]
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "https://api.x.ai/v1"
            default_model = "grok-4.3"
            supports_tool_use = false
            "#,
        )
        .unwrap();
        let registry = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();
        let caps = registry
            .get(&ProviderId::new("xai"))
            .unwrap()
            .capabilities()
            .clone();
        assert_eq!(caps.tool_use.support(), tars_types::Support::Unsupported);
        assert!(!caps.tool_use.parallel());
    }

    #[test]
    fn the_config_type_not_the_url_says_whether_a_server_reads_the_chat_template() {
        let cfg = Config::from_toml(
            r#"
            [providers.gateway]
            type = "openai"
            base_url = "https://llm-gateway.corp.example/openai/v1"
            auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
            default_model = "gpt-4o"

            [providers.local]
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "http://localhost:8000/v1"
            default_model = "qwen3"

            [providers.vllm]
            type = "vllm"
            default_model = "qwen3"

            [providers.xai]
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "http://localhost:9000/v1"
            default_model = "grok-4.5"
            "#,
        )
        .unwrap();
        let reg = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();
        let reads = |id: &str| {
            reg.get(&id.parse::<ProviderId>().unwrap())
                .unwrap()
                .capabilities()
                .supports_chat_template_thinking
        };
        assert!(!reads("gateway"));
        assert!(reads("local"));
        assert!(reads("vllm"));
        // The data file's `xai` block wins over a URL that looks self-hosted.
        assert!(!reads("xai"));
    }

    #[test]
    fn a_default_spec_that_does_not_resolve_fails_the_build() {
        let cfg = Config::from_toml(
            r#"
            [providers.gemini_flash]
            type = "gemini"
            auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
            default_model = "no-such-series@latest"
            "#,
        )
        .unwrap();
        match ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        ) {
            Err(RegistryError::Model { id, source }) => {
                assert_eq!(id.as_str(), "gemini_flash");
                assert!(source.to_string().contains("no-such-series"), "{source}");
            }
            Err(other) => panic!("expected RegistryError::Model, got {other:?}"),
            Ok(_) => panic!("expected the build to fail"),
        }
    }

    #[test]
    fn an_unknown_provider_id_is_refused() {
        let cfg = Config::from_toml(
            r#"
            [providers.x]
            type = "mock"
            canned_response = "ok"
        "#,
        )
        .unwrap();
        let reg = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();
        assert!(matches!(
            reg.get(&ProviderId::new("does_not_exist")),
            Err(RegistryError::UnknownProvider(id)) if id.as_str() == "does_not_exist"
        ));
    }

    /// The built-in `claude_cli` is in every config, so a build without `cli` that refused it at
    /// build time could build no registry at all.
    #[cfg(not(feature = "cli"))]
    #[test]
    fn without_cli_a_registry_builds_and_refuses_a_cli_provider_only_when_asked_for_it() {
        let cfg = Config::from_toml(
            r#"
            [providers.m]
            type = "mock"
            canned_response = "ok"
        "#,
        )
        .unwrap();
        let reg = ProviderRegistry::from_config(
            cfg.providers(),
            tars_config::catalog(),
            http(),
            OutputReserve::default(),
            basic(),
            None,
        )
        .unwrap();
        assert!(reg.get(&ProviderId::new("m")).is_ok());
        assert!(matches!(
            reg.get(&ProviderId::new("claude_cli")),
            Err(RegistryError::FeatureDisabled { id, feature: "cli" }) if id.as_str() == "claude_cli"
        ));
    }
}
