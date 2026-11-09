//! Runs `tars probe <provider-id>`: sends one prompt to a provider and prints each streamed event
//! to stderr:
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
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use clap::Args;
use futures::StreamExt;

use tars_llm::provider::registry::RegistryError;
use tars_llm::{LlmError, ProviderError};
use tars_tools::SandboxMode;
use tars_types::{ChatRequest, ProviderId, TenantId};

use crate::Ending;
use crate::config::{self, LoadConfigError};
use crate::home_env::HomeEnvError;
use crate::registry::{NotConfiguredError, build_registry, configured};
use crate::single_attempt;
use crate::transcript::{StreamEnd, Transcript, event_line, stream_end};

#[derive(Args, Debug)]
pub struct ProbeArgs {
    /// Provider id from your config. Works for every provider type —
    /// CLI subscriptions (`claude_cli` / `codex_cli` / `antigravity`),
    /// direct HTTP APIs (`openai` / `anthropic` / `gemini`), and
    /// local OpenAI-compatible servers (`openai_compat` / `vllm` /
    /// `mlx` / `llamacpp`).
    #[arg(value_parser = ProviderId::from_str)]
    pub provider: ProviderId,

    /// The tenant the probe call is made for.
    #[arg(long, value_parser = TenantId::from_str)]
    pub tenant: TenantId,

    /// Override the provider's `default_model` for this probe, e.g. when
    /// a subscription tier does not allow the configured default; a
    /// `<series>@latest` spec resolves against the provider's catalog.
    #[arg(short, long)]
    pub model: Option<String>,

    /// Override the prompt. Default: a short "say hello" message
    /// the model can answer cheaply.
    #[arg(short, long)]
    pub prompt: Option<String>,

    /// Cap output tokens. Without it the model's ceiling is sent, as on any call, or nothing
    /// when tars does not know the ceiling.
    #[arg(long)]
    pub max_tokens: Option<u32>,
}

const DEFAULT_PROMPT: &str = "Say exactly: hello from your provider. Nothing else.";

/// Why `tars probe` did not reach a verdict, or why the provider failed it.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error(transparent)]
    Config(#[from] LoadConfigError),
    #[error(transparent)]
    Env(#[from] HomeEnvError),
    #[error(transparent)]
    NotConfigured(#[from] NotConfiguredError),
    #[error("building provider registry from config")]
    Registry(#[source] RegistryError),
    #[error("provider `{provider}`")]
    Unavailable {
        provider: ProviderId,
        #[source]
        source: RegistryError,
    },
    #[error("--model for provider `{provider}`")]
    ModelUnresolved {
        provider: ProviderId,
        #[source]
        source: RegistryError,
    },
    #[error("provider `{provider}` refused to open a stream")]
    Refused {
        provider: ProviderId,
        #[source]
        source: LlmError,
    },
    #[error("provider error mid-stream after {events} events")]
    MidStream {
        events: usize,
        #[source]
        source: ProviderError,
    },
    #[error("stream ended without a Finished event ({events} events received)")]
    Unfinished { events: usize },
    #[error(
        "provider returned no text for prompt {prompt:?} using model {model} \
         (saw {events} events but no Delta or tool call)"
    )]
    NoText {
        prompt: String,
        model: String,
        events: usize,
    },
}

impl ProbeError {
    /// Says whether the probe ran: once the prompt is sent, every failure is the provider's
    /// answer to it. A request the provider's profile cannot serve is never sent.
    pub(crate) fn ending(&self) -> Ending {
        match self {
            Self::Config(_)
            | Self::Env(_)
            | Self::NotConfigured(_)
            | Self::Registry(_)
            | Self::Unavailable { .. }
            | Self::ModelUnresolved { .. } => Ending::NotRun,
            Self::Refused {
                source: LlmError::Incompatible { .. },
                ..
            } => Ending::NotRun,
            Self::Refused { .. }
            | Self::MidStream { .. }
            | Self::Unfinished { .. }
            | Self::NoText { .. } => Ending::Failed,
        }
    }
}

pub async fn execute(
    args: ProbeArgs,
    config_path: Option<PathBuf>,
    sandbox: Option<SandboxMode>,
) -> Result<(), ProbeError> {
    let cfg = config::load(config_path)?;

    let prompt = args.prompt.as_deref().unwrap_or(DEFAULT_PROMPT);

    let id = configured(&cfg, args.provider)?;
    let registry =
        build_registry(&cfg, Arc::new(crate::home_env::load()?)).map_err(ProbeError::Registry)?;
    let provider = registry
        .get(&id)
        .map_err(|source| ProbeError::Unavailable {
            provider: id.clone(),
            source,
        })?;
    let model = registry
        .resolve_model(&id, args.model.as_deref())
        .map_err(|source| ProbeError::ModelUnresolved {
            provider: id.clone(),
            source,
        })?;

    let mut req = ChatRequest::user(prompt);
    req.max_output_tokens = args.max_tokens;

    eprintln!("── tars probe {id} (model={model}) ──");

    let mut stream = single_attempt::service(provider, &model)
        .call(req, crate::request_context_for(args.tenant, &cfg, sandbox))
        .await
        .result
        .map_err(|source| ProbeError::Refused {
            provider: id.clone(),
            source,
        })?;

    let started_at = Instant::now();
    let mut transcript = Transcript::default();
    while let Some(ev) = stream.next().await {
        let n = transcript.events + 1;
        match ev {
            Ok(event) => {
                eprintln!("{}", event_line(n, &event));
                transcript = transcript.step(event, started_at.elapsed());
            }
            Err(source) => {
                eprintln!("[evt {n:>2}] ERROR");
                return Err(ProbeError::MidStream { events: n, source });
            }
        }
    }

    eprintln!();
    eprintln!("── final ──");
    eprintln!("text     = {:?}", transcript.text);
    if !transcript.thinking.is_empty() {
        eprintln!("thinking = {:?}", transcript.thinking);
    }
    eprintln!("events   = {}", transcript.events);

    let end = stream_end(
        transcript.finished.is_some(),
        !transcript.text.is_empty(),
        transcript.tool_calls > 0,
    );
    match end {
        StreamEnd::Answered => {}
        StreamEnd::Unfinished => {
            return Err(ProbeError::Unfinished {
                events: transcript.events,
            });
        }
        StreamEnd::Empty => {
            return Err(ProbeError::NoText {
                prompt: prompt.to_string(),
                model,
                events: transcript.events,
            });
        }
    }

    Ok(())
}
