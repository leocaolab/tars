//! Model knowledge base — the parsed, typed view of `data/models.toml`.
//!
//! Model ids, prices, context windows, and the thinking mode change
//! faster than tars releases, so they live as **DATA** in
//! `data/models.toml`, not as string literals in `builtin.rs` or a
//! substring heuristic in a backend adapter. This module `include_str!`s
//! that file (so the KB ships in the binary, no runtime file I/O) and
//! parses it once into [`MODEL_KB`].
//!
//! Adding/retiring a model or changing a default is a `models.toml`
//! edit — no code change here.
//!
//! Fail-loud: a malformed `models.toml` panics on first access to
//! [`MODEL_KB`] rather than silently degrading (see [`MODEL_KB`]).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use serde::Deserialize;

use tars_types::{
    Capabilities, InterfaceKind, Modality, Pricing, PromptCacheKind, StructuredOutputMode,
};

/// Whether a model reasons, and whether "off" is a legal request.
///
/// - `None` — never reasons.
/// - `Optional` — toggleable; an "off"/"minimal" signal is accepted.
/// - `Only` — mandatory reasoning; the model **rejects** an off signal
///   (e.g. Gemini `*-pro` returns HTTP 400 for `thinkingBudget: 0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    None,
    Optional,
    Only,
}

/// Which wire knob a Gemini model uses to control thinking (the two
/// generations disagree, per the official docs):
/// - `Budget` — Gemini 2.5: numeric `thinkingBudget` (0 = off, -1 =
///   dynamic, N = token budget).
/// - `Level` — Gemini 3.x: string `thinking_level` (`minimal` = off,
///   `low`/`medium`/`high`). 3.x does **not** take a numeric budget;
///   `thinkingBudget:0` only still works there as deprecated
///   backward-compat, so we follow the docs and emit `thinking_level`.
///
/// Only Gemini rows set this; other providers ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingParam {
    Budget,
    Level,
}

/// Capability tier of a model within its family. A closed set — typed
/// (not a bare `String`) so a typo in `models.toml` fails to parse loudly.
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

/// Availability of a model on the provider's API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelStatus {
    Ga,
    Preview,
    Deprecated,
}

/// Input modality a model accepts. KB-local (richer than the routing
/// `tars_types::Modality`, which has no `Pdf` and spells vision `Image`);
/// this describes what the wire API takes, not the router's coarse view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KbModality {
    Text,
    Vision,
    Audio,
    Video,
    Pdf,
}

/// One model row from `models.toml`. Price fields are USD per 1M tokens
/// and are `Option` because deprecated/legacy rows may omit them.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    /// Exact API model string.
    pub id: String,
    pub tier: ModelTier,
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cached_input: Option<f64>,
    /// Context window (input tokens).
    pub context: Option<u64>,
    pub max_output: Option<u64>,
    pub thinking: Thinking,
    /// Gemini-only: which wire knob controls thinking (2.5 = `budget`,
    /// 3.x = `level`). `None` for non-Gemini rows / where it doesn't apply.
    #[serde(default)]
    pub thinking_param: Option<ThinkingParam>,
    #[serde(default)]
    pub modalities: Vec<KbModality>,
    pub status: ModelStatus,
    /// ISO retire date for deprecated rows (informational).
    #[serde(default)]
    pub retire: Option<String>,
    /// Alternate ids that resolve to this entry (legacy names, dated
    /// snapshots). Matched by [`ModelKb::find`] alongside `id`.
    #[serde(default)]
    pub aliases: Vec<String>,
}

impl ModelEntry {
    /// True iff this model mandates reasoning and rejects an "off"
    /// signal. The single, data-driven source for the gemini
    /// thinking-off decision.
    pub fn is_thinking_only(&self) -> bool {
        matches!(self.thinking, Thinking::Only)
    }

    /// Per-1M-token [`Pricing`] for this model. An unset price field maps
    /// to `0.0` (a legacy/deprecated row with no prices bills as free);
    /// a `status = "ga"` row is required to carry `input`/`output`
    /// (enforced by a KB test) so a live model can never silently bill
    /// zero. `cache_creation` and `thinking_per_million` are
    /// provider-behavioral (Anthropic bundles thinking into output;
    /// Gemini bills thoughts at the output rate), not carried per-row,
    /// so they default to `0.0` — the backend wiring a specific provider
    /// overrides them.
    pub fn pricing(&self) -> Pricing {
        Pricing {
            input_per_million: self.input.unwrap_or(0.0),
            output_per_million: self.output.unwrap_or(0.0),
            cached_input_per_million: self.cached_input.unwrap_or(0.0),
            cache_creation_per_million: 0.0,
            thinking_per_million: 0.0,
        }
    }
}

/// How a provider bills — the honest discriminator for the "every GA model
/// carries a price" invariant. `interface` alone can't separate a paid HTTP
/// provider (openai) from a free local one (mlx), so this is declared per
/// provider block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingModel {
    /// Per-1M-token pricing (openai, anthropic, gemini, deepseek, xai,
    /// bedrock). A GA model here MUST carry input+output prices.
    PerToken,
    /// Subscription / session / BYO-key — no per-token price to tars
    /// (claude_cli, gemini_cli, codex_cli, opencode, antigravity, claude_sdk).
    /// A blank price is the TRUTH, not missing data.
    Subscription,
    /// Locally hosted, free at the point of use (mlx, vllm, llamacpp).
    Free,
}

/// Config-side spelling of [`PromptCacheKind`]. The runtime enum has a
/// struct variant (`ImplicitPrefix { min_tokens }`) that can't deserialize
/// from a bare TOML string, so the data file uses these unit variants and the
/// assembler fills in the conventional `min_tokens`.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheSpec {
    #[default]
    None,
    ImplicitPrefix,
    ExplicitMarker,
    ExplicitObject,
    Delegated,
}

impl PromptCacheSpec {
    /// OpenAI auto-caches prefixes ≥1024 tokens; use that as the conventional
    /// floor for any `implicit_prefix` provider (the exact threshold isn't
    /// carried per-provider today).
    const IMPLICIT_PREFIX_MIN_TOKENS: u32 = 1024;

    fn to_kind(self) -> PromptCacheKind {
        match self {
            Self::None => PromptCacheKind::None,
            Self::ImplicitPrefix => PromptCacheKind::ImplicitPrefix {
                min_tokens: Self::IMPLICIT_PREFIX_MIN_TOKENS,
            },
            Self::ExplicitMarker => PromptCacheKind::ExplicitMarker,
            Self::ExplicitObject => PromptCacheKind::ExplicitObject,
            Self::Delegated => PromptCacheKind::Delegated,
        }
    }
}

/// Provider-level capability facts — constant across every model on the
/// backend (§3 of the design). All fields have serde defaults so a sparse
/// `[providers.X.capabilities]` block parses.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderCapabilities {
    pub structured_output: StructuredOutputMode,
    pub tool_use: bool,
    pub parallel_tool_calls: bool,
    pub cancel: bool,
    pub streaming: bool,
    pub prompt_cache: PromptCacheSpec,
    /// Provider-behavioral pricing the KB can't carry per model
    /// (anthropic's cache-write surcharge). USD per 1M tokens.
    pub cache_creation_per_million: Option<f64>,
    /// Provider-behavioral thinking price (gemini bills thoughts at the
    /// output rate). USD per 1M tokens.
    pub thinking_per_million: Option<f64>,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            structured_output: StructuredOutputMode::None,
            tool_use: false,
            parallel_tool_calls: false,
            cancel: false,
            streaming: false,
            prompt_cache: PromptCacheSpec::None,
            cache_creation_per_million: None,
            thinking_per_million: None,
        }
    }
}

/// One provider's full definition (`[providers.<name>]`): how tars reaches it
/// (`interface`), how it bills, its provider-level capabilities, and its
/// models. This is `data/models.toml`'s old per-provider block widened with
/// the provider-level facts the KB was missing.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderDef {
    /// How tars reaches and drives this provider.
    pub interface: InterfaceKind,
    /// Billing model — discriminates the price invariant.
    pub billed: BillingModel,
    /// Default model id for general use. `None` for local providers where the
    /// user always picks the model (mlx/vllm/llamacpp carry no fixed default).
    #[serde(default)]
    pub default: Option<String>,
    /// Optional coding-tuned default (e.g. `gpt-5.6-sol`).
    #[serde(default)]
    pub coding_default: Option<String>,
    /// Provider-level capability facts.
    #[serde(default)]
    pub capabilities: ProviderCapabilities,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

impl ProviderDef {
    /// Find a model row by exact id or alias.
    fn find_model(&self, model_id: &str) -> Option<&ModelEntry> {
        self.models
            .iter()
            .find(|m| m.id == model_id || m.aliases.iter().any(|a| a == model_id))
    }

    /// Assemble the runtime [`Capabilities`] for `model_id` on this provider:
    /// provider-level fields from the block ∪ per-model fields from the row
    /// (falling back to the provider's default model, then to a text-only
    /// shape when the DB carries no row at all — a local provider with an
    /// empty model list). This generalizes what the gemini backend used to do
    /// by hand.
    pub fn capabilities_for(&self, model_id: &str) -> Capabilities {
        let cap = &self.capabilities;
        // Resolve the model row: exact/alias match, else the provider default.
        let model = self
            .find_model(model_id)
            .or_else(|| self.default.as_deref().and_then(|d| self.find_model(d)));

        let (max_context_tokens, max_output_tokens, supports_vision, supports_thinking, mods_in, pricing) =
            match model {
                Some(m) => {
                    let mut mods_in: HashSet<Modality> =
                        m.modalities.iter().filter_map(kb_to_modality).collect();
                    // modalities_in must be non-empty (Capabilities::validate).
                    if mods_in.is_empty() {
                        mods_in.insert(Modality::Text);
                    }
                    let mut pricing = m.pricing();
                    pricing.cache_creation_per_million =
                        cap.cache_creation_per_million.unwrap_or(0.0);
                    pricing.thinking_per_million = cap.thinking_per_million.unwrap_or(0.0);
                    (
                        m.context.map(|c| c as u32),
                        m.max_output.map(|o| o as u32),
                        mods_in.contains(&Modality::Image),
                        !matches!(m.thinking, Thinking::None),
                        mods_in,
                        pricing,
                    )
                }
                None => {
                    // No model row (local provider, empty model list). No
                    // per-model facts to enforce; keep the provider-behavioral
                    // pricing so a block that declares it isn't dropped.
                    let mut pricing = Pricing::default();
                    pricing.cache_creation_per_million =
                        cap.cache_creation_per_million.unwrap_or(0.0);
                    pricing.thinking_per_million = cap.thinking_per_million.unwrap_or(0.0);
                    (None, None, false, false, HashSet::from([Modality::Text]), pricing)
                }
            };

        Capabilities {
            interface: self.interface,
            max_context_tokens,
            max_output_tokens,
            supports_tool_use: cap.tool_use,
            supports_parallel_tool_calls: cap.parallel_tool_calls,
            supports_structured_output: cap.structured_output,
            supports_vision,
            supports_thinking,
            supports_cancel: cap.cancel,
            prompt_cache: cap.prompt_cache.to_kind(),
            streaming: cap.streaming,
            modalities_in: mods_in,
            modalities_out: HashSet::from([Modality::Text]),
            pricing,
        }
    }
}

/// Back-compat alias — the block widened from "just models" to a full
/// provider definition, but the old name is still used in a couple of places.
pub type ProviderModels = ProviderDef;

/// Map a KB input modality onto the coarser routing [`Modality`]. `Pdf` has no
/// router equivalent and is dropped (`None`).
fn kb_to_modality(m: &KbModality) -> Option<Modality> {
    match m {
        KbModality::Text => Some(Modality::Text),
        KbModality::Vision => Some(Modality::Image),
        KbModality::Audio => Some(Modality::Audio),
        KbModality::Video => Some(Modality::Video),
        KbModality::Pdf => None,
    }
}

/// The whole parsed knowledge base. Keyed by provider name
/// (`"openai"`, `"anthropic"`, …).
#[derive(Debug, Clone, Deserialize)]
pub struct ModelKb {
    pub schema_version: u32,
    pub verified: String,
    pub providers: HashMap<String, ProviderDef>,
}

impl ModelKb {
    /// Default model id for `provider`, or `None` if the provider isn't
    /// in the KB, or is a local provider with no fixed default (the user
    /// picks — mlx/vllm/llamacpp).
    pub fn default_model(&self, provider: &str) -> Option<&str> {
        self.providers.get(provider).and_then(|p| p.default.as_deref())
    }

    /// Coding-tuned default model id for `provider`, if one is declared.
    pub fn coding_default_model(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)
            .and_then(|p| p.coding_default.as_deref())
    }

    /// Find a model across all providers by exact `id` **or** any alias.
    ///
    /// The same model id can now appear in more than one provider block — e.g.
    /// `gpt-5.4` in both `openai` (priced) and `codex_cli` (blank, subscription),
    /// or `claude-opus-4-8` in `anthropic` / `claude_sdk` / `claude_cli`. A bare
    /// "first match" over the (unordered) provider map would non-deterministically
    /// return a blank-priced CLI row and silently bill $0. So prefer a **priced**
    /// entry (`input.is_some()`) when the id is shared; fall back to the first
    /// match otherwise.
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

    /// Per-model [`Pricing`] from the KB, or `None` for an unknown model.
    pub fn pricing(&self, model_id: &str) -> Option<Pricing> {
        self.find(model_id).map(ModelEntry::pricing)
    }

    /// Assemble runtime [`Capabilities`] for `provider` + `model_id`. If the
    /// provider isn't a named definition (an anonymous `openai_compat` a user
    /// pointed at a local server), fall back to a text-only baseline — the
    /// per-instance `CapabilitiesOverrides` correct it from there.
    pub fn capabilities_for(&self, provider: &str, model_id: &str) -> Capabilities {
        match self.providers.get(provider) {
            Some(def) => def.capabilities_for(model_id),
            None => Capabilities::text_only_baseline(Pricing::default()),
        }
    }
}

/// Assemble runtime [`Capabilities`] for `provider` + `model_id` from the
/// shipped provider DB. The one assembler that replaces the 15 hand-written
/// backend constructors (design §5).
pub fn capabilities_for(provider: &str, model_id: &str) -> Capabilities {
    MODEL_KB.capabilities_for(provider, model_id)
}

/// Parsed-once knowledge base. **Panics on first access** if
/// `data/models.toml` is malformed — the KB is compiled-in data, a
/// parse failure is a build/authoring bug that must fail loud, not a
/// recoverable runtime condition.
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

    #[test]
    fn kb_parses_and_every_default_exists_in_its_models() {
        // Forces the LazyLock parse; panics loudly on malformed TOML.
        let kb = &*MODEL_KB;
        assert_eq!(kb.schema_version, 2);
        assert!(!kb.providers.is_empty());
        for (name, p) in &kb.providers {
            // A default is optional (local providers carry none); when present
            // it MUST resolve to a model row.
            if let Some(def) = &p.default {
                assert!(
                    p.models.iter().any(|m| &m.id == def),
                    "provider `{name}` default `{def}` is not present in its models"
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
        assert_eq!(
            MODEL_KB.default_model("anthropic"),
            Some("claude-opus-4-8")
        );
        assert_eq!(
            MODEL_KB.default_model("gemini"),
            Some("gemini-3.5-flash")
        );
        assert_eq!(
            MODEL_KB.default_model("deepseek"),
            Some("deepseek-v4-flash")
        );
        // Local backends aren't in the KB.
        assert_eq!(MODEL_KB.default_model("mlx"), None);
    }

    #[test]
    fn find_matches_id_and_aliases() {
        // Exact id.
        assert_eq!(MODEL_KB.find("gemini-3.5-flash").unwrap().id, "gemini-3.5-flash");
        // Alias resolves to the canonical entry.
        let by_alias = MODEL_KB.find("deepseek-reasoner").unwrap();
        assert_eq!(by_alias.id, "deepseek-v4-flash");
        assert!(MODEL_KB.find("no-such-model").is_none());
    }

    #[test]
    fn thinking_only_flag_is_data_driven() {
        // `*-pro` family is thinking-only.
        assert!(MODEL_KB.find("gemini-2.5-pro").unwrap().is_thinking_only());
        assert!(
            MODEL_KB
                .find("gemini-3.1-pro-preview")
                .unwrap()
                .is_thinking_only()
        );
        // flash family is optional (can turn thinking off).
        assert!(!MODEL_KB.find("gemini-3.5-flash").unwrap().is_thinking_only());
        assert!(!MODEL_KB.find("gemini-2.5-flash").unwrap().is_thinking_only());
    }

    #[test]
    fn pricing_reads_from_kb() {
        let p = MODEL_KB.pricing("gpt-5.4").unwrap();
        assert_eq!(p.input_per_million, 2.50);
        assert_eq!(p.output_per_million, 15.00);
        assert_eq!(p.cached_input_per_million, 0.25);
        assert!(MODEL_KB.pricing("no-such-model").is_none());
    }

    /// Guards the `default-substituted-for-absent-or-failed` smell: a live
    /// (`ga`) model with a MISSING price silently bills `$0` (the same
    /// under-billing class as the claude_cli cache-fold bug).
    ///
    /// **Scoped to per-token billing.** The invariant "GA ⇒ has a price" is
    /// only true for providers tars bills per token (`billed = "per_token"`).
    /// Subscription/session/BYO-key providers (CLI, claude_sdk) and free local
    /// ones (mlx/vllm/llamacpp) are GA yet carry NO per-token price — a blank
    /// price there is the TRUTH, not missing data, and must NOT be filled with
    /// a fabricated zero. So the assertion iterates only `PerToken` providers.
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
}
