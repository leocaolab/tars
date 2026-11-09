//! POC: the PULL / context-request interface (an INTERFACE experiment).
//!
//! This is NOT an intelligence test. Whether the model solves Two Sum
//! correctly or cleverly is a side note. The question is about the
//! CONTEXT-CONTROL INTERFACE: does a pull protocol function, what does it
//! cost, and what is the failure mode.
//!
//! Same harness / tools / World / input as `two_sum`..`two_sum_v3`. The
//! ONE change is who controls context. The earlier examples auto-carry
//! the entire trajectory. Here the harness does the opposite: each turn
//! the model sees ONLY system + task + the results of its *immediately
//! preceding* turn — all older history is DROPPED. The model therefore
//! has no free memory of what it already did.
//!
//! To get anything back, it must PULL: every observation is saved to a
//! memory store, and two tools let the model request it — `recall_keys()`
//! (what is saved) and `recall(key)` (bring one observation back into
//! context). If it doesn't recall what it needs, it simply doesn't have
//! it.
//!
//! What to watch (the point): does pull work end to end (request → harness
//! serves → re-enters context)? And the FAILURE MODE when the model does
//! NOT recall what it needs — does it re-scan from scratch, lose its
//! cursor, loop, or burn MAX_TURNS? That cost — "the agent must know what
//! to ask, or it fails" — is the result.
//!
//! Task: nums = [2, 7, 11, 15], target = 9 (a valid answer is [0, 1]).
//! Run (needs GEMINI_API_KEY — source the repo `.env` first):
//! ```bash
//!   set -a; source .env; set +a
//!   cargo run -p tars-examples --example two_sum_v4
//!   # POC_DUMP=1 shows the per-turn context SHRINKING back to minimal:
//!   POC_DUMP=1 cargo run -p tars-examples --example two_sum_v4
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

/// The pull-interface memory store: every observation the harness saved,
/// as `(key, value)` pairs, that the model can `recall` by key. Shared
/// between the loop (writer) and the recall tools (readers).
type Mem = Arc<Mutex<Vec<(String, String)>>>;

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
            "Insert an integer key→value pair into a hash map. Overwrites the \
             value if the key already exists.",
            obj_schema(
                "hashmap_put_args",
                json!({
                    "key": { "type": "integer" },
                    "value": { "type": "integer" }
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
            "Look up a key in the hash map. Returns the stored integer value, \
             or null if the key is absent.",
            obj_schema(
                "hashmap_get_args",
                json!({ "key": { "type": "integer" } }),
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
            "Return nums[index].",
            obj_schema(
                "array_get_args",
                json!({ "index": { "type": "integer" } }),
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
            "Return the number of elements in nums.",
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
            "Print a line of text to the log.",
            obj_schema(
                "println_args",
                json!({ "text": { "type": "string" } }),
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
            "Report the final answer: two indices i and j.",
            obj_schema(
                "submit_answer_args",
                json!({
                    "i": { "type": "integer" },
                    "j": { "type": "integer" }
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

/// PULL tool — list the keys of everything currently saved in memory,
/// so the model can discover what it can bring back.
struct RecallKeys {
    mem: Mem,
}
#[async_trait]
impl NativeTool for RecallKeys {
    fn name(&self) -> &str {
        "recall_keys"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "recall_keys",
            "List the keys of every observation currently saved in memory. Use \
             it to see what past results you can bring back with recall(key).",
            obj_schema("recall_keys_args", json!({}), &[]),
        )
        .expect("static name non-empty")
    }
    async fn call(&self, _args: Value) -> Result<String, ToolError> {
        let mem = self.mem.lock().unwrap();
        if mem.is_empty() {
            return Ok("(memory is empty)".to_string());
        }
        Ok(mem
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// PULL tool — retrieve one saved observation by key, re-entering it into
/// context. This IS the pull protocol under test.
struct Recall {
    mem: Mem,
}
#[async_trait]
impl NativeTool for Recall {
    fn name(&self) -> &str {
        "recall"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "recall",
            "Retrieve a saved observation from memory by its key (as shown by \
             recall_keys). Returns the stored value, or null if there is no \
             such key. This is how you bring a past result back into view.",
            obj_schema(
                "recall_args",
                json!({ "key": { "type": "string" } }),
                &["key"],
            ),
        )
        .expect("static name non-empty")
    }
    async fn call(&self, args: Value) -> Result<String, ToolError> {
        let key = args
            .get("key")
            .and_then(Value::as_str)
            .ok_or(ToolError::MissingString { key: "key" })?;
        let mem = self.mem.lock().unwrap();
        // Exact key first, then a forgiving substring match (the model
        // often paraphrases the key it saw).
        if let Some((_, v)) = mem.iter().find(|(k, _)| k == key) {
            return Ok(v.clone());
        }
        if let Some((k, v)) = mem.iter().find(|(k, _)| k.contains(key)) {
            return Ok(format!("{v}   (matched key `{k}`)"));
        }
        Ok("null".to_string())
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
    #[error("v4 pull interface: agent hit MAX_TURNS without finishing (see failure mode above)")]
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

    // 2. The fn-access-map over that live state, PLUS the pull tools over
    //    the memory store (the harness auto-saves every observation there).
    let mem: Mem = Arc::new(Mutex::new(Vec::new()));
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
        Arc::new(RecallKeys { mem: mem.clone() }),
        Arc::new(Recall { mem: mem.clone() }),
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

    // 4. The loop. PULL interface: the harness DROPS all but the last
    //    turn, and the model must recall() anything older it needs.
    let system = "You are an automation agent acting ONLY through tools. \
                  IMPORTANT: you have NO memory of earlier turns. Each turn you \
                  see ONLY the results of your MOST RECENT tool calls — \
                  everything before that has been dropped from your context. \
                  However, every observation you make is saved to a memory \
                  store: call recall_keys() to list what is saved, and \
                  recall(key) to bring a specific past observation back into \
                  view. If you need a value you saw in an earlier turn, you \
                  MUST recall it — otherwise it is gone. Report the final \
                  answer with submit_answer; when done, reply with a short \
                  confirmation and call NO more tools.";
    let task = "Solve Two Sum: there is an integer array `nums` and an integer \
                `target`. Find two DISTINCT indices i and j with \
                nums[i] + nums[j] == target. You are not given nums — read it \
                via array_len / array_get. Report the pair via submit_answer.";

    // POC_DUMP=1 dumps the per-turn context — for v4 it visibly SHRINKS
    // back to (task + last-turn results only) every turn.
    let dump = std::env::var("POC_DUMP").is_ok_and(|v| v != "0" && !v.is_empty());

    // `carry` = the ONLY history shown next turn: the immediately-preceding
    // turn's assistant tool_calls + their results. Everything else dropped.
    let mut carry: Vec<Message> = Vec::new();
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut max_parallel = 0usize;
    let mut finished_cleanly = false;
    // Interface instrumentation.
    let mut recall_calls = 0usize; // recall / recall_keys invocations
    let mut array_reads = 0usize; // array_get invocations (re-scan signal)

    for turn in 1..=MAX_TURNS {
        let mut req = ChatRequest::user("");
        req.system = Some(system.to_string());
        // Minimal-context assembly: task + ONLY last turn's messages.
        req.messages = {
            let mut m = vec![Message::user_text(task)];
            m.extend(carry.clone());
            m
        };
        req.tools = registry.specs();
        req.max_output_tokens = Some(512);

        if dump {
            dump_context(turn, &req);
        }

        let ctx = Caller::new(
            tars_types::TenantId::new(EXAMPLE_TENANT),
            tars_types::TraceId::new("two-sum-v4"),
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
            "  LLM: stop={:?}  tokens in/out={}/{}  tool_calls={}  (context msgs sent: {})",
            reply.stop_reason.unwrap_or(StopReason::Other),
            reply.usage.map_or(0, |u| u.input_tokens),
            reply.usage.map_or(0, |u| u.output_tokens),
            reply.tool_calls.len(),
            1 + carry.len(),
        );
        if !reply.text.trim().is_empty() {
            println!("  text: {}", reply.text.trim());
        }

        if reply.tool_calls.is_empty() {
            if world.submitted.lock().unwrap().is_some() {
                println!("  (no tool_use, answer already submitted — agent is done)\n");
                finished_cleanly = true;
                break;
            }
            println!("  (no tool_use but submit_answer NOT called yet — nudging to continue)");
            // The nudge is the ONLY carry next turn (history still dropped).
            carry = vec![Message::user_text(
                "You have not called submit_answer yet. Remember: earlier turns \
                 are dropped from your context — use recall_keys() and \
                 recall(key) to bring back any values you already observed, \
                 then submit_answer.",
            )];
            println!();
            continue;
        }

        // Build this turn's carry FRESH (older history is discarded).
        let mut next_carry: Vec<Message> = vec![Message::Assistant {
            content: if reply.text.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::text(reply.text.clone())]
            },
            tool_calls: reply.tool_calls.clone(),
        }];

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
            next_carry.push(Message::Tool {
                tool_call_id: tc.id.clone(),
                content: vec![ContentBlock::text(result.clone())],
                is_error,
            });

            // Interface bookkeeping + auto-save every real observation to
            // the pull store (recall's own results are not re-saved).
            match tc.name.as_str() {
                "recall" | "recall_keys" => recall_calls += 1,
                "array_get" => {
                    array_reads += 1;
                    let compact = serde_json::to_string(&tc.arguments).unwrap_or_default();
                    mem.lock()
                        .unwrap()
                        .push((format!("array_get{compact}"), result.clone()));
                }
                name if name != "submit_answer" => {
                    let compact = serde_json::to_string(&tc.arguments).unwrap_or_default();
                    mem.lock()
                        .unwrap()
                        .push((format!("{name}{compact}"), result.clone()));
                }
                _ => {}
            }
        }
        carry = next_carry;
        println!();
    }

    // 5. Report the INTERFACE behavior (execution correctness is a side note).
    println!("══ Result (INTERFACE: pull / context-request) ════════════");
    println!("total tokens in/out: {total_in}/{total_out}");
    println!("max tool_calls in a single turn (parallelism): {max_parallel}");
    println!("recall / recall_keys calls (pull uses): {recall_calls}");
    println!("array_get calls (re-scan signal — 4 = one full pass): {array_reads}");
    println!(
        "memory store entries saved by harness: {}",
        mem.lock().unwrap().len()
    );
    println!(
        "final HashMap state: {:?}",
        world.map.lock().unwrap().clone()
    );
    if !finished_cleanly {
        // The failure mode IS a valid, reported result for this interface.
        println!(
            "✗ FAILURE MODE — agent hit MAX_TURNS ({MAX_TURNS}) without a terminal \
             submit_answer: with history dropped it did not pull what it needed."
        );
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

/// Dump the raw context the model sees on this turn: the system prompt,
/// the (purely factual) tool specs, and the running message list. Proof
/// that nothing advertised to the model hints at the hashmap strategy.
fn dump_context(turn: usize, req: &ChatRequest) {
    println!("┌─ POC_DUMP: context for turn {turn} ─────────────────────────");
    if turn == 1 {
        if let Some(sys) = &req.system {
            println!("│ SYSTEM: {sys}");
        }
        println!("│ TOOLS advertised to the model (name — description):");
        for t in &req.tools {
            println!("│   • {} — {}", t.name, t.description);
        }
    }
    println!("│ MESSAGES ({} in context):", req.messages.len());
    for (i, m) in req.messages.iter().enumerate() {
        let (role, body) = match m {
            Message::User { .. } => ("user", one_line(m)),
            Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                let calls: Vec<String> = tool_calls
                    .iter()
                    .map(|c| format!("{}({})", c.name, Value::Object(c.arguments.clone())))
                    .collect();
                ("assistant", format!("tool_calls: {}", calls.join(", ")))
            }
            Message::Assistant { .. } => ("assistant", one_line(m)),
            Message::Tool { tool_call_id, .. } => {
                ("tool", format!("[{tool_call_id}] {}", one_line(m)))
            }
            Message::System { .. } => ("system", one_line(m)),
        };
        println!("│   [{i}] {role}: {body}");
    }
    println!("└──────────────────────────────────────────────────────────────");
}

/// First text block of a message, trimmed to one short line.
fn one_line(m: &Message) -> String {
    let s = m
        .content()
        .iter()
        .find_map(ContentBlock::as_text)
        .unwrap_or("")
        .replace('\n', " ");
    if s.chars().count() > 100 {
        format!("{}…", s.chars().take(100).collect::<String>())
    } else {
        s
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
