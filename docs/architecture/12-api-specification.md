# 文档 12 — API Specification (Rust / HTTP / gRPC / Python / TypeScript / WASM)

> 范围：定义所有面向消费方的 API 表面——in-process Rust trait、HTTP REST + SSE、gRPC、Python FFI binding、TypeScript binding、可选 WASM 编译。
>
> 上下文：所有 API 都是同一个 Doc 04 `Runtime` trait 的不同投影。Rust 是事实源，其他绑定通过工具链生成或手动维护薄壳。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **单一事实源** | Rust trait 是真理,所有其他 API 由它投影。schema / proto / pyi / d.ts 都是生成的或对齐的 |
| **协议成本可选** | 进程内嵌入用 FFI (PyO3/napi-rs);分布式用 HTTP/gRPC。客户决定 |
| **流式必备** | 所有支持长任务的 API 都必须暴露事件流,不只是 request-response |
| **类型安全** | 强类型语言 (Rust/TS) 必须有完整类型;Python 通过 .pyi stubs |
| **版本兼容** | API 版本演进有契约,major bump 之间向后兼容 |
| **错误统一** | 错误模型在所有语言中语义对等,不只是 string 翻译 |
| **认证统一** | Token / OIDC / mTLS 在所有协议下表现一致 |
| **可发现可测试** | OpenAPI / protobuf / .pyi / d.ts 自动生成,接口可被工具链消费 |

**反目标**：
- 不为了"覆盖所有语言"做无人维护的绑定——优先级看实际需求
- 不让某个语言的 binding 偏离核心语义——所有 binding 经过同一组 conformance 测试
- 不在 binding 层做业务逻辑——binding 是薄壳,逻辑在 Rust 核心
- 不暴露 Rust 内部数据结构（如 `Arc<Trajectory>`）给外部——通过 ID + 查询暴露

---

## 2. API 表面总览

```
┌──────────────────────────────────────────────────────────────┐
│  Rust Core (Doc 04 Runtime trait)                            │
│  ── 真理之源 ──                                              │
└──────────────────────────────────────────────────────────────┘
              │
   ┌──────────┼──────────────────────────────────────┐
   │          │                                      │
   ▼          ▼                                      ▼
┌────────┐ ┌────────┐  ┌────────────┐  ┌─────────┐ ┌────────────┐
│ Native │ │ HTTP + │  │ gRPC       │  │ CLI     │ │ FFI bindings│
│ Rust   │ │ SSE    │  │ (internal) │  │ (Doc 07)│ │  - Python   │
│ (in-   │ │ (REST) │  │            │  │         │ │  - TypeScript│
│ process│ │        │  │            │  │         │ │  - (WASM)   │
└────────┘ └────────┘  └────────────┘  └─────────┘ └────────────┘
   │          │              │             │            │
   ▼          ▼              ▼             ▼            ▼
Embedding   Web /         微服务         Terminal     Notebook /
in Rust     非Rust       后端 / SaaS    用户         脚本 / 自动化
  app       客户端        内部
```

### 2.1 选哪个?

| 场景 | 推荐 API |
|---|---|
| 用 Rust 写新应用,直接嵌入 Runtime | Native Rust |
| Web 前端 / 任何语言客户端 | HTTP REST + SSE |
| Rust 微服务之间高吞吐 | gRPC |
| 终端用户 (开发者) 直接用 | CLI (Doc 07) |
| Python 数据科学 / ML 团队 / 自动化脚本 | PyO3 binding (高性能) 或 HTTP (简单) |
| Node.js / TypeScript 后端集成 | napi-rs binding 或 HTTP |
| 浏览器内运行 (零安装) | WASM (受限子集) |
| Cloudflare Workers / 边缘函数 | WASM |

---

## 3. Native Rust API

这是其他所有 API 的**契约源头**。详见 Doc 04 §12，本节简述供参考：

```rust
#[async_trait]
pub trait Runtime: Send + Sync {
    async fn submit(&self, spec: TaskSpec, principal: Principal) 
        -> Result<TaskHandle, RuntimeError>;
    
    fn subscribe(&self, task: TaskId) 
        -> BoxStream<'static, TrajectoryEvent>;
    
    async fn cancel(&self, task: TaskId) -> Result<(), RuntimeError>;
    async fn suspend(&self, task: TaskId) -> Result<(), RuntimeError>;
    async fn resume(&self, task: TaskId, trigger: ResumeTrigger) -> Result<(), RuntimeError>;
    
    async fn query(&self, filter: TaskFilter) 
        -> Result<Vec<TaskSnapshot>, RuntimeError>;
}
```

**集成示例**（Rust 应用嵌入）：

```rust
use tars_runtime::{Runtime, RuntimeConfig, TaskSpec};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = RuntimeConfig::from_file("./config.toml")?;
    let runtime: Arc<dyn Runtime> = tars_runtime::build(config).await?;
    
    let task = runtime.submit(
        TaskSpec::skill("code-review")
            .arg("repo", "github.com/me/myproject")
            .arg("pr", 42),
        Principal::current_user()?,
    ).await?;
    
    let mut events = runtime.subscribe(task.id);
    while let Some(event) = events.next().await {
        println!("{:?}", event);
    }
    
    Ok(())
}
```

### 3.1 Crate 拆分

```
tars-types        — 公共类型 (TaskSpec / Principal / TrajectoryEvent / errors)
tars-provider     — Doc 01 trait + impls
tars-pipeline     — Doc 02
tars-cache        — Doc 03
tars-runtime      — Doc 04 (主入口)
tars-tools        — Doc 05
tars-config       — Doc 06
tars-frontend     — Doc 07 (CLI / TUI / Web adapter)
tars-melt         — Doc 08
tars-storage      — Doc 09
tars-security     — Doc 10
tars-server       — HTTP + gRPC server (依赖 axum + tonic)
tars-py           — PyO3 binding
tars-node         — napi-rs binding
tars-wasm         — WASM 子集 (可选 feature)
```

下游 Rust 应用通常只依赖 `tars-runtime`（自动拉入 types + provider + pipeline + cache）。

---

## 4. HTTP REST + SSE

### 4.1 为什么是 REST + SSE 而不是纯 gRPC

- **客户端门槛低**——任何语言都有 HTTP client
- **浏览器友好**——SSE 原生支持流式
- **调试友好**——curl / Postman / browser devtools
- **代价**：序列化效率比 gRPC 低 ~30%——但 LLM 调用本身几秒，HTTP 那 1ms 不重要

### 4.2 OpenAPI 规范

完整 spec 在 `api/openapi.yaml`，由 Rust 代码 derive 生成（用 `utoipa`）：

```rust
#[derive(OpenApi)]
#[openapi(
    paths(submit_task, get_task, cancel_task, stream_events, list_tasks),
    components(schemas(TaskSpec, TaskHandle, TaskSnapshot, TrajectoryEvent, ApiError)),
    tags((name = "tasks", description = "Task lifecycle operations"))
)]
pub struct ApiDoc;
```

### 4.3 Endpoint 总览

```
POST   /api/v1/tasks                    创建 task
GET    /api/v1/tasks                    列出 task
GET    /api/v1/tasks/:id                查询单个 task
DELETE /api/v1/tasks/:id                cancel task
POST   /api/v1/tasks/:id/suspend        暂停
POST   /api/v1/tasks/:id/resume         恢复
GET    /api/v1/tasks/:id/events         SSE 事件流

GET    /api/v1/skills                   列出可用 skill
GET    /api/v1/tools                    列出可用 tool
GET    /api/v1/providers                列出可见 provider

GET    /api/v1/me                       当前 principal 信息
GET    /api/v1/me/quotas                配额状态
GET    /api/v1/me/usage                 计费使用量

GET    /healthz                         liveness
GET    /readyz                          readiness
GET    /metrics                         Prometheus scrape
```

### 4.4 创建 Task

```http
POST /api/v1/tasks HTTP/1.1
Authorization: Bearer eyJhbGc...
Content-Type: application/json

{
  "skill": "code-review",
  "args": {
    "repo": "github.com/me/myproject",
    "pr": 42
  },
  "budget": {
    "max_cost_usd": 1.0,
    "max_duration_secs": 600
  },
  "session_id": "sess-abc123"
}
```

```http
HTTP/1.1 202 Accepted
Content-Type: application/json
Location: /api/v1/tasks/task-xyz789

{
  "task_id": "task-xyz789",
  "status": "pending",
  "created_at": "2026-05-02T10:23:14Z",
  "events_url": "/api/v1/tasks/task-xyz789/events"
}
```

### 4.5 SSE 事件流

```http
GET /api/v1/tasks/task-xyz789/events HTTP/1.1
Authorization: Bearer eyJhbGc...
Accept: text/event-stream
```

```http
HTTP/1.1 200 OK
Content-Type: text/event-stream
Cache-Control: no-cache
X-Accel-Buffering: no

event: task_started
data: {"task_id":"task-xyz789","spec":{...}}

event: agent_invoked
data: {"agent":"orchestrator","input_summary":"plan code-review"}

event: agent_completed
data: {"agent":"orchestrator","output_summary":"3 sub-tasks","usage":{"input":1234,"output":567}}

event: partial_artifact
data: {"artifact":{"kind":"finding","severity":"warning","file":"src/auth.rs:42"}}

event: completed
data: {"final_artifact":{...},"total_usage":{...},"total_cost":0.18}

```

**关键约束**：
- `Content-Type: text/event-stream` 触发浏览器 EventSource 处理
- `X-Accel-Buffering: no` 禁用 nginx 缓冲
- 每个事件 `event:` + `data:` 两行 + 空行
- 心跳事件 `event: ping` 每 15s 一次,防止代理超时断连

### 4.6 错误响应统一

```http
HTTP/1.1 403 Forbidden
Content-Type: application/json

{
  "error": {
    "code": "iam_denied",
    "message": "principal lacks required scope: tools:invoke:github.create_issue",
    "trace_id": "abc-123-def",
    "details": {
      "required_scope": "tools:invoke:github.create_issue",
      "principal_scopes": ["tenant:acme:read", "tools:invoke:fs.read_file"]
    }
  }
}
```

错误码列表：

| HTTP | code | 含义 |
|---|---|---|
| 400 | `invalid_request` | 参数 schema 错 |
| 401 | `unauthenticated` | token 无效 |
| 403 | `iam_denied` | 鉴权失败 |
| 404 | `not_found` | 资源不存在 |
| 409 | `conflict` | 状态冲突 (例如 cancel 已完成的 task) |
| 422 | `unprocessable` | 业务规则违反 |
| 429 | `rate_limited` | 限流,带 Retry-After header |
| 429 | `budget_exceeded` | 预算耗尽 |
| 500 | `internal_error` | 我们的 bug |
| 502 | `provider_error` | 上游 LLM provider 错 |
| 503 | `service_unavailable` | 主动维护或熔断 |

---

## 5. gRPC

### 5.1 何时用 gRPC

- Rust 微服务之间内部通信
- 高吞吐场景 (> 1000 RPS)
- 双向流式 (例如 sub-agent runtime 互相通信,Doc 05 §6.4)

### 5.2 protobuf 定义

`api/proto/runtime.proto`：

```protobuf
syntax = "proto3";
package tars.runtime.v1;

import "google/protobuf/timestamp.proto";
import "google/protobuf/struct.proto";

service Runtime {
  rpc Submit(SubmitRequest) returns (SubmitResponse);
  rpc Subscribe(SubscribeRequest) returns (stream TrajectoryEvent);
  rpc Cancel(CancelRequest) returns (CancelResponse);
  rpc Suspend(SuspendRequest) returns (SuspendResponse);
  rpc Resume(ResumeRequest) returns (ResumeResponse);
  rpc Query(QueryRequest) returns (QueryResponse);
}

message SubmitRequest {
  string skill_or_blueprint = 1;
  google.protobuf.Struct args = 2;
  TaskBudget budget = 3;
  optional string session_id = 4;
  Principal principal = 5;
}

message TrajectoryEvent {
  string task_id = 1;
  google.protobuf.Timestamp timestamp = 2;
  oneof event {
    TaskStarted task_started = 10;
    AgentInvoked agent_invoked = 11;
    AgentCompleted agent_completed = 12;
    PartialArtifact partial_artifact = 13;
    Completed completed = 14;
    Failed failed = 15;
    // ... 详见 Doc 04 §12
  }
}

// 错误细节
message ApiError {
  string code = 1;
  string message = 2;
  string trace_id = 3;
  google.protobuf.Struct details = 4;
}
```

由 `tonic-build` 生成 Rust + 客户端 SDK。

### 5.3 双向流 (Bidi Stream)

Sub-Agent runtime 场景：父 runtime 调子 runtime,中间需要 cancel / suspend 信号双向：

```protobuf
service Runtime {
  rpc StreamingTask(stream TaskCommand) returns (stream TrajectoryEvent);
}

message TaskCommand {
  oneof command {
    SubmitRequest submit = 1;
    string cancel_task_id = 2;
    SuspendRequest suspend = 3;
    ResumeRequest resume = 4;
  }
}
```

### 5.4 mTLS

内部 gRPC 强制 mTLS：

```rust
let server_tls = ServerTlsConfig::new()
    .identity(server_identity)
    .client_ca_root(client_ca);

Server::builder()
    .tls_config(server_tls)?
    .add_service(RuntimeServiceServer::new(impl_))
    .serve(addr)
    .await?;
```

客户端证书的 fingerprint 映射到 Principal（Doc 10 §4.2 `MtlsClientCert`）。

---

## 6. Python Binding (PyO3)

### 6.1 设计选择

两条路可走：

| 方案 | 优势 | 劣势 |
|---|---|---|
| **A. PyO3 直接绑定 Rust** | 进程内,延迟极低;能传 native 类型 | 编译复杂 (per-platform wheels);Rust 升级需要重编 Python 包 |
| **B. HTTP 客户端 + Pydantic 类型** | 完全跨进程,任何 Python 都能用;HTTP 服务可独立升级 | 网络延迟;每次请求序列化 |

**默认推荐 A**（PyO3）用于：嵌入到 Jupyter Notebook、ML pipeline、本地脚本工具
**默认推荐 B** 用于：Python web 应用调用远程 TARS 集群

### 6.2 PyO3 binding 设计

`tars-py` crate：

```rust
// src/lib.rs
use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;

#[pymodule]
fn tars(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Runtime>()?;
    m.add_class::<TaskHandle>()?;
    m.add_class::<TrajectoryEvent>()?;
    m.add_class::<TaskSpec>()?;
    m.add_class::<Principal>()?;
    m.add_function(wrap_pyfunction!(build_runtime, m)?)?;
    Ok(())
}

#[pyclass]
struct Runtime {
    inner: Arc<dyn tars_runtime::Runtime>,
}

#[pymethods]
impl Runtime {
    #[staticmethod]
    fn from_config(py: Python<'_>, path: String) -> PyResult<Bound<'_, PyAny>> {
        future_into_py(py, async move {
            let config = tars_config::Config::from_file(&path)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            let inner = tars_runtime::build(config).await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            Ok(Runtime { inner })
        })
    }
    
    fn submit<'py>(
        &self, 
        py: Python<'py>, 
        spec: TaskSpec, 
        principal: Principal,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let handle = inner.submit(spec.into(), principal.into()).await
                .map_err(map_runtime_err)?;
            Ok(TaskHandle::from(handle))
        })
    }
    
    /// 流式订阅,返回 async iterator
    fn subscribe<'py>(&self, py: Python<'py>, task_id: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let task_id = TaskId::parse(&task_id)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        
        future_into_py(py, async move {
            let stream = inner.subscribe(task_id);
            Ok(EventAsyncIterator::new(stream))
        })
    }
    
    fn cancel<'py>(&self, py: Python<'py>, task_id: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let task_id = TaskId::parse(&task_id)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        
        future_into_py(py, async move {
            inner.cancel(task_id).await.map_err(map_runtime_err)?;
            Ok(())
        })
    }
}

/// 包装 BoxStream 为 Python async iterator
#[pyclass]
struct EventAsyncIterator {
    stream: Arc<Mutex<BoxStream<'static, TrajectoryEvent>>>,
}

#[pymethods]
impl EventAsyncIterator {
    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> { slf }
    
    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let stream = self.stream.clone();
        future_into_py(py, async move {
            let mut stream = stream.lock().await;
            match stream.next().await {
                Some(event) => Ok(TrajectoryEvent::from(event)),
                None => Err(PyStopAsyncIteration::new_err("stream ended")),
            }
        })
    }
}
```

### 6.3 Python 用法

```python
import asyncio
from tars import Runtime, TaskSpec, Principal

async def main():
    runtime = await Runtime.from_config("./config.toml")
    
    spec = TaskSpec.skill("code-review", repo="github.com/me/proj", pr=42)
    principal = Principal.current_user()
    
    handle = await runtime.submit(spec, principal)
    print(f"Task: {handle.task_id}")
    
    async for event in runtime.subscribe(handle.task_id):
        print(event)
        if event.is_terminal():
            break

asyncio.run(main())
```

### 6.4 Type stubs (.pyi)

由 `pyo3-stub-gen` 自动生成 `tars.pyi`：

```python
# tars.pyi (snippet)
from typing import AsyncIterator, Optional, Dict, Any

class Runtime:
    @staticmethod
    async def from_config(path: str) -> "Runtime": ...
    async def submit(self, spec: TaskSpec, principal: Principal) -> TaskHandle: ...
    async def subscribe(self, task_id: str) -> AsyncIterator["TrajectoryEvent"]: ...
    async def cancel(self, task_id: str) -> None: ...

class TaskSpec:
    @staticmethod
    def skill(name: str, **kwargs: Any) -> "TaskSpec": ...
    @staticmethod
    def blueprint(blueprint_id: str, **kwargs: Any) -> "TaskSpec": ...

class TrajectoryEvent:
    timestamp: int
    task_id: str
    kind: str
    payload: Dict[str, Any]
    def is_terminal(self) -> bool: ...
```

通过 stubs 让 IDE 补全 + mypy 类型检查工作。

### 6.5 打包与分发

```
build:
  - maturin build --release
  - 生成 wheels: tars-{ver}-{python}-{platform}.whl
  - matrix: linux-x86_64 / linux-aarch64 / macos-x86_64 / macos-arm64 / windows-x86_64
            python 3.10 / 3.11 / 3.12 / 3.13

publish:
  - PyPI: pip install tars
  - 内部 PyPI mirror (企业用户)
```

---

## 7. TypeScript / Node Binding

### 7.1 同样的两条路

| 方案 | 优势 | 劣势 |
|---|---|---|
| **A. napi-rs 直接绑定** | 进程内,Node CLI 工具 / Electron 完美 | 编译复杂 (per-platform .node files) |
| **B. HTTP 客户端 + 类型定义** | 浏览器 + Node 通用 | 网络延迟 |

### 7.2 napi-rs binding (`tars-node`)

```rust
// src/lib.rs
use napi::bindgen_prelude::*;
use napi_derive::napi;

#[napi]
pub struct Runtime {
    inner: Arc<dyn tars_runtime::Runtime>,
}

#[napi]
impl Runtime {
    #[napi(factory)]
    pub async fn from_config(path: String) -> Result<Runtime> {
        let config = tars_config::Config::from_file(&path)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        let inner = tars_runtime::build(config).await
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Runtime { inner })
    }
    
    #[napi]
    pub async fn submit(&self, spec: TaskSpec, principal: Principal) -> Result<TaskHandle> {
        let handle = self.inner.submit(spec.into(), principal.into()).await
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(handle.into())
    }
    
    /// 流式订阅,返回 AsyncIterableIterator
    #[napi]
    pub fn subscribe(&self, task_id: String) -> EventStream {
        let task_id = TaskId::parse(&task_id).unwrap();
        let stream = self.inner.subscribe(task_id);
        EventStream::new(stream)
    }
    
    #[napi]
    pub async fn cancel(&self, task_id: String) -> Result<()> {
        let task_id = TaskId::parse(&task_id)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        self.inner.cancel(task_id).await
            .map_err(|e| Error::from_reason(e.to_string()))
    }
}

#[napi(iterator)]
pub struct EventStream {
    stream: Arc<Mutex<BoxStream<'static, TrajectoryEvent>>>,
}

#[napi]
impl Generator for EventStream {
    type Yield = TrajectoryEvent;
    type Next = ();
    type Return = ();
    
    fn next(&mut self, _: Option<Self::Next>) -> Option<Self::Yield> {
        // napi-rs supports async iterator via different mechanism
        // (实际实现略,用 tokio runtime)
        ...
    }
}
```

### 7.3 TypeScript 用法

```typescript
import { Runtime, TaskSpec, Principal } from '@tars/runtime';

async function main() {
  const runtime = await Runtime.fromConfig('./config.toml');
  
  const spec = TaskSpec.skill('code-review', { repo: 'github.com/me/proj', pr: 42 });
  const principal = Principal.currentUser();
  
  const handle = await runtime.submit(spec, principal);
  console.log(`Task: ${handle.taskId}`);
  
  for await (const event of runtime.subscribe(handle.taskId)) {
    console.log(event);
    if (event.isTerminal()) break;
  }
}

main().catch(console.error);
```

### 7.4 类型定义生成

`@napi-rs/cli` 自动生成 `index.d.ts`：

```typescript
// index.d.ts (snippet)
export class Runtime {
  static fromConfig(path: string): Promise<Runtime>;
  submit(spec: TaskSpec, principal: Principal): Promise<TaskHandle>;
  subscribe(taskId: string): AsyncIterable<TrajectoryEvent>;
  cancel(taskId: string): Promise<void>;
}

export interface TrajectoryEvent {
  taskId: string;
  timestamp: number;
  kind: 'task_started' | 'agent_invoked' | 'agent_completed' | 'partial_artifact' | 'completed' | 'failed';
  payload: Record<string, unknown>;
  isTerminal(): boolean;
}
```

### 7.5 浏览器场景：HTTP 客户端 + 共享类型

`@tars/client-http` 包，纯 TypeScript，无 native binding：

```typescript
import { TarsClient } from '@tars/client-http';

const client = new TarsClient({ 
  baseUrl: 'https://tars.example.com',
  token: 'eyJhbGc...',
});

const handle = await client.submitTask({
  skill: 'code-review',
  args: { repo, pr },
});

// SSE 自动包装为 AsyncIterable
for await (const event of client.streamEvents(handle.taskId)) {
  console.log(event);
}
```

类型定义来自 OpenAPI spec 自动生成（`openapi-typescript`）。

### 7.6 包结构

```
@tars/runtime          — napi-rs binding (Node)
@tars/client-http      — HTTP 客户端 (Node + Browser)
@tars/types            — 共享类型 (从 OpenAPI 生成)
@tars/cli              — 命令行工具 (npm install -g @tars/cli)
```

---

## 8. WASM Binding (可选)

### 8.1 WASM 子集

完整 Runtime 在 WASM 中**跑不起来**——以下功能不可用：
- 子进程 (CLI / MCP) → 没有 syscalls
- Postgres / Redis → 没有 socket
- 文件系统持久化 → 仅 IndexedDB / OPFS

WASM 子集只能做：
- HTTP 客户端调远程 TARS server
- Cache key hash 计算 (本地)
- 类型校验 (schema)
- Token 计数估算

### 8.2 用例

- 浏览器内的 TARS Web Dashboard 客户端逻辑
- Cloudflare Workers / Vercel Edge Functions 做 LLM 路由
- 桌面应用 (Tauri) 直接嵌入

### 8.3 实现

```rust
// tars-wasm/src/lib.rs
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct WasmClient {
    base_url: String,
    token: Option<String>,
}

#[wasm_bindgen]
impl WasmClient {
    #[wasm_bindgen(constructor)]
    pub fn new(base_url: String, token: Option<String>) -> Self {
        Self { base_url, token }
    }
    
    /// 浏览器 fetch API
    pub async fn submit_task(&self, spec_json: String) -> Result<JsValue, JsValue> {
        let url = format!("{}/api/v1/tasks", self.base_url);
        let resp = web_sys::window().unwrap()
            .fetch_with_request(&self.build_request(&url, &spec_json))
            .await?;
        // ... parse response
    }
    
    /// 浏览器 EventSource
    pub fn subscribe(&self, task_id: String, on_event: js_sys::Function) {
        let url = format!("{}/api/v1/tasks/{}/events", self.base_url, task_id);
        let es = web_sys::EventSource::new(&url).unwrap();
        // ... bind on_event callback
    }
}
```

构建：

```bash
wasm-pack build --target web tars-wasm
# 输出 pkg/tars_wasm.js + tars_wasm_bg.wasm
```

---

## 9. CLI API

CLI 是 Doc 07 §5-7 的 Frontend Adapter,本节强调它也是 API 表面：

### 9.1 命令结构

```
tars <subcommand> [args]

tars run <skill_or_blueprint>      在 CI 模式跑
tars chat                          进 TUI
tars dash                          启动 Web Dashboard
tars task list                     列出 task
tars task get <id>                 查询 task
tars task cancel <id>              cancel
tars cache stats                   cache 统计
tars config validate               配置校验
tars config show                   显示当前 effective config
tars tenant list                   (admin) 列租户
tars tenant provision              (admin) 创建租户
```

### 9.2 输出格式

CLI 默认人类可读输出,但**所有命令支持 `--output json`** 让脚本消费：

```bash
$ tars task list --output json
[{"id":"task-xyz","status":"completed",...}]

$ tars task list --output json | jq '.[] | select(.status == "failed")'
```

`--output json` 等价于 HTTP API，用于无法/不愿装 SDK 的脚本场景。

---

## 10. 跨语言契约统一

### 10.1 单一事实源

```
Rust types (src/types.rs)
    │
    ├─ derive(Serialize, Deserialize) → JSON schema
    ├─ derive(ToSchema) → utoipa OpenAPI spec
    ├─ derive(prost::Message) → protobuf (for gRPC)
    ├─ pyo3 binding (manual + macro)
    ├─ napi-rs binding (manual + macro)
    └─ wasm-bindgen (manual)
```

任何字段调整都从 Rust 开始,生成 / 同步到其他语言。

### 10.2 Conformance Test Suite

每个 binding 都跑同一组 `conformance_tests/`：

```
conformance_tests/
  001_submit_and_complete.json     # 提交一个简单 task,验证完整事件序列
  002_cancel_mid_stream.json       # 中途 cancel,验证清理
  003_iam_denied.json              # 无权限场景
  004_budget_exceeded.json         # 预算耗尽
  005_provider_error.json          # 上游 provider 报错
  006_streaming_concurrent.json    # 多并发订阅同 task
  ...
```

每个测试是 declarative spec：

```json
{
  "name": "submit_and_complete",
  "given": {
    "config": "fixtures/config-minimal.toml",
    "mock_provider_responses": [...]
  },
  "when": {
    "actions": [
      { "type": "submit", "spec": {...} },
      { "type": "subscribe", "task_id": "$0.task_id" }
    ]
  },
  "then": {
    "events": [
      { "kind": "task_started" },
      { "kind": "agent_invoked", "agent": "orchestrator" },
      { "kind": "completed" }
    ]
  }
}
```

每个 binding 实现一个 conformance runner,跑所有 test cases。新增 binding 必须通过全套 conformance test 才能 release。

---

## 11. 版本管理

### 11.1 SemVer

- **Major (X.0.0)**：破坏性 API 变更（Rust trait 签名 / HTTP 路径 / proto 字段移除）
- **Minor (1.X.0)**：新功能,向后兼容 (新增 endpoint / 新事件类型 / 新可选字段)
- **Patch (1.0.X)**：bugfix

### 11.2 多版本并存

HTTP / gRPC 通过路径版本化共存：

```
/api/v1/tasks
/api/v2/tasks   ← 新版,可能字段不同
```

老版本 deprecated 后保留 12 个月。

### 11.3 SDK 版本与 server 版本

| 场景 | 兼容性要求 |
|---|---|
| SDK v1.x ↔ Server v1.x | 完全兼容 |
| SDK v1.x ↔ Server v1.y (y > x) | server 向后兼容,SDK 仅用 v1.x 特性 |
| SDK v1.x ↔ Server v2.x | 通过 `/api/v1/` 路径仍可用,但 SDK 拿不到新功能 |
| SDK v2.x ↔ Server v1.x | ❌ 拒绝,提示升级 server |

SDK 启动时调 `/api/version` 检测：

```http
GET /api/version
{
  "server_version": "1.5.2",
  "supported_api_versions": ["v1"],
  "deprecation_notices": []
}
```

不兼容时 SDK 抛清晰错误,不让用户摸黑 debug。

---

## 12. 认证统一

所有 API 接受同一组认证机制：

| 协议 | Auth header / mechanism |
|---|---|
| HTTP / SSE | `Authorization: Bearer <token>` |
| gRPC | metadata `authorization: Bearer <token>` 或 mTLS client cert |
| Python (FFI) | 进程继承 OS user / 显式 Principal::from_token() |
| Node (FFI) | 同上 |
| WASM (浏览器) | HTTP token (browser cookie / localStorage) |
| CLI | `~/.config/tars/credentials` (本地) 或 `--token` flag |

Token 格式统一为 JWT（自签或 OIDC issuer 签）：

```json
{
  "iss": "https://sso.example.com",
  "sub": "user@example.com",
  "tenant_id": "acme-corp",
  "scopes": ["tenant:acme:read", "tools:invoke:fs.*"],
  "exp": 1746180000
}
```

服务端通过 `tars-security` 统一验证。

---

## 13. 错误映射

每个语言用 native idiom 表达同一组逻辑错误：

| 逻辑错误 | Rust | Python | TypeScript | gRPC status | HTTP |
|---|---|---|---|---|---|
| 鉴权失败 | `Err(IamDenied)` | `IamDeniedError` | `IamDeniedError` | `PERMISSION_DENIED (7)` | 403 |
| 资源不存在 | `Err(NotFound)` | `NotFoundError` | `NotFoundError` | `NOT_FOUND (5)` | 404 |
| 预算耗尽 | `Err(BudgetExceeded)` | `BudgetExceededError` | `BudgetExceededError` | `RESOURCE_EXHAUSTED (8)` | 429 |
| 已 cancel | `Err(Cancelled)` | `CancelledError` | `CancelledError` | `CANCELLED (1)` | 499 |
| Provider 错 | `Err(ProviderError)` | `ProviderError` | `ProviderError` | `INTERNAL (13)` | 502 |
| 内部错 | `Err(Internal)` | `RuntimeError` | `Error` | `INTERNAL (13)` | 500 |

错误对象都带：
- `code`: 字符串 enum 值（机器可读）
- `message`: 人类可读
- `trace_id`: 用于 SRE 关联
- `details`: 结构化附加信息

---

## 14. 流式协议

四种协议的流式表现：

| 协议 | 流式机制 | 使用约束 |
|---|---|---|
| Rust | `BoxStream<TrajectoryEvent>` | 调用方持有 stream,Drop 触发 cancel |
| HTTP | SSE (`Content-Type: text/event-stream`) | EventSource 重连自动 / heartbeat 必备 |
| gRPC | server streaming `stream TrajectoryEvent` | 客户端 cancel 通过 close stream |
| Python | `async for event in runtime.subscribe(id)` | asyncio 兼容 |
| TypeScript | `for await (event of stream)` | AsyncIterable 标准 |

跨协议保证：
- **同一事件流的事件 schema 相同**（OpenAPI / proto / pyi / d.ts 同步）
- **terminal 事件标识**：`completed` / `failed` / `cancelled` 是流的最后一个，所有协议都能识别
- **cancel 语义**：调用方关闭 stream → 服务端收到 cancel 信号 → Doc 02 §5 cancel 链触发

---

## 15. 测试矩阵

| 测试类型 | Rust | HTTP | gRPC | Python | TS | WASM | CLI |
|---|---|---|---|---|---|---|---|
| 单元测试 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Conformance | ✅ baseline | ✅ | ✅ | ✅ | ✅ | ✅ subset | ✅ |
| 集成测试 | ✅ | ✅ | ✅ | ✅ | ✅ | - | ✅ |
| Type-check | ✅ rustc | ✅ openapi validator | ✅ proto check | ✅ mypy | ✅ tsc | ✅ tsc | - |
| 端到端 | ✅ | ✅ | ✅ | ✅ | ✅ | - | ✅ |
| 性能 bench | ✅ | ✅ | ✅ | ✅ (PyO3 vs HTTP) | ✅ (napi vs HTTP) | - | - |

CI 矩阵每次 PR 跑：Rust + HTTP + Python + TS conformance。其他在 nightly 跑。

---

## 16. 生成与发布工具链

### 16.1 OpenAPI / proto 生成

```bash
# 从 Rust 代码生成 OpenAPI
cargo run --bin gen-openapi > api/openapi.json

# 从 .proto 生成 Rust + 客户端 SDK
buf generate proto/

# 从 OpenAPI 生成 TS 类型
npx openapi-typescript api/openapi.json --output packages/types/src/api.ts

# 从 Rust 生成 Python stubs
cargo run --bin gen-pyi > tars-py/tars.pyi
```

CI 检查生成的文件已 commit（`git diff --exit-code`）。

### 16.2 Release 流程

```yaml
# .github/workflows/release.yml
on:
  push:
    tags: ['v*']

jobs:
  rust:
    - cargo publish (multi-crate workspace)
  
  python:
    - matrix: linux/macos/windows × py3.10/3.11/3.12/3.13
    - maturin build --release
    - twine upload to PyPI
  
  node:
    - matrix: linux-x64/linux-arm64/macos-x64/macos-arm64/win-x64
    - napi build --release
    - npm publish (@tars/runtime, @tars/types, @tars/client-http)
  
  wasm:
    - wasm-pack build --target web
    - npm publish (@tars/wasm)
  
  binary:
    - matrix: 6 targets (Doc 07 §9.3)
    - cargo build --release
    - sigstore sign
    - upload to GitHub Releases
  
  docker:
    - docker buildx (linux/amd64, linux/arm64)
    - docker push
    - cosign sign
```

---

## 17. 反模式清单

1. **不要让某个 binding 提供"额外"功能**——所有功能都来自 Rust core,binding 是投影。
2. **不要在 Python/TS binding 里写业务逻辑**——逻辑在 Rust,binding 只翻译。
3. **不要假设所有客户端都会升级**——deprecated API 至少保留 12 个月。
4. **不要在 HTTP API 暴露 internal trace_id 之外的内部 ID**——使用稳定的外部 ID。
5. **不要让 WASM 假装能做完整 Runtime**——明确文档化它是 client-only。
6. **不要让 PyO3 / napi-rs binding 跨 binding 共享内存**——每次 FFI 调用复制数据,简单但安全。
7. **不要用全局 lock 保护 FFI 入口**——使用 Arc + 内部细粒度锁。
8. **不要让 HTTP 客户端轮询 task 状态**——必须用 SSE / WebSocket 流式。
9. **不要在 SDK 里硬编码 base_url**——让用户配置。
10. **不要忽略 SDK 的版本协商**——启动时检测 server 版本。
11. **不要在多 binding 里独立维护 schema**——必须从 Rust 单一事实源生成。
12. **不要让 conformance test 在某个 binding 上"豁免"**——全部通过才 release。
13. **不要让 CLI 输出格式在 patch 版本变化**——人类可读输出也算 API,谨慎变更。
14. **不要在 SSE 缺心跳**——长时间无事件时连接被代理 kill,客户端不知道。
15. **不要让 errors 跨语言只翻译 message**——必须有结构化 code,允许程序识别。

---

## 18. 与上下游的契约

### 上游 (客户端 / SDK 用户) 承诺

- 处理所有错误码,不假设"不会发生"
- SSE 客户端正确处理重连
- 遵守 rate limit,触发 429 时退避
- 升级 SDK 至少与 server 同 major version

### 下游 (Rust core trait) 承诺

- 所有公开 API 必须是 `Send + Sync` 且能跨线程使用
- 流式接口的事件类型可序列化 (serde / proto / pyo3)
- 错误类型实现 `Display + Error + Send + Sync + 'static`
- 不直接暴露 `Arc<Mutex<...>>` 这类内部实现细节

### Binding 维护契约

- 任何 Rust core 公开 API 变更必须同时更新所有 binding
- 新增 API 优先在 Rust + HTTP 可用,Python/TS 跟进可以延后 1-2 个 minor 版本
- Binding 维护者拥有该 binding 的最终决策权 (idiomatic 翻译可以差异化)

---

## 19. 待办与开放问题

- [ ] gRPC + GraphQL 是否也需要 (有客户喜欢 GraphQL)
- [ ] WebSocket 是否替代 SSE (双向通信场景)
- [ ] Python async / sync 双 API (有些环境无 asyncio)
- [ ] Node.js 同时支持 ESM + CJS
- [ ] napi-rs 在 Bun / Deno 的兼容性测试
- [ ] WASM 的 WASI 路径 (服务端 WASM 运行 Rust core 的子集?)
- [ ] Java / Kotlin / Go binding 的优先级 (取决于用户)
- [ ] iOS / Android binding 通过 UniFFI 评估
- [ ] CLI 输出格式版本化 (保持 stdout 稳定供脚本消费)
- [ ] OpenAPI spec 的契约测试 (Pact-style consumer-driven)
- [ ] gRPC reflection API 启用与否 (调试便利 vs 攻击面)
- [ ] **Out-of-process Python via UDS subprocess pool**: 当 Python skill 不能 in-process 跑 (fork 需求 / GIL 受限库 / 进程隔离防 crash) 时,补充 §6 PyO3 in-process 路径之外的方案。长连接 worker pool + Unix Domain Socket,比 HTTP/TCP 开销显著低。
