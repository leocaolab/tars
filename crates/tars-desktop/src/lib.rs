//! tars-desktop — the TARS-native backend core for the desktop debug GUI
//! (Doc 22). Pure Rust, headlessly testable; the Tauri shell (added next) is a
//! thin wrapper that exposes these methods as commands. Reuses
//! `tars_server::AppState` (a pipeline per configured provider) +
//! `tars_runtime::Session` (chat + telemetry).
//!
//! M0 surfaces what `Session` supports per turn (system + max_output_tokens).
//! The full parameter panel (temperature / structured output / thinking) and
//! multi-turn session management land in later milestones (Doc 22 §M1/M2).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use serde::Serialize;
use tokio::sync::Mutex;

use tars_config::Config;
use tars_pipeline::RequestContext;
use tars_runtime::{Budget, Session, SessionOptions};
use tars_server::AppState;
pub use tars_storage::EventRecord;
use tars_storage::{AgentEventLog, open_agent_event_log_at_path};
use tars_types::{
    Capabilities, ChatEvent, ChatRequest, ChatResponse, ChatResponseBuilder, ContentBlock, Message,
    ModelHint, Pricing, SharedTelemetry, StopReason, TelemetryAccumulator, TraceId, TrajectoryId,
    new_shared_telemetry,
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

/// A chat session shown in the sidebar.
#[derive(Debug, Clone, Serialize)]
pub struct ConversationMeta {
    pub id: String,
    pub title: String,
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// One rendered message when a conversation is (re)loaded into the transcript.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMsgView {
    pub role: String, // "user" | "assistant"
    pub text: String,
}

/// Server-side conversation state (history lives here; the GUI is a view).
struct Conversation {
    seq: u64,
    meta: ConversationMeta,
    system: Option<String>,
    max_output_tokens: Option<u32>,
    messages: Vec<Message>,
}

/// One trajectory in the trace sidebar (an agent run — e.g. an arc fix round).
#[derive(Debug, Clone, Serialize)]
pub struct TrajectorySummary {
    pub id: String,
    pub event_count: u64,
}

/// The TARS-native backend the Tauri commands drive.
pub struct Backend {
    state: Arc<AppState>,
    conversations: Mutex<HashMap<String, Conversation>>,
    counter: AtomicU64,
    /// The shared trajectory event store (`events.sqlite`) — what `tars run` /
    /// agents (incl. arc) write their decision trees to. `None` if it can't be
    /// opened; the trace view is then simply empty.
    trajectory_store: Option<Arc<dyn AgentEventLog>>,
}

impl Backend {
    /// Build from a loaded config — reuses tars-server's per-provider pipelines.
    /// Opens the shared trajectory store at its default location for the trace
    /// view (best-effort: `None` if it can't be opened).
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let trajectory_store = default_trajectory_store_path()
            .and_then(|p| open_agent_event_log_at_path(&p).ok())
            .map(|s| s as Arc<dyn AgentEventLog>);
        Self::with_trajectory_store(config, trajectory_store)
    }

    /// Build with an explicit (or no) trajectory store — used by tests.
    pub fn with_trajectory_store(
        config: &Config,
        trajectory_store: Option<Arc<dyn AgentEventLog>>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            state: AppState::from_config(config, None)?,
            conversations: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            trajectory_store,
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

    /// Like [`send_once`](Self::send_once) but **streams**: it drives the
    /// pipeline directly and calls `on_delta` for each text increment as it
    /// arrives, returning the finalized turn (text + metrics) once the stream
    /// ends. The Tauri command passes an `on_delta` that emits a Tauri event;
    /// tests pass one that collects. (M1: single-turn; multi-turn streaming is
    /// a follow-on.)
    pub async fn stream_chat<F: FnMut(&str)>(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
        params: &ChatParams,
        user_text: &str,
        mut on_delta: F,
    ) -> anyhow::Result<ChatTurn> {
        let (llm, model) = self.state.llm_for(provider, model)?;
        let req = ChatRequest {
            system: params.system.clone(),
            max_output_tokens: params.max_output_tokens,
            ..ChatRequest::user(ModelHint::Explicit(model), user_text)
        };
        let telemetry: SharedTelemetry = new_shared_telemetry();
        let mut ctx = RequestContext::personal(TraceId::new(uuid::Uuid::new_v4().to_string()));
        ctx.telemetry = telemetry.clone();

        let mut stream = llm.call(req, ctx).await?;
        let mut builder = ChatResponseBuilder::new();
        while let Some(ev) = stream.next().await {
            let ev = ev?;
            if let ChatEvent::Delta { text } = &ev {
                on_delta(text);
            }
            builder.apply(ev);
        }
        let resp = builder.finish();
        let tel = telemetry.lock().map(|g| g.clone()).unwrap_or_default();
        Ok(ChatTurn {
            text: resp.text.clone(),
            thinking: resp.thinking.clone(),
            metrics: metrics_from(&resp, &tel),
        })
    }

    // ── Conversations (multi-turn, sidebar) ─────────────────────────────

    /// Open a new conversation over a provider/model; returns its sidebar meta.
    pub async fn new_conversation(
        &self,
        provider: Option<String>,
        model: Option<String>,
        system: Option<String>,
        max_output_tokens: Option<u32>,
    ) -> ConversationMeta {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let id = format!("conv-{seq}");
        let meta = ConversationMeta {
            id: id.clone(),
            title: "New chat".to_string(),
            provider,
            model,
        };
        self.conversations.lock().await.insert(
            id,
            Conversation {
                seq,
                meta: meta.clone(),
                system: system.filter(|s| !s.trim().is_empty()),
                max_output_tokens,
                messages: Vec::new(),
            },
        );
        meta
    }

    /// All conversations, newest first (for the sidebar list).
    pub async fn list_conversations(&self) -> Vec<ConversationMeta> {
        let map = self.conversations.lock().await;
        let mut convs: Vec<&Conversation> = map.values().collect();
        convs.sort_by_key(|c| std::cmp::Reverse(c.seq));
        convs.into_iter().map(|c| c.meta.clone()).collect()
    }

    /// The rendered transcript of a conversation (for switching to it).
    pub async fn conversation_messages(&self, id: &str) -> Vec<ChatMsgView> {
        let map = self.conversations.lock().await;
        map.get(id)
            .map(|c| c.messages.iter().filter_map(msg_view).collect())
            .unwrap_or_default()
    }

    /// Send a turn into a conversation and **stream** the reply, keeping
    /// history. The mutex is never held across the model call.
    pub async fn stream_in_conversation<F: FnMut(&str)>(
        &self,
        id: &str,
        user_text: &str,
        mut on_delta: F,
    ) -> anyhow::Result<ChatTurn> {
        // Append the user turn + snapshot request inputs under a brief lock.
        let (provider, model, system, max_tok, history) = {
            let mut map = self.conversations.lock().await;
            let conv = map
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("unknown conversation `{id}`"))?;
            conv.messages.push(Message::user_text(user_text));
            if conv.meta.title == "New chat" {
                conv.meta.title = title_from(user_text);
            }
            (
                conv.meta.provider.clone(),
                conv.meta.model.clone(),
                conv.system.clone(),
                conv.max_output_tokens,
                conv.messages.clone(),
            )
        };

        // Stream with no lock held.
        let (llm, resolved) = self.state.llm_for(provider.as_deref(), model.as_deref())?;
        let req = ChatRequest {
            system,
            max_output_tokens: max_tok,
            messages: history,
            ..ChatRequest::user(ModelHint::Explicit(resolved), "")
        };
        let telemetry: SharedTelemetry = new_shared_telemetry();
        let mut ctx = RequestContext::personal(TraceId::new(uuid::Uuid::new_v4().to_string()));
        ctx.telemetry = telemetry.clone();
        let mut stream = llm.call(req, ctx).await?;
        let mut builder = ChatResponseBuilder::new();
        while let Some(ev) = stream.next().await {
            let ev = ev?;
            if let ChatEvent::Delta { text } = &ev {
                on_delta(text);
            }
            builder.apply(ev);
        }
        let resp = builder.finish();
        let tel = telemetry.lock().map(|g| g.clone()).unwrap_or_default();

        // Append the assistant turn under a brief lock.
        if let Some(conv) = self.conversations.lock().await.get_mut(id) {
            conv.messages.push(Message::Assistant {
                content: vec![ContentBlock::text(resp.text.clone())],
                tool_calls: Vec::new(),
            });
        }

        Ok(ChatTurn {
            text: resp.text.clone(),
            thinking: resp.thinking.clone(),
            metrics: metrics_from(&resp, &tel),
        })
    }

    // ── Trajectories (the trace view — arc writes its runs here too) ─────

    /// List trajectories in the shared store (an agent run each). Order is
    /// store-unspecified; the GUI can sort.
    pub async fn list_trajectories(&self) -> Vec<TrajectorySummary> {
        let Some(store) = &self.trajectory_store else {
            return Vec::new();
        };
        let ids = store.list_trajectories().await.unwrap_or_default();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let event_count = store.high_water(&id).await.unwrap_or(0);
            out.push(TrajectorySummary {
                id: id.to_string(),
                event_count,
            });
        }
        out
    }

    /// Every event of a trajectory, in `sequence_no` order — the decision tree.
    pub async fn trajectory_events(&self, id: &str) -> Vec<EventRecord> {
        let Some(store) = &self.trajectory_store else {
            return Vec::new();
        };
        store
            .read_all(&TrajectoryId::new(id))
            .await
            .unwrap_or_default()
    }
}

fn default_trajectory_store_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("tars").join("events.sqlite"))
}

fn title_from(s: &str) -> String {
    let t: String = s
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(40)
        .collect();
    if t.is_empty() {
        "New chat".to_string()
    } else {
        t
    }
}

fn msg_view(m: &Message) -> Option<ChatMsgView> {
    let (role, content) = match m {
        Message::User { content } => ("user", content),
        Message::Assistant { content, .. } => ("assistant", content),
        _ => return None,
    };
    Some(ChatMsgView {
        role: role.to_string(),
        text: content
            .iter()
            .filter_map(|b| b.as_text().map(str::to_string))
            .collect::<Vec<_>>()
            .join(""),
    })
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

    #[tokio::test]
    async fn stream_chat_drains_and_finalizes() {
        let config = ConfigManager::load_from_str("[providers.mock]\ntype = \"mock\"\n").unwrap();
        let backend = Backend::from_config(&config).unwrap();

        // Collect streamed deltas via the callback; assert the finalized turn.
        let mut streamed = String::new();
        let turn = backend
            .stream_chat(Some("mock"), None, &ChatParams::default(), "hi", |d| {
                streamed.push_str(d)
            })
            .await
            .unwrap();
        assert!(!turn.text.is_empty(), "streamed turn has final text");
        assert!(turn.metrics.stop_reason.is_some(), "metrics populated");
    }

    #[tokio::test]
    async fn conversation_keeps_history_across_turns() {
        let config = ConfigManager::load_from_str("[providers.mock]\ntype = \"mock\"\n").unwrap();
        let backend = Backend::from_config(&config).unwrap();

        let meta = backend
            .new_conversation(Some("mock".into()), None, None, None)
            .await;
        assert!(
            backend
                .list_conversations()
                .await
                .iter()
                .any(|c| c.id == meta.id)
        );

        backend
            .stream_in_conversation(&meta.id, "first", |_| {})
            .await
            .unwrap();
        backend
            .stream_in_conversation(&meta.id, "second", |_| {})
            .await
            .unwrap();

        // 2 user + 2 assistant = history retained across turns.
        let msgs = backend.conversation_messages(&meta.id).await;
        assert_eq!(msgs.len(), 4, "history kept across turns");
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].text, "first");

        // Title set from the first message.
        let title = backend
            .list_conversations()
            .await
            .into_iter()
            .find(|c| c.id == meta.id)
            .unwrap()
            .title;
        assert_eq!(title, "first");
    }

    #[tokio::test]
    async fn trajectory_view_lists_and_reads() {
        // A temp trajectory store with one trajectory + two events.
        let dir = tempfile::tempdir().unwrap();
        let store = open_agent_event_log_at_path(&dir.path().join("events.sqlite")).unwrap();
        let tid = TrajectoryId::new("traj-1");
        store
            .append(
                &tid,
                &[
                    serde_json::json!({ "type": "started" }),
                    serde_json::json!({ "type": "step_completed" }),
                ],
            )
            .await
            .unwrap();

        let config = ConfigManager::load_from_str("[providers.mock]\ntype = \"mock\"\n").unwrap();
        let backend =
            Backend::with_trajectory_store(&config, Some(store as Arc<dyn AgentEventLog>)).unwrap();

        let trajs = backend.list_trajectories().await;
        assert!(
            trajs.iter().any(|t| t.id == "traj-1" && t.event_count == 2),
            "trajectory listed with its event count"
        );
        let events = backend.trajectory_events("traj-1").await;
        assert_eq!(events.len(), 2, "both events read back in order");
        assert_eq!(events[0].sequence_no, 1);
    }
}
