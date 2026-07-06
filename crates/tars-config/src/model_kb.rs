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

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Deserialize;

use tars_types::Pricing;

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

/// The models + defaults for a single provider (`[providers.<name>]`).
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderModels {
    /// Default model id for general use.
    pub default: String,
    /// Optional coding-tuned default (e.g. `gpt-5.3-codex`).
    #[serde(default)]
    pub coding_default: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

/// The whole parsed knowledge base. Keyed by provider name
/// (`"openai"`, `"anthropic"`, …).
#[derive(Debug, Clone, Deserialize)]
pub struct ModelKb {
    pub schema_version: u32,
    pub verified: String,
    pub providers: HashMap<String, ProviderModels>,
}

impl ModelKb {
    /// Default model id for `provider`, or `None` if the provider isn't
    /// in the KB (e.g. local `openai_compat` backends — the user picks).
    pub fn default_model(&self, provider: &str) -> Option<&str> {
        self.providers.get(provider).map(|p| p.default.as_str())
    }

    /// Coding-tuned default model id for `provider`, if one is declared.
    pub fn coding_default_model(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)
            .and_then(|p| p.coding_default.as_deref())
    }

    /// Find a model across all providers by exact `id` **or** any alias.
    pub fn find(&self, model_id: &str) -> Option<&ModelEntry> {
        self.providers
            .values()
            .flat_map(|p| p.models.iter())
            .find(|m| m.id == model_id || m.aliases.iter().any(|a| a == model_id))
    }

    /// Per-model [`Pricing`] from the KB, or `None` for an unknown model.
    pub fn pricing(&self, model_id: &str) -> Option<Pricing> {
        self.find(model_id).map(ModelEntry::pricing)
    }
}

/// Parsed-once knowledge base. **Panics on first access** if
/// `data/models.toml` is malformed — the KB is compiled-in data, a
/// parse failure is a build/authoring bug that must fail loud, not a
/// recoverable runtime condition.
pub static MODEL_KB: LazyLock<ModelKb> = LazyLock::new(|| {
    const RAW: &str = include_str!("../data/models.toml");
    toml::from_str(RAW).expect(
        "crates/tars-config/data/models.toml is malformed — fix the model KB (this is DATA, \
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
        assert_eq!(kb.schema_version, 1);
        assert!(!kb.providers.is_empty());
        for (name, p) in &kb.providers {
            assert!(
                p.models.iter().any(|m| m.id == p.default),
                "provider `{name}` default `{}` is not present in its models",
                p.default
            );
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
    /// under-billing class as the claude_cli cache-fold bug). Every GA row
    /// MUST carry input + output prices; legacy/deprecated/preview rows may
    /// omit them. Fail loud at test time, not silently at billing time.
    #[test]
    fn every_ga_model_carries_input_and_output_price() {
        for (provider, p) in &MODEL_KB.providers {
            for m in &p.models {
                if matches!(m.status, ModelStatus::Ga) {
                    assert!(
                        m.input.is_some() && m.output.is_some(),
                        "GA model `{}` (provider `{provider}`) is missing a price — a live \
                         model must never silently bill $0 (add input/output to models.toml)",
                        m.id
                    );
                }
            }
        }
    }
}
