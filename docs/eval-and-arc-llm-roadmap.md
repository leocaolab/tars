---
status: notes from 2026-05-20 discussion
audience: future self / next session
---

# Eval framework + arc_llm collapse — roadmap

捕获 2026-05-20 那次讨论的结论。两条线，强相关：
**arc 的实证经验** 推翻了 Doc 16 现在的形状，
**arc_llm 该死** 又依赖 tars 暴露三个便利 API。

---

## 0. 已决定**不做**的事

| 想法 | 决定 | 触发条件 |
|---|---|---|
| Token profiler（cProfile-style 按 segment 切 input token） | 不做 | 等 tars 自己或第一个外部 user 真的搭起 coordinator + 多 subagent workflow，开始抱怨 token 账单/latency 时再回来 |
| Doc 16 §7.1 那 6 个内置 deterministic evaluator（`SchemaComplianceEvaluator` 等） | 不做 | 形状错了——arc 一年实证证明 production eval 不是 per-call 流式打分。除非有 user 明确要 |
| MCP server 让 PM 自然语言查 events.db | 不做 | 没用户在痛 |
| Streaming critic output（arc TODO 里列着） | 删 | 结构化 JSON 输出本来就不该 stream |

---

## 1. Eval 框架重新设计（替代 Doc 16 §7.1）

**核心洞察**：arc 一年下来真正用上的是
**per-run 聚合 + 离线 LLM-as-judge phase + corpus replay**——
不是 Doc 16 假设的 "per-call deterministic dimension score → time-series"。

### v1 要做的三件事（按 ROI 排）

#### 1.1 `RunReport` — 把 trajectory 聚合成 arc 的 `RunBenchmark`

```
tars run-report <trajectory_id> [--json]
```

从 `LlmCallFinished` + trajectory events 聚出：`wall_clock_sec`、
`llm_calls`、`cache_hits`、`tokens_{in,out,cached}`、`retry_count`、
`per_tool_calls`、`validation_summary`、`per_role_breakdown`（按 tags）。

80% 数据已经在事件里，只缺聚合层。**ROI 最高，先做这个。**

参考实现：arc `.arc/benchmarks/*_scan.json`（见
`arc/crates/arc_shell/src/benchmark.rs`）。

#### 1.2 Offline `Judge` framework — 给 arc `--verify` 一个一等公民

```rust
trait Judge {
    async fn judge(&self, item: &JudgeItem) -> JudgeVerdict;
}
enum JudgeVerdict { TP, FP, Unsure { reason: String } }
```

跑法：`tars eval judge --run <run_id> --judge anthropic:claude-opus-4-7`

- 遍历 run 里的 outputs，每条 item 问一次 judge
- verdict 写进 EventStore 一个新事件 `ItemJudged`
- **强制 anti-incest**：judge.provider ≠ critic.provider（arc 学到的 lesson）
- 注意是 **per-item** 不是 per-call（一次 call 可能产生 N 条待判 finding）

参考实现：arc `scan --verify` 模式——直接拿它的 prompt + 输出 schema
作为 trait 设计样本。arc 当前是 subprocess 调 claude-cli，迁过来后
直接走 tars pipeline（自带 cache/retry/telemetry）。

#### 1.3 Corpus replay — `tars eval run --corpus <dir> --pipeline <cfg>`

固定输入集 + 固定 pipeline config → 跑出一份 trajectory + RunReport 落盘。
让 "prompt 从 A 改到 B 是好是坏" 变成可重复的数字。

参考：arc `docs/review-eval-harness.md` §3 的 `arc-bench/` 设计
（arc 自己也没建，但形状是对的）。

### 明确**不在 v1**

- Pareto 6 维（Recall/Precision/Severity/Actionability/Stability/Scope）——arc 域，
  tars 不该假设所有 agent 都关心这些。留给应用层组装。
- Seeded corpus 构造工具——corpus 是 input 不是 framework。
- Dashboard / UI——tars 给数据，UI 是 consumer 的事。

---

## 2. arc_llm crate 应该消失

**论点**：`arc_llm` 89% 的代码在做"tars 应该自带但没暴露的便利层"。
上推之后 crate 整个删除。

### tars-side 改动（解锁删除）

#### 2.1 `Pipeline::default_chain(provider, PipelineOpts)`

Rust API 对齐 Python `Pipeline.from_default()`。
`PipelineOpts { events_dir, validators, cache_origin_extra, retry }`
一次性吃掉 `arc_llm::build_pipeline_with_validators` + `open_event_stores`
+ cache origin namespace 三个需求。

*吞掉 arc：`llm.rs:88-215`，~130 行*

#### 2.2 `ValidationMiddleware::new` 收 `Vec<Arc<dyn OutputValidator>>`

不再要求 Box。`arc_wrapper_box` / `ArcValidator` adapter（`llm.rs:217-235`）
完全消失。**这是 tars API 设计缺陷的修复，arc 只是第一个 beneficiary。**

#### 2.3 `tars_runtime::shared_runtime()` + `LlmService::complete_sync(req, ctx)`

进程级 `LazyLock<Runtime>` + sync stream drain + `ValidationOutcome` 侧 channel
拼回 response——这些都是每个 sync 调用方现在自己造的轮子。

*吞掉 arc：`TOKIO` static、`run_request`、`LlmClientInner::run`，~80 行*

#### 2.4 （可选 polish）

- `impl Display for CompatibilityCheck` → 删 `format_preflight_failure`
- `ChatRequest::single_turn` / `::from_messages` 构造器 → 删 `build_*_turn`
- `CacheKeyFactory` 提供 origin namespace 钩子（含 rubric version / config hash）

### arc-side 改动

#### 2.5 删 `LlmClient` + `ArcSession`

`arc_review` critic/verifier 改用 `Arc<dyn LlmService>` + `tars_runtime::Session` 直接。
TokenCounters Mutex 一并删——要 token 数从 `pipeline_events.db` 按 tag 查。

*删：`llm_client.rs` 整个，~900 行*

**前置依赖（已解除 2026-05-20）**：depyo3 Wave 28-30 已完成，Python 代码现在只是参考。`LlmClient` 不再是 PyO3 ABI 入口，可以直接删。

#### 2.6 搬家（不改逻辑）

- `llm_validators.rs`（3 个 validator + `validators_for_role`）→ `arc_review/src/validators.rs`
- `role_requirements_core` → `arc_review`
- `llm_config.rs`（`.arc/config.toml` 解析、roles map）→ `arc_core::config` 或新 `arc_config`

*搬：~1180 行，纯 mv*

#### 2.7 删 `arc_llm` crate + 更新 workspace `Cargo.toml`

### 净账

- 上推到 tars 通用 API：~250 行 arc 代码（其他 tars user 也受益）
- arc 内部搬家：~1180 行
- arc 净减少：~250 行 + 一层 crate boundary
- `LlmClient` 这个 legacy `TarsClient` 兼容鬼魂消失

---

## 3. arc_llm 中等优先级修复（不依赖 crate 删除）

不上推也能立刻做的 hygiene，按 ROI 排：

| # | 改动 | ROI 理由 |
|---|---|---|
| 1 | `RequestContext::test_default()` 用在 production 路径上 → 暴露 tags / run_id / role 给调用方 | 解锁整个 eval 切片能力；目前 `tars events list --tag` 没法筛 arc 的事件 |
| 2 | Cache origin 包含 rubric version hash | **correctness bug**——rubric 改了但 prompt 文本没变会错命中 cache |
| 3 | `ArcSession` 改用 `tars_runtime::Budget::Tokens` + Tokenizer | char-based 400k trim 对 CJK / 代码不准 |
| 4 | 删 in-memory TokenCounters Mutex，走 events DB | 双 source of truth 必然漂移 |

#1 和 #2 可以现在做；#3 等 2.5 的 `LlmClient` 删除时一起；#4 等 2.5。

---

## 4. **不变**的设计决策

巩固今天的讨论结论，避免下次又回来翻：

- **`OutputValidator` trait 不动**——纯函数 / 同步 / cache-safe 这些约束设计正确，
  arc 3 个 validator 都在用，B-20 W1→W4 一路打磨到这个形状。
- **不要给 `OutputValidator` 加 async 版本**——会炸 Cache×Validator 假设。
- **"LLM-validator"** 这个词分两半看：
  - 用 LLM 判 response 形状对不对再决定 reject → **不做**，是 anti-pattern，
    走 `response_schema` 在 Provider 层解决。
  - 用 LLM 评 response 质量打分 → **走 Judge framework**（§1.2），offline phase，
    不入 hot path。
- **Doc 15（validation）与 Doc 16（eval）的硬边界保留**——validation 改 response，
  eval 不改 response；hot path / offline phase；deterministic / non-deterministic。
  arc 实证支持这条边界。

---

## 5. 执行顺序建议

1. **现在可以做**：arc-side 修复 #1（tags），#2（cache origin namespace）——
   不依赖 tars 改动，立刻见效。
2. **下一步 tars 改动**：§2.1 `Pipeline::default_chain` + §2.2 `Arc<OutputValidator>` 收口。
   这俩对其他 tars user 也有正面价值，作为 v0.3 的 Pipeline API 改进打包。
3. **并行 arc 搬家**：§2.6 文件 mv，纯机械，不动逻辑。
4. **eval 框架先做 §1.1 `RunReport`**——单独可发布，立刻解锁 arc 的 dashboard。
5. **§1.2 Judge framework** 紧跟着——arc `--verify` 现成的 reference impl。
6. **`LlmClient` 删除**——depyo3 已完成（2026-05-20），不再有前置依赖，跟搬家可以并行。
7. **`arc_llm` crate 整个删**是最后一步。
