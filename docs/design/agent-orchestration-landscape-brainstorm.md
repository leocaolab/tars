# Brainstorm:Agent 编排层的业界现状 × 我们的三个赌注

**Topic:** agent orchestration 编排层 —— 业界 2026 现状,以及 tars 的 anneal/world-model/MSR 路线站在哪
**Date(s):** 2026-08-19
**Participants:**
- **Leo**(maintainer)—— 从 fix workflow 的形状问起,推到"从 runtime 搞定走向编排层"的判断,要业界对位
- **Claude** —— 读码 grounding + web 搜业界现状(Aug 2026)+ 定位
**Status:** `live`
**一句话:** 编排层这半年被业界打出了一个"赢家架构",它跟我们的骨架逐字对齐;我们真正的分歧只在三个赌注上,且都落在业界还空的地方。

**配套:** [`tars-loop-engine.md`](./tars-loop-engine.md)(一原语 anneal 的 interface-lock)· arc `fix`/`auto` workflow 源码 · [[agent-needs-world-model]] · [[prodspec-annealing-reference]] · [[mcp-is-a-weak-agent-crutch]]

---

## 2. TL;DR — 最终决定(最终决定)

1. **"从 runtime 走向编排" 的判断成立,且踩在业界节奏上。** runtime/内环(ReAct tool-loop)已被 OpenAI/Anthropic tool-use 商品化;编排/外环是现在业界打的地方。—— *依据:先例(web Aug 2026)*
2. **我们的主干 = 业界 2026 赢家架构,逐字对齐。** 业界共识:"deterministic flow backbone + node 上放 LLM + 控制交回引擎";我们的 `Workflow=DAG 数据 / StepBody 在节点 / engine tick 返回 Outcome` 就是它。**不是异端。** —— *依据:先例(web + [`tars-loop-engine.md`](./tars-loop-engine.md))*
3. **Durable execution 赢了,成了基线(不是选项)。** 我们的 Store(节点生命周期)+ Parked/resume + 崩溃从事件重建 属于这一派。—— *依据:先例(web)*
4. **三个真正的赌注,都落在业界还空/非主流的地方:**(a)一原语 anneal / reconcile-到-不动点 / world-model;(b)derived-edges correct-by-construction;(c)MSR-at-corpus-scale。—— *依据:(a)(b) 判死(价值承诺);(c) OPEN,需证真需求*
5. **fix 具体决定:保持单阶段 fan-out Flow,不套 emit-sink-fold;task #21 应删。** 不动点属于 walk/auto,不属于 bare fix。—— *依据:查死(源码 file:line)*
6. ⚠️ **doc 漂移待订正:** fix `mod.rs:180-182` 注释说 auto "loops across outer rounds",与实现 `converge.rs`(SINGLE PASS, no outer loop)矛盾;注释过时。

---

## 3. 参与者 & 立场

- **Leo** —— 直觉"fixer 不该还是 flow 了吧",怀疑它是 MSR / 不动点;进而拔高到"我们开发从 runtime 走向编排层"这个元判断,要业界对位。倾向省结构、反臃肿(本 session 前半刚批过 verify 的 emit-sink-fold 是过度工程)。
- **Claude** —— 先读码把 fix 的真实形状钉死(单阶段 fan-out,不是不动点),再 web 搜业界 Aug 2026 现状,给出"我们对齐主干、分歧在三赌注"的定位。**修正过自己一次**:一开始把"给 fix 套 emit-sink-fold"当任务(#21),读码后判定那是纯臃肿、应删。

---

## 4. 讨论方向(讨论方向)

### D1 — fix 是 flow / MSR / 不动点?(读码钉死)

**问题:** Leo 问 fix 现在是什么形状,是不是 MSR,算不算不动点。

**证据(查死,arc 源码):**
- fix 的 `diagonal.rs:359` = `Flow::from_goals(goals).map("fix", max_concurrent, step)` —— **单阶段 fan-out**,一个 `.map`,无下游 sink/fold。业界话语里 = **scatter without gather**(map 没 reduce)。
- `fixed`/terminal 事件**在 source 写**:`fix.rs:969` `flush_staged`(single-writer,commit 到 `arc/fix-*` 分支)。
- test-gen + `test_planner` 在**每个 chunk 的 fix body 内部**(`fix.rs:561-594`;`fix.rs:566-567` 显式区分 plain `test_planner` ≠ agentic `planner`),不是独立 flow stage。
- bare `arc fix` = **单 pass**(`mod.rs:44,182`),失败 finding 留 open 给"下次 run"。
- **真正的进程内(有界)不动点有两个,都不在 bare fix:** per-file walk 的 `fix↔verify` 到 `--max-turns`(`converge.rs:126,144`);merge_reconcile 的 build-green,cap=3(`merge_reconcile.rs:160`)。
- `arc auto` **刻意不做 outer re-review loop**(`converge.rs:1-15`):re-review 改过的文件是 discovery,冒新 finding,board 永不收敛 —— 收敛委托给人重跑。

**Pro/Con(给 fix 套 emit-sink-fold,即 task #21):**

| | Pro | Con |
|---|---|---|
| 套 emit-sink-fold | 形式跟 review/verify 统一 | fix 已 at-source 写、无 monolith 可拆;加 sink stage + serialize passthrough = 纯臃肿(与本 session 批过的 verify 同病) |
| 保持单阶段 fan-out | 已经对、已经在;merge REDUCE 已是独立顶层 stage | "不统一"的观感 |

**Verdict:** **DECIDED —— 保持单阶段 fan-out,#21 应删。** fix 是"算子的一次应用",不是"迭代到不动点";board 级 review-fix 不动点当前被刻意不做。*依据:查死(源码)。*

### D2 — 业界编排的"赢家架构"是什么?我们对不对齐?

**问题:** 从 runtime 走向编排,业界编排层现在长什么样。

**Options / 现状(先例,web Aug 2026):** 五范式 —— 可循环状态图(**LangGraph**,事实标准)/ actor 消息传递(AutoGen, OpenAI Agents SDK handoff, CrewAI)/ **durable execution**(Temporal, DBOS, Restate, Inngest)/ planner 分层(LLMCompiler)/ map-reduce scatter-gather。协议层 MCP+A2A+ACP 进 Linux Foundation。

**赢家架构(反复逐字出现):** "deterministic backbone(the flow)+ 特定步骤部署智能;agent 被 flow 有意调用;完成后控制权永远交回 backbone。" + "**Supervisor 是 2026 生产默认**。"

**Positions:**
- **Claude:** 我们的 `Workflow=DAG 数据 / StepBody 节点 / engine tick 返回` **逐字**就是这个赢家主干;业界 #1 失败模式"无限 handoff 循环"的解法"硬性 max round count" = 我们的 fuel/有界 crank/`MAX_RECONCILE_ROUNDS`。**主干对齐,不是异端。**
- **Leo:** (接受;推进到"那我们的差异在哪")

**Verdict:** **DECIDED —— 主干对齐。** *依据:先例(web + [`tars-loop-engine.md`](./tars-loop-engine.md))。*

### D3 — 赌注一:一原语 anneal / reconcile-到-不动点 / world-model

**问题:** 我们"底层只有一个原语 `anneal(world, spec)`,DAG/loop 是两个尺度"([`tars-loop-engine.md`](./tars-loop-engine.md) §0)—— 业界有没有这么干的?

**证据(先例,web):** 明确搜过 —— 业界说 "reconciliation" 指**冲突消解**(多数投票 / 置信度打分 / 转人),**不是** "把 world 收敛到 gap 空"。fixpoint / world-model 框架业界基本没有。业界主流是 Supervisor(LLM 路由 agent)+ 图遍历到终止节点。**旁证:LangGraph Q2 2026 加了 `DeltaChannel`(每步只存增量 delta)—— 业界在往 World/Diff coalgebra 的直觉上挪**(我们 `tars-agent2/src/diff.rs` 已有),说明方向不孤。

**Pro/Con:**

| Pro | Con |
|---|---|
| 极度 parsimonious(一个引擎两次 instantiate,`runtime.rs:81`);血统硬(k8s reconcile / Prodspec-Annealing) | 非主流 → 认知成本高;唯一风险:**会不会只是"把 LangGraph 重造得更难懂"** |
| world-model 直接对治"从 message history 重建状态"的失败模式 [[agent-needs-world-model]] | 大部分是 DESIGN(interface-lock),未落地成完整引擎 |

**Verdict:** **OPEN(判死为主 + 一个待证风险)。** 作为价值承诺采纳;风险"是否只是难懂版 LangGraph"要**靠把它建出来、跑真 CUJ**才能证伪,不是辩论能settle的。*依据:推理 + 先例;落地度 = ASPIRATIONAL。*

### D4 — 赌注二:derived-edges correct-by-construction vs Supervisor

**问题:** 我们边从 reads/writes 推(`Workflow::dag()`,读无人写的 key = build-time 错,[`tars-loop-engine.md`](./tars-loop-engine.md) §4);业界默认 Supervisor(LLM 路由)。

**Pro/Con:**

| 我们:derived-edges | 业界:Supervisor |
|---|---|
| 确定、可静态查错、省 token、无 LLM 调度员;更像 build system(Bazel/dataflow/Salsa 增量) | 灵活、动态路由、LLM 决定谁干下一步 |
| 弃掉了运行时动态重路由的灵活 | 无限 handoff 循环是其 #1 失败模式 |

**Verdict:** **DECIDED(判死 —— 价值承诺)。** 我们要确定性 + 静态可验证,接受放弃动态路由。业界没人这么干,是差异点。*依据:判死。*

### D5 — 赌注三:MSR-at-corpus-scale(业界空档)

**问题:** 我们的 MSR(map over N 文件语料,如 arc review)是不是业界空白?

**证据(先例,web):** 业界的 "map-reduce / scatter-gather" 全是**小扇出的多专家打同一个任务**(一份合同同时跑 legal+financial+compliance 再 merge),**不是**批量 map over 大语料。数据尺度的 MSR 多靠 Ray/Spark+LLM UDF 手搓,agent 框架里不是一等公民。我们 `FnStep < LlmStep < AgentStep` 阶梯把 "MSR-map=LlmStep 一次调用" 收进最低档(`tars-agent2/src/step.rs`)。

**Pro/Con:**

| Pro | Con |
|---|---|
| 业界一等公民缺失 → 最可能变护城河;跟 rust-data-engine 方向合流 | 需证明"corpus-scale MSR"是真需求而非我们自嗨;吃得下规模才算数 |

**Verdict:** **OPEN。** 采纳为方向;"是不是真需求 + 我们能不能吃下规模"待证。*依据:先例(空档确认)+ 推理(需求假设未证)。*

---

## 5. Open questions / parked

- **[D3] "只是难懂版 LangGraph?"** —— 唯一 settle 方式:把 anneal 引擎建出来,跑 arc 的真 CUJ(review/fix/walk),看它在**认知成本 / token / 正确性**上是否真优于"LangGraph×Temporal"的等价实现。辩论不能 settle。
- **[D5] corpus-scale MSR 是真需求吗?** —— 需要一个规模场景(N≫ 的语料批处理)证明它不是小扇出多专家能覆盖的。
- **[tars-loop-engine §9 遗留]** World Component vs Cx Capability 的切线(git 状态物化多少进 world);节点内 trajectory 是否走 tars-runtime AgentEventLog;edge 推导 build-time 检查放构造期 panic 还是前置 `validate()`。

---

## 6. Next step(出口)

- **落地度诚实:** 主干对齐(D1/D2)是**查死 + 先例**;三赌注里 D4 是**判死(价值承诺)**,D3/D5 各带一个**待证经验前提**(难懂性 / 真需求),但都**不是**"读别人 tech testimony 能定"的,而是"把我们自己的东西建出来 + 跑真 CUJ"才能证——所以**不是 spike-first,是 build-instrumented**。
- **可立即动作(本 session 产出):**
  1. **删 task #21**(给 fix 套 emit-sink-fold = 纯臃肿)。
  2. **订正 fix `mod.rs:180-182` 注释**(与 `converge.rs` SINGLE-PASS 矛盾,过时)—— 纯文档,不改行为。
- **方向:** → 继续沿 [`tars-loop-engine.md`](./tars-loop-engine.md) 把 anneal 引擎建出来(这就是 D3 的证伪装置);建的时候带 instrument,好拿 arc CUJ 做认知成本/token/正确性对比。不需要独立 spike。

---

## 7. 原始对话记录(历史对话记录)

1. **Leo:** "看看 fixer 怎么修应该不是 flow 了吧。fix workflow 现在还包括 test gen, planner,是不是一起修了。它算不动点吗?"
2. **Claude**(读码后):fix 的 diagonal 已是**单阶段 fan-out Flow**(`diagonal.rs:359`),不是多阶段 emit-sink-fold;`fixed` 已 at-source 写(`fix.rs:969`);test-gen+`test_planner` 在 chunk 内部(`fix.rs:561-594`),非独立 stage;bare fix **单 pass**,不是不动点;真正有界不动点在 walk(`fix↔verify` 到 `--max-turns`)和 merge_reconcile(cap 3);auto **刻意不做** outer re-review loop(`converge.rs:1-15`)。→ **判定 task #21(套 emit-sink-fold)应删**,并报一个 doc 漂移(`mod.rs:180` vs `converge.rs`)。
3. **Leo:** "它是 msr 吗,tars-runtime 支持 msr"
4. **Claude:** 查到 "MSR-map" 是我们自己的词(`tars-agent2/src/step.rs`,[`tars-loop-engine.md`](./tars-loop-engine.md):68);业界叫 map-reduce/scatter-gather。正要查 Flow 定义时被打断。
5. **Leo**(打断,拔高):"agent 编排现在的支持都是怎么样的。看来我们的开发,现在有点从 runtime 搞定,走向编排层面了。" —— 再澄清:"我问的是业界情况。"
6. **Claude:** 给出五范式 + 赢家架构 + 我们三赌注的定位(先据 1 月知识)。
7. **Leo:** "要"(web 搜最新)。
8. **Claude**(web Aug 2026):durable execution 赢了成基线(LangGraph 1.0 Oct'25 / OpenAI Agents SDK / Pydantic AI;Temporal $300M/$5B;73% 部署一年内死);MS Agent Framework 1.0 GA ~4 月(AutoGen+SK 合并,MCP+A2A native);LangGraph 成默认(Q2'26 加 TimeoutPolicy/typed NodeError→recovery/**DeltaChannel**/v2 streaming);赢家架构 = deterministic backbone + node LLM + 控制交回,Supervisor 默认;#1 失败=无限 handoff 循环,解=硬 max round;reconcile 框架业界=冲突消解≠不动点(我们的框架仍非主流);map-reduce=小扇出多专家≠corpus-scale(空档);协议 MCP+A2A+ACP 进 LF。→ 定位:主干对齐,分歧在三赌注。
9. **Leo:** "要"(落成本 brainstorm 文档)。
