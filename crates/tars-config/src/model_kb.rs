//! Parses `data/provider.toml`, compiled into the binary, into the shipped layer of the model
//! catalog: each provider's interface, billing and capabilities, and each model's id, prices,
//! limits and thinking mode.
//!
//! These change faster than tars releases, so they are data in that file rather than literals in
//! `builtin.rs` or a substring test in a backend adapter; the one model literal left in
//! `builtin.rs` is each local server's placeholder, which the user replaces. The live layer that
//! `tars models update` refreshes sits on top of it in [`crate::model_catalog`].

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use tars_types::{
    CatalogSource, InterfaceKind, Modalities, Modality, Pricing, PromptCacheKind, ProviderProfile,
    Rate, StructuredOutputMode, Support, ToolUse,
};

use crate::model_spec::{ModelSpec, ResolveModelSpecError, ResolvedModel, SeriesPattern};

/// Says whether a model reasons and whether "off" is a legal request. `Only` models reject an
/// off signal: Gemini `*-pro` answers HTTP 400 to `thinkingBudget: 0`. `param` is set on Gemini
/// rows only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    None,
    Optional { param: Option<ThinkingParam> },
    Only { param: Option<ThinkingParam> },
}

/// Names the request field that controls a Gemini model's thinking; other providers ignore it.
///
/// Gemini 2.5 takes a numeric `thinkingBudget` (0 off, -1 dynamic, N tokens). Gemini 3.x takes a
/// string `thinking_level` (`minimal` is off); it still accepts `thinkingBudget: 0` only as a
/// deprecated form, so 3.x rows use `Level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingParam {
    Budget,
    Level,
}

/// The capability tier of a model within its family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Flagship,
    Mid,
    Fast,
    Lite,
    Coding,
    Reasoning,
    Legacy,
}

/// The availability of a model on the provider's API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    Ga,
    Preview,
    /// `retire` holds the ISO retire date when the provider announced one; nothing acts on it.
    Deprecated {
        retire: Option<String>,
    },
}

/// One model row of `provider.toml`. Prices are USD per million tokens, and a deprecated or
/// legacy row may omit them.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "ModelRow")]
pub struct ModelEntry {
    pub id: String,
    pub tier: ModelTier,
    pub input: Option<Rate>,
    pub output: Option<Rate>,
    pub cached_input: Option<Rate>,
    /// Prices writing a token into the prompt cache; `None` where the provider does not bill
    /// cache writes separately. Anthropic bills a 5-minute write at 1.25× base input and a 1-hour
    /// write at 2×; this holds the 5-minute rate, the API's default TTL.
    ///
    /// It cannot be derived from `input` across providers, and a missing rate is a large
    /// under-bill: on one week of a real Anthropic workload cache writes were 1.5% of tokens but
    /// about 15% of the bill.
    pub cache_write: Option<Rate>,
    /// Is the context window in input tokens.
    pub context: Option<u64>,
    pub max_output: Option<u64>,
    pub thinking: Thinking,
    pub modalities: Vec<Modality>,
    pub status: ModelStatus,
    /// Lists other ids that find this row (legacy names, dated snapshots).
    pub aliases: Vec<String>,
}

/// A model row as the data file spells it: flat keys, where `thinking_param` and `retire` belong
/// to one value of `thinking` and `status`.
#[derive(Deserialize)]
struct ModelRow {
    id: String,
    tier: ModelTier,
    input: Option<Rate>,
    output: Option<Rate>,
    cached_input: Option<Rate>,
    #[serde(default)]
    cache_write: Option<Rate>,
    context: Option<u64>,
    max_output: Option<u64>,
    thinking: ThinkingWord,
    #[serde(default)]
    thinking_param: Option<ThinkingParam>,
    #[serde(default)]
    modalities: Vec<Modality>,
    status: StatusWord,
    #[serde(default)]
    retire: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ThinkingWord {
    None,
    Optional,
    Only,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StatusWord {
    Ga,
    Preview,
    Deprecated,
}

/// Says that a model row sets a key its `thinking` or `status` does not have.
#[derive(Debug, thiserror::Error)]
enum ModelRowError {
    #[error("model `{id}`: thinking_param is set but thinking = \"none\"")]
    ParamWithoutThinking { id: String },
    #[error("model `{id}`: retire is set but status is not \"deprecated\"")]
    RetireWithoutDeprecated { id: String },
}

impl TryFrom<ModelRow> for ModelEntry {
    type Error = ModelRowError;

    fn try_from(row: ModelRow) -> Result<Self, Self::Error> {
        let thinking = match (row.thinking, row.thinking_param) {
            (ThinkingWord::None, None) => Thinking::None,
            (ThinkingWord::None, Some(_)) => {
                return Err(ModelRowError::ParamWithoutThinking { id: row.id });
            }
            (ThinkingWord::Optional, param) => Thinking::Optional { param },
            (ThinkingWord::Only, param) => Thinking::Only { param },
        };
        let status = match (row.status, row.retire) {
            (StatusWord::Deprecated, retire) => ModelStatus::Deprecated { retire },
            (StatusWord::Ga, None) => ModelStatus::Ga,
            (StatusWord::Preview, None) => ModelStatus::Preview,
            (StatusWord::Ga | StatusWord::Preview, Some(_)) => {
                return Err(ModelRowError::RetireWithoutDeprecated { id: row.id });
            }
        };
        Ok(Self {
            id: row.id,
            tier: row.tier,
            input: row.input,
            output: row.output,
            cached_input: row.cached_input,
            cache_write: row.cache_write,
            context: row.context,
            max_output: row.max_output,
            thinking,
            modalities: row.modalities,
            status,
            aliases: row.aliases,
        })
    }
}

impl ModelEntry {
    /// Returns whether this model always reasons and rejects an "off" signal.
    pub fn is_thinking_only(&self) -> bool {
        matches!(self.thinking, Thinking::Only { .. })
    }

    /// Returns the request field that controls this model's thinking, set on Gemini rows only.
    pub fn thinking_param(&self) -> Option<ThinkingParam> {
        match self.thinking {
            Thinking::None => None,
            Thinking::Optional { param } | Thinking::Only { param } => param,
        }
    }

    /// Returns this row's [`Pricing`], an unset price as zero; a test requires every GA row of
    /// a per-token provider to carry `input` and `output`. `thinking_per_million` is always
    /// zero: a row has no thinking price.
    pub fn pricing(&self) -> Pricing {
        Pricing {
            input_per_million: self.input.unwrap_or(Rate::ZERO),
            output_per_million: self.output.unwrap_or(Rate::ZERO),
            cached_input_per_million: self.cached_input.unwrap_or(Rate::ZERO),
            cache_creation_per_million: self.cache_write.unwrap_or(Rate::ZERO),
            thinking_per_million: Rate::ZERO,
        }
    }
}

/// Says how a provider bills, which decides whether its GA models must carry a price;
/// `interface` cannot tell a paid HTTP provider (openai) from a free local one (mlx).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingModel {
    /// Bills per token (openai, anthropic, gemini, deepseek, xai); a GA model carries
    /// input and output prices.
    PerToken,
    /// Bills by subscription, session or the user's own key (claude_cli, codex_cli, opencode,
    /// antigravity, claude_sdk), so a blank price is the truth, not missing data.
    Subscription,
    /// Runs locally and costs nothing per call (mlx, vllm, llamacpp).
    Free,
}

/// The capabilities a provider has for every model it serves; an absent key takes its default.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "CapabilitiesRow")]
pub struct ProviderCapabilities {
    pub structured_output: StructuredOutputMode,
    pub tool_use: ToolUse,
    pub cancel: bool,
    pub streaming: bool,
    pub prompt_cache: PromptCacheKind,
    pub chat_template_thinking: bool,
    pub cache_creation_per_million: Option<Rate>,
    pub thinking_per_million: Option<Rate>,
}

/// A provider's capabilities as the data file spells them: tool use and parallel calls as two
/// flags.
#[derive(Deserialize)]
#[serde(default)]
struct CapabilitiesRow {
    structured_output: StructuredOutputMode,
    tool_use: bool,
    parallel_tool_calls: bool,
    cancel: bool,
    streaming: bool,
    /// Is written as `"explicit_marker"` or, with the provider's threshold,
    /// `{ implicit_prefix = { min_tokens = 1024 } }`.
    pub prompt_cache: PromptCacheKind,
    /// Whether the server serves its model through a chat template that reads
    /// `chat_template_kwargs.enable_thinking` (vLLM, mlx_lm.server, llama-server).
    pub chat_template_thinking: bool,
    /// Replaces every row's `cache_write` price in the provider profile, USD per million tokens;
    /// when absent each row's own `cache_write` stands.
    pub cache_creation_per_million: Option<Rate>,
    /// Prices thinking tokens for every model of the provider, USD per million tokens; absent
    /// means `0.0`, since a row has no thinking price.
    pub thinking_per_million: Option<Rate>,
}

/// A data file that declares parallel tool calls for a provider without tool use.
#[derive(Debug, thiserror::Error)]
#[error("`parallel_tool_calls = true` needs `tool_use = true`")]
pub struct ParallelWithoutToolUseError;

impl TryFrom<CapabilitiesRow> for ProviderCapabilities {
    type Error = ParallelWithoutToolUseError;

    fn try_from(row: CapabilitiesRow) -> Result<Self, Self::Error> {
        let tool_use = match (row.tool_use, row.parallel_tool_calls) {
            (true, parallel) => ToolUse::Supported { parallel },
            (false, false) => ToolUse::Unsupported,
            (false, true) => return Err(ParallelWithoutToolUseError),
        };
        Ok(Self {
            structured_output: row.structured_output,
            tool_use,
            cancel: row.cancel,
            streaming: row.streaming,
            prompt_cache: row.prompt_cache,
            chat_template_thinking: row.chat_template_thinking,
            cache_creation_per_million: row.cache_creation_per_million,
            thinking_per_million: row.thinking_per_million,
        })
    }
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            structured_output: StructuredOutputMode::None,
            tool_use: ToolUse::Unsupported,
            cancel: false,
            streaming: false,
            prompt_cache: PromptCacheKind::None,
            chat_template_thinking: false,
            cache_creation_per_million: None,
            thinking_per_million: None,
        }
    }
}

/// An absent key reads as [`ProviderCapabilities::default`] has it.
impl Default for CapabilitiesRow {
    fn default() -> Self {
        let d = ProviderCapabilities::default();
        Self {
            structured_output: d.structured_output,
            tool_use: d.tool_use.support() == Support::Supported,
            parallel_tool_calls: d.tool_use.parallel(),
            cancel: d.cancel,
            streaming: d.streaming,
            prompt_cache: d.prompt_cache,
            chat_template_thinking: d.chat_template_thinking,
            cache_creation_per_million: d.cache_creation_per_million,
            thinking_per_million: d.thinking_per_million,
        }
    }
}

/// One `[providers.<name>]` block of the data file.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderDef {
    pub interface: InterfaceKind,
    /// Names who serves the API as OpenTelemetry GenAI does; absent for a server that may serve
    /// anyone's weights (vllm, mlx, llamacpp) or a tool that may call several (opencode).
    #[serde(default)]
    pub gen_ai_provider_name: Option<String>,
    pub billed: BillingModel,
    /// Holds a concrete id or a `<series>@latest` spec; `None` for local providers, where the
    /// user always names the model.
    #[serde(default)]
    pub default: Option<String>,
    /// Names a concrete coding-tuned model.
    #[serde(default)]
    pub coding_default: Option<String>,
    #[serde(default)]
    pub capabilities: ProviderCapabilities,
    /// Maps a series name to an id pattern with one `{version}` slot (see [`crate::model_spec`]).
    #[serde(default)]
    pub series: BTreeMap<String, SeriesPattern>,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

impl ProviderDef {
    /// Returns the row whose id or alias is `model_id`.
    pub fn find_model(&self, model_id: &str) -> Option<&ModelEntry> {
        self.models
            .iter()
            .find(|m| m.id == model_id || m.aliases.iter().any(|a| a == model_id))
    }

    /// Resolves `spec` for provider `name` against `ids`, the live list or this block's own rows
    /// as `searched` says.
    pub fn resolve_among<'i>(
        &self,
        name: &str,
        spec: &str,
        ids: impl IntoIterator<Item = &'i str>,
        searched: CatalogSource,
    ) -> Result<ResolvedModel, ResolveModelSpecError> {
        let series = match ModelSpec::parse(spec)? {
            ModelSpec::Concrete(id) => {
                return Ok(ResolvedModel {
                    id: id.to_string(),
                    picked_from: None,
                });
            }
            ModelSpec::Latest { series } => series,
        };
        if self.series.is_empty() {
            return Err(ResolveModelSpecError::NoSeries {
                provider: name.to_string(),
                series: series.to_string(),
            });
        }
        let Some(pattern) = self.series.get(series) else {
            return Err(ResolveModelSpecError::UnknownSeries {
                provider: name.to_string(),
                series: series.to_string(),
                known: self.series.keys().cloned().collect(),
            });
        };
        let ids: Vec<&str> = ids.into_iter().collect();
        match pattern.latest(ids.iter().copied()) {
            Some(id) => Ok(ResolvedModel {
                id: id.to_string(),
                picked_from: Some(searched),
            }),
            None => Err(ResolveModelSpecError::NoMatch {
                provider: name.to_string(),
                series: series.to_string(),
                pattern: Box::new(pattern.clone()),
                searched,
                candidates: ids.len(),
            }),
        }
    }

    /// Returns the row the `default` spec resolves to among this block's own rows.
    fn default_row(&self, name: &str, verified: &str) -> Option<&ModelEntry> {
        let spec = self.default.as_deref()?;
        let resolved = self
            .resolve_among(
                name,
                spec,
                self.models.iter().map(|m| m.id.as_str()),
                CatalogSource::Shipped {
                    verified: verified.to_string(),
                },
            )
            .ok()?;
        self.find_model(&resolved.id)
    }

    /// Returns the [`ProviderProfile`] of `model_id`: the block's capabilities with the row's
    /// limits, modalities and prices. An unknown model takes the default model's row. A block with
    /// no matching row (a local server, whose model the user picks) knows nothing of the model:
    /// thinking and vision are unknown and no ceiling is enforced.
    ///
    /// A price the capabilities set replaces the row's; an unset one leaves the row's price.
    fn capabilities_for(&self, name: &str, verified: &str, model_id: &str) -> ProviderProfile {
        let cap = &self.capabilities;
        let model = self
            .find_model(model_id)
            .or_else(|| self.default_row(name, verified));

        let (max_context_tokens, supports_thinking, mods_in, pricing) = match model {
            Some(m) => {
                // A row that names no modality takes text alone.
                let mods_in = if m.modalities.is_empty() {
                    Modalities::text_only()
                } else {
                    Modalities::listed(m.modalities.iter().copied())
                };
                let mut pricing = m.pricing();
                if let Some(rate) = cap.cache_creation_per_million {
                    pricing.cache_creation_per_million = rate;
                }
                if let Some(rate) = cap.thinking_per_million {
                    pricing.thinking_per_million = rate;
                }
                (
                    m.context.map(|c| c as u32),
                    Support::from(!matches!(m.thinking, Thinking::None)),
                    mods_in,
                    pricing,
                )
            }
            None => {
                let pricing = Pricing {
                    cache_creation_per_million: cap
                        .cache_creation_per_million
                        .unwrap_or(Rate::ZERO),
                    thinking_per_million: cap.thinking_per_million.unwrap_or(Rate::ZERO),
                    ..Default::default()
                };
                (
                    None,
                    Support::Unknown,
                    Modalities::text_and_unknown(),
                    pricing,
                )
            }
        };

        ProviderProfile {
            interface: self.interface,
            gen_ai_provider_name: self.gen_ai_provider_name.clone(),
            max_context_tokens,
            tool_use: cap.tool_use,
            supports_structured_output: Some(cap.structured_output),
            supports_thinking,
            supports_chat_template_thinking: cap.chat_template_thinking,
            supports_cancel: cap.cancel,
            prompt_cache: cap.prompt_cache,
            streaming: cap.streaming,
            modalities_in: mods_in,
            modalities_out: Modalities::text_only(),
            pricing,
        }
    }
}

/// Is another name for [`ProviderDef`]; nothing in the workspace uses it.
pub type ProviderModels = ProviderDef;

/// The lab that made a model, as the data file's `[vendors.<name>]` names it (`anthropic`,
/// `google`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Vendor(String);

impl Vendor {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Vendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for Vendor {
    fn from(name: String) -> Self {
        Self(name)
    }
}

/// One `[vendors.<name>]` block: how the ids of that vendor's models start.
#[derive(Debug, Clone, Deserialize)]
pub struct VendorDef {
    pub model_id_prefixes: Vec<String>,
}

/// The parsed data file.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelKb {
    pub schema_version: u32,
    pub verified: String,
    #[serde(default)]
    pub vendors: BTreeMap<Vendor, VendorDef>,
    pub providers: HashMap<String, ProviderDef>,
}

impl ModelKb {
    /// Returns the default model spec of `provider`, or `None` when the provider is not in the
    /// data file or is a local one with no default.
    pub fn default_model(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)
            .and_then(|p| p.default.as_deref())
    }

    /// Returns the coding-tuned model id of `provider`, if one is declared.
    pub fn coding_default_model(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)
            .and_then(|p| p.coding_default.as_deref())
    }

    /// Returns a row, from any provider, whose id or alias is `model_id`, preferring one with an
    /// `input` price.
    ///
    /// One id can sit in several blocks (`gpt-5.4` in `openai`, priced, and in `codex_cli`,
    /// blank); the first match of the unordered map could be the blank row and bill $0. Among
    /// several priced rows, or several unpriced ones, which is returned is not fixed.
    pub fn find(&self, model_id: &str) -> Option<&ModelEntry> {
        let mut fallback: Option<&ModelEntry> = None;
        for m in self.providers.values().flat_map(|p| p.models.iter()) {
            if m.id == model_id || m.aliases.iter().any(|a| a == model_id) {
                if m.input.is_some() {
                    return Some(m);
                }
                fallback.get_or_insert(m);
            }
        }
        fallback
    }

    /// Returns the [`Pricing`] of `model_id` per [`Self::find`], or `None` for an unknown model.
    pub fn pricing(&self, model_id: &str) -> Option<Pricing> {
        self.find(model_id).map(ModelEntry::pricing)
    }

    /// Returns the vendor whose model ids start like `model_id`, whichever provider serves it;
    /// `None` for an id no vendor's prefix matches (a local model, a proxy's own naming).
    pub fn vendor_of(&self, model_id: &str) -> Option<&Vendor> {
        self.vendors.iter().find_map(|(vendor, def)| {
            def.model_id_prefixes
                .iter()
                .any(|prefix| model_id.starts_with(prefix.as_str()))
                .then_some(vendor)
        })
    }

    /// Returns whether the data file has a block named `provider`.
    pub fn knows(&self, provider: &str) -> bool {
        self.providers.contains_key(provider)
    }

    /// Returns the [`ProviderProfile`] of `model_id` on `provider`. A provider the data file does
    /// not name (an `openai_compat` pointed at a local server) gets
    /// [`ProviderProfile::undescribed_http`], which the instance's `CapabilitiesOverrides` then
    /// correct.
    pub fn capabilities_for(&self, provider: &str, model_id: &str) -> ProviderProfile {
        match self.providers.get(provider) {
            Some(def) => def.capabilities_for(provider, &self.verified, model_id),
            None => ProviderProfile::undescribed_http(),
        }
    }
}

/// The data file, parsed on first access.
///
/// # Panics
///
/// Panics on first access when `data/provider.toml` is malformed: it is compiled in, so that is
/// an authoring bug, not a runtime condition.
pub static MODEL_KB: LazyLock<ModelKb> = LazyLock::new(|| {
    const RAW: &str = include_str!("../data/provider.toml");
    toml::from_str(RAW).expect(
        "crates/tars-config/data/provider.toml is malformed — fix the provider DB (this is DATA, \
         parsed once at first use)",
    )
});

#[cfg(test)]
mod tests {
    use super::*;

    fn row(extra: &str) -> Result<ModelEntry, toml::de::Error> {
        toml::from_str(&format!(
            "id = \"m\"\ntier = \"mid\"\ninput = 1.0\noutput = 2.0\ncached_input = 0.5\n\
             context = 1000\nmax_output = 100\n{extra}"
        ))
    }

    #[test]
    fn a_thinking_param_belongs_to_a_model_that_thinks() {
        let gemini =
            row("thinking = \"only\"\nthinking_param = \"level\"\nstatus = \"ga\"").unwrap();
        assert_eq!(
            gemini.thinking,
            Thinking::Only {
                param: Some(ThinkingParam::Level)
            }
        );
        assert!(row("thinking = \"none\"\nthinking_param = \"level\"\nstatus = \"ga\"").is_err());
    }

    #[test]
    fn a_retire_date_belongs_to_a_deprecated_model() {
        let old =
            row("thinking = \"none\"\nstatus = \"deprecated\"\nretire = \"2026-01-01\"").unwrap();
        assert_eq!(
            old.status,
            ModelStatus::Deprecated {
                retire: Some("2026-01-01".into())
            }
        );
        assert!(row("thinking = \"none\"\nstatus = \"ga\"\nretire = \"2026-01-01\"").is_err());
    }

    #[test]
    fn kb_parses_and_every_default_exists_in_its_models() {
        let kb = &*MODEL_KB;
        assert_eq!(kb.schema_version, 2);
        assert!(!kb.providers.is_empty());
        for (name, p) in &kb.providers {
            if let Some(def) = &p.default {
                assert!(
                    p.default_row(name, &kb.verified).is_some(),
                    "provider `{name}` default `{def}` does not resolve to one of its models"
                );
            }
            for (series, pattern) in &p.series {
                assert!(
                    pattern
                        .latest(p.models.iter().map(|m| m.id.as_str()))
                        .is_some(),
                    "provider `{name}` series `{series}` matches none of its models"
                );
            }
            if let Some(cd) = &p.coding_default {
                assert!(
                    p.models.iter().any(|m| &m.id == cd),
                    "provider `{name}` coding_default `{cd}` is not present in its models"
                );
            }
        }
    }

    #[test]
    fn default_model_resolves_for_api_providers() {
        assert_eq!(MODEL_KB.default_model("openai"), Some("gpt-5.4"));
        assert_eq!(MODEL_KB.default_model("anthropic"), Some("claude-opus-4-8"));
        assert_eq!(MODEL_KB.default_model("gemini"), Some("flash@latest"));
        assert_eq!(MODEL_KB.default_model("deepseek"), Some("deepseek-flash"));
        // Local backends carry no default.
        assert_eq!(MODEL_KB.default_model("mlx"), None);
    }

    #[test]
    fn find_matches_id_and_aliases() {
        assert_eq!(
            MODEL_KB.find("gemini-3.5-flash").unwrap().id,
            "gemini-3.5-flash"
        );
        let by_alias = MODEL_KB.find("deepseek-v4-flash").unwrap();
        assert_eq!(by_alias.id, "deepseek-flash");
        assert!(MODEL_KB.find("no-such-model").is_none());
    }

    #[test]
    fn thinking_only_flag_is_data_driven() {
        assert!(MODEL_KB.find("gemini-2.5-pro").unwrap().is_thinking_only());
        assert!(
            MODEL_KB
                .find("gemini-3.1-pro-preview")
                .unwrap()
                .is_thinking_only()
        );
        assert!(
            !MODEL_KB
                .find("gemini-3.5-flash")
                .unwrap()
                .is_thinking_only()
        );
        // 3.7/3.8 flash reject the `minimal` level (verified live).
        assert!(
            MODEL_KB
                .find("gemini-3.8-flash")
                .unwrap()
                .is_thinking_only()
        );
        assert!(
            !MODEL_KB
                .find("gemini-2.5-flash")
                .unwrap()
                .is_thinking_only()
        );
    }

    #[test]
    fn shipped_default_spec_resolves_to_the_newest_row() {
        let gemini = &MODEL_KB.providers["gemini"];
        let row = gemini.default_row("gemini", &MODEL_KB.verified).unwrap();
        assert_eq!(row.id, "gemini-3.8-flash");
        let pro = gemini
            .resolve_among(
                "gemini",
                "pro@latest",
                gemini.models.iter().map(|m| m.id.as_str()),
                CatalogSource::Shipped {
                    verified: MODEL_KB.verified.clone(),
                },
            )
            .unwrap();
        assert_eq!(pro.id, "gemini-3.1-pro-preview");
        let err = MODEL_KB.providers["deepseek"]
            .resolve_among(
                "deepseek",
                "flash@latest",
                [],
                CatalogSource::Shipped {
                    verified: String::new(),
                },
            )
            .unwrap_err();
        assert!(
            matches!(err, ResolveModelSpecError::NoSeries { .. }),
            "{err}"
        );
    }

    #[test]
    fn a_cache_write_is_priced_and_costs_more_than_a_read() {
        for id in ["claude-opus-5", "claude-fable-5-1", "claude-sonnet-5"] {
            let p = MODEL_KB
                .pricing(id)
                .unwrap_or_else(|| panic!("{id} has no pricing"));
            assert!(
                p.cache_creation_per_million.usd_per_million() > 0.0,
                "{id}: a cache write bills at zero — `cache_write` is missing from provider.toml \
                 or no longer reaches Pricing"
            );
            assert!(
                p.cache_creation_per_million > p.input_per_million,
                "{id}: a cache write must cost MORE than plain input (Anthropic: 1.25× at 5m)"
            );
            assert!(
                p.cache_creation_per_million > p.cached_input_per_million,
                "{id}: a cache write must cost more than a cache read"
            );
        }
        // Exact rates, so a typo in the column is caught rather than just "> 0".
        assert_eq!(
            MODEL_KB
                .pricing("claude-opus-5")
                .unwrap()
                .cache_creation_per_million
                .usd_per_million(),
            6.25
        );
        assert_eq!(
            MODEL_KB
                .pricing("claude-fable-5-1")
                .unwrap()
                .cache_creation_per_million
                .usd_per_million(),
            12.50
        );
    }

    #[test]
    fn the_provider_profile_prices_cache_writes_from_the_row_when_the_block_sets_no_rate() {
        let profile = MODEL_KB.capabilities_for("anthropic", "claude-opus-5");
        assert_eq!(
            profile.pricing.cache_creation_per_million.usd_per_million(),
            6.25
        );
    }

    #[test]
    fn parallel_tool_calls_without_tool_use_do_not_read() {
        let got = toml::from_str::<ModelKb>(
            r#"
            schema_version = 1
            verified = "2026-11-04"
            [providers.p]
            interface = "http"
            billed = "per_token"
            [providers.p.capabilities]
            tool_use = false
            parallel_tool_calls = true
            "#,
        );
        let err = got.expect_err("parallel calls without tool use must not read");
        assert!(
            err.to_string()
                .contains(&ParallelWithoutToolUseError.to_string()),
            "{err}"
        );
    }

    #[test]
    fn a_rate_the_block_sets_replaces_the_rows_rate_in_the_provider_profile() {
        let kb: ModelKb = toml::from_str(
            r#"
            schema_version = 1
            verified = "2026-11-04"
            [providers.p]
            interface = "http"
            billed = "per_token"
            [providers.p.capabilities]
            cache_creation_per_million = 9.0
            thinking_per_million = 4.0
            [[providers.p.models]]
            id = "m"
            tier = "flagship"
            input = 1.0
            output = 2.0
            cache_write = 1.25
            thinking = "none"
            status = "ga"
            "#,
        )
        .unwrap();
        let pricing = kb.capabilities_for("p", "m").pricing;
        assert_eq!(pricing.cache_creation_per_million.usd_per_million(), 9.0);
        assert_eq!(pricing.thinking_per_million.usd_per_million(), 4.0);
    }

    #[test]
    fn pricing_reads_from_kb() {
        let p = MODEL_KB.pricing("gpt-5.4").unwrap();
        assert_eq!(p.input_per_million.usd_per_million(), 2.50);
        assert_eq!(p.output_per_million.usd_per_million(), 15.00);
        assert_eq!(p.cached_input_per_million.usd_per_million(), 0.25);
        assert!(MODEL_KB.pricing("no-such-model").is_none());
    }

    /// A GA model with no price bills $0; subscription and local providers are skipped because
    /// a blank price is true for them.
    #[test]
    fn every_per_token_ga_model_carries_input_and_output_price() {
        for (provider, p) in &MODEL_KB.providers {
            if p.billed != BillingModel::PerToken {
                continue;
            }
            for m in &p.models {
                if matches!(m.status, ModelStatus::Ga) {
                    assert!(
                        m.input.is_some() && m.output.is_some(),
                        "GA model `{}` (per-token provider `{provider}`) is missing a price — a \
                         live model must never silently bill $0 (add input/output to provider.toml)",
                        m.id
                    );
                }
            }
        }
    }

    #[test]
    fn a_local_block_with_no_model_row_knows_tool_use_but_not_thinking_vision_or_window() {
        for provider in ["vllm", "mlx", "llamacpp"] {
            let p = MODEL_KB.capabilities_for(provider, "qwen3");
            assert_eq!(p.tool_use.support(), Support::Supported, "{provider}");
            assert_eq!(p.supports_thinking, Support::Unknown, "{provider}");
            assert_eq!(p.modalities_in.image, Support::Unknown, "{provider}");
            assert_eq!(p.max_context_tokens, None, "{provider}");
        }
    }

    #[test]
    fn a_row_states_thinking_and_vision_as_known() {
        let p = MODEL_KB.capabilities_for("anthropic", "claude-opus-5");
        assert_eq!(p.supports_thinking, Support::Supported);
        assert_eq!(p.modalities_in.image, Support::Supported);
    }

    #[test]
    fn a_provider_the_data_file_does_not_name_is_undescribed() {
        let p = MODEL_KB.capabilities_for("my_local_server", "whatever");
        assert_eq!(p.tool_use.support(), Support::Unknown);
        assert_eq!(p.max_context_tokens, None);
    }

    #[test]
    fn every_shipped_model_id_and_alias_has_exactly_one_vendor() {
        for (provider, def) in &MODEL_KB.providers {
            for id in def
                .models
                .iter()
                .flat_map(|m| std::iter::once(&m.id).chain(&m.aliases))
            {
                let vendors: Vec<&Vendor> = MODEL_KB
                    .vendors
                    .iter()
                    .filter(|(_, v)| {
                        v.model_id_prefixes
                            .iter()
                            .any(|p| id.starts_with(p.as_str()))
                    })
                    .map(|(name, _)| name)
                    .collect();
                assert_eq!(
                    vendors.len(),
                    1,
                    "`{id}` of `{provider}` matches {vendors:?}"
                );
            }
        }
    }

    #[test]
    fn claude_is_anthropics_whichever_provider_serves_it() {
        let vendor = MODEL_KB.vendor_of("claude-opus-4-7").map(Vendor::as_str);
        assert_eq!(vendor, Some("anthropic"));
    }

    #[test]
    fn a_model_id_no_vendor_prefix_matches_has_no_vendor() {
        assert_eq!(MODEL_KB.vendor_of("Qwen/Qwen3-32B"), None);
        assert_eq!(MODEL_KB.vendor_of("claude"), None);
    }
}
