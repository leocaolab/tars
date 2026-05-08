# 文档 13 — Operational Runbook

> 范围：on-call SRE 应急手册 + 常见故障 playbook + 例行运维操作 + 复盘模板。
>
> **状态**：本文档先于实现完成。实施阶段会有具体命令 / kubectl 操作 / dashboard URL 替换占位符 (`<TBD>` 标记)。但**决策树和流程**应保持稳定。
>
> 范围限制：仅适用于 Team / SaaS / Hybrid 部署 (有 SRE 团队介入)。Personal 模式由用户自负。

---

## 1. 设计目标

| 目标 | 说明 |
|---|---|
| **凌晨三点也能用** | Playbook 假设 on-call 半睡半醒,前三步必须无脑可执行 |
| **决策优先于操作** | 先判断"是否真的故障 / 严重程度 / 影响范围",再动手 |
| **Containment > Eradication > Recovery** | 先止血再修复,优先恢复服务而非追究根因 |
| **不破坏 audit** | 所有应急操作都有审计记录,即使是紧急权限使用 |
| **可逆操作优先** | 能回滚的方案优于一次性硬动作 |
| **沟通透明** | 用户感知的事故必须主动公告,不要被动响应 |
| **复盘强制** | P0/P1 必须有 post-mortem,P2 按需 |

**反目标**：
- 不让 runbook 变成"决策树的迷宫"——超过 5 层嵌套的逻辑应该重新设计
- 不在 runbook 写死 dashboard URL / 命令——用占位符 + 链接到 wiki
- 不依赖某个特定 SRE 的"经验"——所有知识必须文档化

---

## 2. 严重等级定义

| 等级 | 定义 | 响应时间 | 通知 |
|---|---|---|---|
| **P0 (Critical)** | 多租户服务全停 / 数据泄漏 / 数据损坏 / 严重安全事件 | 5 min ack, 15 min mitigation | 整个团队 + 产品 + 法务 (如安全) |
| **P1 (High)** | 单租户全停 / 主要功能故障 / 显著性能下降 / SLO 击穿 | 15 min ack, 1h mitigation | On-call + 主管 |
| **P2 (Medium)** | 边缘功能故障 / 单用户问题 / 非关键告警 | 1h ack, 工作日内修复 | On-call + ticket |
| **P3 (Low)** | 监控异常但无用户影响 / 文档错误 / 优化建议 | 工作日 ack | Ticket |

### 2.1 自动升级规则

- P2 累积 5 个相关告警 → 自动升 P1
- P1 持续 4 小时未 mitigation → 自动升 P0
- 安全相关任何告警先按 P1 处理,确认严重性后调整

### 2.2 严重性快速判断 cheatsheet

```
Q1: 多个租户受影响?            → 是 → 至少 P1
Q2: 是否数据丢失/损坏/泄漏?    → 是 → P0
Q3: 用户能正常完成核心工作流?  → 否 → 至少 P1
Q4: SLO 击穿?                  → 是 → 至少 P1
Q5: 安全相关?                  → 是 → P1 起步,可能 P0
其他                            → P2
```

---

## 3. On-call 职责

### 3.1 当班职责

- **首要**：保持服务可用,止血优先
- **响应**：所有告警按 SLA 响应
- **沟通**：维护 incident channel,定期更新状态
- **记录**：实时记录处置过程,事后整理为 post-mortem
- **不做**：复杂代码修改 / 长期架构改动 (留给业务时间)

### 3.2 交接

每天/每班次交接，必须覆盖：
- 当前 active incident 状态
- pending 调查的告警
- 计划内维护
- 异常但已观察的指标 ("CPU 平均比平时高 10%")

交接通过结构化模板 (incident channel pinned message):

```
=== On-call Handoff <date> ===
Incoming: <name>
Outgoing: <name>

Active incidents: (链接 ticket)
- INC-2026-1234 P1 mitigated, RCA pending
- INC-2026-1235 P2 monitoring

Pending investigations:
- 租户 acme_corp cache hit rate 异常下降
- Provider claude_api 偶尔 timeout

Watch list:
- Postgres connection pool 接近上限,可能下午需要扩
- 计划晚间 19:00 部署 v1.5.2

Notes: (任何 outgoing 想交代的)
```

### 3.3 Escalation paths

```
Tier 1: Primary on-call SRE
  ↓ 15 min 无响应或无法 mitigation
Tier 2: Secondary on-call + 工程团队 lead
  ↓ 30 min 无响应或事件升级 P0
Tier 3: 工程总监 + CTO (P0 only)
  ↓ 60 min 仍无解决方案
Tier 4: 厂商 support (Anthropic/OpenAI/Google) + 客户成功
```

每 Tier 升级必须 **同时** 通知,不串行等待。

---

## 4. 通用响应流程

任何 incident 的标准动作：

### Step 1: Triage (5 min)

- [ ] 确认告警真实性 (不是 false positive)
- [ ] 评估影响范围：哪些租户 / 多少用户 / 哪些功能
- [ ] 评级 P0-P3
- [ ] 创建 incident ticket,获取 INC-ID
- [ ] 开 incident channel `#incident-<ID>` (P0/P1)
- [ ] Page 必要的 escalation tier

### Step 2: Communicate (5 min)

- [ ] 在 incident channel 发起始消息：
  ```
  🚨 INCIDENT-1234 [P1] declared
  Summary: <一句话>
  Impact: <谁受影响>
  Lead: <on-call name>
  Status page: updating...
  ```
- [ ] 更新 status page (面向客户)
- [ ] 初始通知客户成功 / 销售 (如果客户可见)

### Step 3: Mitigate (优先于 Eradicate)

- [ ] 找到能立即止血的动作 (即使是粗暴的)：
  - 切流量
  - 重启服务
  - 回滚部署
  - 暂停受影响 tenant
  - 启用降级模式
- [ ] 验证 mitigation 有效 (用户感知 + 监控指标)
- [ ] 更新 incident channel: "MITIGATED at <time>"

### Step 4: Investigate (mitigation 后)

- [ ] 收集证据 (logs / metrics / traces) 在 ticket 附件
- [ ] 找根本原因
- [ ] 评估是否需要更彻底的修复

### Step 5: Resolve & Close

- [ ] 永久修复部署 (可能是后续工作)
- [ ] 验证恢复完整
- [ ] 更新 status page: resolved
- [ ] 通知客户
- [ ] 安排 post-mortem (P0/P1 必须)

### Step 6: Post-mortem (24-72h 内)

模板见 §15。

---

## 5. 常见故障 Playbook

### 5.1 LLM Provider 完全宕机

**症状**：
- `llm.provider.errors_total{provider=X}` 飙升
- Circuit breaker for provider X 处于 open 状态
- 所有 tenant 涉及该 provider 的请求失败

**Triage**:
1. 看 provider 自己的 status page (`status.openai.com` / `status.anthropic.com` / `status.cloud.google.com`)
2. 确认是 provider 侧问题还是我们的网络问题:
   - 在 K8s pod 里 `curl https://api.<provider>.com/v1/models -H "Authorization: ..."`
   - 如果我们的请求能通,但 SDK 失败 → 我们的 bug
   - 如果我们的请求也通不,但其他外部测试能通 → 我们的网络问题
   - 如果其他人测试也通不 → provider outage

**Mitigation**:
- ✅ Circuit breaker 自动失败转移到 fallback provider (Doc 02 §4.7)
- ✅ Routing policy 应已切到备用 provider (Doc 02 §4.6)
- 监控 fallback provider 的负载是否能承担——如果不能,启用降级 (短回复 / 跳过非关键 step)

**手动干预**（如自动 fallback 不工作）:

```bash
# 1. 强制 disable 故障 provider (TBD: 实际命令)
tars admin provider disable --id <provider_id> --reason "outage" --duration 1h

# 2. 验证流量已切走
# 看 metric: llm.provider.request_total{provider=<id>} 应该归零

# 3. 通知所有受影响 tenant (如果业务需要)
```

**恢复**:
- Provider 恢复后,enable + 监控 5 min 确认稳定
- 如果有积压请求,batch 处理避免再次过载

**预防**:
- 至少 2 个 provider 配置可作 fallback (每个 tier)
- Routing policy 包含 `LatencyPolicy` 不是死配置

### 5.2 LLM Provider 限流 (rate limited)

**症状**：
- `llm.provider.errors_total{kind="rate_limited"}` 上升
- Headers 中带 `Retry-After`
- 多租户共享同一 provider 配额时尤其明显

**Triage**:
- 是单个 tenant 暴增导致挤爆 → 见 §5.6
- 是整体 QPS 突破 provider 总配额 → 真实容量不足

**Mitigation**:
1. **短期**：retry middleware 自动按 retry_after 退避
2. **中期**：申请 provider 配额提升
3. **长期**：多账号 + Routing 分散

**强制单租户限流** (如某 tenant 滥用):
```bash
tars admin tenant rate-limit --id <tenant> --tpm 1000 --duration 4h
```

### 5.3 单租户成本失控 (Budget Runaway)

**症状**：
- `budget.soft_limit_exceeded_total{tenant=X}` 告警
- 该 tenant 的成本曲线斜率异常陡峭
- 可能伴随 `agent.backtrack_total{tenant=X}` 异常 (任务卡循环)

**Triage**:
1. 是正常增长还是异常? 看历史 baseline
2. 是 agent 自循环 (Doc 04 §6.4) 还是真实业务?
3. 是 attack 还是 misconfiguration?

**Mitigation** (按严重性递进):

```
Soft limit (告警) → 通知 tenant 管理员 (邮件/IM),不阻塞业务

Soft limit + 异常增速 (5x baseline) → 自动降级
  - Routing 切到便宜 model tier
  - 提示 tenant 检查

Hard limit 即将触发 → 主动暂停非关键 task
  tars admin tenant tasks suspend --id <tenant> --filter "priority=low"

Hard limit 触发 → 全租户 budget exceeded
  - 自动进入 read-only 模式
  - 通知 tenant 立即沟通
```

**误判恢复**:
```bash
# 临时调高 budget (需要审批)
tars admin tenant budget set --id <tenant> --daily 1000 --justification "INC-1234 emergency" --approver <name>
```

### 5.4 Cache Redis 故障

**症状**：
- `cache.lookup_errors_total` 飙升
- LLM 请求量增加 (因为 cache miss)
- 成本突然增加

**Triage**:
- Redis ping 是否成功? `redis-cli -h <host> ping`
- 是 master 还是 replica 问题?
- 网络问题还是 Redis 自身问题?

**Mitigation**:

我们的设计 (Doc 03 §4.3) 让 cache 错误不阻塞业务:
- L1 (内存) 仍工作
- L2 (Redis) miss 后 fallback 到 Provider
- 业务能继续,但 **成本会显著增加**

```
立即:
- ✅ 业务自动降级 (cache 全 miss),无需手动操作

短期:
- 触发 budget 告警阈值 (因为成本上升),需要短期调高 (避免误熔断)

中期:
- Redis failover (master 故障) → 等待 sentinel 切换或手动:
  redis-cli -h <sentinel> SENTINEL FAILOVER tars-master
```

**警告**：Redis 长时间宕机 (> 1h) → cost burn rate 可能 10x → 主动通知用户 + 监控 budget

**恢复**:
- Redis 恢复后,新请求自动开始用 cache
- 不需要 "warm up"——cache 会随业务自然填充

### 5.5 Postgres 主库故障

**症状**：
- `db.query_errors_total` 飙升
- 写入 100% 失败
- Read 可能在 replica 上仍工作

**Triage**:
- pg_isready 检查
- 监控 replica lag
- 是否是 master crashed,replica 已自动 promote?

**Mitigation**:

**A. RDS / 云托管**:
- 自动 failover 通常 30-120s
- 期间应用层会拒绝写入 (合理) 或排队 (危险,可能 OOM)
- 我们的应用应实现 backpressure (Doc 11 §5.2),拒绝而非排队

**B. 自管 Postgres + Patroni**:
```bash
# 检查集群状态
patronictl -c /etc/patroni.yml list

# 强制 failover (如果自动不触发)
patronictl -c /etc/patroni.yml failover --master <current-master> --candidate <replica>
```

**C. 极端情况:从备份恢复**:
- 见 §10 备份恢复演练章节
- 数据丢失窗口 = 最近备份时间 (RPO 1h)

**关键不变量**:
- 永远不能手动改 Postgres 数据 (即使 emergency)
  - 例外：紧急权限走 §11.2 流程
- 永远不能跳过 audit log 写入

### 5.6 单租户暴增 (Hot Tenant)

**症状**：
- 单 tenant 的 task 提交速率 100x 平时
- 占用大部分资源,可能影响其他 tenant
- 可能是合法 CI 突发,也可能是 attack

**Triage**:
- 看 tenant 的请求来源 IP 分布——单 IP 暴增 → 可能 attack
- 看请求 spec——重复同样 task → 可能是 bug
- 联系 tenant 管理员确认

**Mitigation**:

```bash
# 1. 立即限流该 tenant (我们设计有 per-tenant BoundedExecutor)
tars admin tenant throttle --id <tenant> --max-concurrent 5 --duration 1h

# 2. 如果是 attack,suspend
tars admin tenant suspend --id <tenant> --reason "abnormal_traffic_pattern"

# 3. 通知 tenant
```

**post-mitigation**:
- 调查是否真 attack,如是→走 security incident (§5.10)
- 如果合法但超容量,谈话调整 quota 或扩容

### 5.7 Subprocess Pool 耗尽 (CLI / MCP)

**症状**：
- `tool.subprocess_spawn_failures_total` 上升
- 报错: `Too many open files` 或 `Cannot allocate memory`
- 新 task submit 失败

**Triage**:
- `lsof -p <pid>` 看实际句柄数
- `ps -ef | grep claude\|mcp-` 看子进程数
- 是泄漏 (子进程不退) 还是真的负载高?

**Mitigation**:

```bash
# 1. 立即清理 idle subprocess (我们的 janitor 应每 5 min 跑,可能卡住了)
tars admin subprocess prune --idle-secs 60

# 2. 强制 kill stuck 子进程
tars admin subprocess kill --status stuck

# 3. 临时限制新 subprocess 创建
tars admin subprocess limit --max 50 --duration 1h
```

**根因调查**:
- 是某个 MCP server 实现 buggy (不响应 SIGTERM)?
- 是用户 session 真的需要这么多?
- ulimit 配置是否合理?

### 5.8 Trajectory 卡死 (Stuck Trajectory)

**症状**：
- 单个 trajectory 跑 > 30 min 仍 active
- 无新事件追加
- 可能阻塞资源 (subprocess / connection)

**Triage**:
```bash
# 看该 trajectory 的最后事件时间
tars admin trajectory inspect --id <traj_id>

# 检查 cancel signal 是否传递
# 看 Doc 02 §5 Cancel 链路
```

**Mitigation**:

```bash
# 1. 软取消 (尝试 graceful)
tars admin trajectory cancel --id <traj_id>

# 2. 等 30s,如仍未 dead,强制 abort
tars admin trajectory abort --id <traj_id> --force

# 3. 如果是 system bug 导致 cancel signal 不传播,重启该 instance
# (会触发 trajectory recovery 流程,Doc 04 §7)
```

**警告**：
- 强制 abort 会跳过 compensation (Doc 04 §6)
- 必须人工评估是否需要补偿 (例如已创建的 cloud resource)
- 写 audit: `EmergencyTrajectoryAbort`

### 5.9 Compensation 失败 (Doc 04 §6.3)

**症状**：
- `compensation.failed_total` 告警
- 严重度: P0 (系统进入不一致状态)
- PagerDuty 立即唤醒

**Triage**:
- 哪个 trajectory? 哪个 compensation 失败?
- 失败原因: 网络? 权限? 业务规则?
- 受影响的 resource 是什么状态?

**Mitigation** (绝对不要忽略):

1. **冻结相关资源**：避免基于不一致状态做新决策
   ```bash
   tars admin tenant freeze --id <tenant> --resources <resource_refs>
   ```

2. **手动 inspect**:
   ```bash
   # 看 compensation 详情
   tars admin compensation get --id <comp_id>
   
   # 看 trajectory 完整事件流
   tars admin trajectory events --id <traj_id>
   ```

3. **手动执行补偿** (如果可能):
   - 例如:删除 compensation 没删掉的 cloud resource
   - 必须 audit 记录每一步

4. **通知**:
   - 受影响的 tenant 管理员
   - 安全 / 合规团队 (数据可能不一致)
   - 工程团队 (修复 bug)

5. **标记 trajectory**:
   ```bash
   tars admin trajectory mark --id <traj_id> --status "manual_recovery_required"
   ```

**Recovery**:
- 写详细 post-mortem
- 修复 bug (compensation 失败的 root cause)
- 评估是否需要修改架构 (例如把某些 ReversibleResource 提升为 Irreversible 限制 commit phase)

### 5.10 Tenant Isolation Breach (P0 安全事件)

**症状**：
- `security.tenant_isolation_breach` audit event
- 这种事件**永远不应该发生**——发生即架构 bug 或攻击

**响应**: 立即 P0,跳过所有正常流程

1. **保留证据**:
   - 不要删 / 改任何相关数据
   - 立即 snapshot 所有相关存储
   ```bash
   tars admin snapshot --tenant <a>,<b> --reason "INC-1234 P0 security"
   ```

2. **隔离**:
   - 立即 suspend 涉事 tenant (双方,即使 victim 也 suspend 防止 attacker 继续读)
   - 切断该 instance 的流量 (其他 instance 继续服务)

3. **通知**:
   - **必须** 通知:CTO + 法务 + 安全 lead
   - **可能必须** 通知:受影响客户 (法律评估)
   - **可能必须** 通知:监管机构 (GDPR 72h 通知期等)

4. **取证**:
   - 完整 audit log dump
   - 相关 trace_id 的所有 events
   - Cache key fingerprint 比对

5. **修复**:
   - 修复后**必须** 三人 review 才能上线
   - 修复后**必须** 加 regression test (Doc 10 §16.2 fuzz)

**绝对不能做**:
- 删除任何 audit 记录 (即使包含 PII)
- 直接 hot fix 上线 (必须 review)
- 单方面通知客户 (必须法务先评估)

### 5.11 数据存储满

**症状**：
- `db.disk_usage_percent` > 90%
- 写入开始 fail
- 备份开始失败

**Triage**:
- 哪个表占空间最多? `SELECT pg_size_pretty(pg_total_relation_size(...))`
- 是 retention policy 没跑还是真的增长太快?
- 是 application bug (写入异常多)?

**Mitigation**:

```bash
# 短期:扩容
tars admin db expand --target 200GB

# 立即清理 (如果 retention 没跑)
tars admin db cleanup --apply-retention-policy

# 紧急释放 (drop 旧 partition)
tars admin db drop-partition --before 2025-12-01
```

### 5.12 部署回滚

**触发条件**:
- 新版本部署后 P0/P1 incident
- 错误率 / 延迟显著恶化
- 关键 SLO 击穿

**Step**:

```bash
# 1. 立即 stop ongoing rollout (如果 canary)
kubectl rollout pause deployment/tars-server

# 2. 回滚到上一版
kubectl rollout undo deployment/tars-server

# 3. 验证回滚成功
kubectl rollout status deployment/tars-server

# 4. 验证 metric 恢复
# 看 dashboard: error_rate / latency / saturation

# 5. 通知 incident channel
```

**Post-rollback**:
- 不要立即重新部署"修复版"——先彻底分析
- 在 staging 完整复现问题再尝试
- 至少 24h 观察期再考虑下次部署

---

## 6. 例行运维操作

### 6.1 租户 Provisioning

详见 Doc 06 §8.1，操作步骤：

```bash
# 1. 创建 tenant (走审批流程)
tars admin tenant create \
  --display-name "Acme Corp" \
  --owner-email admin@acme.com \
  --providers claude_api,gemini_api \
  --quota-tpm 100000 \
  --quota-daily-cost-usd 100

# 2. 输出 tenant_id,记下

# 3. 设置初始 secret (admin 自己操作,不让 tenant 知道)
tars admin secret set \
  --tenant <tenant_id> \
  --key "anthropic_api_key" \
  --from-vault "secret/data/customers/acme/anthropic"

# 4. 验证 health
tars admin tenant health --id <tenant_id>

# 5. 给 tenant admin 发 onboarding 文档
```

### 6.2 租户 Suspend

```bash
# 软 suspend (默认)
tars admin tenant suspend \
  --id <tenant_id> \
  --reason "billing_overdue" \
  --notify-tenant true

# 验证已生效
tars admin tenant status --id <tenant_id>  # 应显示 "suspended"

# 验证 in-flight 请求被排干
tars admin tenant tasks list --id <tenant_id> --status active
# 应该 60s 内逐渐归零
```

### 6.3 租户 Delete (GDPR)

详见 Doc 06 §8.3,严格按 30 天延迟流程。**不允许跳过延迟期**。

### 6.4 配置热加载

```bash
# 1. 准备新配置 (在 git PR 中 review)
git checkout -b config-update-2026-05-02

# 2. 编辑 /etc/tars/config.toml

# 3. PR review + merge

# 4. CI 自动触发 reload (或手动)
tars admin config reload

# 5. 验证生效
tars admin config show --diff-from-previous
```

### 6.5 Secret Rotation

```bash
# 1. 在 Vault 写入新 secret (新 path 或 new version)
vault kv put secret/tars/openai/v2 api_key="sk-new..."

# 2. 更新配置引用
# 编辑 config:
#   auth = { source = "vault", path = "secret/data/tars/openai/v2" }

# 3. 配置 reload
tars admin config reload

# 4. 监控 5-10 min,确认无 auth error

# 5. revoke 旧 secret
vault kv delete secret/tars/openai/v1
```

### 6.6 Provider 配置变更

```bash
# 添加新 provider
tars admin provider add \
  --id "groq_llama3" \
  --type "openai_compat" \
  --base-url "https://api.groq.com/openai/v1" \
  --auth-secret "secret/data/tars/groq"

# 修改 routing policy (谨慎)
# 通过 config reload (不是直接 admin API),保留 git 痕迹
```

---

## 7. 例行健康检查

### 7.1 每日 (自动化)

- [ ] Backup 完整性 (`pg_verify_backup` + S3 checksum)
- [ ] 备份恢复演练 (在 staging,选 1 个 tenant 数据)
- [ ] Cardinality metric 检查 (Doc 08 §5.5)
- [ ] Provider quota 使用率
- [ ] Disk usage trend
- [ ] Compensation 失败累计 (应为 0)
- [ ] 安全告警累计

### 7.2 每周

- [ ] 性能 baseline 对比 (Doc 11 §10)
- [ ] Cost per tenant 异常检测
- [ ] L3 cache 容量趋势
- [ ] 死 trajectory 清理
- [ ] CVE 扫描 (Doc 10 §14.1)
- [ ] On-call handoff review

### 7.3 每月

- [ ] DR 演练 (从备份完整恢复一个 tenant)
- [ ] Audit log 完整性抽查
- [ ] 容量规划 review (Doc 11 §4)
- [ ] Provider 账单对账 (与我们的 billing_events)
- [ ] Tenant quota review (有无人长期超预算需要谈)

### 7.4 每季度

- [ ] 第三方 pentest (Doc 10 §16.3)
- [ ] Red team exercise (Doc 10 §16.4)
- [ ] Postgres VACUUM FULL (低峰期)
- [ ] Long-term metric retention 审查
- [ ] 供应链审查 (cargo audit + SBOM diff)

### 7.5 每年

- [ ] 完整 SOC2 / ISO27001 audit
- [ ] DR 演练 (整个 region 失败模拟)
- [ ] 主签名 key 轮换
- [ ] 团队 incident response 演练 (game day)

---

## 8. 监控 Dashboard 速查

`<TBD: Grafana / Datadog 仪表盘 URL>`

**核心 dashboard**:
- **Service Overview**: 整体可用性 / 错误率 / 延迟 / QPS
- **Per-Provider**: 每个 LLM provider 的健康度
- **Per-Tenant**: 各租户的 spend / usage / SLO
- **Cache Performance**: hit rate / latency / size
- **Cost Tracking**: 实时 spend / projected monthly / cache savings
- **Subprocess Health**: pool size / spawn rate / kill rate
- **Storage**: DB size / Redis usage / S3 growth
- **Security**: auth failures / IAM denies / breach attempts (应为 0)

每个 alert 在 ticket 里**必须** 链接到对应的 dashboard panel。

---

## 9. 通信模板

### 9.1 Status Page 更新

**Investigating**:
```
We are investigating reports of <symptom>. Updates will follow.
Affected services: <list>
Updated: <time>
```

**Identified**:
```
We have identified the issue affecting <service>. <one sentence on cause>.
Mitigation in progress.
ETA to resolution: <estimate>
Updated: <time>
```

**Monitoring**:
```
A fix has been implemented and we are monitoring the results.
Updated: <time>
```

**Resolved**:
```
This incident has been resolved. <summary>
We will publish a post-mortem within 5 business days.
Updated: <time>
```

### 9.2 Customer Notification (高接触客户)

```
Subject: [Action Required / FYI] Service Incident <INC-ID>

Dear <customer>,

This is a notification regarding an incident on <service> from 
<start time> to <end time> (UTC).

Impact:
- <specific to this customer>

Cause:
- <high level, no internal details>

Actions taken:
- <what we did>

Actions you may need to take:
- <if any>

A detailed post-mortem will follow within 5 business days.

Apologies for the inconvenience.
- <oncall lead> on behalf of TARS Operations
```

### 9.3 Internal Incident Channel Updates (每 30 min)

```
[INC-1234] Status update <time>
- Current status: mitigated / investigating / monitoring
- Latest action: <last 30 min>
- Next: <plan>
- Blocker: <if any>
```

---

## 10. 备份与恢复演练

### 10.1 备份清单

```
Postgres: 每小时 incremental + 每日 full → S3 跨区
Redis: RDB 每小时 → S3
S3 ContentRef: cross-region replication 实时
Secrets: Vault 自身备份 (Vault enterprise 内置)
Audit log: 实时 mirror 到 SIEM (Splunk)
```

### 10.2 恢复演练 (季度必做)

**目标**: 验证从备份能恢复到指定时间点。

**步骤**:

```bash
# 1. 在 isolated 环境准备
make staging-clean

# 2. 选择恢复时间点 (例如 7 天前)
RESTORE_TIME="2026-04-25T10:00:00Z"

# 3. 从 S3 拉备份
aws s3 cp s3://tars-prod-backup/postgres/2026-04-25/full.dump ./

# 4. 恢复 Postgres
pg_restore -h staging-db --create --dbname=postgres ./full.dump

# 5. Apply WAL 到指定时间点 (point-in-time recovery)
# (TBD: 详细命令依赖 RDS 设置)

# 6. 恢复 Redis (从 RDB)
redis-cli -h staging-redis SHUTDOWN
cp /backup/redis-2026-04-25.rdb /var/lib/redis/dump.rdb
systemctl start redis

# 7. 启动应用
kubectl apply -f staging/

# 8. 验证
tars admin verify --comprehensive
# 应输出:
#   ✓ DB schema correct
#   ✓ Tenant data accessible
#   ✓ Cache hit rate > 0
#   ✓ Sample task can run end-to-end

# 9. 清理 staging
make staging-clean

# 10. 记录:
#   - 总耗时 (RTO 目标 4h)
#   - 数据丢失窗口 (RPO 目标 1h)
#   - 任何意外问题
```

**Acceptance criteria**:
- RTO < 4h
- RPO < 1h
- 无人工 hack (全自动化)

---

## 11. 紧急权限 (Break-glass)

### 11.1 何时使用

- P0 incident 且常规 admin 不够
- 生产环境直接操作
- 例:手动改 DB / 直接调 provider API / 执行未审批的脚本

### 11.2 流程

详见 Doc 10 §15.3,实操步骤:

```bash
# 1. Request emergency access (双人 approve)
tars admin emergency request \
  --justification "INC-1234: need to manually trigger compensation for traj X" \
  --duration 1h \
  --approver-needed 2

# 2. 等审批 (PagerDuty 通知第二位 admin)

# 3. 拿到临时 token
export TARS_EMERGENCY_TOKEN="..."

# 4. 执行操作 (所有动作自动 audit)
tars admin <whatever-needed>

# 5. token 自动 expire (默认 4h)

# 6. Post-incident: 写紧急权限使用报告 (季度 review)
```

### 11.3 滥用检测

- 紧急权限使用频率监控,某人/某月 > 3 次需要谈话
- 紧急权限+无对应 incident ticket = 严重违规
- 季度 audit committee review 所有使用记录

---

## 12. Vendor / Provider 沟通

### 12.1 Provider Outage

如果 LLM provider 宕机超过 1h,我们应该:

```
1. 在 incident channel @ 客户成功 + 销售
2. 向 provider 开 critical ticket
3. 跟踪 provider 的 status page 更新
4. 评估是否切到 backup provider 长期运行 (cost impact)
5. 准备给客户的 SLA credit (如需)
```

### 12.2 Provider 配额耗尽

```
1. 立即向 provider 申请 quota 临时提升 (大账户通常 < 4h 响应)
2. 启用 routing 偏向其他 provider
3. 评估是否需要长期 quota 提升 (跟商务谈)
```

### 12.3 上游 SDK / Library bug

如果发现 reqwest / sqlx / pyo3 等关键库的 bug:

```
1. 在我们的 codebase 加临时 workaround
2. 向上游 GitHub issue 报告 (附最小复现)
3. 跟踪修复进度
4. 修复后第一时间升级
```

---

## 13. SLA / SLO 影响处理

### 13.1 SLO 击穿决策

```
单次 incident 击穿 SLO:
  → 评估 error budget 剩余
  → 如果 budget > 50% → 继续按计划部署
  → 如果 budget < 50% → 减缓部署节奏,优先稳定性 work
  → 如果 budget < 0 → 部署冻结,所有 work 转向稳定性

连续多次击穿 SLO:
  → 上升到产品讨论
  → 调整 SLO (如果不合理) 或调整产品策略
```

### 13.2 SLA 违约 (面向客户)

如果触发 SLA 条款 (例如 99.5% 月度可用性):

```
1. 计算 credit (按合同条款)
2. 主动通知客户 (不要被动)
3. 写 post-mortem 给客户
4. 客户成功跟进商务影响
```

---

## 14. 例行变更管理

### 14.1 变更分级

| 级别 | 例子 | 审批 |
|---|---|---|
| Standard | Docker image patch / log level 调整 | 自助 |
| Normal | 新 provider 上线 / 新 routing rule | Lead approve |
| Emergency | 修复 P0 的 hot fix | Director approve + post-fact review |
| Significant | 数据库 schema 重大变更 / 架构调整 | CAB review |

### 14.2 变更窗口

- 工作日 (Mon-Thu) 09:00-16:00 (本地时区)
- 严禁周五下午 / 周末 / 节假日前后 (除非 P0)
- 变更前 24h 在 #ops-changes 公告

### 14.3 Canary 部署

```
1. Deploy to 1 canary instance (5% traffic)
2. 观察 30 min: error_rate / latency / cost
3. 任何指标恶化 → 立即 rollback
4. 通过 → 25% → 50% → 100% (每档 30 min 观察)
5. 完成后 24h 观察期
```

---

## 15. Post-mortem 模板

P0/P1 必须在 incident 解决后 5 个工作日内完成。

```markdown
# Post-Mortem: <Incident ID> - <Short Title>

## TL;DR
<2 句话:发生了什么 + 影响>

## Severity & Timeline
- Severity: P0 / P1 / ...
- Detected: <time> by <how>
- Mitigated: <time>
- Resolved: <time>
- Total impact duration: <hh:mm>

## Impact
- Users affected: <count / percentage>
- Tenants affected: <list>
- Data loss: <yes/no/details>
- SLA impact: <calculation>

## Root Cause
<完整解释 - 不只是 "what" 还要 "why" 和 "为什么没被 catch">

## Detection
- 怎么发现的? (alert / customer report / random discovery)
- 改进点: 怎么能更早发现?

## Response
<时间线: t+0 / t+5 / t+15 ...>
<什么决策正确? 什么走弯路了?>

## Resolution
<最终修复方案>

## What Went Well
- ...

## What Went Wrong
- ...

## Lucky Factors
<没有这些幸运因素会更糟的事>

## Action Items
| ID | Description | Owner | Priority | Due |
|----|-------------|-------|----------|-----|
| 1  | Add metric for X | Alice | P1 | 2026-05-15 |
| 2  | Refactor compensation handler | Bob | P2 | 2026-06-01 |

## Lessons Learned
<给未来的人看的智慧总结>

## Appendix
- Incident channel link
- Relevant traces / dashboards
- Customer communications sent
```

**复盘文化原则** (Blameless):
- 关注**系统**和**流程**,不是个人
- 不写 "X 应该更小心"——而是 "我们的工具应该让 X 不可能犯错"
- 鼓励诚实,即使是自己的错误
- Action items 必须有 owner + due date + 跟踪到完成

---

## 16. 反模式清单

1. **不要在 incident 中"安静地处理"**——必须 open channel,即使你以为很快能解决。
2. **不要跳过 mitigation 直接做 root cause analysis**——先止血。
3. **不要在 P0 期间做大改动**——只做最小止血,大修复事后做。
4. **不要忽略 post-mortem action items**——每周 review,长期未做要 escalate。
5. **不要让一个人独自承担 P0**——开放 channel,其他人能 lurk 帮忙。
6. **不要用紧急权限做"普通"事情**——它是 break-glass 不是日常工具。
7. **不要在没演练过的情况下信任备份**——必须季度演练。
8. **不要在 incident 期间隐瞒信息给客户**——透明 + 真诚比"完美故事"重要。
9. **不要用错误的严重等级**——宁可 over-classify 然后降级,也不要 under-classify 漏响应。
10. **不要修改 audit log 即使在 emergency**——append-only 的承诺不能破。
11. **不要在 staging 验证不充分就推 production**——任何 hot fix 也要至少 smoke test。
12. **不要让 on-call 一个人决定大变更**——单点失败,需要至少二人 approve。
13. **不要忽略"小"告警的累积**——P3 告警频繁可能预示 P1 即将发生。
14. **不要在 post-mortem 指责个人**——blameless 文化是组织韧性的基础。
15. **不要让 runbook 过期**——每季度 review,outdated 的 playbook 比没有更危险。

---

## 17. 与上下游的契约

### 上游 (业务 / 客户) 承诺

- 在 SLA 范围内提供服务 (可用性 / 延迟 / 数据完整性)
- Incident 透明沟通
- Post-mortem 适当脱敏后分享
- SLA 违约时按合同提供 credit

### 下游 (Provider / 基础设施) 依赖

- LLM Provider: 公布 status page + 按 SLA 履约
- Cloud Provider (AWS/GCP): 按 SLA 履约,DR 能力
- Vault: 高可用,99.99%+
- SIEM: 事件实时摄入

### 团队内部

- 工程团队: bug fix SLA,新功能完整 oncall handoff
- 产品团队: 听取 SLO 数据指导优先级
- 销售团队: SLA 谈判与 SRE 沟通能力规划
- 法务团队: 合规事件第一时间介入

---

## 18. 待办与开放问题

- [ ] 实际命令填充 (`tars admin ...` 都是占位符,实施后替换)
- [ ] Dashboard URL 填充 (Grafana 部署后)
- [ ] PagerDuty / OpsGenie 集成配置
- [ ] Status page 选型 (statuspage.io / 自建)
- [ ] Game day 演练剧本 (季度)
- [ ] Chaos engineering 集成 (Litmus / Chaos Mesh)
- [ ] AI 辅助的 incident triage (LLM 看 alert + log 给假设)
- [ ] Auto-remediation playbook (哪些可以无人工自动修)
- [ ] Customer-visible incident metrics (status.tars.example.com)
- [ ] 多语言客户沟通模板 (英 / 中 / 日 / 法 等)
- [ ] On-call 培训材料与认证流程
