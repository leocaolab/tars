# Doc 13 — Operational Runbook

> Scope: on-call SRE incident handbook + common-failure playbooks + routine ops procedures + post-mortem template.
>
> **Status**: this document is finalized ahead of implementation. During implementation, concrete commands / kubectl operations / dashboard URLs will replace the placeholders (marked `<TBD>`). The **decision trees and procedures**, however, should remain stable.
>
> Scope limit: applies only to Team / SaaS / Hybrid deployments (where an SRE team is involved). Personal mode is the user's own responsibility.

---

## 1. Design goals

| Goal | Description |
|---|---|
| **Usable at 3 a.m.** | Playbooks assume the on-call is half-asleep; the first three steps must be brain-dead executable |
| **Decision before action** | First judge "is this really a failure / how severe / blast radius", then act |
| **Containment > Eradication > Recovery** | Stop the bleeding before fixing; prioritize restoring service over root-cause hunting |
| **Don't break audit** | Every emergency action has an audit record, even emergency-privilege use |
| **Reversible actions first** | Rollback-able solutions trump one-shot hard moves |
| **Transparent communication** | User-visible incidents must be proactively announced, not passively responded to |
| **Post-mortem mandatory** | P0/P1 must have a post-mortem; P2 on-demand |

**Non-goals**:
- Don't let the runbook become a "decision-tree maze" — logic nested deeper than 5 levels should be redesigned
- Don't hard-code dashboard URLs / commands in the runbook — use placeholders + links to the wiki
- Don't depend on any specific SRE's "experience" — all knowledge must be documented

---

## 2. Severity definitions

| Level | Definition | Response time | Notification |
|---|---|---|---|
| **P0 (Critical)** | Multi-tenant service fully down / data leak / data corruption / serious security event | 5 min ack, 15 min mitigation | Whole team + product + legal (if security) |
| **P1 (High)** | Single-tenant fully down / major feature broken / significant performance degradation / SLO breach | 15 min ack, 1h mitigation | On-call + manager |
| **P2 (Medium)** | Edge feature broken / single-user issue / non-critical alert | 1h ack, fix within working day | On-call + ticket |
| **P3 (Low)** | Monitoring anomaly with no user impact / doc errors / optimization suggestions | Working-day ack | Ticket |

### 2.1 Auto-escalation rules

- P2 with 5 related alerts accumulated → auto-escalate to P1
- P1 not mitigated for 4 hours → auto-escalate to P0
- Any security-related alert is handled as P1 first, then adjusted after confirming severity

### 2.2 Severity quick-judgment cheatsheet

```
Q1: Multiple tenants affected?              → Yes → at least P1
Q2: Data loss / corruption / leak?          → Yes → P0
Q3: Can users complete the core workflow?   → No  → at least P1
Q4: SLO breached?                           → Yes → at least P1
Q5: Security related?                       → Yes → P1 starting, possibly P0
Otherwise                                    → P2
```

---

## 3. On-call responsibilities

### 3.1 Shift duties

- **Primary**: keep service available, stop the bleeding first
- **Respond**: respond to all alerts per SLA
- **Communicate**: maintain the incident channel, post regular status updates
- **Record**: record handling steps in real time, organize into post-mortem afterwards
- **Don't do**: complex code changes / long-term architecture work (leave to business hours)

### 3.2 Handoff

For every daily/shift handoff, must cover:
- Current active incident status
- Pending alert investigations
- Planned maintenance
- Anomalous-but-observed metrics ("CPU avg 10% above usual")

Handoff goes through a structured template (incident channel pinned message):

```
=== On-call Handoff <date> ===
Incoming: <name>
Outgoing: <name>

Active incidents: (link to ticket)
- INC-2026-1234 P1 mitigated, RCA pending
- INC-2026-1235 P2 monitoring

Pending investigations:
- Tenant acme_corp cache hit rate dropping abnormally
- Provider claude_api occasional timeout

Watch list:
- Postgres connection pool nearing limit, may need to scale this afternoon
- Planned 19:00 deploy of v1.5.2

Notes: (anything outgoing wants to mention)
```

### 3.3 Escalation paths

```
Tier 1: Primary on-call SRE
  ↓ 15 min no response or unable to mitigate
Tier 2: Secondary on-call + engineering team lead
  ↓ 30 min no response or incident escalates to P0
Tier 3: Engineering director + CTO (P0 only)
  ↓ 60 min still no resolution
Tier 4: Vendor support (Anthropic/OpenAI/Google) + customer success
```

Each tier escalation must notify **simultaneously**, not wait serially.

---

## 4. Generic response procedure

Standard actions for any incident:

### Step 1: Triage (5 min)

- [ ] Confirm the alert is real (not a false positive)
- [ ] Assess blast radius: which tenants / how many users / which features
- [ ] Rate P0-P3
- [ ] Create incident ticket, get INC-ID
- [ ] Open incident channel `#incident-<ID>` (P0/P1)
- [ ] Page necessary escalation tier

### Step 2: Communicate (5 min)

- [ ] Post the kickoff message in the incident channel:
  ```
  🚨 INCIDENT-1234 [P1] declared
  Summary: <one sentence>
  Impact: <who is affected>
  Lead: <on-call name>
  Status page: updating...
  ```
- [ ] Update status page (customer-facing)
- [ ] Initial notification to customer success / sales (if customer-visible)

### Step 3: Mitigate (before Eradicate)

- [ ] Find an action that stops the bleeding immediately (even if crude):
  - Cut traffic
  - Restart service
  - Rollback deployment
  - Suspend affected tenant
  - Enable degraded mode
- [ ] Verify mitigation works (user perception + monitoring metrics)
- [ ] Update incident channel: "MITIGATED at <time>"

### Step 4: Investigate (after mitigation)

- [ ] Collect evidence (logs / metrics / traces) attached to the ticket
- [ ] Find the root cause
- [ ] Assess whether a more thorough fix is needed

### Step 5: Resolve & Close

- [ ] Permanent fix deployed (may be follow-up work)
- [ ] Verify recovery is complete
- [ ] Update status page: resolved
- [ ] Notify customers
- [ ] Schedule post-mortem (mandatory for P0/P1)

### Step 6: Post-mortem (within 24-72h)

Template in §15.

---

## 5. Common-failure playbooks

### 5.1 LLM Provider fully down

**Symptoms**:
- `llm.provider.errors_total{provider=X}` spikes
- Circuit breaker for provider X is open
- All tenant requests involving that provider fail

**Triage**:
1. Check the provider's own status page (`status.openai.com` / `status.anthropic.com` / `status.cloud.google.com`)
2. Confirm whether it's a provider-side issue or our network issue:
   - From a K8s pod run `curl https://api.<provider>.com/v1/models -H "Authorization: ..."`
   - If our request goes through but the SDK fails → our bug
   - If our request also fails but other external tests succeed → our network issue
   - If everyone else's tests also fail → provider outage

**Mitigation**:
- ✅ Circuit breaker auto-fails over to fallback provider (Doc 02 §4.7)
- ✅ Routing policy should already have switched to backup provider (Doc 02 §4.6)
- Watch whether the fallback provider's load is sustainable — if not, enable degradation (shorter replies / skip non-critical steps)

**Manual intervention** (when auto-fallback isn't working):

```bash
# 1. Force-disable the failing provider (TBD: actual command)
tars admin provider disable --id <provider_id> --reason "outage" --duration 1h

# 2. Verify traffic has shifted away
# Check metric: llm.provider.request_total{provider=<id>} should drop to zero

# 3. Notify all affected tenants (if business requires)
```

**Recovery**:
- After provider recovers, enable + monitor for 5 min to confirm stability
- If there's a backlog, batch-process it to avoid re-overloading

**Prevention**:
- At least 2 providers configured as fallbacks (per tier)
- Routing policy includes `LatencyPolicy`, not a static config

### 5.2 LLM Provider rate limited

**Symptoms**:
- `llm.provider.errors_total{kind="rate_limited"}` rising
- `Retry-After` in headers
- Especially obvious when multiple tenants share the same provider quota

**Triage**:
- A single tenant's spike is crowding out others → see §5.6
- Overall QPS is breaching the provider's total quota → real capacity shortfall

**Mitigation**:
1. **Short-term**: retry middleware automatically backs off per retry_after
2. **Mid-term**: request a provider quota increase
3. **Long-term**: multi-account + Routing dispersion

**Force-throttle a single tenant** (if a tenant is abusing):
```bash
tars admin tenant rate-limit --id <tenant> --tpm 1000 --duration 4h
```

### 5.3 Single-tenant cost runaway (Budget Runaway)

**Symptoms**:
- `budget.soft_limit_exceeded_total{tenant=X}` alert
- This tenant's cost curve has an abnormally steep slope
- May coincide with anomalous `agent.backtrack_total{tenant=X}` (task stuck in a loop)

**Triage**:
1. Normal growth or anomalous? Check historical baseline
2. Agent self-loop (Doc 04 §6.4) or genuine business?
3. Attack or misconfiguration?

**Mitigation** (escalating by severity):

```
Soft limit (alert) → notify tenant admin (email/IM), don't block business

Soft limit + abnormal growth rate (5x baseline) → auto-degrade
  - Routing switches to cheaper model tier
  - Prompt tenant to investigate

Hard limit imminent → proactively suspend non-critical tasks
  tars admin tenant tasks suspend --id <tenant> --filter "priority=low"

Hard limit hit → tenant-wide budget exceeded
  - Auto-enter read-only mode
  - Notify tenant for immediate communication
```

**Recovery from misjudgment**:
```bash
# Temporarily raise budget (requires approval)
tars admin tenant budget set --id <tenant> --daily 1000 --justification "INC-1234 emergency" --approver <name>
```

### 5.4 Cache Redis failure

**Symptoms**:
- `cache.lookup_errors_total` spikes
- LLM request volume rises (because of cache misses)
- Cost suddenly grows

**Triage**:
- Does Redis ping succeed? `redis-cli -h <host> ping`
- Master or replica issue?
- Network problem or Redis itself?

**Mitigation**:

Our design (Doc 03 §4.3) makes cache errors non-blocking:
- L1 (in-memory) still works
- L2 (Redis) miss falls back to Provider
- Business continues, but **cost rises significantly**

```
Immediate:
- ✅ Business auto-degrades (full cache miss), no manual op needed

Short-term:
- Budget alert thresholds will trigger (because of higher cost), need a short-term raise (avoid spurious circuit-breaks)

Mid-term:
- Redis failover (master failure) → wait for sentinel to switch, or manually:
  redis-cli -h <sentinel> SENTINEL FAILOVER tars-master
```

**Warning**: Redis down for a long time (> 1h) → cost burn rate may reach 10x → proactively notify users + monitor budget

**Recovery**:
- After Redis recovers, new requests will automatically use the cache
- No "warm up" needed — cache fills naturally with traffic

### 5.5 Postgres primary failure

**Symptoms**:
- `db.query_errors_total` spikes
- Writes are 100% failing
- Reads may still work on replicas

**Triage**:
- pg_isready check
- Monitor replica lag
- Did the master crash and a replica auto-promoted?

**Mitigation**:

**A. RDS / managed cloud**:
- Auto failover usually 30-120s
- During the gap, the application layer should refuse writes (correct) or queue (dangerous, possibly OOM)
- Our app should implement backpressure (Doc 11 §5.2), reject rather than queue

**B. Self-managed Postgres + Patroni**:
```bash
# Check cluster status
patronictl -c /etc/patroni.yml list

# Force failover (if auto doesn't trigger)
patronictl -c /etc/patroni.yml failover --master <current-master> --candidate <replica>
```

**C. Extreme case: restore from backup**:
- See §10 backup-recovery drill section
- Data loss window = most recent backup time (RPO 1h)

**Key invariants**:
- Never manually edit Postgres data (even in emergencies)
  - Exception: emergency privileges go through §11.2 process
- Never skip the audit log write

### 5.6 Single-tenant burst (Hot Tenant)

**Symptoms**:
- A single tenant's task submission rate is 100x normal
- Hogging most resources, possibly affecting other tenants
- Could be a legitimate CI burst, or an attack

**Triage**:
- Look at the tenant's request source IP distribution — single-IP burst → possibly attack
- Look at request specs — same task repeated → possibly a bug
- Contact tenant admin to confirm

**Mitigation**:

```bash
# 1. Immediately throttle this tenant (we have per-tenant BoundedExecutor by design)
tars admin tenant throttle --id <tenant> --max-concurrent 5 --duration 1h

# 2. If it's an attack, suspend
tars admin tenant suspend --id <tenant> --reason "abnormal_traffic_pattern"

# 3. Notify the tenant
```

**Post-mitigation**:
- Investigate whether it really is an attack; if yes → security incident track (§5.10)
- If legitimate but over-capacity, talk to adjust quota or scale

### 5.7 Subprocess pool exhausted (CLI / MCP)

**Symptoms**:
- `tool.subprocess_spawn_failures_total` rising
- Errors: `Too many open files` or `Cannot allocate memory`
- New task submissions fail

**Triage**:
- `lsof -p <pid>` to see actual fd count
- `ps -ef | grep claude\|mcp-` to see subprocess count
- Is it a leak (subprocesses don't exit) or genuinely high load?

**Mitigation**:

```bash
# 1. Immediately clean idle subprocesses (our janitor should run every 5 min, may be stuck)
tars admin subprocess prune --idle-secs 60

# 2. Force-kill stuck subprocesses
tars admin subprocess kill --status stuck

# 3. Temporarily limit new subprocess creation
tars admin subprocess limit --max 50 --duration 1h
```

**Root-cause investigation**:
- Is some MCP server implementation buggy (unresponsive to SIGTERM)?
- Does the user session genuinely need that many?
- Is the ulimit configuration sensible?

### 5.8 Trajectory stuck (Stuck Trajectory)

**Symptoms**:
- A single trajectory has run > 30 min and is still active
- No new events appended
- May be holding resources (subprocess / connection)

**Triage**:
```bash
# Look at last event time of this trajectory
tars admin trajectory inspect --id <traj_id>

# Check whether the cancel signal is being delivered
# See Doc 02 §5 Cancel pipeline
```

**Mitigation**:

```bash
# 1. Soft cancel (try graceful)
tars admin trajectory cancel --id <traj_id>

# 2. Wait 30s; if still alive, force abort
tars admin trajectory abort --id <traj_id> --force

# 3. If a system bug is preventing cancel propagation, restart that instance
# (will trigger trajectory recovery flow, Doc 04 §7)
```

**Warning**:
- Force abort skips compensation (Doc 04 §6)
- Must manually evaluate whether compensation is needed (e.g., for already-created cloud resources)
- Write audit: `EmergencyTrajectoryAbort`

### 5.9 Compensation failure (Doc 04 §6.3)

**Symptoms**:
- `compensation.failed_total` alert
- Severity: P0 (system entered an inconsistent state)
- PagerDuty wakes immediately

**Triage**:
- Which trajectory? Which compensation failed?
- Failure cause: network? permissions? business rule?
- What state are the affected resources in?

**Mitigation** (absolutely do not ignore):

1. **Freeze related resources**: avoid making new decisions on inconsistent state
   ```bash
   tars admin tenant freeze --id <tenant> --resources <resource_refs>
   ```

2. **Manual inspect**:
   ```bash
   # See compensation details
   tars admin compensation get --id <comp_id>
   
   # See full trajectory event stream
   tars admin trajectory events --id <traj_id>
   ```

3. **Manually execute compensation** (if possible):
   - E.g., delete cloud resources that compensation didn't delete
   - Must audit-record every step

4. **Notify**:
   - Affected tenant admin
   - Security / compliance team (data may be inconsistent)
   - Engineering team (fix the bug)

5. **Mark trajectory**:
   ```bash
   tars admin trajectory mark --id <traj_id> --status "manual_recovery_required"
   ```

**Recovery**:
- Write a detailed post-mortem
- Fix the bug (root cause of compensation failure)
- Evaluate whether architectural changes are needed (e.g., promote some ReversibleResource to Irreversible to restrict the commit phase)

### 5.10 Tenant Isolation Breach (P0 security event)

**Symptoms**:
- `security.tenant_isolation_breach` audit event
- This event **should never happen** — its occurrence indicates an architectural bug or attack

**Response**: immediately P0, skip all normal procedures

1. **Preserve evidence**:
   - Don't delete / modify any related data
   - Immediately snapshot all related storage
   ```bash
   tars admin snapshot --tenant <a>,<b> --reason "INC-1234 P0 security"
   ```

2. **Isolate**:
   - Immediately suspend the involved tenants (both, even the victim, to prevent the attacker from continuing to read)
   - Cut traffic to that instance (other instances continue serving)

3. **Notify**:
   - **Must** notify: CTO + legal + security lead
   - **May be required** to notify: affected customers (legal review)
   - **May be required** to notify: regulators (GDPR 72h notification window, etc.)

4. **Forensics**:
   - Full audit log dump
   - All events for the relevant trace_id
   - Cache key fingerprint comparison

5. **Fix**:
   - After fix, **must** have three-person review before going live
   - After fix, **must** add regression test (Doc 10 §16.2 fuzz)

**Absolutely must not**:
- Delete any audit record (even if it contains PII)
- Hot-fix directly to production (must review)
- Unilaterally notify customers (legal must evaluate first)

### 5.11 Data storage full

**Symptoms**:
- `db.disk_usage_percent` > 90%
- Writes start failing
- Backups start failing

**Triage**:
- Which table takes the most space? `SELECT pg_size_pretty(pg_total_relation_size(...))`
- Did the retention policy fail to run, or is growth genuinely too fast?
- Is this an application bug (writing abnormally much)?

**Mitigation**:

```bash
# Short-term: scale up
tars admin db expand --target 200GB

# Immediate cleanup (if retention didn't run)
tars admin db cleanup --apply-retention-policy

# Emergency release (drop old partition)
tars admin db drop-partition --before 2025-12-01
```

### 5.12 Deployment rollback

**Trigger conditions**:
- P0/P1 incident after a new version is deployed
- Error rate / latency significantly worsened
- Critical SLO breached

**Steps**:

```bash
# 1. Immediately stop ongoing rollout (if canary)
kubectl rollout pause deployment/tars-server

# 2. Rollback to previous version
kubectl rollout undo deployment/tars-server

# 3. Verify rollback succeeded
kubectl rollout status deployment/tars-server

# 4. Verify metrics recover
# Check dashboard: error_rate / latency / saturation

# 5. Notify incident channel
```

**Post-rollback**:
- Don't immediately redeploy a "fixed" version — analyze thoroughly first
- Fully reproduce the issue in staging before retrying
- At least 24h observation period before considering the next deploy

---

## 6. Routine ops procedures

### 6.1 Tenant provisioning

See Doc 06 §8.1 for details. Procedure:

```bash
# 1. Create tenant (go through approval flow)
tars admin tenant create \
  --display-name "Acme Corp" \
  --owner-email admin@acme.com \
  --providers claude_api,gemini_api \
  --quota-tpm 100000 \
  --quota-daily-cost-usd 100

# 2. Output tenant_id, record it

# 3. Set initial secret (admin does this themselves; tenant should not see it)
tars admin secret set \
  --tenant <tenant_id> \
  --key "anthropic_api_key" \
  --from-vault "secret/data/customers/acme/anthropic"

# 4. Verify health
tars admin tenant health --id <tenant_id>

# 5. Send onboarding doc to tenant admin
```

### 6.2 Tenant suspend

```bash
# Soft suspend (default)
tars admin tenant suspend \
  --id <tenant_id> \
  --reason "billing_overdue" \
  --notify-tenant true

# Verify it's effective
tars admin tenant status --id <tenant_id>  # should show "suspended"

# Verify in-flight requests have drained
tars admin tenant tasks list --id <tenant_id> --status active
# Should gradually go to zero within 60s
```

### 6.3 Tenant delete (GDPR)

See Doc 06 §8.3. Strictly follow the 30-day delay procedure. **Skipping the delay period is not allowed.**

### 6.4 Hot config reload

```bash
# 1. Prepare new config (review in a git PR)
git checkout -b config-update-2026-05-02

# 2. Edit /etc/tars/config.toml

# 3. PR review + merge

# 4. CI auto-triggers reload (or manual)
tars admin config reload

# 5. Verify in effect
tars admin config show --diff-from-previous
```

### 6.5 Secret rotation

```bash
# 1. Write new secret to Vault (new path or new version)
vault kv put secret/tars/openai/v2 api_key="sk-new..."

# 2. Update config reference
# Edit config:
#   auth = { source = "vault", path = "secret/data/tars/openai/v2" }

# 3. Config reload
tars admin config reload

# 4. Monitor for 5-10 min, confirm no auth errors

# 5. Revoke the old secret
vault kv delete secret/tars/openai/v1
```

### 6.6 Provider config changes

```bash
# Add a new provider
tars admin provider add \
  --id "groq_llama3" \
  --type "openai_compat" \
  --base-url "https://api.groq.com/openai/v1" \
  --auth-secret "secret/data/tars/groq"

# Modify routing policy (carefully)
# Via config reload (not direct admin API), to preserve git trail
```

---

## 7. Routine health checks

### 7.1 Daily (automated)

- [ ] Backup integrity (`pg_verify_backup` + S3 checksum)
- [ ] Backup-restore drill (in staging, restoring 1 tenant's data)
- [ ] Cardinality metric checks (Doc 08 §5.5)
- [ ] Provider quota usage
- [ ] Disk usage trend
- [ ] Compensation failure tally (should be 0)
- [ ] Security alert tally

### 7.2 Weekly

- [ ] Performance baseline comparison (Doc 11 §10)
- [ ] Cost-per-tenant anomaly detection
- [ ] L3 cache capacity trend
- [ ] Dead-trajectory cleanup
- [ ] CVE scan (Doc 10 §14.1)
- [ ] On-call handoff review

### 7.3 Monthly

- [ ] DR drill (full restore of one tenant from backup)
- [ ] Audit log integrity spot-check
- [ ] Capacity-planning review (Doc 11 §4)
- [ ] Provider invoice reconciliation (against our billing_events)
- [ ] Tenant quota review (anyone chronically over budget who needs a conversation)

### 7.4 Quarterly

- [ ] Third-party pentest (Doc 10 §16.3)
- [ ] Red team exercise (Doc 10 §16.4)
- [ ] Postgres VACUUM FULL (low-traffic window)
- [ ] Long-term metric retention review
- [ ] Supply-chain review (cargo audit + SBOM diff)

### 7.5 Annually

- [ ] Full SOC2 / ISO27001 audit
- [ ] DR drill (whole-region failure simulation)
- [ ] Master signing-key rotation
- [ ] Team incident-response exercise (game day)

---

## 8. Monitoring dashboard quick reference

`<TBD: Grafana / Datadog dashboard URLs>`

**Core dashboards**:
- **Service Overview**: overall availability / error rate / latency / QPS
- **Per-Provider**: each LLM provider's health
- **Per-Tenant**: each tenant's spend / usage / SLO
- **Cache Performance**: hit rate / latency / size
- **Cost Tracking**: real-time spend / projected monthly / cache savings
- **Subprocess Health**: pool size / spawn rate / kill rate
- **Storage**: DB size / Redis usage / S3 growth
- **Security**: auth failures / IAM denies / breach attempts (should be 0)

Every alert in a ticket **must** link to the corresponding dashboard panel.

---

## 9. Communication templates

### 9.1 Status page updates

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

### 9.2 Customer notification (high-touch customers)

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

### 9.3 Internal incident-channel updates (every 30 min)

```
[INC-1234] Status update <time>
- Current status: mitigated / investigating / monitoring
- Latest action: <last 30 min>
- Next: <plan>
- Blocker: <if any>
```

---

## 10. Backup and recovery drill

### 10.1 Backup inventory

```
Postgres: hourly incremental + daily full → cross-region S3
Redis: RDB hourly → S3
S3 ContentRef: real-time cross-region replication
Secrets: Vault's own backup (built into Vault enterprise)
Audit log: real-time mirror to SIEM (Splunk)
```

### 10.2 Recovery drill (mandatory quarterly)

**Goal**: verify we can restore from backup to a specified point in time.

**Steps**:

```bash
# 1. Prepare an isolated environment
make staging-clean

# 2. Pick a recovery point in time (e.g., 7 days ago)
RESTORE_TIME="2026-04-25T10:00:00Z"

# 3. Pull backup from S3
aws s3 cp s3://tars-prod-backup/postgres/2026-04-25/full.dump ./

# 4. Restore Postgres
pg_restore -h staging-db --create --dbname=postgres ./full.dump

# 5. Apply WAL up to the specified point in time (point-in-time recovery)
# (TBD: detailed command depends on RDS setup)

# 6. Restore Redis (from RDB)
redis-cli -h staging-redis SHUTDOWN
cp /backup/redis-2026-04-25.rdb /var/lib/redis/dump.rdb
systemctl start redis

# 7. Start the application
kubectl apply -f staging/

# 8. Verify
tars admin verify --comprehensive
# Should output:
#   ✓ DB schema correct
#   ✓ Tenant data accessible
#   ✓ Cache hit rate > 0
#   ✓ Sample task can run end-to-end

# 9. Clean staging
make staging-clean

# 10. Record:
#   - Total time taken (RTO target 4h)
#   - Data loss window (RPO target 1h)
#   - Any unexpected issues
```

**Acceptance criteria**:
- RTO < 4h
- RPO < 1h
- No manual hacks (fully automated)

---

## 11. Emergency privileges (Break-glass)

### 11.1 When to use

- P0 incident and normal admin isn't enough
- Direct production-environment operation
- E.g., manually edit DB / direct provider-API call / run unapproved scripts

### 11.2 Procedure

See Doc 10 §15.3. Operational steps:

```bash
# 1. Request emergency access (two-person approve)
tars admin emergency request \
  --justification "INC-1234: need to manually trigger compensation for traj X" \
  --duration 1h \
  --approver-needed 2

# 2. Wait for approval (PagerDuty notifies the second admin)

# 3. Receive a temporary token
export TARS_EMERGENCY_TOKEN="..."

# 4. Execute operations (every action is auto-audited)
tars admin <whatever-needed>

# 5. Token auto-expires (default 4h)

# 6. Post-incident: write an emergency-privilege usage report (quarterly review)
```

### 11.3 Abuse detection

- Monitor emergency-privilege use frequency; > 3 times per person/month requires a conversation
- Emergency privileges + no corresponding incident ticket = serious violation
- Quarterly audit committee reviews all use records

---

## 12. Vendor / Provider communication

### 12.1 Provider outage

If an LLM provider is down for more than 1h, we should:

```
1. @ customer success + sales in the incident channel
2. Open a critical ticket with the provider
3. Track provider's status page updates
4. Evaluate whether to switch to a backup provider for long-term operation (cost impact)
5. Prepare SLA credits for customers (if needed)
```

### 12.2 Provider quota exhausted

```
1. Immediately request a temporary quota increase from the provider (large accounts usually < 4h response)
2. Enable routing bias toward other providers
3. Evaluate whether a long-term quota increase is needed (talk with sales)
```

### 12.3 Upstream SDK / library bug

If a bug is found in a critical library like reqwest / sqlx / pyo3:

```
1. Add a temporary workaround in our codebase
2. Report a GitHub issue upstream (with minimal repro)
3. Track the fix's progress
4. Upgrade as soon as the fix lands
```

---

## 13. SLA / SLO impact handling

### 13.1 SLO breach decisions

```
A single incident breaches the SLO:
  → Assess remaining error budget
  → If budget > 50% → continue deploys as planned
  → If budget < 50% → slow deploy cadence, prioritize stability work
  → If budget < 0  → deploy freeze, all work shifts to stability

Repeated SLO breaches:
  → Escalate to product discussion
  → Adjust SLO (if unreasonable) or adjust product strategy
```

### 13.2 SLA violation (customer-facing)

If SLA terms are triggered (e.g., 99.5% monthly availability):

```
1. Compute credit (per contract terms)
2. Proactively notify the customer (not passively)
3. Write a post-mortem for the customer
4. Customer success follows up on commercial impact
```

---

## 14. Routine change management

### 14.1 Change classification

| Level | Examples | Approval |
|---|---|---|
| Standard | Docker image patch / log level adjustment | Self-service |
| Normal | New provider go-live / new routing rule | Lead approve |
| Emergency | Hot fix for a P0 | Director approve + post-fact review |
| Significant | Major DB schema change / architectural change | CAB review |

### 14.2 Change windows

- Working days (Mon-Thu) 09:00-16:00 (local timezone)
- Strictly forbidden Friday afternoon / weekends / right before/after holidays (unless P0)
- Announce in #ops-changes 24h before the change

### 14.3 Canary deployment

```
1. Deploy to 1 canary instance (5% traffic)
2. Observe for 30 min: error_rate / latency / cost
3. Any metric worsening → immediate rollback
4. Pass → 25% → 50% → 100% (30 min observation per stage)
5. After completion, 24h observation period
```

---

## 15. Post-mortem template

P0/P1 must complete within 5 business days after incident resolution.

```markdown
# Post-Mortem: <Incident ID> - <Short Title>

## TL;DR
<Two sentences: what happened + impact>

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
<Full explanation — not just "what" but also "why" and "why it wasn't caught">

## Detection
- How was it discovered? (alert / customer report / random discovery)
- Improvements: how can we detect earlier?

## Response
<Timeline: t+0 / t+5 / t+15 ...>
<What decisions were correct? What was a detour?>

## Resolution
<Final fix>

## What Went Well
- ...

## What Went Wrong
- ...

## Lucky Factors
<Things that would have made it worse if not for these strokes of luck>

## Action Items
| ID | Description | Owner | Priority | Due |
|----|-------------|-------|----------|-----|
| 1  | Add metric for X | Alice | P1 | 2026-05-15 |
| 2  | Refactor compensation handler | Bob | P2 | 2026-06-01 |

## Lessons Learned
<Wisdom summary for future readers>

## Appendix
- Incident channel link
- Relevant traces / dashboards
- Customer communications sent
```

**Post-mortem culture principles** (Blameless):
- Focus on **systems** and **processes**, not individuals
- Don't write "X should have been more careful" — write "our tooling should have made it impossible for X to make the mistake"
- Encourage honesty, even about your own mistakes
- Action items must have owner + due date + tracking to completion

---

## 16. Anti-pattern checklist

1. **Do not "handle quietly" during an incident** — must open a channel even if you think you can solve it quickly.
2. **Do not skip mitigation and go straight to root-cause analysis** — stop the bleeding first.
3. **Do not make large changes during a P0** — make only the minimal stop-the-bleeding change; major fixes go after the fact.
4. **Do not ignore post-mortem action items** — review weekly; long-overdue items must be escalated.
5. **Do not let one person carry a P0 alone** — open the channel; others can lurk and help.
6. **Do not use emergency privileges for "ordinary" things** — it's break-glass, not a daily tool.
7. **Do not trust backups you haven't drilled** — quarterly drills are mandatory.
8. **Do not hide information from customers during an incident** — transparency + honesty trumps a "perfect story".
9. **Do not use the wrong severity** — better to over-classify and downgrade than under-classify and miss the response.
10. **Do not modify the audit log even in an emergency** — append-only is a promise that cannot be broken.
11. **Do not push to production with insufficient staging validation** — even hot fixes need at least a smoke test.
12. **Do not let on-call decide major changes alone** — single point of failure; require at least two-person approval.
13. **Do not ignore the accumulation of "small" alerts** — frequent P3 alerts may foreshadow an imminent P1.
14. **Do not blame individuals in post-mortems** — blameless culture is the foundation of organizational resilience.
15. **Do not let the runbook go stale** — quarterly review; an outdated playbook is more dangerous than none.

---

## 17. Contracts with upstream and downstream

### Upstream (business / customer) commitments

- Provide service within SLA (availability / latency / data integrity)
- Transparent incident communication
- Share post-mortems (appropriately redacted)
- Provide credit per contract on SLA violation

### Downstream (provider / infrastructure) dependencies

- LLM Provider: publish status page + meet SLA
- Cloud Provider (AWS/GCP): meet SLA, DR capability
- Vault: high availability, 99.99%+
- SIEM: real-time event ingestion

### Internal teams

- Engineering team: bug-fix SLA, full on-call handoff for new features
- Product team: take in SLO data to guide priorities
- Sales team: SLA negotiation coordinated with SRE capacity planning
- Legal team: engage immediately on compliance events

---

## 18. TODOs and open questions

- [ ] Fill in actual commands (`tars admin ...` are placeholders, replace post-implementation)
- [ ] Fill in dashboard URLs (after Grafana is deployed)
- [ ] PagerDuty / OpsGenie integration setup
- [ ] Status page selection (statuspage.io / self-hosted)
- [ ] Game day exercise scripts (quarterly)
- [ ] Chaos engineering integration (Litmus / Chaos Mesh)
- [ ] AI-assisted incident triage (LLM looks at alert + log and proposes hypotheses)
- [ ] Auto-remediation playbook (which can be auto-fixed without humans)
- [ ] Customer-visible incident metrics (status.tars.example.com)
- [ ] Multi-language customer-communication templates (English / Chinese / Japanese / French / etc.)
- [ ] On-call training material and certification process
