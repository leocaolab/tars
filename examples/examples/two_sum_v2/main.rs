//! POC: loop-react agent inventing an algorithm over in-process objects.
//!
//! IDENTICAL harness / tools / World to `two_sum` — the ONLY difference
//! is the task prompt, which here gives the PROBLEM, the input-access
//! rule, and the available data structures, but **NOT the algorithm**.
//! `two_sum` spells out the one-pass hashmap trick; this one does not.
//! So this example tests whether the model *derives* an approach
//! (reasoning) versus merely *executing* a given recipe.
//!
//! Task: nums = [2, 7, 11, 15], target = 9 (a valid answer is indices
//! [0, 1]). The array is not in the prompt — readable only via
//! array_len / array_get. The model picks its own strategy; the harness
//! only verifies the submitted pair.
//!
//! Run (needs GEMINI_API_KEY — source the repo `.env` first):
//! ```bash
//!   set -a; source .env; set +a
//!   cargo run -p tars-examples --example two_sum_v2
//!   TARS_POC_PROVIDER=openai cargo run -p tars-examples --example two_sum
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};

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
// The "app as world": live in-process software state the agent drives.
// ─────────────────────────────────────────────────────────────────────

struct World {
    /// Read-only input array. NOT in the prompt — the agent reads it via
    /// `array_get` / `array_len`.
    nums: Vec<i64>,
    target: i64,
    /// The live HashMap capability under test (value → index).
    map: Mutex<HashMap<i64, i64>>,
    /// Where the agent reports its terminal answer.
    submitted: Mutex<Option<(usize, usize)>>,
}

// ─────────────────────────────────────────────────────────────────────
// The fn-access-map: native Rust tools over that state.
// ─────────────────────────────────────────────────────────────────────

#[async_trait]
trait NativeTool: Send + Sync {
    fn name(&self) -> &str;
    fn spec(&self) -> ToolSpec;
    async fn call(&self, args: Value) -> Result<String, ToolError>;
}

/// Small helper to build an object schema.
fn obj_schema(name: &str, props: Value, required: &[&str]) -> JsonSchema {
    JsonSchema::strict(
        name,
        tars_types::json_object! {
            "type": "object",
            "properties": props,
            "required": (required),
            "additionalProperties": false
        },
    )
}

/// Why a tool call did not run; its words go back to the model as the tool's error result.
#[derive(Debug, thiserror::Error)]
enum ToolError {
    #[error("missing integer arg `{key}`")]
    MissingInteger { key: &'static str },
    #[error("missing string arg `{key}`")]
    MissingString { key: &'static str },
    #[error("arg `{key}` = {value} is not a valid index")]
    NotAnIndex {
        key: &'static str,
        value: i64,
        #[source]
        source: std::num::TryFromIntError,
    },
    #[error("index {index} out of bounds (len {len})")]
    OutOfBounds { index: usize, len: usize },
}

fn arg_i64(args: &Value, key: &'static str) -> Result<i64, ToolError> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or(ToolError::MissingInteger { key })
}

fn arg_usize(args: &Value, key: &'static str) -> Result<usize, ToolError> {
    let value = arg_i64(args, key)?;
    usize::try_from(value).map_err(|source| ToolError::NotAnIndex { key, value, source })
}

struct HashMapPut {
    world: Arc<World>,
}
#[async_trait]
impl NativeTool for HashMapPut {
    fn name(&self) -> &str {
        "hashmap_put"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "hashmap_put",
            "Insert an entry into the live in-process HashMap<i64,i64>: \
             maps `key` → `value`, overwriting any existing key. Use this to \
             remember, as you scan, that a number was seen at a given index \
             (store value→index).",
            obj_schema(
                "hashmap_put_args",
                json!({
                    "key": { "type": "integer", "description": "the map key" },
                    "value": { "type": "integer", "description": "the map value" }
                }),
                &["key", "value"],
            ),
        )
        .expect("static name non-empty")
    }
    async fn call(&self, args: Value) -> Result<String, ToolError> {
        let key = arg_i64(&args, "key")?;
        let value = arg_i64(&args, "value")?;
        self.world.map.lock().unwrap().insert(key, value);
        Ok(format!("ok: map[{key}] = {value}"))
    }
}

struct HashMapGet {
    world: Arc<World>,
}
#[async_trait]
impl NativeTool for HashMapGet {
    fn name(&self) -> &str {
        "hashmap_get"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "hashmap_get",
            "Look up `key` in the live HashMap<i64,i64>. Returns the stored \
             value, or null if the key is absent.",
            obj_schema(
                "hashmap_get_args",
                json!({ "key": { "type": "integer", "description": "the map key to look up" } }),
                &["key"],
            ),
        )
        .expect("static name non-empty")
    }
    async fn call(&self, args: Value) -> Result<String, ToolError> {
        let key = arg_i64(&args, "key")?;
        Ok(match self.world.map.lock().unwrap().get(&key) {
            Some(v) => format!("{v}"),
            None => "null".to_string(),
        })
    }
}

struct ArrayGet {
    world: Arc<World>,
}
#[async_trait]
impl NativeTool for ArrayGet {
    fn name(&self) -> &str {
        "array_get"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "array_get",
            "Read nums[index] from the input array. This is the ONLY way to \
             see the array's values — they are not given to you otherwise.",
            obj_schema(
                "array_get_args",
                json!({ "index": { "type": "integer", "description": "0-based index into nums" } }),
                &["index"],
            ),
        )
        .expect("static name non-empty")
    }
    async fn call(&self, args: Value) -> Result<String, ToolError> {
        let idx = arg_usize(&args, "index")?;
        let n = self.world.nums.len();
        let v = self
            .world
            .nums
            .get(idx)
            .ok_or(ToolError::OutOfBounds { index: idx, len: n })?;
        Ok(format!("{v}"))
    }
}

struct ArrayLen {
    world: Arc<World>,
}
#[async_trait]
impl NativeTool for ArrayLen {
    fn name(&self) -> &str {
        "array_len"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "array_len",
            "Return the number of elements in the input array `nums`.",
            obj_schema("array_len_args", json!({}), &[]),
        )
        .expect("static name non-empty")
    }
    async fn call(&self, _args: Value) -> Result<String, ToolError> {
        Ok(format!("{}", self.world.nums.len()))
    }
}

/// Output capability — the agent "thinks out loud" to real stdout.
struct Println;
#[async_trait]
impl NativeTool for Println {
    fn name(&self) -> &str {
        "println"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "println",
            "Print a line of text to the program's stdout. Use it to narrate \
             your reasoning or note intermediate findings.",
            obj_schema(
                "println_args",
                json!({ "text": { "type": "string", "description": "text to print" } }),
                &["text"],
            ),
        )
        .expect("static name non-empty")
    }
    async fn call(&self, args: Value) -> Result<String, ToolError> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or(ToolError::MissingString { key: "text" })?;
        println!("      [agent stdout] {text}");
        Ok("printed".to_string())
    }
}

/// Terminal act — record the answer into the harness.
struct SubmitAnswer {
    world: Arc<World>,
}
#[async_trait]
impl NativeTool for SubmitAnswer {
    fn name(&self) -> &str {
        "submit_answer"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "submit_answer",
            "Report the final answer: the two distinct indices i and j whose \
             values sum to the target. Call this exactly once when you have the \
             solution.",
            obj_schema(
                "submit_answer_args",
                json!({
                    "i": { "type": "integer", "description": "first index" },
                    "j": { "type": "integer", "description": "second index" }
                }),
                &["i", "j"],
            ),
        )
        .expect("static name non-empty")
    }
    async fn call(&self, args: Value) -> Result<String, ToolError> {
        let i = arg_usize(&args, "i")?;
        let j = arg_usize(&args, "j")?;
        *self.world.submitted.lock().unwrap() = Some((i, j));
        Ok(format!("recorded answer: indices [{i}, {j}]"))
    }
}

/// Registry: name → tool.
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
// Stream draining — identical shape to loop_react: correlate the
// two-event tool call by index, carry thought_signature through.
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
    #[error("agent never called submit_answer")]
    NoAnswer,
    #[error("FAIL — submitted index out of bounds (len {len}): [{i}, {j}]")]
    OutOfBounds { len: usize, i: usize, j: usize },
    #[error("FAIL — indices must be distinct, got [{i}, {j}]")]
    SameIndex { i: usize, j: usize },
    #[error("FAIL — nums[{i}] + nums[{j}] = {sum} != target {target}")]
    WrongSum {
        i: usize,
        j: usize,
        sum: i64,
        target: i64,
    },
}

const MAX_TURNS: usize = 24;

async fn run() -> Result<(), DemoError> {
    // 1. The in-process world. nums is DELIBERATELY not in the prompt.
    let world = Arc::new(World {
        nums: vec![2, 7, 11, 15],
        target: 9,
        map: Mutex::new(HashMap::new()),
        submitted: Mutex::new(None),
    });
    println!(
        "harness world: nums=<hidden, {} elems, tool-only>  target={}\n",
        world.nums.len(),
        world.target
    );

    // 2. The fn-access-map over that live state.
    let registry = ToolRegistry::new(vec![
        Arc::new(HashMapPut {
            world: world.clone(),
        }),
        Arc::new(HashMapGet {
            world: world.clone(),
        }),
        Arc::new(ArrayGet {
            world: world.clone(),
        }),
        Arc::new(ArrayLen {
            world: world.clone(),
        }),
        Arc::new(Println),
        Arc::new(SubmitAnswer {
            world: world.clone(),
        }),
    ]);

    // 3. Real provider from real config.
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
    let model = cfg
        .providers()
        .providers
        .get(&provider_id)
        .map(|p| p.default_model().to_string())
        .filter(|m| !m.is_empty())
        .ok_or(DemoError::NoDefaultModel)?;
    println!("provider: {provider_id}   model: {model}\n");

    let pipeline = LlmService::builder_with_inner(LlmService::of(provider, model))
        .layer(TelemetryMiddleware::new())
        .layer(RetryMiddleware::default())
        .build();

    // 4. The loop.
    let system = "You are an automation agent that can ONLY act through the \
                  provided tools — you have no other access to the program's \
                  state. Work step by step, one or more tool calls per turn. \
                  When you have submitted the final answer and there is nothing \
                  left to do, reply with a short plain-text confirmation and \
                  call NO more tools.";
    // NOTE: unlike `two_sum`, this prompt states the PROBLEM + the
    // input-access rule + the available tools, but gives NO algorithm.
    // The model must decide the strategy itself.
    let task = "Solve Two Sum. There is an integer array `nums` and an integer \
                `target`. Find two DISTINCT indices i and j such that \
                nums[i] + nums[j] == target. You are NOT told the array's \
                contents — read them via array_len and array_get(index). \
                Available tools: a live integer→integer HashMap (hashmap_put / \
                hashmap_get), println for notes, and submit_answer(i, j) to \
                report the result. Solve it however you see fit and report the \
                pair of indices via submit_answer.";

    let mut messages: Vec<Message> = vec![Message::user_text(task)];
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut max_parallel = 0usize;
    let mut finished_cleanly = false;

    for turn in 1..=MAX_TURNS {
        let mut req = ChatRequest::user("");
        req.system = Some(system.to_string());
        req.messages = messages.clone();
        req.tools = registry.specs();
        req.max_output_tokens = Some(512);

        let ctx = Caller::new(
            tars_types::TenantId::new(EXAMPLE_TENANT),
            tars_types::TraceId::new("two-sum-v2"),
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
        max_parallel = max_parallel.max(reply.tool_calls.len());

        println!("── Turn {turn} ───────────────────────────────────────────");
        println!(
            "  LLM: stop={:?}  model={}  tokens in/out={}/{}  tool_calls={}",
            reply.stop_reason.unwrap_or(StopReason::Other),
            reply.actual_model.as_deref().unwrap_or("?"),
            reply.usage.map_or(0, |u| u.input_tokens),
            reply.usage.map_or(0, |u| u.output_tokens),
            reply.tool_calls.len(),
        );
        if !reply.text.trim().is_empty() {
            println!("  text: {}", reply.text.trim());
        }

        if reply.tool_calls.is_empty() {
            // "No tool_use" alone is NOT a reliable done signal — the task
            // is done iff the terminal act (submit_answer) actually fired.
            // gemini_flash here sometimes ends a turn empty (0 tokens, no
            // text) mid-solve; treating that as "done" would drop the
            // answer. So: done only if submitted; otherwise nudge & loop.
            if world.submitted.lock().unwrap().is_some() {
                println!("  (no tool_use, answer already submitted — agent is done)\n");
                finished_cleanly = true;
                break;
            }
            println!("  (no tool_use but submit_answer NOT called yet — nudging to continue)");
            if !reply.text.trim().is_empty() {
                messages.push(Message::assistant_text(reply.text.clone()));
            }
            messages.push(Message::user_text(
                "You have not called submit_answer yet, so the task is not \
                 finished. Continue the one-pass scan from where you left off \
                 and submit the pair of indices via submit_answer.",
            ));
            println!();
            continue;
        }

        messages.push(Message::Assistant {
            content: if reply.text.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::text(reply.text.clone())]
            },
            tool_calls: reply.tool_calls.clone(),
        });

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
            println!(
                "    → {}{} => {}",
                if is_error { "ERROR " } else { "" },
                tc.name,
                result.replace('\n', "\\n"),
            );
            messages.push(Message::Tool {
                tool_call_id: tc.id.clone(),
                content: vec![ContentBlock::text(result)],
                is_error,
            });
        }
        println!();
    }

    // 5. Verify the agent solved it through the tools.
    println!("══ Result ════════════════════════════════════════════════");
    println!("total tokens in/out: {total_in}/{total_out}");
    println!("max tool_calls in a single turn (parallelism): {max_parallel}");
    println!(
        "final HashMap state: {:?}",
        world.map.lock().unwrap().clone()
    );
    if !finished_cleanly {
        return Err(DemoError::Unfinished);
    }
    let (i, j) = world.submitted.lock().unwrap().ok_or(DemoError::NoAnswer)?;
    println!("submitted answer: indices [{i}, {j}]");

    let n = world.nums.len();
    if i >= n || j >= n {
        return Err(DemoError::OutOfBounds { len: n, i, j });
    }
    if i == j {
        return Err(DemoError::SameIndex { i, j });
    }
    let sum = world.nums[i] + world.nums[j];
    if sum == world.target {
        println!(
            "✔ PASS — nums[{i}] + nums[{j}] = {} + {} = {} == target {} (valid Two Sum pair)",
            world.nums[i], world.nums[j], sum, world.target,
        );
        Ok(())
    } else {
        Err(DemoError::WrongSum {
            i,
            j,
            sum,
            target: world.target,
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

/// Resolve the tars config path like tars consumers do: `$TARS_CONFIG`
/// → `~/.tars/config.toml` → XDG default. First existing wins.
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
