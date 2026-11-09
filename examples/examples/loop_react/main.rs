//! POC: a minimal **loop-and-react** agent over the real TARS pipeline.
//!
//! Proves the cycle end to end:
//!
//! ```text
//!   offer tools  →  LLM decides a tool_use  →  dispatch to a native
//!   Rust handler  →  feed the result back  →  loop  →  until the LLM
//!   emits no tool_use (done) or we hit max_turns.
//! ```
//!
//! The LLM call goes through the *real* provider path — config from
//! `~/.tars/config.toml`, a live provider selected by id — NOT a
//! scripted fake (contrast `multi_step_with_tools`, which cans the
//! stream). Tool-calling is **native**: we put `ToolSpec`s on the
//! `ChatRequest` and read `ChatEvent::ToolCall{Start,End}` back off the
//! stream; no JSON-in-text convention.
//!
//! Toy task (forces ≥1 tool call and ≥1 read of a tool result):
//!
//!   "read scratch/input.txt, then write its line count to scratch/out.txt"
//!
//! Run (needs GEMINI_API_KEY in the env — source the repo `.env` first):
//! ```bash
//!   set -a; source .env; set +a
//!   cargo run -p tars-examples --example loop_react
//!   # override the provider (must be a native tool-calling backend):
//!   TARS_POC_PROVIDER=openai cargo run -p tars-examples --example loop_react
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use tars_config::Config;
use tars_llm::provider::registry::ProviderRegistry;
use tars_llm::{LlmService, RetryMiddleware, TelemetryMiddleware};
use tars_types::{
    Caller, ChatEvent, ChatRequest, ContentBlock, JsonSchema, Message, ProviderId, StopReason,
    ToolCall, ToolSpec, Usage,
};
use tars_utils::display_chain;

/// An example's tenant: a real deployment passes whose work the run is.
const EXAMPLE_TENANT: &str = "example";

// ─────────────────────────────────────────────────────────────────────
// The fn-access-map: native Rust tools the agent can call.
// A tool = a name + a JSON schema + an async Rust handler.
// ─────────────────────────────────────────────────────────────────────

#[async_trait]
trait NativeTool: Send + Sync {
    fn name(&self) -> &str;
    /// The schema we advertise to the model (goes into `request.tools`).
    fn spec(&self) -> ToolSpec;
    /// The actual Rust function the model's tool_use dispatches to.
    async fn call(&self, args: Value) -> Result<String, ToolError>;
}

/// `read_file(path) -> String` — jailed to a scratch root.
struct ReadFile {
    root: PathBuf,
}

#[async_trait]
impl NativeTool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "read_file",
            "Read a UTF-8 text file from the scratch directory and return its \
             full contents. Use this to inspect a file before acting on it. \
             `path` is relative to the scratch dir (e.g. \"input.txt\").",
            JsonSchema::strict(
                "read_file_args",
                tars_types::json_object! {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "relative path under the scratch dir" }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                },
            ),
        )
        .expect("static tool name is non-empty")
    }
    async fn call(&self, args: Value) -> Result<String, ToolError> {
        let rel = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or(ToolError::MissingString {
                tool: "read_file",
                key: "path",
            })?;
        let full = jail(&self.root, rel)?;
        let body = tokio::fs::read_to_string(&full)
            .await
            .map_err(|source| ToolError::Read { path: full, source })?;
        Ok(body)
    }
}

/// `write_file(path, contents) -> String` — jailed to a scratch root.
struct WriteFile {
    root: PathBuf,
}

#[async_trait]
impl NativeTool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "write_file",
            "Write (create or overwrite) a UTF-8 text file in the scratch \
             directory. `path` is relative to the scratch dir; `contents` is \
             the exact bytes to write. Returns a short confirmation.",
            JsonSchema::strict(
                "write_file_args",
                tars_types::json_object! {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "relative path under the scratch dir" },
                        "contents": { "type": "string", "description": "text to write" }
                    },
                    "required": ["path", "contents"],
                    "additionalProperties": false
                },
            ),
        )
        .expect("static tool name is non-empty")
    }
    async fn call(&self, args: Value) -> Result<String, ToolError> {
        let rel = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or(ToolError::MissingString {
                tool: "write_file",
                key: "path",
            })?;
        let contents =
            args.get("contents")
                .and_then(Value::as_str)
                .ok_or(ToolError::MissingString {
                    tool: "write_file",
                    key: "contents",
                })?;
        let full = jail(&self.root, rel)?;
        tokio::fs::write(&full, contents)
            .await
            .map_err(|source| ToolError::Write { path: full, source })?;
        Ok(format!("wrote {} bytes to {}", contents.len(), rel))
    }
}

/// Why a tool call did not run; its words go back to the model as the tool's error result.
#[derive(Debug, thiserror::Error)]
enum ToolError {
    #[error("{tool}: missing string arg `{key}`")]
    MissingString {
        tool: &'static str,
        key: &'static str,
    },
    #[error("path `{rel}` escapes the scratch jail")]
    OutsideJail { rel: String },
    #[error("read_file: reading {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("write_file: writing {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Reject path traversal — every tool path must resolve inside `root`.
fn jail(root: &Path, rel: &str) -> Result<PathBuf, ToolError> {
    let candidate = root.join(rel);
    // Cheap structural check (the file may not exist yet, so we can't
    // canonicalize the leaf). Good enough for a jailed scratch POC.
    if rel.contains("..") || Path::new(rel).is_absolute() {
        return Err(ToolError::OutsideJail {
            rel: rel.to_string(),
        });
    }
    Ok(candidate)
}

/// The registry: name → tool. This IS the "app as world" map the agent
/// reaches into.
struct ToolRegistry {
    tools: HashMap<String, Arc<dyn NativeTool>>,
}

impl ToolRegistry {
    fn new(tools: Vec<Arc<dyn NativeTool>>) -> Self {
        Self {
            tools: tools
                .into_iter()
                .map(|t| (t.name().to_string(), t))
                .collect(),
        }
    }
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }
    fn get(&self, name: &str) -> Option<Arc<dyn NativeTool>> {
        self.tools.get(name).cloned()
    }
}

// ─────────────────────────────────────────────────────────────────────
// One turn's worth of what we scraped off the provider stream.
// ─────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct TurnReply {
    text: String,
    tool_calls: Vec<ToolCall>,
    stop_reason: Option<StopReason>,
    /// `None` when the provider reported no usage; the totals then count it as zero.
    usage: Option<Usage>,
    actual_model: Option<String>,
}

/// Drain one streaming completion into a `TurnReply`. Native tool-calls
/// arrive as `ToolCallStart` (name+id) then `ToolCallEnd` (parsed args +
/// thought_signature); we correlate them by `index`.
async fn drain(mut stream: tars_llm::LlmEventStream) -> Result<TurnReply, tars_llm::ProviderError> {
    let mut reply = TurnReply::default();
    let mut names: HashMap<usize, (String, String)> = HashMap::new(); // index → (id, name)

    while let Some(ev) = stream.next().await {
        match ev? {
            ChatEvent::Started { actual_model, .. } => reply.actual_model = actual_model,
            ChatEvent::Delta { text } => reply.text.push_str(&text),
            ChatEvent::Degraded { .. } | ChatEvent::ThinkingDelta { .. } => {}
            ChatEvent::ToolCallStart { index, id, name } => {
                names.insert(index, (id, name));
            }
            ChatEvent::ToolCallEnd {
                index,
                id,
                parsed_args,
                thought_signature,
            } => {
                // Prefer the name captured at Start; fall back to the id.
                let name = names
                    .get(&index)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| id.clone());
                reply.tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: parsed_args,
                    thought_signature,
                });
            }
            ChatEvent::ToolCallArgsDelta { .. } => {}
            ChatEvent::UsageProgress { .. }
            | ChatEvent::Measured { .. }
            | ChatEvent::Validated { .. } => {}
            ChatEvent::Finished { stop_reason, usage } => {
                reply.stop_reason = Some(stop_reason);
                reply.usage = usage;
            }
        }
    }
    Ok(reply)
}

// ─────────────────────────────────────────────────────────────────────

/// Why the demo stopped, or why the agent's answer is wrong.
#[derive(Debug, thiserror::Error)]
enum DemoError {
    #[error("creating scratch tempdir")]
    Scratch(#[source] std::io::Error),
    #[error("seeding input.txt")]
    Seed(#[source] std::io::Error),
    #[error("TARS_POC_PROVIDER `{raw}` is not a provider id")]
    ProviderIdInvalid {
        raw: String,
        #[source]
        source: tars_types::ParseProviderIdError,
    },
    #[error("could not locate a tars config.toml")]
    NoConfig,
    #[error("loading tars config from {}", path.display())]
    Config {
        path: PathBuf,
        #[source]
        source: tars_config::ConfigError,
    },
    #[error("building provider registry")]
    Registry(#[source] tars_llm::RegistryError),
    #[error("provider `{id}` not in config; available: {available:?}")]
    Provider {
        id: ProviderId,
        available: Vec<ProviderId>,
        #[source]
        source: tars_llm::RegistryError,
    },
    #[error("provider has no default_model; set one in config")]
    NoDefaultModel,
    #[error("turn {turn}: opening provider stream")]
    Open {
        turn: usize,
        #[source]
        source: tars_llm::LlmError,
    },
    #[error("turn {turn}: draining provider stream")]
    Drain {
        turn: usize,
        #[source]
        source: tars_llm::ProviderError,
    },
    #[error("agent hit MAX_TURNS ({MAX_TURNS}) without finishing")]
    Unfinished,
    #[error("agent never wrote {}", path.display())]
    NotWritten {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("out.txt is not a bare integer: {text:?}")]
    NotACount {
        text: String,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("FAIL — out.txt says {got} lines, expected {expected}")]
    WrongCount { got: usize, expected: usize },
}

const MAX_TURNS: usize = 8;

async fn run() -> Result<(), DemoError> {
    // 1. Scratch world: input.txt the agent must read, out.txt it must write.
    let scratch = tempfile::tempdir().map_err(DemoError::Scratch)?;
    let root = scratch.path().to_path_buf();
    let input_body = "apple\nbanana\ncherry\ndate\nelderberry\n";
    let expected_lines = input_body.lines().count(); // 5
    tokio::fs::write(root.join("input.txt"), input_body)
        .await
        .map_err(DemoError::Seed)?;
    println!("scratch dir: {}", root.display());
    println!("seeded input.txt with {expected_lines} lines\n");

    // 2. The fn-access-map.
    let registry = ToolRegistry::new(vec![
        Arc::new(ReadFile { root: root.clone() }),
        Arc::new(WriteFile { root: root.clone() }),
    ]);

    // 3. Real provider from real config. Provider id via env, default
    //    gemini_flash (cheap, fast, native tool-calling).
    let raw_provider = std::env::var("TARS_POC_PROVIDER").unwrap_or_else(|_| "gemini_flash".into());
    let provider_id: ProviderId =
        raw_provider
            .parse()
            .map_err(|source| DemoError::ProviderIdInvalid {
                raw: raw_provider.clone(),
                source,
            })?;
    let cfg_path = resolve_config_path().ok_or(DemoError::NoConfig)?;
    println!("loading config from {}", cfg_path.display());
    let cfg = Config::load_file(&cfg_path).map_err(|source| DemoError::Config {
        path: cfg_path.clone(),
        source,
    })?;
    let registry_p =
        ProviderRegistry::from_config_default(cfg.providers(), tars_config::catalog(), None)
            .map_err(DemoError::Registry)?;
    let provider = registry_p
        .get(&provider_id)
        .map_err(|source| DemoError::Provider {
            id: provider_id.clone(),
            available: cfg.providers().providers.keys().cloned().collect(),
            source,
        })?;
    let model = provider_id_default_model(&cfg, &provider_id).ok_or(DemoError::NoDefaultModel)?;
    println!("provider: {provider_id}   model: {model}\n");

    // Minimal real pipeline: telemetry + retry over the leaf service.
    let pipeline = LlmService::builder_with_inner(LlmService::of(provider, model.clone()))
        .layer(TelemetryMiddleware::new())
        .layer(RetryMiddleware::default())
        .build();

    // 4. The loop.
    let system = "You are an automation agent. You accomplish the user's task \
                  ONLY by calling the provided tools; you cannot see or touch \
                  files except through them. When the task is fully done, reply \
                  with a short plain-text confirmation and DO NOT call any more \
                  tools.";
    let task = "Read the file input.txt, count how many lines it has, then write \
                that count (as a decimal number, nothing else) to out.txt.";

    let mut messages: Vec<Message> = vec![Message::user_text(task)];
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut finished_cleanly = false;

    for turn in 1..=MAX_TURNS {
        // Assemble the request: full running conversation + tool schemas.
        let mut req = ChatRequest::user(""); // placeholder; overwrite messages
        req.system = Some(system.to_string());
        req.messages = messages.clone();
        req.tools = registry.specs();
        req.max_output_tokens = Some(512);

        // POC_DUMP=1 → print the FULL context sent to the LLM this turn.
        // The LLM is stateless: everything it "knows" is in these messages,
        // which grow by (assistant tool_call + tool result) each turn.
        if std::env::var("POC_DUMP").is_ok() {
            println!(
                "\n╔══ Turn {turn}: CONTEXT → LLM  ({} messages, {} tools) ══",
                messages.len(),
                registry.specs().len()
            );
            println!("  system: {system}");
            for (i, m) in messages.iter().enumerate() {
                println!("  [{i}] {m:?}");
            }
            println!("╚═══════════════════════════════════════════════════════════\n");
        }

        let ctx = Caller::new(
            tars_types::TenantId::new(EXAMPLE_TENANT),
            tars_types::TraceId::new("loop-react"),
        );
        let stream = pipeline
            .call(req, ctx)
            .await
            .result
            .map_err(|source| DemoError::Open { turn, source })?;
        let reply = drain(stream)
            .await
            .map_err(|source| DemoError::Drain { turn, source })?;

        total_in += reply.usage.map_or(0, |u| u.input_tokens);
        total_out += reply.usage.map_or(0, |u| u.output_tokens);

        println!("── Turn {turn} ───────────────────────────────────────────");
        println!(
            "  LLM: stop={:?}  model={}  tokens in/out={}/{}",
            reply.stop_reason.unwrap_or(StopReason::Other),
            reply.actual_model.as_deref().unwrap_or("?"),
            reply.usage.map_or(0, |u| u.input_tokens),
            reply.usage.map_or(0, |u| u.output_tokens),
        );
        if !reply.text.trim().is_empty() {
            println!("  text: {}", reply.text.trim());
        }

        // No tool call → the model considers itself done.
        if reply.tool_calls.is_empty() {
            println!("  (no tool_use — agent is done)\n");
            finished_cleanly = true;
            break;
        }

        // Record the assistant's tool-call turn verbatim (incl. any text
        // and the thought_signature the provider requires echoed back).
        messages.push(Message::Assistant {
            content: if reply.text.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::text(reply.text.clone())]
            },
            tool_calls: reply.tool_calls.clone(),
        });

        // Dispatch each tool_use to its native Rust handler, feed results back.
        for tc in &reply.tool_calls {
            println!(
                "  tool_use: {}({})",
                tc.name,
                serde_json::to_string(&tc.arguments).unwrap_or_default()
            );
            let (result, is_error) = match registry.get(&tc.name) {
                Some(tool) => match tool.call(Value::Object(tc.arguments.clone())).await {
                    Ok(out) => (out, false),
                    Err(e) => (format!("tool error: {}", display_chain(&e)), true),
                },
                None => (format!("no such tool: {}", tc.name), true),
            };
            let preview: String = result.chars().take(80).collect();
            println!(
                "    → dispatched {} → {}{}{}",
                tc.name,
                if is_error { "ERROR " } else { "" },
                preview.replace('\n', "\\n"),
                if result.chars().count() > 80 {
                    "…"
                } else {
                    ""
                },
            );
            messages.push(Message::Tool {
                tool_call_id: tc.id.clone(),
                content: vec![ContentBlock::text(result)],
                is_error,
            });
        }
        println!();
    }

    // 5. Verify the world actually changed the way the task demanded.
    println!("══ Result ════════════════════════════════════════════════");
    println!("total tokens in/out: {total_in}/{total_out}");
    if !finished_cleanly {
        return Err(DemoError::Unfinished);
    }
    let out_path = root.join("out.txt");
    let produced = tokio::fs::read_to_string(&out_path)
        .await
        .map_err(|source| DemoError::NotWritten {
            path: out_path.clone(),
            source,
        })?;
    println!("out.txt contents: {produced:?}");
    let got: usize = produced
        .trim()
        .parse()
        .map_err(|source| DemoError::NotACount {
            text: produced.clone(),
            source,
        })?;
    if got == expected_lines {
        println!("✔ PASS — agent wrote the correct line count ({got}) via the tool loop");
        Ok(())
    } else {
        Err(DemoError::WrongCount {
            got,
            expected: expected_lines,
        })
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {}", display_chain(&e));
            ExitCode::FAILURE
        }
    }
}

/// Resolve where the tars provider config lives, the way tars consumers
/// (arc, scripts) do: `$TARS_CONFIG` → `~/.tars/config.toml` → the XDG
/// default. Returns the first path that exists.
fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TARS_CONFIG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let candidates = [
        dirs::home_dir().map(|h| h.join(".tars").join("config.toml")),
        dirs::config_dir().map(|d| d.join("tars").join("config.toml")),
    ];
    candidates.into_iter().flatten().find(|p| p.is_file())
}

/// Look up a provider's `default_model` from config.
fn provider_id_default_model(
    cfg: &tars_config::Config,
    provider_id: &ProviderId,
) -> Option<String> {
    cfg.providers()
        .providers
        .get(provider_id)
        .map(|p| p.default_model().to_string())
        .filter(|m| !m.is_empty())
}
