// tars-desktop frontend (M1). Talks to the Rust backend via Tauri's global
// invoke (no npm — `withGlobalTauri: true`). Conversations live in the backend;
// this is a view. Tokens stream live via the `chat-delta` event.

const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const $ = (id) => document.getElementById(id);
const providerSel = $("provider");
const systemInput = $("system");
const maxtokInput = $("maxtok");
const transcript = $("transcript");
const input = $("input");
const sendBtn = $("send");
const convList = $("convlist");
const newChatBtn = $("newchat");
const tabChats = $("tab-chats");
const tabTraces = $("tab-traces");
const composer = document.querySelector(".composer");

let currentConvId = null;
let view = "chats"; // "chats" | "traces"

// ── Minimal, safe markdown → HTML ───────────────────────────────────────
function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function renderMarkdown(src) {
  const blocks = src.split(/```/);
  let html = "";
  blocks.forEach((block, i) => {
    if (i % 2 === 1) {
      const body = block.replace(/^[^\n]*\n/, "");
      html += `<pre><code>${escapeHtml(body.replace(/\n$/, ""))}</code></pre>`;
      return;
    }
    for (const line of block.split("\n")) {
      let h = escapeHtml(line);
      h = h.replace(/`([^`]+)`/g, "<code>$1</code>");
      h = h.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
      h = h.replace(/(^|[^*])\*([^*]+)\*/g, "$1<em>$2</em>");
      const head = line.match(/^(#{1,3})\s+(.*)$/);
      if (head) {
        html += `<h${head[1].length}>${escapeHtml(head[2])}</h${head[1].length}>`;
      } else if (/^\s*[-*]\s+/.test(line)) {
        html += `<div>• ${h.replace(/^\s*[-*]\s+/, "")}</div>`;
      } else if (line.trim() === "") {
        html += "<br/>";
      } else {
        html += `<div>${h}</div>`;
      }
    }
  });
  return html;
}

// ── Transcript ──────────────────────────────────────────────────────────
function dropEmpty() {
  const e = transcript.querySelector(".empty");
  if (e) e.remove();
}

function appendMessage(role, text, metrics) {
  dropEmpty();
  const msg = document.createElement("div");
  msg.className = `msg ${role}`;
  const label = role === "user" ? "You" : role === "error" ? "Error" : "Assistant";
  const bubble =
    role === "assistant" ? renderMarkdown(text) : `<div>${escapeHtml(text)}</div>`;
  msg.innerHTML = `<div class="role">${label}</div><div class="bubble">${bubble}</div>`;
  if (metrics) msg.appendChild(metricsRow(metrics));
  transcript.appendChild(msg);
  transcript.scrollTop = transcript.scrollHeight;
  return msg;
}

function metricsRow(m) {
  const parts = [];
  if (m.tok_per_sec != null) parts.push(`${m.tok_per_sec.toFixed(1)} tok/sec`);
  parts.push(`${m.total_tokens} tokens`);
  if (m.latency_ms != null) parts.push(`${m.latency_ms} ms`);
  if (m.stop_reason) parts.push(`stop: ${m.stop_reason}`);
  if (m.cache_hit) parts.push("cache hit");
  if (m.retry_count > 0) parts.push(`${m.retry_count} retries`);
  if (m.provider) parts.push(m.provider);
  const row = document.createElement("div");
  row.className = "metrics";
  row.innerHTML = parts
    .map((p, i) => `<span${i ? ' class="dot"' : ""}>${escapeHtml(p)}</span>`)
    .join("");
  return row;
}

const CURSOR = '<span class="cursor">▌</span>';
function startAssistantMessage() {
  dropEmpty();
  const msg = document.createElement("div");
  msg.className = "msg assistant";
  msg.innerHTML = `<div class="role">Assistant</div><div class="bubble">${CURSOR}</div>`;
  transcript.appendChild(msg);
  transcript.scrollTop = transcript.scrollHeight;
  return { msg, bubble: msg.querySelector(".bubble") };
}

function showEmpty() {
  transcript.innerHTML = '<div class="empty">Send a message to start.</div>';
}

function renderTranscript(messages) {
  transcript.innerHTML = "";
  if (!messages.length) {
    showEmpty();
    return;
  }
  for (const m of messages) appendMessage(m.role, m.text);
}

// ── Providers + conversations ───────────────────────────────────────────
async function loadProviders() {
  try {
    const providers = await invoke("list_providers");
    providerSel.innerHTML = "";
    for (const p of providers) {
      const opt = document.createElement("option");
      opt.value = p.id;
      opt.textContent = p.default_model ? `${p.id} · ${p.default_model}` : p.id;
      if (p.is_default) opt.selected = true;
      providerSel.appendChild(opt);
    }
  } catch (e) {
    appendMessage("error", `Failed to load providers: ${e}`);
  }
}

async function refreshConvList() {
  const convs = await invoke("list_conversations");
  convList.innerHTML = "";
  for (const c of convs) {
    const el = document.createElement("div");
    el.className = "conv" + (c.id === currentConvId ? " active" : "");
    el.textContent = c.title || "New chat";
    el.title = c.provider ? `${c.title} · ${c.provider}` : c.title;
    el.addEventListener("click", () => switchConv(c.id));
    convList.appendChild(el);
  }
}

async function newChat() {
  const meta = await invoke("new_conversation", {
    provider: providerSel.value || null,
    model: null,
    system: systemInput.value || null,
    maxOutputTokens: maxtokInput.value ? parseInt(maxtokInput.value, 10) : null,
  });
  currentConvId = meta.id;
  showEmpty();
  await refreshConvList();
}

async function switchConv(id) {
  currentConvId = id;
  const msgs = await invoke("conversation_messages", { id });
  renderTranscript(msgs);
  await refreshConvList();
}

// ── Traces (trajectories — incl. arc runs) ──────────────────────────────
async function refreshTraceList() {
  const trajs = await invoke("list_trajectories");
  convList.innerHTML = "";
  if (!trajs.length) {
    convList.innerHTML =
      '<div class="conv" style="color: var(--muted); cursor: default">No trajectories yet. Run an agent (or arc).</div>';
    return;
  }
  for (const t of trajs) {
    const el = document.createElement("div");
    el.className = "conv";
    el.textContent = t.id;
    el.title = `${t.id} · ${t.event_count} events`;
    el.addEventListener("click", () => showTrajectory(t.id, el));
    convList.appendChild(el);
  }
}

function eventSummary(p) {
  if (p && typeof p === "object" && !Array.isArray(p)) {
    if (typeof p.type === "string") return p.type;
    const keys = Object.keys(p);
    if (keys.length === 1) return keys[0];
  }
  return "event";
}

async function showTrajectory(id, el) {
  if (el) {
    document.querySelectorAll(".conv.active").forEach((e) => e.classList.remove("active"));
    el.classList.add("active");
  }
  const events = await invoke("trajectory_events", { id });
  transcript.innerHTML = "";
  if (!events.length) {
    showEmpty();
    return;
  }
  for (const ev of events) {
    const card = document.createElement("div");
    card.className = "event";
    card.innerHTML =
      `<div class="event-head"><span class="seq">#${ev.sequence_no}</span>${escapeHtml(eventSummary(ev.payload))}</div>` +
      `<pre class="event-json">${escapeHtml(JSON.stringify(ev.payload, null, 2))}</pre>`;
    transcript.appendChild(card);
  }
}

function setView(v) {
  view = v;
  tabChats.classList.toggle("active", v === "chats");
  tabTraces.classList.toggle("active", v === "traces");
  newChatBtn.style.display = v === "chats" ? "" : "none";
  composer.style.display = v === "chats" ? "" : "none";
  if (v === "chats") {
    refreshConvList();
    if (currentConvId) switchConv(currentConvId);
    else showEmpty();
  } else {
    showEmpty();
    refreshTraceList();
  }
}

// ── Send ────────────────────────────────────────────────────────────────
function setBusy(busy) {
  sendBtn.disabled = busy;
  sendBtn.textContent = busy ? "…" : "Send";
}

async function send() {
  const text = input.value.trim();
  if (!text || sendBtn.disabled || !currentConvId) return;
  appendMessage("user", text);
  input.value = "";
  input.style.height = "auto";
  setBusy(true);

  const { msg, bubble } = startAssistantMessage();
  let acc = "";
  const unlisten = await listen("chat-delta", (e) => {
    acc += e.payload;
    bubble.innerHTML = renderMarkdown(acc) + CURSOR;
    transcript.scrollTop = transcript.scrollHeight;
  });
  try {
    const turn = await invoke("send_message", {
      conversationId: currentConvId,
      userText: text,
    });
    bubble.innerHTML = renderMarkdown(turn.text);
    msg.appendChild(metricsRow(turn.metrics));
    refreshConvList(); // the title may have been set from the first message
  } catch (e) {
    bubble.innerHTML = `<div style="color: var(--error)">${escapeHtml(String(e))}</div>`;
  } finally {
    unlisten();
    setBusy(false);
  }
}

// ── Wiring ──────────────────────────────────────────────────────────────
sendBtn.addEventListener("click", send);
newChatBtn.addEventListener("click", newChat);
tabChats.addEventListener("click", () => setView("chats"));
tabTraces.addEventListener("click", () => setView("traces"));
input.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
    e.preventDefault();
    send();
  }
});
input.addEventListener("input", () => {
  input.style.height = "auto";
  input.style.height = Math.min(input.scrollHeight, 180) + "px";
});

async function init() {
  await loadProviders();
  const convs = await invoke("list_conversations");
  if (convs.length) {
    await switchConv(convs[0].id);
  } else {
    await newChat();
  }
}
init();
