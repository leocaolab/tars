//! tars-desktop — the TARS-native backend core for the desktop debug GUI
//! (Doc 22). Pure Rust, headlessly testable; the Tauri shell (added next) is a
//! thin wrapper that exposes these methods as commands. Reuses
//! `tars_server::AppState` (a pipeline per configured provider) +
//! `tars_runtime::Session` (chat + telemetry).
//!
//! M0 surfaces what `Session` supports per turn (system + max_output_tokens).
//! The full parameter panel (temperature / structured output / thinking) and
//! multi-turn session management land in later milestones (Doc 22 §M1/M2).

use std::sync::Arc;

use serde::Serialize;
use tars_config::Config;
use tars_runtime::{Budget, Session, SessionOptions};
use tars_server::AppState;
use tars_types::{
    Capabilities, ChatResponse, ModelHint, Pricing, StopReason, TelemetryAccumulator,
};

/// A configured provider, for the model picker.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub default_model: Option<String>,
    pub is_default: bool,
}

/// Per-turn parameters the GUI can set (M0: the subset `Session` supports).
#[derive(Debug, Clone, Default)]
pub struct ChatParams {
    pub system: Option<String>,
    pub max_output_tokens: Option<u32>,
}

/// Per-message metrics shown under each reply (LM-Studio-style).
#[derive(Debug, Clone, Default, Serialize)]
pub struct TurnMetrics {
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub latency_ms: Option<u64>,
    pub tok_per_sec: Option<f64>,
    pub stop_reason: Option<String>,
    pub cache_hit: bool,
    pub retry_count: u32,
    pub provider: Option<String>,
}

/// The result of one chat turn.
#[derive(Debug, Clone, Serialize)]
pub struct ChatTurn {
    pub text: String,
    pub thinking: String,
    pub metrics: TurnMetrics,
}

/// The TARS-native backend the Tauri commands drive.
pub struct Backend {
    state: Arc<AppState>,
}

impl Backend {
    /// Build from a loaded config — reuses tars-server's per-provider pipelines.
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            state: AppState::from_config(config, None)?,
        })
    }

    /// The configured providers, for the picker dropdown.
    pub fn providers(&self) -> Vec<ProviderInfo> {
        let default = self.state.default_provider();
        self.state
            .provider_ids()
            .into_iter()
            .map(|id| ProviderInfo {
                default_model: self.state.default_model_for(&id).map(str::to_string),
                is_default: Some(id.as_str()) == default,
                id,
            })
            .collect()
    }

    /// Run one chat turn: build a `Session` over the chosen provider, send
    /// `user_text`, return the reply + per-call metrics. (M0: single-turn, no
    /// stored history — M1 holds `Session`s for multi-turn conversations.)
    pub async fn send_once(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
        params: &ChatParams,
        user_text: &str,
    ) -> anyhow::Result<ChatTurn> {
        let (llm, model) = self.state.llm_for(provider, model)?;
        // M0: a baseline capabilities object — `Session` only uses it for
        // budget trimming, and the budget is effectively unbounded here.
        // Threading each provider's real `Capabilities` through `AppState` is a
        // follow-on seam (matters once the GUI shows context windows / pricing).
        let capabilities = Capabilities::text_only_baseline(Pricing::default());
        let mut session = Session::new(
            llm,
            capabilities,
            SessionOptions {
                system: params.system.clone().unwrap_or_default(),
                budget: Budget::Chars(usize::MAX / 2),
                tools: None,
                tool_ctx: Default::default(),
                default_max_output_tokens: params.max_output_tokens,
                model: ModelHint::Explicit(model),
            },
        );
        let (resp, telemetry) = session.send(user_text, params.max_output_tokens).await?;
        Ok(ChatTurn {
            text: resp.text.clone(),
            thinking: resp.thinking.clone(),
            metrics: metrics_from(&resp, &telemetry),
        })
    }
}

fn metrics_from(resp: &ChatResponse, tel: &TelemetryAccumulator) -> TurnMetrics {
    let output_tokens = resp.usage.output_tokens;
    let total_tokens =
        resp.usage.input_tokens + resp.usage.output_tokens + resp.usage.thinking_tokens;
    let latency_ms = tel.pipeline_total_ms;
    let tok_per_sec = latency_ms
        .filter(|&ms| ms > 0)
        .map(|ms| output_tokens as f64 / (ms as f64 / 1000.0));
    TurnMetrics {
        output_tokens,
        total_tokens,
        latency_ms,
        tok_per_sec,
        stop_reason: resp.stop_reason.map(stop_reason_str),
        cache_hit: tel.cache_hit,
        retry_count: tel.retry_count,
        provider: tel.provider_id.clone(),
    }
}

fn stop_reason_str(s: StopReason) -> String {
    match s {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::ContentFilter => "content_filter",
        StopReason::Cancelled => "cancelled",
        StopReason::Other => "other",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_config::ConfigManager;

    #[tokio::test]
    async fn send_once_over_mock_returns_text_and_metrics() {
        let config = ConfigManager::load_from_str("[providers.mock]\ntype = \"mock\"\n").unwrap();
        let backend = Backend::from_config(&config).unwrap();

        // Provider picker sees the mock provider.
        let providers = backend.providers();
        assert!(providers.iter().any(|p| p.id == "mock"));

        // One turn returns text + populated metrics (the M0 spine).
        let turn = backend
            .send_once(Some("mock"), None, &ChatParams::default(), "hello")
            .await
            .unwrap();
        assert!(!turn.text.is_empty(), "mock should return some text");
        assert!(
            turn.metrics.stop_reason.is_some(),
            "metrics should be populated (stop_reason)"
        );
    }
}
