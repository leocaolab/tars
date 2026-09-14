//! The model catalog the runtime reads: the shipped `data/provider.toml`
//! ([`MODEL_KB`]) with the live `$TARS_HOME/models.json`
//! ([`ModelLibrary`]) laid over it.
//!
//! What each layer owns:
//!
//! | fact                         | live (API, per refresh) | shipped (docs, per release) |
//! |------------------------------|-------------------------|-----------------------------|
//! | which ids exist (`@latest`)  | wins when present       | used when no live list      |
//! | context / output limit       | wins when the API says  | used otherwise              |
//! | price, thinking mode, tier   | —                       | only source                 |
//!
//! Every answer carries its [`CatalogSource`], so a caller can say where a
//! number came from.
//!
//! The process-global catalog is installed by [`crate::init_tars`] from its
//! home, or — for a composition root that installs its config through
//! `Config::set` — loaded on first use from the default home
//! ([`crate::resolve_home`]).

use std::path::Path;
use std::sync::OnceLock;

use tars_types::{CatalogSource, OutputLimit, ProviderProfile};

use crate::model_kb::{MODEL_KB, ModelKb};
use crate::model_library::{ModelLibrary, ProviderEntry, library_path};
use crate::model_spec::{ModelSpec, ModelSpecError, ResolvedModel};

/// Shipped KB + live library.
#[derive(Debug)]
pub struct ModelCatalog<'kb> {
    kb: &'kb ModelKb,
    live: Option<ModelLibrary>,
}

impl<'kb> ModelCatalog<'kb> {
    /// A catalog over `kb`, with `live` laid over it when present.
    pub fn new(kb: &'kb ModelKb, live: Option<ModelLibrary>) -> Self {
        Self { kb, live }
    }

    /// The shipped layer.
    pub fn kb(&self) -> &'kb ModelKb {
        self.kb
    }

    /// The live layer, if a refresh has been recorded.
    pub fn library(&self) -> Option<&ModelLibrary> {
        self.live.as_ref()
    }

    /// The freshest live list for catalog block `provider`.
    pub fn live_list(&self, provider: &str) -> Option<&ProviderEntry> {
        self.live
            .as_ref()
            .and_then(|lib| lib.freshest_by_catalog().get(provider).copied())
    }

    fn shipped_source(&self) -> CatalogSource {
        CatalogSource::Shipped {
            verified: self.kb.verified.clone(),
        }
    }

    /// Resolve `spec` (a concrete id or `<series>@latest`) for catalog block
    /// `provider`. A concrete id passes through untouched — even for a
    /// provider the catalog doesn't name (a local server's model). A
    /// `@latest` spec picks from the live id list when one was recorded, else
    /// from the shipped rows.
    pub fn resolve(&self, provider: &str, spec: &str) -> Result<ResolvedModel, ModelSpecError> {
        let Some(def) = self.kb.providers.get(provider) else {
            // No block: a concrete id is fine; there is no series to search.
            return match ModelSpec::parse(spec)? {
                ModelSpec::Concrete(id) => Ok(ResolvedModel {
                    id: id.to_string(),
                    picked_from: None,
                }),
                ModelSpec::Latest { series } => Err(ModelSpecError::NoSeries {
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

    /// The output ceiling of `model` (a concrete id) on catalog block
    /// `provider`: the live limit when the API reported one, else the shipped
    /// row's, else [`OutputLimit::Unknown`].
    pub fn output_limit(&self, provider: &str, model: &str) -> OutputLimit {
        match self.limit(provider, model, |l| l.max_output, |m| m.max_output) {
            Some((tokens, source)) => OutputLimit::Model { tokens, source },
            None => OutputLimit::Unknown,
        }
    }

    /// The context window of `model` on catalog block `provider`, same
    /// precedence as [`Self::output_limit`].
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

    /// Runtime [`ProviderProfile`] for `model` (a concrete id, or empty for
    /// the provider's default) on catalog block `provider`: the shipped
    /// assembly with live limits laid over it.
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

/// How one provider instance answers "what is the output ceiling of model X":
/// its own config's `max_output_tokens` when set, else the model catalog's
/// number for X in the instance's catalog block. Asked per model, because one
/// instance serves any model a caller binds to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputLimitRule {
    /// The `data/provider.toml` block the instance's models belong to.
    pub catalog: String,
    /// `max_output_tokens` from the instance's config.
    pub configured: Option<u32>,
}

impl OutputLimitRule {
    /// Catalog lookups only, nothing configured.
    pub fn catalog(catalog: impl Into<String>) -> Self {
        Self {
            catalog: catalog.into(),
            configured: None,
        }
    }

    /// With the instance's configured `max_output_tokens`.
    pub fn configured(mut self, tokens: Option<u32>) -> Self {
        self.configured = tokens;
        self
    }

    /// The ceiling for `model`, from the process-global catalog.
    pub fn output_limit(&self, model: &str) -> OutputLimit {
        match self.configured {
            Some(tokens) => OutputLimit::Configured { tokens },
            None => catalog().output_limit(&self.catalog, model),
        }
    }
}

static CATALOG: OnceLock<ModelCatalog<'static>> = OnceLock::new();

/// Why the live layer could not be installed.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("model catalog already installed — install it once, at startup")]
    AlreadyInstalled,
    #[error("reading the model library {path}: {source}")]
    Library {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

/// Load `<home>/models.json` over the shipped KB. A missing file is not an
/// error — the catalog is then the shipped layer alone.
pub fn load_catalog(home: &Path) -> Result<ModelCatalog<'static>, CatalogError> {
    let path = library_path(home);
    let live =
        ModelLibrary::load(&path).map_err(|source| CatalogError::Library { path, source })?;
    Ok(ModelCatalog::new(&MODEL_KB, live))
}

/// Install the process-global catalog from `home`. Once only.
pub fn install_catalog(home: &Path) -> Result<(), CatalogError> {
    let catalog = load_catalog(home)?;
    CATALOG
        .set(catalog)
        .map_err(|_| CatalogError::AlreadyInstalled)
}

/// The process-global catalog. If nothing installed one, it is loaded here
/// from the default home; a library that exists but can't be read is logged
/// with its path and error and the shipped layer is used alone — model limits
/// are then the documented ones, and every LLM call still runs.
pub fn catalog() -> &'static ModelCatalog<'static> {
    CATALOG.get_or_init(|| {
        let Some(home) = crate::resolve_home(None) else {
            return ModelCatalog::new(&MODEL_KB, None);
        };
        match load_catalog(&home) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "model catalog: using shipped provider.toml only");
                ModelCatalog::new(&MODEL_KB, None)
            }
        }
    })
}

/// Runtime [`ProviderProfile`] for catalog block `provider` + `model` from the
/// process-global catalog. The one assembler behind every backend
/// constructor.
pub fn capabilities_for(provider: &str, model: &str) -> ProviderProfile {
    catalog().capabilities_for(provider, model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_library::{LiveModel, ModelLibrary, tests::entry};

    const LIVE_AT: &str = "2026-09-14T00:00:00Z";

    fn live(catalog: &str, models: Vec<LiveModel>) -> ModelLibrary {
        let mut lib = ModelLibrary::new(LIVE_AT.into());
        let mut e = entry(catalog, &[], LIVE_AT);
        e.models = models;
        lib.providers.insert(format!("{catalog}_x"), e);
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
            Err(ModelSpecError::NoSeries { .. })
        ));
        assert!(matches!(
            cat.resolve("gemini", "flsh@latest"),
            Err(ModelSpecError::UnknownSeries { .. })
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
        // Listed live without a limit → the shipped row's number.
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
        // Empty model = the provider default, `flash@latest` resolved against
        // the live list — the live-only 3.9.
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
