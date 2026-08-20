# Brainstorm:Fiber / durable-execution agent —— replay DevEx × 三个硬骨头

**Topic:** 给动态 agent 一个"直线代码 + 隐式容错"的 DevEx(`FiberScope`),底层用事件重放 + 结构化并发。评估一份外部 LLM 给的执行引擎骨架。
**Date:** 2026-08-19
**Participants:**
- **Leo**(maintainer)—— 从"类 ZIO fiber 的 agent"问起;纠正了"砸掉 Flow"的误判,定了双引擎;要看全代码的 DevEx
- **Gemini(pasted)**—— 给出 `FiberScope` 三步骨架 + React 编排 + 完整 review_agent DevEx 示例;主张"降维打击"
- **Claude** —— grounding + 批判评估(三个硬骨头 + 具体 bug),定性为"好北极星、危险蓝图"
**Status:** `live`
**一句话:** DevEx 愿景对,而且它就是 Temporal/DBOS 式 durable execution 搬到 agent;但那份骨架是 90% 语法糖,跳过了 durable execution 的全部难度(determinism / 幂等 effect 边界 / 父子 journal 分区)。

**配套:** [`durable-agent-runtime.md`](./durable-agent-runtime.md)(设计) · [`agent-orchestration-landscape-brainstorm.md`](./agent-orchestration-landscape-brainstorm.md) · [`concurrency-benchmark.md`](./concurrency-benchmark.md) · [[agent-failure-two-phase-commit]] · [[runtime-convergence-standing-decision]] · [[agent-needs-world-model]]

---

## 2. TL;DR — 最终决定

1. **双引擎成立(Leo 定)。** Flow 管数据 DAG(静态拓扑,LlmStep 批处理利器,map→llm→reduce);Fiber 管动态推理 agent(拓扑跑时才知道、可能无限 loop)。**不砸 Flow。** —— *依据:判死(价值/场景切分),Leo*
2. **Fiber DevEx 目标对、值得建,但 provisional。** "直线代码 + 自动重放 = 隐式容错"对动态 agent 确实降维;但这是 durable execution(Temporal/DBOS/Restate)的已知形态,不是新发明。—— *依据:推理 + 先例(业界),provisional until spikes*
3. **那份 `FiberScope` 骨架是好北极星、危险蓝图。** 照字面实现会上线**双发的 GitHub 评论**和**静默 replay 腐败**。—— *依据:推理(逐行审)*
4. **真正的活儿是三个硬骨头,全被 sketch 略过:**(a)determinism 强制;(b)幂等 effect 边界(不可逆外部写不能双发);(c)父子 journal 分区(并发下确定)。—— *依据:推理 + [[agent-failure-two-phase-commit]]*
5. **底座部分存在,但 `FiberScope` 是愿景不是代码。** `DurableStore`/`durable/scheduler.rs`/`Outcome::Parked`/blackboard 在;`FiberScope`/`JournalCap`/`SpawnCap`/`wait_for_all` 是 paste 造的名字。—— *依据:先例(源码)*
6. ⚠️ **仍卡在 P0:** old `Flow` vs agent2 `anneal` 的收敛是 Leo own 的架构决定([[runtime-convergence-standing-decision]]),Fiber 引擎建在谁上、跟 anneal 什么关系,未拍。

---

## 3. 参与者 / 立场

- **Leo:** 双引擎的裁决者。先被外部 LLM 带着"扬弃 Flow",立刻纠正"我要 flow,大哥,那个是数据处理用 llmstep 的利器,这个是另外的应用"——把场景切清。要看真实 DevEx 代码再判。
- **Gemini(pasted):** 提供三步骨架(`FiberScope` 底座 + call_llm/spawn 拦截重放 + run_agent_fiber 引擎)+ React 编排(plan→spawn→wait_for_all→reduce)+ blackboard 通信 + 完整 review_agent 示例。基调是"绝了/降维打击/永远不会出 bug"。**采纳:DevEx 方向、blackboard 通信、双引擎(纠正后)。拒绝/存疑:'永不出 bug' 的容错声明、位置重放、effect-后-journal 顺序。**
- **Claude:** grounding + 逐行审,拒绝 rubber-stamp。定性"好北极星、危险蓝图",拎出三个硬骨头 + 一串具体 bug。

---

## 4. 讨论方向

### 方向 A — 一个引擎还是两个?
- **问题:** Fiber 要不要取代 `tars-runtime::Flow`?
- **选项:** (1)Fiber 一统;(2)双引擎(Flow=数据 DAG,Fiber=动态 agent)。
- **pro/con:**
  - Fiber 一统:pro=一套心智;**con=数据批处理丢了静态 DAG 拓扑,状态极难追踪,反而灾难**。
  - 双引擎:pro=各用其长(Flow 的静态大图 + Fiber 的动态微图);con=两套引擎要维护、要想清共享底座。
- **立场:** Gemini 先主张(1),Leo 纠正为(2)并给出判据(拓扑是否跑时才知道)。
- **裁决:DECIDED(2)。** —— *依据:判死(场景切分),Leo。这是本 doc 里最重要的一句反转(见 §7)。*

### 方向 B — Fiber 的 DevEx:直线代码 + 重放
- **问题:** 值不值得给动态 agent 造 `FiberScope`(开发者写直线 async 闭包,底层重放 + 结构化并发)?
- **pro:** 零状态包袱(局部变量即状态)、原生控制流(for/if/match,没有 evaluate/fold 状态机扭曲)、隐式容错(崩了从 journal 快进)。对动态 agent 是真降维。
- **con:** 这不是新发明,是 durable execution(Temporal/DBOS/Restate)的形态;"隐式容错"的代价全在开发者看不见的三个硬骨头上——一旦漏了,容错变**静默腐败**,比显式状态机更坑。
- **立场:** Gemini 主张且宣称"永不出 bug";Claude 同意目标、否定"永不出 bug"。
- **裁决:DECIDED-推理(值得建),但 provisional。** DevEx 是易的那半;kill-run 见 §6。 —— *依据:推理 + 先例(业界 durable execution)*

### 方向 C — 三个硬骨头(sketch 全略过)
- **问题:** durable replay 真正难在哪?sketch 的方案够不够?
- **C1 determinism 强制:** 位置重放 `history[cursor]` 只在闭包两次 effect 间**完全确定**时成立。没 journal 的时钟/`rand`/HashMap 迭代序,或开发者加/挪一个 effect,cursor 对错档 → `from_value().unwrap()` 把 A 的结果塞进 B → 静默腐败。**sketch 零防护;Temporal 靠禁非确定 API + 沙箱。**
- **C2 幂等 effect 边界:** sketch 把 journal 写在 effect **之后**。`actual_llm_call()` 成功→断电→append 没落→重启重跑 → **`github.post_comment` 双发**。Exactly-once 不存在,真解=write-ahead intent + dedup key。**sketch 只对 spawn 嘴上说幂等,对 LLM/外部写全漏。** 违反 [[agent-failure-two-phase-commit]]。
- **C3 父子 journal 分区:** `spawn_parallel` + 共享位置 cursor,子完成顺序随机 → 位置重放把子 A 结果读进子 B 槽。每个子 fiber 要按 child task_id **keyed 的子流**,不是父的 `history[cursor]`。**sketch 用一句 `derive_child_scope` 糊过去——所有难度都在这。**
- **裁决:OPEN。** 这三个是 durable execution 的全部复杂度预算所在,必须 spike 证死(§6)。 —— *依据:推理*

### 方向 D — agent 间通信:message vs blackboard
- **问题:** 父子/兄弟 agent 怎么通信?
- **裁决:DECIDED — blackboard(持久化共享内存 + journal keyed),不用点对点 message passing。** message + replay = 因果一致性噩梦;blackboard 重放确定。**这条是 Gemini 说对的点,采纳。** —— *依据:推理 + 先例(tars-storage blackboard 已在)*

### 具体 bug 清单(证明 sketch 是示意图非蓝图)
- `serde_json::from_value(..).unwrap()`:跨部署 schema 漂移 panic,journal 版本迁移没处理,且 unwrap 吃真错(违反"说人话")。
- `tokio::signal::ctrl_c()` 挂每个 fiber:进程级信号,不是结构化父级 cancel。
- `scope` move 进 `agent_logic(scope)` 后又在 `select!` else 用 `scope.cancel_token.cancel()`:move 后借用,编不过。
- replay 每次恢复 O(history),无 snapshot / continue-as-new → 无界重放。
- `Mutex<usize>` cursor:单 fiber 多余,并发子不够(要 keyed 流)。

---

## 5. Open questions / parked

- **P0(parked,Leo own):** Fiber 引擎建在 old `Flow` 还是 agent2 `anneal` 上?跟 `anneal`(reconcile-到-不动点 + world-model)什么关系?—— [[runtime-convergence-standing-decision]]。**Fiber 的"直线闭包重放"心智 vs anneal 的"世界模型 + gap 驱动"心智,是同一引擎两副皮,还是两个引擎?未拍。**
- replay 成本上界:长 agent 要不要 snapshot / continue-as-new?判据待定。
- determinism 在 Rust 里怎么强制(没有 Temporal 那种沙箱)?靠 lint / 靠把所有非确定性都走 Cap?OPEN。

---

## 6. Next step(出口 —— MVP / kill-run first)

**不要**先建那个直线 DevEx —— 那是最容易的糖,建了也证明不了什么。**risk-first spike 三件,每件先定判据再跑:**

1. **幂等 effect 边界(最不可逆,先证)。**
   - 判据:一个 `github.post_comment`(或等价外部写)的 fiber,在 effect 与 journal.append 之间**注入崩溃**,重启后**只发一次**。方案:write-ahead intent + dedup key。跑不过就说明"隐式容错"是假的。
2. **determinism + keyed(非位置)journal。**
   - 判据:在闭包里**故意加一个 effect / 改分支**后重启,replay **不静默腐败**(要么正确快进,要么显式报 divergence),而不是把结果串位。
3. **父子 journal 分区(并发确定)。**
   - 判据:`spawn` 3 个子、乱序完成,崩溃重启后每个子的结果**回到自己的槽**,不串位。

三个 PoC 过了,再谈 DevEx 降维打击。**spike 代码是证据、可弃**,别让它靠惯性变成实现。

**Deadlock 提示:** 方向 B 的"值得建"是推理级,且前提含"replay 在 Rust/我们的 effect 形状下能做对"——这是第三方形态在我们用法里的行为,**属于 spike-first,不是直接进 design**。

---

## 7. Raw record(faithful)

- **反转(最有价值的一行):** Gemini 开场"把老 Flow 扔掉……几百行骨架完全替代 `tars-runtime::Flow`"。Leo 立即纠正:**"我要 flow,大哥,那个是数据处理用 llmstep 的利器。这个是另外的应用。"** Gemini 随即改口为双引擎("Flow 管流水线,Fiber 管单兵特种兵")。→ 定了 §4-A。
- **Gemini 的 `FiberScope` 三步骨架**(原文见对话):底座持 `journal/caps/history/cursor/cancel_token`;call_llm/invoke_cap/spawn 内部"有历史读档假跑、没历史真跑存盘";`spawn_parallel` 用 `JoinSet + child_token` 做结构化取消;`run_agent_fiber` 加载 history→建 scope→`select!` await 业务闭包。
- **Gemini 的 React 编排**:plan(call_llm)→spawn(SpawnCap 写 Queue,声称幂等)→wait_for_all(挂起等)→reduce(call_llm 汇总);通信主 blackboard、拒 message passing。
- **Gemini 的 review_agent 全示例**:git.diff→call_llm 判 is_complex→小改自审 / 大改 spawn 子 agent→wait_for_all→聚合→github.post_comment。宣称"砸主板换服务器都能 10ms 快进、永不出 bug"。
- **Claude 逐行审**:定性"好北极星、危险蓝图";三个硬骨头(§4-C)+ bug 清单;指出 `github.post_comment` 会双发、位置重放会静默腐败、`ctrl_c` 挂错层、move-后-借用编不过;grounding 出 `FiberScope` 等名字是 paste 造的、底座只部分存在。
- **待 Leo:** §6 三个 spike 的方向 + P0 收敛(Fiber vs anneal 一副皮还是两引擎)。

---

# 第二轮(2026-08-19,append):ZIO/Akka 借鉴 × agent 到底怎么写 × 收敛判决

> Continue-mode 追加。上面 §1–§7 一字未改。本轮从 Gemini 继续推 FiberScope(auto 重灾区 / ZIO / Akka / Fiber×Actor),Leo 连续纠偏(避邮箱 / 问 DevEx / 要求收敛 tars-runtime),Claude grounding 到 tars-runtime 源码,落到一个硬收敛判决。

## 8. 本轮最终决定(landings)

1. **假二选一被戳穿。** 外部讨论全程只比"老 Flow-DAG vs FiberScope-直线",**不知道 tars 有第三个、已建、已 benchmark 赢的模型:agent2 coalgebra / anneal**。—— *依据:先例(`tars-agent2/src/runtime.rs` anneal)*
2. **最根本一点被搞反:state 放哪。** FiberScope = state 在局部变量 + 崩了 replay 重建 = **正是 [[agent-needs-world-model]] 已证会输的"从历史重建"模式**;agent2 = state 在 world,崩了重新 observe。—— *依据:先例(Leo 的 memory-strategy benchmark,v11 world-native 赢)*
3. **durable 的杀手锏。** world-model → durable = **reload 一个 world 快照 + 重跑 anneal**(硬骨头 1 determinism & 3 父子分区直接蒸发);FiberScope → durable = **重放代码**(三骨头全在)。只有 #2(幂等 effect)两模型都躲不掉。—— *依据:推理*
4. **编排底座 = 持久 queue,不是语言 async(Leo 的底层公理)。** raw async(tokio JoinSet/CancellationToken)当 fabric 四缺陷:易失 / 黑箱不可观测 / 不确定 / 不可分发。降为 queue op 则四反转。**async 只在 I/O 叶子(实测单机 5 万并发),queue 在 agent/step 之间。** 后果:**queue 写并发 = 编排天花板 → sqlite 单写墙(见 [`concurrency-benchmark.md`](./concurrency-benchmark.md))→ 并发写后端不是可选是前提。** —— *依据:实测 + 推理*
5. **DevEx:直线对单 agent 内循环客观更好,但不是免费。** 三笔账:determinism rulebook(看着普通 Rust 其实不能 `now()`/`rand()`/乱序)/ `await` 藏跨崩溃跨小时 suspend / 多 agent 直线丢拓扑。**深层 trade:full-linear+locals → 逼 replay(三骨头);snapshot durability → 要显式 state(没那么直线)。Rust 活着的 async 栈帧不能被 snapshot,是物理约束,所以 linear 只能配 replay。** 全行业中间路 = LangGraph(命令式 node + 显式 State 装 durable + checkpointer 快照)。**纪律:控制流可直线,但要跨崩溃活下来的 state 必须显式进 world。** —— *依据:推理 + 先例(Temporal/LangGraph)*
6. **收敛判决(本轮最硬落点)。** **tars-runtime 已经是一个 durable-execution 引擎,而且比 FiberScope 的 sketch 更对。** `DurableScheduler`+`AnswerStore`+`StepIdempotencyKey`:**plan/step-id keyed、依赖驱动、frontier 每遍从 store 重新 DERIVE、memoized re-run(LLM 绝不重调)**——by-construction 绕开三骨头(不是 positional cursor、不重放代码)。→ **FiberScope 的 durable 需求完美收敛;编程模型只在"扔掉 positional-cursor、改用 stable step-id"时收敛。** `anneal`(内层单 agent)+ `DurableScheduler`(外层编排)**是同一个 reconcile 原语的两个尺度**(step.rs 注释"一原语两尺度嵌套")。**#42 真正的活 = 动态 frontier(运行时插 stable-id 子步),现在没建。** —— *依据:先例(`tars-runtime/src/durable/scheduler.rs:5/97`、`store.rs`、`event.rs:45`)*

## 9. 本轮方向(directions)

### E — auto/walker 是不是 Fiber 的重灾区?
- **Gemini:** auto 是"DAG 终极受害者"——DAG 恨 `while` 循环,fix↔verify 打回重做被迫拆进 Reconciler/多文件状态机;局部变量丢失,上下文被迫塞进 `arc_db`(walker/fixer 深耦合 DB)。Fiber 版 `run_auto_walker` 是直白 `loop { fix; verify; if ok break }`。
- **裁决:诊断对、药错。** 老 Flow 写 auto 确实痛(对);但解药不是 FiberScope,是 **anneal 本身就是那个原生 loop-to-fixpoint**(gap = 未修/未验的 issue;runtime 去 crank),state 在 world 不在 DB 全局态。anneal **已经**解决"DAG 恨循环",比 FiberScope 更进一步,不是退回去。—— *依据:先例(anneal)+ 推理*

### F — ZIO 除 fiber 还借什么?
| 借什么 | 裁决 | 理由 |
|---|---|---|
| Scope / acquireRelease(finalizer) | **借** = #40 finalizer 契约 | cancel 时删 worktree / 关流 |
| Error vs Defect | **已有,别重造** | `Reason` 分层 + `Outcome::Blocked{真错}` vs `Exhausted{gap}` 就是这个 split |
| R / 环境注入(Cap mock 纯测) | **借实质,拒 type 塔** | 注入 Cap bag + mock 纯内存测=真价值;`Agent<R=(..)>` 类型塔是 Rust 过度设计 |
| Schedule(声明式重试) | **runtime 已有** | RetryMiddleware + QueueRunner requeue;别写进 agent 代码 |
- *依据:推理 + 先例*

### G — Akka 借什么?
| 借什么 | 裁决 | 理由 |
|---|---|---|
| Supervision / Let-it-crash | **借语义,拒字面 `panic!`** | 子返回 typed `Outcome`,父 `match` 决定 restart/stop/escalate。Rust 里 `panic!` 当控制流是反模式(跨 await unwind / poison) |
| Event sourcing | **缓** | 就是 replay 模型;snapshot-of-world 对我们更优(§8.3) |
| Location transparency | **YAGNI** | 跨机器路由,现在不需要([[pipeline-builder-no-in-process-multitenancy]]) |
| Behavior swapping(become) | **coalgebra 已有** | decide 随 world 变,不需要 become |
- *依据:推理 + 先例*

### H — 邮箱 vs Fork/Join + Blackboard
- **反转:** Gemini 为凑 Akka 经典形态,给了"程序员 Agent 有邮箱、`receive_message`/`send_message`"的例子。**Leo 纠正:"我们开始设计时候就想着避免邮箱模式,这里?"** Gemini 改口:去掉邮箱,`coder_agent(scope, task)` 生来做一件事做完就死(输入 TaskSpec、输出 Outcome),通信改 Blackboard。
- **裁决:DECIDED — 无邮箱,Fork/Join + Blackboard。** 点对点消息 + replay = 因果一致性地狱;派生/聚合 + 共享黑板天然确定。**Leo 早在设计初期就避开了,对。** —— *依据:判死(Leo)+ 推理*

### I — agent 到底怎么写(六条)
1. state 进 world(Component:render+handlers+version),从当前 render 决策,不从 replay;
2. 循环 = anneal-to-fixpoint,不手写 while;
3. effect 即数据(`Decision::Emit`),Cap 是词汇表,注入+mock;
4. 子 agent:结构化并发 + finalizer(#40),父 match 子的 typed Outcome 做监督,**不 panic**;
5. 通信:Fork/Join + Blackboard,无邮箱;
6. durable(#42)= world 快照 + 幂等 effect 边界,不是位置重放;HITL = park 等人类 Wake 的 Cap effect。
- **裁决:DECIDED-推理(这是 Claude 对 P0 的 grounded 主张,Leo 拥有最终拍板)。** —— *依据:推理 + 先例(agent2)*

### J — LangGraph / CrewAI 支持"直线多 agent"吗?
- **Leo 问:** coalgebra 是终极梦想,但 fiber 简化模型看上去也好——不用写 fiber、运行时替我处理 agent 间调度/通信/协作、用直线。LangGraph/CrewAI 支持吗?
- **grounded 答(先例,web + [`agent-orchestration-landscape-brainstorm.md`](./agent-orchestration-landscape-brainstorm.md) §60/§74):**
  - **LangGraph(事实标准):不是直线,是显式循环状态图** —— node+edge(含环)、共享 typed State、checkpointer durable + interrupt(HITL);多 agent = Supervisor/hierarchical 画在图上,通信走共享 State(黑板式),非邮箱。Q2'26 加 DeltaChannel(每步 delta)→ **往 World/Diff coalgebra 挪**。
  - **CrewAI:** 角色/任务 declarative(sequential/hierarchical,manager 委派),非 linear-imperative-replay。
  - **直线-代码-durable = Temporal/DBOS/Restate**(另一派);即便 Temporal,多 agent 也得显式 child workflow/signal。**没有任何一家把多 agent 编排藏进看不见的直线代码。**
- **裁决:DECIDED — 直线是单 agent 内循环甜区,多 agent 编排全行业保持显式**(可观测 > 写着爽)。de-facto 标准 LangGraph 验证的是 world+图+durable 方向,不是 FiberScope-直线。—— *依据:先例(web)*

### K — 编排底座 = queue,不是语言 async
- **Leo:** "这就是为什么我们不用 python/rust async……必须在底层转化为 queue 的操作。"
- **裁决:DECIDED(Leo 的底层公理)。** async 叶子 / queue 编排;queue 写并发 = 编排天花板;sqlite 单写墙(实测);determinism 需"记下完成顺序 + keyed 结果"(§4-C3),queue 把父子分区从代码重放噩梦降为队列 schema 问题。—— *依据:实测 + 推理*

### L — 收敛判决:FiberScope 必须收敛 tars-runtime(否则 reconsider)
- **Leo 的硬约束:** "FiberScope 最后要收敛在 tars-runtime 上啊。如果不能我们必须再思考的。"
- **grounded(源码):** tars-runtime 已是 durable 引擎(§8.6):`DurableScheduler`(frontier 从 `AnswerStore` 每遍 DERIVE、memoized re-run)、`StepIdempotencyKey`(replay dedupe external ops)、plan/step-id keyed。**by-construction 绕开三骨头。**
- **裁决:能收敛——但不是 positional-cursor 那个 FiberScope。** 收敛形态:`直线 DevEx facade → lower 成 (stable step-id, depends_on) 的 durable step → DurableScheduler(+动态 frontier)→ 外层就是 anneal 的多 agent 尺度`。**若坚持 sketch 的 positional-cursor + 局部变量 replay,就是 Leo 说的"必须再思考"——再思考的答案:扔掉 cursor,用 tars-runtime 已有的 step-id keying。不是 tars-runtime 将就 FiberScope,是 FiberScope 的 keying 将就已经更对的 tars-runtime。**
- **caveat:**(1)动态 frontier(运行时插 stable-id 子步)是真活儿(#42),现在没建(API 无 `add_step`);(2)纯 static-plan-building 的直线代码没法 branch-on-result,真动态 agent 必须动态插步;(3)DevEx facade 最后加、keyed by stable id,先把"anneal↔DurableScheduler 一原语两尺度"跑真 CUJ 坐实。
- *依据:先例(`tars-runtime/src/durable/scheduler.rs`、`store.rs`、`event.rs`、`orchestrator.rs` Plan/PlanStep)*

## 10. 原始记录续(faithful,attributed)

- **Gemini(auto):** auto = DAG 终极受害者;DAG 恨 while;打回重做被迫拆进 Reconciler/多文件;局部变量→被迫塞 `arc_db`(walker/fixer 深耦合 DB);Fiber 版 `run_auto_walker` 直线 loop + `spawn_parallel` 试错。
- **Gemini(#40/#42 spike 策略):** #40 走路线 (b) executor 当外层、anneal 当内层、Finalizer 用 Drop Guard/`select!`;#42 三硬骨头拆解:determinism=Agent 变纯函数一切 IO 走 Effect;幂等=两阶段 Intent+Idempotency Key;父子分区=子有独立子 Journal、父 WaitAll 排序聚合写回。Roadmap:别碰上层 FiberScope、先钉底层 `Intent→执行→Result→注入` 微闭环、HITL 白送。
- **Gemini(ZIO):** R 环境(Cap 注入纯测)/ Error vs Defect / Schedule(声明式重试)/ Scope(acquireRelease finalizer)四借。
- **Gemini(Akka):** Supervision+Let-it-crash / Akka Persistence(=你直觉独立推出的 event sourcing)/ Location Transparency / Behavior Swapping 四借;Fiber×Actor"天作之合",给了 coder `panic!("我不干了")` + architect 监督重试的例子。
- **Leo(反转 1,邮箱):** "我们开始设计时候就想着避免邮箱模式,这里?" → Gemini 改口 Fork/Join + Blackboard、去掉 `receive_message`。
- **Leo:** "coalgebra 是终极梦想,但是 fiber 这个简化编程模型看上去也很好啊……用直线方式。你看看那个 runtime langgraph, crewAI,支持他们吗?"
- **Claude:** 照行业——LangGraph 显式循环图非直线、CrewAI 角色任务、Temporal 才是直线-durable 但多 agent 也显式;**直线=单 agent 甜区,多 agent 全行业显式**。
- **Leo(公理):** "这个就是为什么我们不用 python/rust async 的原因。我们必须在底层转化为 queue 的操作。"
- **Claude:** 确认+锐化:async 叶子/queue 编排/queue 写并发=天花板/sqlite 单写墙/determinism 记完成顺序;FiberScope raw-async 是从已有 queue 设计往回退。
- **Leo(聚焦):** "我问的是用户体验,如果写成这样,会不会体验会好?"
- **Claude:** 好(单 agent 内循环真更好)但抽象漏(rulebook/suspend/丢拓扑);深层 trade linear-locals→replay vs snapshot→显式 state,Rust 活栈帧不能 snapshot;中间路 LangGraph;纪律"控制流直线、durable state 进 world"。
- **Leo(硬约束):** "FiberScope 最后要收敛在 tars-runtime 上啊。如果不能我们必须再思考的。"
- **Claude(grounding + 判决):** 读 `tars-runtime/src/durable/*`+`event.rs`+`orchestrator.rs`——tars-runtime 已是 plan/step-id-keyed 的 durable 引擎(比 sketch 对);durable 需求完美收敛、编程模型需扔 cursor 改 step-id;anneal + DurableScheduler = 一原语两尺度;#42 = 动态 frontier。
- **待 Leo:** 拍 P0(anneal↔DurableScheduler 收敛的具体形)+ 是否启动 #42 动态 frontier 的 spike(先 §6 幂等 effect 边界)。

## 11. 落地注解:`call_llm` 的 lowering + facade 可推迟(把收敛判决从口号变成 10 行映射)

**问题(Leo):** 如果写 Fiber,底层会转化为 LlmStep / 基于 step 的东西吗?
**答:会,而且那个 step 底座是现成的。** 一次 `scope.call_llm` 拆开 = 三步,每步都是 tars 已有:

```
scope.call_llm(step_id, prompt):
  1. AnswerStore.answer(job_id, step_id)         // tars-runtime durable/store.rs
       Some(a) => return a                         //   读档 → LLM 绝不重调(memoized)
       None    => 往下
  2. let r = LlmStep.run(world, view).await        // agent2 step.rs:56(或老路 tars_runtime::LlmStep)
  3. AnswerStore.commit_step(job, step_id, r)      // 存盘 → 解锁 dependents
```
`call_llm` ≈ 一个 `AnswerStore` memoize 包在 `LlmStep` 外面(~10 行);而这个 "有答案 skip、没答案跑再 commit" 的循环 **`DurableScheduler` 已经在做**(scheduler.rs)。

**thin 的半 vs tax 的半:**
- **effect → step:thin,白送**(执行=LlmStep,持久=AnswerStore,去重=`StepIdempotencyKey` event.rs:45)。
- **tax 有两处:**(1)**stable step-id 从哪来** —— 直线没 Plan,得给每个 effect 显式 id 或 macro 生,**不能用 `cursor++`**;(2)**两 effect 之间的控制流不是 step**,replay 重跑(要确定)或 reify 成 plan。

**关键推论(直接决定优先级):因为 lower 是 thin 的,现在不用赌 Fiber。**
- 最简且现在该做:**step-based durable 核(`DurableScheduler`+`AnswerStore`+`LlmStep`)已存在** —— 在真 CUJ(auto)上跑通、证 anneal↔DurableScheduler 一原语。
- **Fiber 直线 facade 先别建**:它只是 memoize 包 LlmStep,**以后加代价小,deferring 无惩罚**。不知道用户要不要直线 → 别赌,先坐实 step 核(反正 Fiber 也 lower 到它)。
- *依据:先例(`durable/store.rs`、`durable/scheduler.rs`、agent2 `step.rs:56`、`event.rs:45`)*

## 12. 优先级(出口 —— 不是"两个 feature",是一个 P0 spike)

**#40 / #42 不该当两个独立 feature 排期,它们都是 P0(anneal↔DurableScheduler 收敛)的产物。** 正确的第一步是**一个 risk-first 收敛 spike**,两者从中掉出来:

**Spike:把 auto 的 fix↔verify 跑在 durable step 核上**(`DurableScheduler` 驱动;agent2 `AgentStep`/`LlmStep` 当 Worker;fix/verify fan-out)。**判据先定:**
1. **崩溃中途重启 → 已完成 step skip、LLM 不重调**(#42 durable 核证真);
2. **fan-out 并发 + first-error 级联取消**(#40 证真);
3. **代码不比 LangGraph×Temporal 等价实现更痛**(orchestration brainstorm §D3 那个唯一风险的证伪)。

**spike 暴露出的才是 #40/#42 的具体活:** 动态 frontier(运行时插 issue 为 stable-id 子步)、cancel 时跑 finalizer、HITL = 一个 parked step。**顺序:durable-memoize-resume(最已建、最去风险)→ 动态 frontier → #40 finalizer → HITL(resume 建对后白送)。**

- **裁决:P0 spike 优先,不是 #40/#42 平行开工。** Leo 拥有 P0 方向的拍板([[runtime-convergence-standing-decision]])——spike 要不要按"auto 跑 durable step 核"这个形起步,等 Leo 点头。—— *依据:推理 + 先例*
- **✅ Leo 点头(2026-08-19):按"auto 跑 durable step 核"起步。** 先做**判据 1 的最小闭环**:一个会崩的 durable job(fix/verify 形状,worker 记 LLM 调用次数),崩溃重启后**已完成 step skip、LLM 不重调**。spike 代码是证据、可弃(§6 纪律)。目标是回答 **#40/#42 能否解决**,不是一次建完。
