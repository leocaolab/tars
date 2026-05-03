//! `tars probe <provider-id>` — sanity-check a CLI provider.
//!
//! ## What it does
//!
//! Loads the user's config, builds the named provider (must be one of
//! `claude_cli` / `gemini_cli` / `codex_cli`), sends a fixed "say
//! hello" prompt, and streams every [`ChatEvent`] to stderr in
//! human-readable form. Exits non-zero on any error or if the stream
//! ends without a `Finished` event.
//!
//! ## Why CLI-only
//!
//! HTTP providers (openai / anthropic / gemini / vllm / mlx /
//! llamacpp) already give clear failure signals — auth errors, network
//! errors, 400/401/403 — straight from the wire. There's no need for
//! a separate probe command; `tars run -P openai_main --prompt ...`
//! is just as informative.
//!
//! CLI providers are different — they shell out to a user-installed
//! binary, do credential resolution out-of-band (`~/.codex/auth.json`
//! / `claude login` / `gemini auth login`), and surface most failures
//! as opaque non-zero exits or JSONL stream errors. The probe command
//! makes the streaming + auth + binary plumbing visible so a user
//! debugging "why doesn't `tars run-task -P codex_cli` work" sees
//! exactly which step broke.
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

use tars_config::ProviderConfig;
use tars_provider::auth::basic;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::registry::ProviderRegistry;
use tars_types::{ChatEvent, ChatRequest, ModelHint, ProviderId, RequestContext};

use crate::config_loader;

#[derive(Args, Debug)]
pub struct ProbeArgs {
    /// Provider id to probe. Must be a CLI-type provider
    /// (`claude_cli` / `gemini_cli` / `codex_cli`); HTTP providers
    /// are rejected with a hint.
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

    if !is_cli_provider(provider_cfg) {
        bail!(
            "`tars probe` only supports CLI providers (`claude_cli` / `gemini_cli` / \
             `codex_cli`). `{}` is type `{}` — for HTTP providers use `tars run -P {} \
             --prompt 'say hi'` which gives the same signal via the normal request path.",
            args.provider,
            provider_cfg.type_label(),
            args.provider,
        );
    }

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

fn is_cli_provider(cfg: &ProviderConfig) -> bool {
    matches!(
        cfg,
        ProviderConfig::ClaudeCli { .. }
            | ProviderConfig::GeminiCli { .. }
            | ProviderConfig::CodexCli { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::Auth;

    fn http_cfg() -> ProviderConfig {
        ProviderConfig::Openai {
            base_url: None,
            auth: Auth::None,
            default_model: "gpt-4o".into(),
            extras: Default::default(),
        }
    }

    fn claude_cli_cfg() -> ProviderConfig {
        ProviderConfig::ClaudeCli {
            executable: "claude".into(),
            timeout_secs: 300,
            default_model: "sonnet".into(),
        }
    }

    #[test]
    fn is_cli_provider_accepts_all_three_cli_types() {
        assert!(is_cli_provider(&claude_cli_cfg()));
        assert!(is_cli_provider(&ProviderConfig::GeminiCli {
            executable: "gemini".into(),
            timeout_secs: 300,
            default_model: "gemini-2.5-pro".into(),
        }));
        assert!(is_cli_provider(&ProviderConfig::CodexCli {
            executable: "codex".into(),
            timeout_secs: 600,
            sandbox: tars_config::CodexSandboxConfig::ReadOnly,
            skip_git_repo_check: true,
            default_model: "gpt-5.5".into(),
        }));
    }

    #[test]
    fn is_cli_provider_rejects_http_types() {
        assert!(!is_cli_provider(&http_cfg()));
        assert!(!is_cli_provider(&ProviderConfig::Mock {
            canned_response: "x".into(),
        }));
    }
}
