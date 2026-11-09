//! Answers which models exist and what their limits are, from the shipped `data/provider.toml`
//! ([`MODEL_KB`]) with the live `<home>/models.json` ([`ModelLibrary`]) laid over it.
//!
//! What each layer owns:
//!
//! | fact                         | live (API, per refresh) | shipped (docs, per release) |
//! |------------------------------|-------------------------|-----------------------------|
//! | which ids exist (`@latest`)  | wins when present       | used when no live list      |
//! | context / output limit       | wins when the API says  | used otherwise              |
//! | price, thinking mode, tier   | —                       | only source                 |
//!
//! Every answer carries its [`CatalogSource`], so a caller can say where a number came from.
//!
//! [`crate::init_tars`] installs the process-global catalog from its home. Until one is installed,
//! a reader gets the catalog of [`crate::resolve_home`], loaded on first use and kept apart from
//! the installed one, so reading early never stops a later install.

use std::path::Path;
use std::sync::OnceLock;

use tars_types::{CatalogSource, OutputLimit, ProviderProfile};

use crate::model_kb::{MODEL_KB, ModelKb};
use crate::model_library::{Listing, ModelLibrary, library_path};
use crate::model_spec::{ModelSpec, ResolveModelSpecError, ResolvedModel};

#[derive(Debug)]
pub struct ModelCatalog<'kb> {
    kb: &'kb ModelKb,
    live: Option<ModelLibrary>,
}

impl<'kb> ModelCatalog<'kb> {
    /// Returns a catalog over `kb`, with `live` laid over it when present.
    pub fn new(kb: &'kb ModelKb, live: Option<ModelLibrary>) -> Self {
        Self { kb, live }
    }

    /// Returns the shipped layer.
    pub fn kb(&self) -> &'kb ModelKb {
        self.kb
    }

    /// Returns the live layer, or `None` when no refresh has been recorded.
    pub fn library(&self) -> Option<&ModelLibrary> {
        self.live.as_ref()
    }

    /// Returns the freshest live list for catalog block `provider`.
    pub fn live_list(&self, provider: &str) -> Option<&Listing> {
        self.live
            .as_ref()
            .and_then(|lib| lib.freshest_by_catalog().get(provider).copied())
    }

    fn shipped_source(&self) -> CatalogSource {
        CatalogSource::Shipped {
            verified: self.kb.verified.clone(),
        }
    }

    /// Resolves `spec` for catalog block `provider`. A concrete id passes through untouched, even
    /// for a provider the catalog does not name (a local server's model); a `@latest` spec picks
    /// from the live id list when one was recorded, else from the shipped rows.
    pub fn resolve(
        &self,
        provider: &str,
        spec: &str,
    ) -> Result<ResolvedModel, ResolveModelSpecError> {
        let Some(def) = self.kb.providers.get(provider) else {
            return match ModelSpec::parse(spec)? {
                ModelSpec::Concrete(id) => Ok(ResolvedModel {
                    id: id.to_string(),
                    picked_from: None,
                }),
                ModelSpec::Latest { series } => Err(ResolveModelSpecError::NoSeries {
                    provider: provider.to_string(),
                    series: series.to_string(),
                }),
            };
        };
        match self.live_list(provider) {
            Some(live) => def.resolve_among(
                provider,
                spec,
                live.ids(),
                CatalogSource::Live {
                    queried_at: live.queried_at.clone(),
                },
            ),
            None => def.resolve_among(
                provider,
                spec,
                def.models.iter().map(|m| m.id.as_str()),
                self.shipped_source(),
            ),
        }
    }

    /// Returns the output ceiling of the concrete id `model` on catalog block `provider`: the
    /// live limit when the API reported one, else the shipped row's, else
    /// [`OutputLimit::Unknown`].
    pub fn output_limit(&self, provider: &str, model: &str) -> OutputLimit {
        match self.limit(provider, model, |l| l.max_output, |m| m.max_output) {
            Some((tokens, source)) => OutputLimit::Model { tokens, source },
            None => OutputLimit::Unknown,
        }
    }

    /// Returns the context window of `model` on catalog block `provider`, by the same precedence
    /// as [`Self::output_limit`].
    pub fn context_limit(&self, provider: &str, model: &str) -> Option<(u32, CatalogSource)> {
        self.limit(provider, model, |l| l.context, |m| m.context)
    }

    fn limit(
        &self,
        provider: &str,
        model: &str,
        live_field: impl Fn(&crate::model_library::LiveModel) -> Option<u64>,
        shipped_field: impl Fn(&crate::model_kb::ModelEntry) -> Option<u64>,
    ) -> Option<(u32, CatalogSource)> {
        let row = self
            .kb
            .providers
            .get(provider)
            .and_then(|def| def.find_model(model));
        // A live row is keyed by the id the API lists; a caller may name the
        // model by a shipped alias of it.
        if let Some(live) = self.live_list(provider) {
            let ids = std::iter::once(model).chain(row.map(|r| r.id.as_str()));
            for id in ids {
                if let Some(n) = live.model(id).and_then(&live_field) {
                    return Some((
                        clamp_u32(n),
                        CatalogSource::Live {
                            queried_at: live.queried_at.clone(),
                        },
                    ));
                }
            }
        }
        row.and_then(shipped_field)
            .map(|n| (clamp_u32(n), self.shipped_source()))
    }

    /// Returns the [`ProviderProfile`] of `model` on catalog block `provider`, the shipped profile
    /// with live limits laid over it. An empty `model` means the provider's default; when that
    /// does not resolve, the profile is built for an empty model id.
    pub fn capabilities_for(&self, provider: &str, model: &str) -> ProviderProfile {
        let model = if model.is_empty() {
            self.kb
                .default_model(provider)
                .and_then(|spec| self.resolve(provider, spec).ok())
                .map(|r| r.id)
                .unwrap_or_default()
        } else {
            model.to_string()
        };
        let mut caps = self.kb.capabilities_for(provider, &model);
        if self.live_list(provider).is_some() {
            if let OutputLimit::Model { tokens, .. } = self.output_limit(provider, &model) {
                caps.max_output_tokens = Some(tokens);
            }
            if let Some((tokens, _)) = self.context_limit(provider, &model) {
                caps.max_context_tokens = Some(tokens);
            }
        }
        caps
    }
}

fn clamp_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Decides one provider instance's output ceiling for a model: its configured
/// `max_output_tokens` when set, else the catalog's number for that model. It is asked per model
/// because one instance serves any model a caller binds to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputLimitRule {
    pub catalog: String,
    pub configured: Option<u32>,
}

impl OutputLimitRule {
    /// Returns a rule that only looks the catalog up.
    pub fn catalog(catalog: impl Into<String>) -> Self {
        Self {
            catalog: catalog.into(),
            configured: None,
        }
    }

    /// Returns the rule with the instance's configured `max_output_tokens`.
    pub fn configured(mut self, tokens: Option<u32>) -> Self {
        self.configured = tokens;
        self
    }

    /// Returns the ceiling for `model`, consulting the process-global catalog when nothing is
    /// configured.
    pub fn output_limit(&self, model: &str) -> OutputLimit {
        match self.configured {
            Some(tokens) => OutputLimit::Configured { tokens },
            None => catalog().output_limit(&self.catalog, model),
        }
    }
}

static CATALOG: OnceLock<ModelCatalog<'static>> = OnceLock::new();

/// The default home's catalog, for readers that come before an install (a provider built
/// before `init`, a config installed with `Config::set`).
static UNINSTALLED: OnceLock<ModelCatalog<'static>> = OnceLock::new();

/// Says why the live layer could not be installed.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("model catalog already installed — install it once, at startup")]
    AlreadyInstalled,
    #[error("reading the model library {path}")]
    Library {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

/// Loads `<home>/models.json` over the shipped knowledge base; a missing file gives the shipped
/// layer alone.
///
/// # Errors
///
/// Returns [`CatalogError::Library`] when the file exists but cannot be read or parsed.
pub fn load_catalog(home: &Path) -> Result<ModelCatalog<'static>, CatalogError> {
    let path = library_path(home);
    let live =
        ModelLibrary::load(&path).map_err(|source| CatalogError::Library { path, source })?;
    Ok(ModelCatalog::new(&MODEL_KB, live))
}

/// Installs `catalog` as the process-global catalog.
///
/// # Errors
///
/// Returns [`CatalogError::AlreadyInstalled`] on a second call.
pub fn install_catalog(catalog: ModelCatalog<'static>) -> Result<(), CatalogError> {
    CATALOG
        .set(catalog)
        .map_err(|_| CatalogError::AlreadyInstalled)
}

/// Returns the installed catalog, or before any install the default home's, loaded on first use.
/// A library that exists but cannot be read is logged with its path and error, and the shipped
/// layer is used alone, so every LLM call still runs.
pub fn catalog() -> &'static ModelCatalog<'static> {
    CATALOG.get().unwrap_or_else(uninstalled_catalog)
}

fn uninstalled_catalog() -> &'static ModelCatalog<'static> {
    UNINSTALLED.get_or_init(|| {
        let Some(home) = crate::resolve_home(None) else {
            return ModelCatalog::new(&MODEL_KB, None);
        };
        match load_catalog(&home) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    error = %tars_utils::display_chain(&e),
                    "model catalog: using shipped provider.toml only"
                );
                ModelCatalog::new(&MODEL_KB, None)
            }
        }
    })
}

/// Returns the [`ProviderProfile`] of `model` on catalog block `provider` from the process-global
/// catalog.
pub fn capabilities_for(provider: &str, model: &str) -> ProviderProfile {
    catalog().capabilities_for(provider, model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_library::{LiveModel, ModelLibrary, tests::listed};

    const LIVE_AT: &str = "2026-09-14T00:00:00Z";

    fn live(catalog: &str, models: Vec<LiveModel>) -> ModelLibrary {
        let mut lib = ModelLibrary::new(LIVE_AT.into());
        lib.providers
            .insert(format!("{catalog}_x"), listed(catalog, models, LIVE_AT));
        lib
    }

    fn live_at() -> CatalogSource {
        CatalogSource::Live {
            queried_at: LIVE_AT.into(),
        }
    }

    #[test]
    fn latest_picks_from_the_live_list_when_there_is_one() {
        let shipped = ModelCatalog::new(&MODEL_KB, None);
        let r = shipped.resolve("gemini", "flash@latest").unwrap();
        assert_eq!(r.id, "gemini-3.8-flash");
        assert!(matches!(r.picked_from, Some(CatalogSource::Shipped { .. })));

        // A model released after this tars build: only the live list has it.
        let cat = ModelCatalog::new(
            &MODEL_KB,
            Some(live(
                "gemini",
                vec![
                    LiveModel::id_only("gemini-3.8-flash"),
                    LiveModel::id_only("gemini-3.9-flash"),
                ],
            )),
        );
        let r = cat.resolve("gemini", "flash@latest").unwrap();
        assert_eq!(r.id, "gemini-3.9-flash");
        assert_eq!(r.picked_from, Some(live_at()));
    }

    #[test]
    fn concrete_ids_pass_through_even_without_a_block() {
        let cat = ModelCatalog::new(&MODEL_KB, None);
        let r = cat
            .resolve("qwen_coder_local", "qwen/qwen3-coder-30b")
            .unwrap();
        assert_eq!(r.id, "qwen/qwen3-coder-30b");
        assert_eq!(r.picked_from, None);
        assert!(matches!(
            cat.resolve("qwen_coder_local", "flash@latest"),
            Err(ResolveModelSpecError::NoSeries { .. })
        ));
        assert!(matches!(
            cat.resolve("gemini", "flsh@latest"),
            Err(ResolveModelSpecError::UnknownSeries { .. })
        ));
    }

    #[test]
    fn output_limit_live_then_shipped_then_unknown() {
        let shipped = ModelCatalog::new(&MODEL_KB, None);
        assert_eq!(
            shipped.output_limit("deepseek", "deepseek-flash"),
            OutputLimit::Model {
                tokens: 393_216,
                source: CatalogSource::Shipped {
                    verified: MODEL_KB.verified.clone()
                },
            }
        );
        // Named by its shipped alias.
        assert_eq!(
            shipped
                .output_limit("deepseek", "deepseek-v4-flash")
                .tokens(),
            Some(393_216)
        );
        assert_eq!(
            shipped.output_limit("qwen_coder_local", "qwen/qwen3-coder-30b"),
            OutputLimit::Unknown
        );

        let cat = ModelCatalog::new(
            &MODEL_KB,
            Some(live(
                "gemini",
                vec![
                    LiveModel {
                        id: "gemini-3.8-flash".into(),
                        context: Some(2_000_000),
                        max_output: Some(100_000),
                    },
                    LiveModel {
                        id: "gemini-3.9-flash".into(),
                        context: None,
                        max_output: Some(131_072),
                    },
                ],
            )),
        );
        assert_eq!(
            cat.output_limit("gemini", "gemini-3.8-flash"),
            OutputLimit::Model {
                tokens: 100_000,
                source: live_at()
            }
        );
        // Live-only model: its API limit is all there is.
        assert_eq!(
            cat.output_limit("gemini", "gemini-3.9-flash").tokens(),
            Some(131_072)
        );
        // Not in the live list: the shipped row's number.
        assert!(matches!(
            cat.output_limit("gemini", "gemini-3.1-pro-preview"),
            OutputLimit::Model {
                tokens: 65_536,
                source: CatalogSource::Shipped { .. }
            }
        ));

        let caps = cat.capabilities_for("gemini", "gemini-3.8-flash");
        assert_eq!(caps.max_output_tokens, Some(100_000));
        assert_eq!(caps.max_context_tokens, Some(2_000_000));
        // An empty model is the default `flash@latest`, which the live list resolves to 3.9.
        assert_eq!(
            cat.capabilities_for("gemini", "").max_output_tokens,
            Some(131_072)
        );
    }

    #[test]
    fn a_missing_library_is_the_shipped_layer() {
        let dir = tempfile::tempdir().unwrap();
        let cat = load_catalog(dir.path()).unwrap();
        assert!(cat.library().is_none());
    }
}
