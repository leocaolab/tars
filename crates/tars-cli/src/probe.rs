//! `tars probe <provider-id>` — sanity-check any provider.
//!
//! ## What it does
//!
//! Loads the user's config, builds the named provider, sends a fixed
//! "say hello" prompt, and streams every [`ChatEvent`] to stderr in
//! human-readable form. Exits non-zero on any error or if the stream
//! ends without a `Finished` event.
//!
//! ## Works for every provider type
//!
//! Originally CLI-only (`claude_cli` / `gemini_cli` / `codex_cli`)
//! because those are the trickiest to get auth + binary lookup right.
//! Loosened to **all provider types** since the event-by-event dump
//! is genuinely more informative than `tars run` for HTTP providers
//! too — you see usage breakdown, model echo, individual deltas, and
//! exact error variants. Especially useful for local OpenAI-compat
//! servers (LM Studio / vLLM / MLX / llama.cpp) where the user
//! typically wants to confirm "yes, the local model server is up,
//! the loaded model name is X, and tars can talk to it" before using
//! it in `tars run-task`.
//!
//! ## Output shape
//!
//! Mirrors the smoke test format (`tars-provider/tests/*_smoke.rs`):
//!
//! ```text
//! ── tars probe codex_cli (model=gpt-5.5) ──
//! [evt  1] Started     model=gpt-5.5
//! [evt  2] Delta       text="hello from codex"
//! [evt  3] Finished    stop=EndTurn in=13023 out=19 cached=11648 thinking=9
//!
//! ── final ──
//! text     = "hello from codex"
//! events   = 3
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Args;
use futures::StreamExt;

use tars_provider::auth::basic;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::registry::ProviderRegistry;
use tars_types::{ChatEvent, ChatRequest, ModelHint, ProviderId, RequestContext};

use crate::config_loader;

#[derive(Args, Debug)]
pub struct ProbeArgs {
    /// Provider id from your config. Works for every provider type —
    /// CLI subscriptions (`claude_cli` / `gemini_cli` / `codex_cli`),
    /// direct HTTP APIs (`openai` / `anthropic` / `gemini`), and
    /// local OpenAI-compatible servers (`openai_compat` / `vllm` /
    /// `mlx` / `llamacpp`).
    pub provider: String,

    /// Override the provider's `default_model` for this probe.
    /// Useful when a subscription tier doesn't allow the configured
    /// default (e.g. `--model gpt-5.5` for ChatGPT accounts that
    /// can't access `gpt-5`).
    #[arg(short, long)]
    pub model: Option<String>,

    /// Override the prompt. Default: a short "say hello" message
    /// the model can answer cheaply.
    #[arg(short, long)]
    pub prompt: Option<String>,
}

const DEFAULT_PROMPT: &str = "Say exactly: hello from your provider. Nothing else.";

pub async fn execute(args: ProbeArgs, config_path: Option<PathBuf>) -> Result<()> {
    let cfg = config_loader::load(config_path)?;
    let provider_id = ProviderId::new(args.provider.clone());

    // Look up the config entry first so we can reject non-CLI types
    // with a clear message before any subprocess work.
    let provider_cfg = cfg.providers.get(&provider_id).ok_or_else(|| {
        let configured: Vec<String> =
            cfg.providers.iter().map(|(id, _)| id.to_string()).collect();
        anyhow::anyhow!(
            "provider `{}` not in config. Configured: [{}]",
            args.provider,
            configured.join(", "),
        )
    })?;

    let model = args
        .model
        .unwrap_or_else(|| provider_cfg.default_model().to_string());
    let prompt = args.prompt.as_deref().unwrap_or(DEFAULT_PROMPT);

    let http = HttpProviderBase::default_arc().context("constructing reqwest client")?;
    let registry = ProviderRegistry::from_config(&cfg.providers, http, basic())
        .context("building provider registry from config")?;
    let provider = registry.get(&provider_id).ok_or_else(|| {
        anyhow::anyhow!(
            "registry missing provider `{provider_id}` (validated config but build failed?)",
        )
    })?;

    let req = ChatRequest::user(ModelHint::Explicit(model.clone()), prompt);

    eprintln!(
        "── tars probe {} (model={model}) ──",
        args.provider,
    );

    let mut stream = Arc::clone(&provider)
        .stream(req, RequestContext::test_default())
        .await
        .with_context(|| format!("provider `{}` stream() failed", args.provider))?;

    let mut event_count: usize = 0;
    let mut text_chunks: Vec<String> = Vec::new();
    let mut thinking_chunks: Vec<String> = Vec::new();
    let mut saw_finished = false;
    while let Some(ev) = stream.next().await {
        event_count += 1;
        match ev {
            Ok(ChatEvent::Started { actual_model, .. }) => {
                eprintln!("[evt {event_count:>2}] Started     model={actual_model}");
            }
            Ok(ChatEvent::Delta { text }) => {
                eprintln!("[evt {event_count:>2}] Delta       text={text:?}");
                text_chunks.push(text);
            }
            Ok(ChatEvent::ThinkingDelta { text }) => {
                eprintln!("[evt {event_count:>2}] Thinking    text={text:?}");
                thinking_chunks.push(text);
            }
            Ok(ChatEvent::ToolCallStart { id, name, .. }) => {
                eprintln!("[evt {event_count:>2}] ToolStart   id={id} name={name}");
            }
            Ok(ChatEvent::ToolCallArgsDelta { index, args_delta }) => {
                eprintln!("[evt {event_count:>2}] ToolArgs    idx={index} delta={args_delta:?}");
            }
            Ok(ChatEvent::ToolCallEnd { id, .. }) => {
                eprintln!("[evt {event_count:>2}] ToolEnd     id={id}");
            }
            Ok(ChatEvent::UsageProgress { partial }) => {
                eprintln!("[evt {event_count:>2}] UsageProg   {partial:?}");
            }
            Ok(ChatEvent::Finished { stop_reason, usage }) => {
                eprintln!(
                    "[evt {event_count:>2}] Finished    stop={stop_reason:?} \
                     in={} out={} cached={} thinking={}",
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cached_input_tokens,
                    usage.thinking_tokens,
                );
                saw_finished = true;
            }
            Err(e) => {
                eprintln!("[evt {event_count:>2}] ERROR       {e:?}");
                bail!("provider error mid-stream after {event_count} events");
            }
        }
    }

    let full_text = text_chunks.concat();
    let full_thinking = thinking_chunks.concat();

    eprintln!();
    eprintln!("── final ──");
    eprintln!("text     = {full_text:?}");
    if !full_thinking.is_empty() {
        eprintln!("thinking = {full_thinking:?}");
    }
    eprintln!("events   = {event_count}");

    if !saw_finished {
        bail!("stream ended without a Finished event ({event_count} events received)");
    }
    if full_text.is_empty() {
        bail!("provider returned no text (saw {event_count} events but no Delta)");
    }

    Ok(())
}

