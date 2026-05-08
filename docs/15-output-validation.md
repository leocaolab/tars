# 文档 15 — Output Validation（输出校验中间件）

> 范围：在 LLM 返回 Response 之后、还给 caller 之前，按一组可注册的 `OutputValidator` 检查 / 改写 / 拒绝输出，把"校验输出契约"这件事从每个 consumer各自实现 → 抽到 tars-pipeline 内置中间件。
>
> 上游：Doc 02 Middleware Pipeline（本文档新增的 ValidationMiddleware 是其中一层）；Doc 01 LLM Provider（`Response` / `ChatRequest` 类型）。
>
> 下游：所有需要"输出符合契约"的 consumer。第一个 user 是 downstream consumer 的 critic agent，把目前内联在 `app/core/critic_agent.py` 的 `_known_rule_ids` 白名单逻辑迁过来。
>
> **明确不做的事**：本文档讨论的是**同步、单 call、影响 Response 的契约校验**。多维度评分 / 时间序列 / 数据集对比 / 离线 release gate 是**评估**而不是**校验**——见 Doc 16。两条概念不能混在同一个 trait 里。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **共享单一实现** | rule_id 白名单 / JSON shape 校验 / 长度检查这种"输出契约"代码不该每个 consumer 各写一遍 |
| **明确 Pass / Filter / Reject / Annotate 四种处置** | 每个 outcome 语义清晰，validator 写出来不用想"我这个该 transform 还是该抛错"|
| **复用现有 retry 路径** | Reject 走 `ProviderError::ValidationFailed`，由现有 `RetryMiddleware` 决定是否重试；不引入第二条重试机制 |
| **Plugin 式注册** | Caller `Pipeline::builder().layer(ValidationMiddleware::new(vec![...]))` 一行加；built-in 几条覆盖 80% 用例，custom 通过 trait 实现 |
| **跨语言** | Python 通过 `tars.OutputValidator` base class 实现；未来 Node 同形（同 Stage 3 PyTool 模式）|
| **Order 显式** | `add_validator` 注册顺序 = 执行顺序，左到右；Filter 链式（每个看到的是上一个 Filter 后的 Response）|
| **流式不阻塞** | Validator 永远拿到完整 Response 工作（drain 流后调），但 token-by-token streaming UX 在 caller 那一侧仍可保持——validator 在 stream 结束时同步执行一遍即可 |

**反目标**：

- **不替代 schema 约束**：`response_schema` kwarg（Doc 12）仍是首选——它在 provider 解码时就生效。validation 是 schema 不顶用 / 不可用时的兜底。两者并存，不互斥。
- **不做评估**：validator 只产生 outcome，不产生 dimension score；不写时间序列；不做离线 dataset 对比。这些归 Doc 16。
- **不在 validator 里跑昂贵 LLM-as-judge**：每 call 阻塞跑、影响延迟。如果你真要 LLM-judge，做成 Doc 16 的 async evaluator，结果通过 EventStore 反馈，不阻塞响应。

---

## 2. 架构总览

ValidationMiddleware 在 [Doc 02 §2 洋葱图](./02-middleware-pipeline.md)中的位置：**Retry 内、Provider 外**。

```
   ... → Retry → ValidationMiddleware → Provider
                       │
                       │ (drain stream into Response)
                       ▼
                  ┌─────────────────────────────────┐
                  │  validators[0].validate(resp)   │
                  │  ↓ Pass | Filter | Reject | Annotate
                  │  validators[1].validate(resp')  │
                  │  ↓ ...                          │
                  └─────────────────────────────────┘
                       │
                       │ Pass        → 流回原样
                       │ Filter      → resp 替换为 transformed,流回 transformed
                       │ Reject      → Err(ProviderError::ValidationFailed{retriable})
                       │              ↑ Retry 看到这条决定是否重试
                       │ Annotate    → 流回原样,但写指标到 RequestContext.attributes
                       ▼
                  caller 拿到的 Response
```

**为什么是 Retry 内、Provider 外**：

- **Retry 外**（Provider 内）会让 validation 失败时无法重试——validator 拒绝了我们想再调一次模型，但 Pipeline 已经返回，太晚
- **Retry 内**（Provider 内层），ValidationMiddleware 抛 `ValidationFailed { retriable: true }`，外层 RetryMiddleware 看到 `ErrorClass::Retriable` 自然重试，**复用现有重试基础设施零额外耦合**
- **Provider 内**（更里）会让 ValidationMiddleware 看不到 Response，因为 Provider 直接产 stream

**为什么必须 drain 整个流**：

Validator 大多数有意义的检查（rule_id 白名单、JSON shape、findings 数量、tag 完整性）需要完整 Response。Token-stream 中段不可能判断。**Validator 是 post-stream 概念。**

代价：Pipeline 给 caller 的 stream 体感仍是流式（caller 写 `for chunk in stream: print(chunk)` 没变），但每个 chunk 在 ValidationMiddleware 内部是先 drain 完整再重新 emit 出去的。caller 看到的"流"是回放，不是真正的 token-by-token——延迟相当于"完整生成 + 单次 emit"。对**非交互式 review / classification / 后台 batch** 场景影响为零；对**用户面对面打字的 chatbot UX** 有影响（首 token 延迟到完整 generate 完成）。后者不该开 ValidationMiddleware，或开但只跑非阻塞 Annotate validators。

---

## 3. 核心类型

### 3.1 `OutputValidator` trait

```rust
// 位置: tars-pipeline::validation
pub trait OutputValidator: Send + Sync {
    /// Stable name used for telemetry, logs, and ordering hints.
    fn name(&self) -> &str;

    /// Run the validator against a (req, resp) pair. The request is
    /// included so validators that need original prompt context
    /// (e.g. SnippetGroundingValidator wants the source file) can use
    /// it; most validators ignore it.
    fn validate(&self, req: &ChatRequest, resp: &Response) -> ValidationOutcome;
}

pub enum ValidationOutcome {
    /// Response unchanged, no metrics recorded.
    Pass,

    /// Response transformed in-place. The new Response is what
    /// downstream sees. `dropped` is a free-form list of
    /// "what got removed/changed" — used for telemetry, not for
    /// caller-side decisions.
    Filter {
        response: Response,
        dropped: Vec<String>,
    },

    /// Validator considers the response unacceptable. Surfaces as
    /// `ProviderError::ValidationFailed`; existing RetryMiddleware
    /// decides whether to retry based on the `retriable` flag.
    Reject {
        reason: String,
        retriable: bool,
    },

    /// Response unchanged. Validator wants to record per-call metrics
    /// (e.g. "this finding count was unusually low") that downstream
    /// code can read from `RequestContext.attributes` or from
    /// `Response.validation_summary` (see §4.2).
    Annotate {
        metrics: HashMap<String, serde_json::Value>,
    },
}
```

**关于 `validate` 的契约：**

- 必须是纯函数，输入相同 → 输出相同。**禁止依赖外部状态**（除非该状态在 validator 构造时已经捕获，例如 `RuleIdWhitelistValidator(allowed: HashSet<String>)`）
- 必须 panic-safe；validator panic 当作 Reject 处理，error_message 是 panic 原因，不阻塞其他 validators 执行
- 不允许 await async work；validator 是同步函数。需要异步 IO 的（fetch 远程数据 / RPC）请放到 evaluator（Doc 16），那里 async 是天然的

### 3.2 `Filter` 字段说明

为什么 Filter 必须返回完整 `Response` 而不是只 `transformed_text: String`：

`Response` 不只是 text。它包含 `text`、`thinking`、`tool_calls`、`usage`、`stop_reason`、`telemetry`。validator 可能想：

- 修改 `tool_calls`（"这个 tool 是 hallucinated 的，删掉"）
- 调整 `usage`（filter 后 token 计数应该相应减少）
- 重写 `stop_reason`（filter 大量内容后"end_turn"可能不再准确）
- 重写 `thinking`（如果 thinking 通道也有内容要过滤）

让 caller 返回一个完整、自洽的 Response 是最干净的——validator 自己保证内部一致性。`dropped: Vec<String>` 是 audit trail，给 telemetry / debug 看用，不是 control flow。

### 3.3 `ProviderError::ValidationFailed`

```rust
// 位置: tars-types::error
pub enum ProviderError {
    ...existing variants...

    /// An OutputValidator rejected the response. This is the bridge
    /// between the validation layer and the existing retry/error
    /// handling — surfaces through normal error class machinery.
    #[error("validation failed: {validator}: {reason}")]
    ValidationFailed {
        validator: String,
        reason: String,
        retriable: bool,
    },
}

impl ProviderError {
    pub fn class(&self) -> ErrorClass {
        match self {
            ...existing arms...
            ValidationFailed { retriable: true, .. } => ErrorClass::Retriable,
            ValidationFailed { retriable: false, .. } => ErrorClass::Permanent,
        }
    }
}
```

**Python 暴露（tars-py::errors）**：
- `kind = "validation_failed"`
- `validator: str` — 触发的 validator 名
- `reason: str` — 拒绝原因（同 message）
- `is_retriable: bool` — 已有字段，自动填好

downstream consumer 那边 `except tars.TarsProviderError as e: if e.kind == "validation_failed": ...` 一行处理。

---

## 4. ValidationMiddleware

### 4.1 实现骨架

```rust
// 位置: tars-pipeline::validation
pub struct ValidationMiddleware {
    validators: Vec<Box<dyn OutputValidator>>,
}

impl ValidationMiddleware {
    pub fn new(validators: Vec<Box<dyn OutputValidator>>) -> Self {
        Self { validators }
    }
}

impl LlmService for ValidationMiddleware {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // Telemetry: this layer was traversed.
        if let Ok(mut t) = ctx.telemetry.lock() {
            t.layers.push("validation".into());
        }

        // Drain inner stream into a complete Response. We can't
        // validate token-by-token; validators need the whole response.
        let inner_stream = self.inner.clone().call(req.clone(), ctx.clone()).await?;
        let mut builder = ChatResponseBuilder::new();
        let mut events_held = Vec::new();
        let mut s = inner_stream;
        while let Some(ev) = s.next().await {
            let ev = ev?;
            events_held.push(ev.clone());
            builder.apply(ev);
        }
        let mut response = builder.finish();

        // Run validators in order. Filter chains; first Reject short-circuits.
        let mut summary = ValidationSummary::default();
        for v in &self.validators {
            let outcome = v.validate(&req, &response);
            match outcome {
                ValidationOutcome::Pass => {
                    summary.outcomes.insert(v.name().into(), OutcomeSummary::Pass);
                }
                ValidationOutcome::Filter { response: new_resp, dropped } => {
                    summary.outcomes.insert(v.name().into(),
                        OutcomeSummary::Filter { dropped: dropped.clone() });
                    response = new_resp;
                    // Subsequent validators see the filtered response.
                }
                ValidationOutcome::Reject { reason, retriable } => {
                    return Err(ProviderError::ValidationFailed {
                        validator: v.name().to_string(),
                        reason,
                        retriable,
                    });
                }
                ValidationOutcome::Annotate { metrics } => {
                    summary.outcomes.insert(v.name().into(),
                        OutcomeSummary::Annotate { metrics });
                }
            }
        }

        // Stash the summary on Response (see §4.2). Re-emit the
        // (potentially Filtered) response as a stream so downstream
        // consumers see a stream-shaped flow.
        attach_validation_summary(&mut response, summary);
        let final_stream = response_to_stream(response);
        Ok(Box::pin(final_stream))
    }
}
```

### 4.2 `Response.validation_summary`

新字段，每次 call 后填好：

```rust
pub struct Response {
    ...existing fields (text, thinking, usage, stop_reason, telemetry)...

    /// Per-call validation outcomes, populated by ValidationMiddleware.
    /// Empty when no ValidationMiddleware was in the pipeline.
    pub validation_summary: ValidationSummary,
}

#[derive(Clone, Debug, Default)]
pub struct ValidationSummary {
    /// One entry per validator that ran, in registration order.
    pub outcomes: BTreeMap<String, OutcomeSummary>,
    pub validators_run: Vec<String>,
    pub total_wall_ms: u64,
}

#[derive(Clone, Debug)]
pub enum OutcomeSummary {
    Pass,
    Filter { dropped: Vec<String> },
    Annotate { metrics: HashMap<String, serde_json::Value> },
    // Reject doesn't appear in summary — the call returned Err instead.
}
```

Caller 视角（Python）：

```python
r = pipeline.complete(...)
r.text                    # 最终 text(已 Filter 过的)
r.usage                   # token 计数
r.telemetry               # cache_hit / retry_count(Stage 4)
r.validation_summary      # validator outcomes(本 doc 新加)
   .outcomes              # dict[validator_name, outcome_dict]
   .validators_run        # list[str] — 执行顺序
   .total_wall_ms         # validation 总耗时
```

跟 Stage 4 `Response.telemetry` 不冲突——一个是 infra metric、一个是 semantic metric，并存：

| 字段 | 内容 | 谁产 |
|---|---|---|
| `Response.telemetry.cache_hit` | 缓存命中（运行时事实）| TelemetryMiddleware |
| `Response.telemetry.retry_count` | 重试次数（运行时事实）| RetryMiddleware |
| `Response.validation_summary.outcomes["x"]` | 校验器 x 的处置（语义判断）| ValidationMiddleware |

---

## 5. Built-in validators

v1 内置一组覆盖 80% 用例的 deterministic validators。LLM-as-judge 风格 validator **不**作为 built-in 提供——见 §10 anti-patterns。

### 5.1 `JsonShapeValidator`

```rust
pub struct JsonShapeValidator {
    schema: serde_json::Value,
    on_fail: ShapeFailMode,  // Reject | Annotate
}

pub enum ShapeFailMode { Reject { retriable: bool }, Annotate }
```

行为：尝试 `serde_json::from_str(&resp.text)`，再用 [jsonschema](https://crates.io/crates/jsonschema) crate 验证。
- 解析或 validate 失败 → 按 `on_fail` 决定 Reject 还是 Annotate（`shape_violation: true`）
- 解析成功 → Pass

为什么这条作为 v1 第一个 built-in：downstream consumer 的 critic 输出本来就是 strict JSON，schema 已经定好，跨 consumer 复用度最高。

### 5.2 `RuleIdWhitelistValidator`

downstream consumer 的 inline 实现升级版。

```rust
pub struct RuleIdWhitelistValidator {
    json_path: String,           // e.g. "$.findings[*].rule_id"
    allowed: HashSet<String>,
    on_unknown: UnknownIdMode,   // DemoteToAdHoc | Reject | Annotate
}
```

行为：parse text 为 JSON → JSONPath 提取所有 rule_id → 跟 `allowed` 集对比 → 按 `on_unknown` 处置。

`DemoteToAdHoc` 模式：把所有不在白名单的 rule_id 改写成 `"ad-hoc"`，在 evidence 字段里记录原值（跟 downstream consumer 当前 inline 实现一致）。**Filter outcome**——返回改写后的 Response。

### 5.3 `MaxLengthValidator`

```rust
pub struct MaxLengthValidator {
    field: ResponseField,        // Text | Thinking | ToolCallsCount
    max: usize,
    on_exceed: LengthFailMode,   // Reject | TruncateAndAnnotate
}
```

防御 prompt injection / runaway generation——caller 不希望 critic 输出超过 N KiB。

### 5.4 `RegexBannedValidator`

```rust
pub struct RegexBannedValidator {
    patterns: Vec<regex::Regex>,
    on_match: BannedMatchMode,   // Reject | Filter
}
```

输出不能含 `password=`、API key 形态、PII 模式等。Filter 模式删除匹配片段；Reject 模式整次拒绝。

### 5.5 `EvidenceTagValidator (downstream-consumer-specific，作为 example，不进 built-in）

```rust
pub struct EvidenceTagValidator {
    json_path: String,           // "$.findings[*].evidence"
    required_keys: Vec<String>,  // ["kind", "axis", "action", "confidence"]
    on_missing: MissingTagMode,  // Reject | Annotate
}
```

**这条不进 built-in**——downstream consumer-specific evidence schema。但 trait 形态完全一致，consumer 自己注册。Doc 给出实现示例方便复用。

---

## 6. Python 绑定

### 6.1 实现 OutputValidator 的 Python 形态

复用 Stage 3 PyTool 的 PyO3 模式。Python 写 base class，Rust 包装成 `OutputValidator`。

```python
import tars

class RuleIdWhitelistValidator(tars.OutputValidator):
    def __init__(self, allowed: set[str], json_path: str = "$.findings[*].rule_id"):
        super().__init__(name="rule_id_whitelist")
        self.allowed = allowed
        self.json_path = json_path

    def validate(self, req, resp):
        # parse resp.text as JSON, walk JSONPath, find unknowns
        try:
            data = json.loads(resp.text)
        except json.JSONDecodeError as e:
            return tars.Reject(reason=f"not valid JSON: {e}", retriable=True)

        unknowns = []
        for finding in data.get("findings", []):
            rid = finding.get("rule_id")
            if rid and rid not in self.allowed:
                unknowns.append(rid)
                # demote in place
                finding["evidence"] = f"hallucinated_rule_id={rid}; " + finding.get("evidence", "")
                finding["rule_id"] = "ad-hoc"

        if not unknowns:
            return tars.Pass()

        # Build a new Response with the rewritten text.
        new_resp = resp.with_text(json.dumps(data))
        return tars.Filter(response=new_resp, dropped=unknowns)
```

四个 outcome 工厂函数：`tars.Pass()` / `tars.Filter(response, dropped)` / `tars.Reject(reason, retriable)` / `tars.Annotate(metrics)`。

### 6.2 注册到 Pipeline

```python
p = (
    tars.Pipeline.builder("qwen_coder_local")
        .add_validator(RuleIdWhitelistValidator(KNOWN_IDS))
        .add_validator(MaxLengthValidator(field="text", max=50_000))
        .build()
)
```

需要 PyO3 暴露的 `Pipeline.builder()` —— 当前只有 `Pipeline.from_default(id)` 和 `from_str(toml, id)`，需要新增 builder API（B-6c 在 TODO 已记录，借这次一起做）。

### 6.3 Sync 安全 / GIL

PyO3 wrapper（`PyValidatorAdapter`）在 `validate` 调用时 acquire GIL → 调 Python 的 `validate(req, resp)` → 转换 outcome 为 Rust enum。这跟 PyTool 完全同形（见 `tars-py/src/session.rs:267`）。

注意：**Validator 是同步的，async 工作不允许**。Python 写法上不能用 `async def validate`；如果你的 validator 需要 IO，要么把 IO 推到构造时（预加载），要么改写成 evaluator（Doc 16）。

---

## 7. downstream consumer 迁移路径

### 7.1 现状（commit `1fe6cbc`）

```python
# app/core/critic_agent.py 内部
self._known_rule_ids: set[str] = ...  # 加载时构建
# scan_file 末尾:
if self._known_rule_ids:
    for uid, finding in parsed.items():
        rid = finding.get("rule_id", "")
        if rid and rid not in self._known_rule_ids:
            finding["rule_id"] = "ad-hoc"
            finding["evidence"] = f"hallucinated_rule_id={rid}; " + (finding.get("evidence") or "")
            demoted += 1
```

特征：
- inline 在 critic_agent，跟 LLM call 紧耦合
- 只对 downstream consumer 自己用
- 测试在 downstream consumer tests 里
- 单一 mode（demote-to-ad-hoc）

### 7.2 迁移后

```python
# consumer/validators.py 新文件
import tars

class ArcRuleIdWhitelistValidator(tars.OutputValidator):
    def __init__(self, rubric_paths: list[str]):
        super().__init__(name="consumer_rule_id_whitelist")
        self.allowed = RubricParser.known_rule_ids(rubric_paths)

    def validate(self, req, resp):
        # ... 调内置 RuleIdWhitelistValidator 的 Python 等价逻辑
```

```python
# app/core/critic_agent.py 调整
self._pipeline = (
    tars.Pipeline.builder("qwen_coder_local")
        .add_validator(ArcRuleIdWhitelistValidator(rubric_paths))
        .build()
)
# scan_file 不再做 post-filter — Validator 已经在 Pipeline 内部跑了
parsed = self._pipeline.complete(...).text  # 已 Filter 过
```

迁移收益：
- 50 行 critic_agent 代码 → 0 行（搬到 validator 类、`add_validator` 一行）
- 测试：Validator 单元测试 + downstream consumer 集成测试一起验
- 复用：future tools 也想做这个时，import 同 validator 即可

### 7.3 迁移代价 / 不动的地方

- **dogfood 数据 schema 不变**——`evidence` 字段里的 `hallucinated_rule_id=...` 标记保留，下游 metric 兼容
- 时间：~30 分钟（validator 类 ~30 行、pipeline 装配 ~3 行、删旧 inline ~20 行、测试调整 ~10 行）
- 风险：低——同等价行为，单元测试覆盖

---

## 8. 顺序 / 组合语义

### 8.1 执行顺序 = 注册顺序

`add_validator(A)`, `add_validator(B)`, `add_validator(C)` → 执行顺序 A → B → C。

### 8.2 Filter 链式

每个 Filter validator 看到的是上一个 Filter 后的 Response。

```rust
// validator A returns Filter(resp=A')
// validator B sees A', returns Filter(resp=B')
// final response: B'
```

### 8.3 Reject 短路

第一个 Reject 立刻 abort 整个 validation chain。后续 validators 不执行。

理由：Reject 本来就要 retry / fail，没必要继续累计指标。

### 8.4 Annotate 不影响 control flow

Annotate 写指标到 `summary.outcomes`，response 不变，后续 validator 正常执行。

### 8.5 推荐排序

**deterministic 便宜的在前，expensive 在后**：

1. `JsonShapeValidator`（解析 JSON）—— 解析失败立刻 Reject，不浪费后面 validator 的功夫
2. `MaxLengthValidator`（O(N) 字符数）
3. `RuleIdWhitelistValidator`（O(N) findings 数）
4. `EvidenceTagValidator`（O(N) findings 数）
5. `RegexBannedValidator`（regex 编译预先做，match O(N)）

**有依赖的 validator 必须在依赖之后**：例如 `RuleIdWhitelistValidator` 依赖能 parse JSON，必须在 `JsonShapeValidator` 之后。文档说清楚，让 caller 自己负责正确排序——不引入显式依赖图（YAGNI）。

---

## 9. Telemetry & 集成

### 9.1 Layer trace

ValidationMiddleware 在 `Response.telemetry.layers` 里加一个 "validation" 标签。Caller 看到 layer chain 是 `["telemetry", "cache_lookup", "retry", "validation", "provider"]`。

### 9.2 OutputValidator 失败的 telemetry

Validator 自己 panic 或抛出 unexpected error 时，ValidationMiddleware 转成 Reject + 写 tracing warn。**不让 validator bug 静默吃掉 response**。

### 9.3 跟 Doc 16 evaluation 的协作

Validation 的 Annotate 指标写到 `Response.validation_summary`。Doc 16 evaluation 的 dimension scores 写到 `EventStore` 的 `EvaluationScored` 事件。两个不重叠：

- Validation Annotate = "本次单 call 的快照"，立即可读
- Evaluation = "时间窗内多 call 的趋势"，从 EventStore 查

caller 想做哪一个看用例：
- 想立即决定下一步行动（"score < 0.5 走 fallback"）→ Validation Annotate
- 想看趋势 / 投仪表盘 → Evaluation（Doc 16）

---

## 10. Anti-patterns（明确不做）

### 10.1 不要把 evaluation 塞进 validator

```python
# ❌ 反例
class CriticQualityValidator(tars.OutputValidator):
    def validate(self, req, resp):
        # 调一个 LLM-as-judge 给 critic 打分
        judge_score = llm.complete(f"Rate this critique 0-1: {resp.text}")
        if judge_score < 0.7:
            return tars.Reject(...)
```

问题：
1. 每 call 多调一次 LLM——成本翻倍、延迟翻倍
2. LLM-judge 非确定，validation 失去 reproducibility
3. 把 validation gate 跟 evaluation 仪表板混了——分数掉的时候不知道是真的质量降了还是 judge 模型本身抖动

正确做法：判断 critique 质量是 **evaluation** 维度，做成 Doc 16 的 async evaluator，结果通过 EventStore 反馈，不参与 validation gate。

### 10.2 不要在 validator 里做异步 IO

```python
# ❌ 反例
class FactCheckValidator(tars.OutputValidator):
    async def validate(self, req, resp):       # 接口不允许 async
        facts = await fetch_knowledge_base(...)
        ...
```

问题：validator 是同步契约——每 call 阻塞工作。async IO（数据库 / RPC）拉长 critical path。

正确做法：Doc 16 的 async evaluator 自然支持。

### 10.3 不要把 validator 当配置 toggle

```python
# ❌ 反例
class DebugValidator(tars.OutputValidator):
    def validate(self, req, resp):
        if os.getenv("ARC_DEBUG"):
            print(f"DEBUG: {resp}")
        return tars.Pass()
```

问题：validator 为了 debug 副作用——但它会跑在每 call 上，副作用不可控。debug 应该走 logging / `tracing::*`。

### 10.4 不要让 Filter 隐式改 schema

```python
# ❌ 反例
class TruncateAllStringsValidator(tars.OutputValidator):
    def validate(self, req, resp):
        # 把所有 string 字段截到 100 字符
        return tars.Filter(response=truncated, dropped=["..."])
```

问题：caller 拿到的 Response.text 不再符合原 prompt 的 schema 期望——下游 parser 可能 break。Filter 应该**删除/替换非法部分**，不是**统一改写所有字段**。

正确做法：MaxLengthValidator 显式针对一个字段，截断行为可预期。

---

## 11. 实施路径

参考 [Doc 14 §9 Implementation Path](./14-implementation-path.md) 的 milestone 体例。本文档对应 **M9 wave 1**。

### 11.1 阶段拆分

| 阶段 | 内容 | 估时 |
|---|---|---|
| **W1.1** | `OutputValidator` trait + `ValidationOutcome` enum + `ProviderError::ValidationFailed` 加变体 | 0.5 天 |
| **W1.2** | `ValidationMiddleware` 实现 + 集成到 Pipeline builder + drain-then-emit stream wrapping | 1 天 |
| **W1.3** | `Response.validation_summary` 字段 + tars-py 暴露 + `tars.Pipeline.builder()` API | 0.5 天 |
| **W1.4** | Built-in validators：`JsonShapeValidator` + `RuleIdWhitelistValidator` + `MaxLengthValidator` + `RegexBannedValidator` | 1 天 |
| **W1.5** | PyO3 cross-language `tars.OutputValidator` base class + `tars.Pass/Filter/Reject/Annotate` 工厂 | 1 天 |
| **W1.6** | 单元测试 + 集成测试 + downstream consumer 迁移示例 + CHANGELOG | 0.5 天 |
| **总计** | | **~4.5 天** |

### 11.2 落地后立刻动作

1. downstream consumer 把 `_known_rule_ids` 迁出来（B-15 TODO 现行已注，参见 consumer 仓库 commit `1fe6cbc`）
2. downstream consumer 把 `MaxLengthValidator` 接上（防 prompt injection 输出爆）
3. 写一个 `EvidenceTagValidator (downstream-consumer 专用）作为 plugin example，进下游 consumer 仓库 `consumer/validators.py`
4. tars 这边监控 validation overhead——TelemetryMiddleware 已经在 `pipeline_total_ms` 里包含 validation 时间，验证 < 5% pipeline overhead

### 11.3 不在 W1 做（推到 W2 或砍）

- 配置驱动的 plugin discovery（`[validators]` TOML 段）—— 静态注册够用 99%
- streaming-aware validator（在 token-by-token 中段判断是否 Reject）—— 大多数场景不需要，YAGNI
- 单独 `tars-validation` crate —— Built-in 只 4 个，进 `tars-pipeline::validation` 子模块即可

---

## 12. 跨文档引用

- **Doc 02 Middleware Pipeline** — 本文档定义的 ValidationMiddleware 是其中第 4 层（位于 Retry 内、Provider 外）
- **Doc 12 API Specification** — `Pipeline::builder().add_validator(...)` API 在那里规范化
- **Doc 16 Evaluation Framework** — 评估（多维度评分 / 时间序列 / 数据集）是另一回事，看那篇
- **Doc 04 Agent Runtime** — Agent 怎么用 ValidationMiddleware 装配 Pipeline 在那里讲

---

## 13. Open questions

### 13.1 Validator 之间显式依赖图？

当前：caller 自己负责排序。例：`RuleIdWhitelistValidator` 假定 JSON 已经能 parse，所以放在 `JsonShapeValidator` 之后。

替代：trait 加 `fn depends_on() -> Vec<&str>`，runtime 拓扑排序。

判断：YAGNI。8 个 validator 之内手动排序没问题，超过这个量级再考虑。

### 13.2 流式 validator？

当前：validator 拿到完整 Response。

替代：`fn validate_partial(&self, partial: &PartialResponse) -> Option<ValidationOutcome>`——validator 中段决定是否提前 reject（例如 max_length 在到达阈值时立刻 abort）。

判断：v1 不做。max_length 通过 `max_output_tokens` 在 provider 层就能截，不需要 validator 介入。streaming-aware 是优化，不是新能力。

### 13.3 跨 call 的 validator state？

当前：每 call validator 独立，无状态。

替代：validator 持有 `Arc<Mutex<State>>`，跨 call 累积（例如"过去 100 call 的 ad-hoc rate"，超过阈值开始 Reject）。

判断：**这是 evaluation 而不是 validation**——见 Doc 16。Validator 必须是无状态纯函数。

### 13.4 Per-tenant 配置？

当前：ValidationMiddleware 在 Pipeline 构造时确定 validator 列表。

替代：从 `RequestContext.tenant_id` 查租户配置，动态选 validator 集。

判断：等 Multi-tenant 落地（Doc 06 §3）后再加。当前 Pipeline 是 single-tenant assumption。
