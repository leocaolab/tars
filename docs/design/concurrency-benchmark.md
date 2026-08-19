# 并发基准:host 容量 × storage 落盘墙 × TPM 背压

**Topic:** tars 的编排机器到底能扛多高并发?瓶颈在哪(host / storage / provider 速率)?
**Date:** 2026-08-19
**Participants:**
- **Leo**(maintainer)—— 从"类 ZIO fiber 的 agent"问起,推到"我能开 1 万并发连接,机器强不强",要求用 mock 真跑、别猜
- **Claude** —— 读码 grounding + 建 latency-mock bench + 实测 + 建 `TpmRateLimiter`
**Status:** `settled`(实测结论)
**一句话:** 机器不是瓶颈(实测单机扛 5 万并发 parked step),真瓶颈是外部 provider 速率——现在用 `TpmRateLimiter` 背压住了;storage 在 agent2 热路径是内存,不落盘。

**配套:** [`agent-orchestration-landscape-brainstorm.md`](./agent-orchestration-landscape-brainstorm.md) · [`durable-agent-runtime.md`](./durable-agent-runtime.md) · bench:`crates/tars-pipeline/tests/concurrency_bench.rs` · 中间件:`crates/tars-pipeline/src/middleware/rate_limit.rs`

---

## 1. TL;DR — 结论(全部实测)

1. **Host 侧廉价挂 1 万–5 万个 parked-on-LLM 的 step。** agent 一步 = `await` 一个远程异步 LLM,host 只攥 future 不烧 CPU,是经典 c10k I/O-bound。**实测:10k 并发 71ms 跑完、50k 并发 164ms**(串行要 500s)。—— *依据:实测*
2. **"并发被 rate-limit 卡在个位到百级"是错的**(Claude 原话,已收回)。那是把 **host 容量**和 **provider 速率**两个天花板混了:host 能挂 10k+,provider TPM 是另一张外部的表。—— *依据:实测 + 反转*
3. **今天唯一真有的闸是并发 Semaphore**(`tars-runtime/src/executor.rs:750`,`RunPlanConfig::max_concurrent`)。行为干净 = `(N/C)×latency`。**没有** proactive 速率限流器。—— *依据:先例(源码)*
4. **`TpmRateLimiter` 现在建出来了(#41 落地)。** 一个 async token-bucket 中间件,把 provider TPM 当**背压**(park,不丢调用)。**实测限速曲线:achieved TPM 全程贴配置值 99–103%,单调。** —— *依据:实测*
5. **Storage 不在 agent2 热路径上。** agent2 的 `World` 是内存 `BTreeMap`(`crates/tars-agent2/src/world.rs:54`),每个 `anneal` 独占 `&mut World`,**无锁、无 sqlite**。sqlite 只在持久化 event-log(#42)才进场,且可换库。—— *依据:先例(源码)+ 反转(Claude 先前误判 sqlite 在热路径)*
6. **sqlite 落盘是单写墙,但可换。** `SqliteAgentEventLog` 是单 `Arc<Mutex<Connection>>`(`crates/tars-storage/src/sqlite.rs:46`),读写全串行。它藏在 trait 后面(`AgentEventLog`/`DurableStore`/`BlackboardStore`),换 redis/postgres(服务端并发 + 池)墙就没了。—— *依据:先例(源码)*

---

## 2. 方法:latency-only mock,其余全真

要量的是**编排机器**的容量,不是 provider 的速率。所以 mock 只把**远程网络往返**替成 `sleep(latency)`,其它全是真路径:真 `ChatRequest`、真 `LlmService` 中间件洋葱、真 stream drain。

- **为什么必须 `sleep` 而不是 `FnStep`:** `FnStep`(`tars-agent2/src/step.rs:46`)同步跑完、不 `await`,量到的是 dispatch 吞吐(CPU-bound),**不是** parked-on-I/O 的容量。`sleep` 才复现真实的 park。
- **为什么不用 stock `MockProvider`:** 它 `stream()` 零延迟返回、且每次抢一把全局 `Mutex` 记 history(`tars-provider/src/backends/mock.rs:177`)——1 万并发全挤这把锁,量出来是**mock 自己的锁争用**,不是 runtime。bench 里另写了无锁的 `LatencyMock`。
- 硬件/构建:一台 Mac、debug build、4 worker 线程。**是"证明 regime 成立"的地板数,不是生产 SLA**(真实 LLM 是 KB–MB 的流,每任务内存 + parse 会拉低天花板;release 会拉高)。

---

## 3. 发现 A:host 并发容量(latency = 50ms/call)

```
service    gate            N            wall       throughput
chain      unbounded     1000     54ms          18.4k/s
chain      unbounded    10000     71ms         140k/s
chain      unbounded    50000    164ms         305k/s
chain      sem(500)     10000   1.05s           9.6k/s
chain      sem(2000)    10000    269ms          37k/s
bare       unbounded    10000     66ms         152k/s
```
**并发证明:1 万 × 50ms 无 gate → 70ms 跑完(串行 500s,≈7000× 加速)。**

**读法:**
- **10k→71ms、50k→164ms**,约等于"一次延迟 + 一点调度开销",不是 `N×延迟`。host 挂 5 万并发 parked step 轻松,离墙远。
- **并发 Semaphore 行为精确 = `(N/C)×latency`**:sem(500)→1.05s(理论 1.0s)、sem(2000)→269ms(理论 250ms)。这是 tars 今天真有的那把闸(executor)。
- **中间件洋葱开销 ~20%**:bare 66ms vs chain 71ms。真实但不是墙。
- 吞吐被 `C/L` 卡,不被 CPU 卡:无 gate 50k 给到 ~305k calls/s。

---

## 4. 发现 B:TPM 限流器(#41,新建 + 实测)

**背景:** 查遍 `tars-pipeline`/`tars-provider`/`tars-runtime`,**没有** proactive 速率限流器。只有:①并发 Semaphore(限并发≠限速率);②`retry.rs` 是**被动**处理 provider 返回的 429/`Retry-After`。所以 #41 的主动 TPM gate 是从零建的。

**设计(`rate_limit.rs`):**
- **token = semaphore permit。** `acquire_many(cost).await` 用 tokio 的公平 FIFO 队列 park——**不是**每个 caller 抢一把 `Mutex<Bucket>`(那会把限流器自己变成瓶颈,重演 sqlite 单锁的教训)。
- **单 refiller task**,`add_permits` 按**真实 elapsed** 补(不是名义 tick,避免 sleep 抖动把速率压低),capped 在 burst。
- **cost = 估值 → reconcile。** 事前 `chars/4` 估 input + reserved output;事后从 `ChatEvent::Finished{usage}` 拿真实用量,把**多预留的退回桶**——所以限速 bind 在**实际 token**上,不是最坏预留。
- **背压语义:** 超速率的调用在 `acquire` 里 park 到桶补够,不丢、不 429。

**实测曲线(真中间件,~100 tok/call,latency=5ms):**
```
配置 TPM        wall      achieved TPM   achieved/cfg   calls/s
 3,000,000    3.89s       3,084,024        103%         514
 6,000,000    1.95s       6,144,300        102%        1024
15,000,000    0.79s      15,112,684        101%        2519
30,000,000    0.40s      30,018,271        100%        5003
      none    0.01s              —            —      221219
```
**achieved TPM 全程贴配置值 99–103%(残余 ~3% 是 6000-token burst 摊在 200k-token run 上),单调。** 无 gate 这台机器能跑 221k calls/s,挂上限流器被死死摁到配置速率:`calls/s = TPM/60/cost`。

**建的过程里挖出一个真坑(记下来):** semaphore + 离散 tick 的 refiller,若 `rate×tick > capacity`,桶会溢出、把速率**悄悄压到 `capacity/tick` 以下**。第一版 burst=1000 就让所有 TPM 塌成 ~193 calls/s。修法:①tick 调细到 10ms;②`new()` 里若 `burst < rate×tick` 就 `tracing::warn`(surface 不 cover);③生产默认 burst=1 秒 TPM,天然满足约束。—— **不变量:桶容量必须 ≥ 一个 tick 的补给。**

---

## 5. 发现 C:storage —— 热路径内存,落盘可换

**两处别混(Claude 先前混了,已收回):**

| | agent2 热路径 World | tars-storage 落盘 event-log |
|---|---|---|
| 载体 | 内存 `BTreeMap<CompId, Box<dyn Component>>`(`tars-agent2/src/world.rs:54`) | sqlite 单连接 `Arc<Mutex<Connection>>`(`tars-storage/src/sqlite.rs:46`) |
| 并发 | 每 `anneal` 独占 `&mut World`,**无锁**;版本是 MVCC CAS token | 读写全串行(单 mutex);WAL 开着但单连接吃不到并发读 |
| 何时用 | 每一步 | 只在需要 durable persist(#42)时 |

- **今天 agent2 anneal 是纯内存、易失的**,压根不碰 sqlite。所以"落盘墙"是 **#42(durable suspend/resume)** 的事,不是当前瓶颈。
- **sqlite 的单写墙可换:** 落盘在 trait 后面(`AgentEventLog` `agent_event_log.rs:48` / `DurableStore` `durable_store.rs:379` / `BlackboardStore`)。sqlite 的 `Arc<Mutex<Connection>>` 是**那个 impl 的选择**(进程内 gap-free `sequence_no`),不是契约——换 redis/postgres(服务端并发 + 连接池)墙就没了;要留 sqlite 也可以按 `trajectory_id` 分连接(正确性只要 per-trajectory 有序)+ 读连接池吃 WAL 并发读。
- **memstore(`InMemoryBlackboard`)** 是一把 `Mutex<State>`,临界区纳秒级,当队列用,10k 并发不是瓶颈。

---

## 6. 反转记录(faithful — 别抹掉)

1. **"agent 并发被 provider rate-limit 卡在个位到百级,别写 fiber scheduler。"**(Claude)
   → **收回。** host 侧是 c10k I/O-bound,实测挂 5 万并发。正确形态:**并发容量靠 tokio 的 M:N async(c10k 白送),不用手写 scheduler;ZIO fiber 值钱的是上面那层监督/取消/finalizer**(#40),不是调度器。provider 速率是另一张外部表(#41 背压),不是限制 host 并发的理由。
2. **"10k 并发全funnel到 sqlite 单锁 → sqlite 是扩展墙。"**(Claude)
   → **收回(部分)。** agent2 热路径是内存 MVCC World,不碰 sqlite。sqlite 单写墙只在 #42 落盘时才相关,且可换库。

---

## 7. 结论 × 出口

- **machinery 不是瓶颈**(实测 host 扛 5 万并发);**真瓶颈是外部 provider 速率**——现在有 `TpmRateLimiter` 背压住了。
- **闸的全景:** 并发用 Semaphore(executor,已有)· 速率用 `TpmRateLimiter`(新建)· 成本用 `budget`/`tenant_budget`(已有)· 429 用 `retry`(已有,被动)。
- **待定/下一步:**
  - `TpmRateLimiter` 目前是**独立 layer**(`builder.layer(...)`),没塞进 `default_chain`——因为限速是**部署策略**,该由调用方按 provider 账户配。要不要进 `ChainOpts` 是策略决定,留给 Leo。
  - 生产化补口:cost 估值现在是 `chars/4`(house heuristic),真 tokenizer 是另一条线;refiller 的 thundering-herd 在极高并发下可换 per-key 分片桶。
  - #40(结构化并发 + 取消级联)仍卡在 P0 runtime 收敛(old Flow vs anneal),Leo owns。

**可复跑:** `cargo test -p tars-pipeline --test concurrency_bench -- --nocapture`
