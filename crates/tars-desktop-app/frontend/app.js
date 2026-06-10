// tars-desktop frontend (M0). Talks to the Rust backend via Tauri's global
// invoke (no npm — `withGlobalTauri: true`). Markdown is rendered with a tiny
// vanilla converter; KaTeX / a real markdown lib come with a later milestone.

const invoke = window.__TAURI__.core.invoke;

const $ = (id) => document.getElementById(id);
const providerSel = $("provider");
const systemInput = $("system");
const maxtokInput = $("maxtok");
const transcript = $("transcript");
const empty = $("empty");
const input = $("input");
const sendBtn = $("send");

// ── Minimal, safe markdown → HTML ───────────────────────────────────────
function escapeHtml(s) {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function renderMarkdown(src) {
  const blocks = src.split(/```/);
  let html = "";
  blocks.forEach((block, i) => {
    if (i % 2 === 1) {
      // fenced code block — drop an optional language line
      const body = block.replace(/^[^\n]*\n/, "");
      html += `<pre><code>${escapeHtml(body.replace(/\n$/, ""))}</code></pre>`;
      return;
    }
    for (let line of block.split("\n")) {
      let h = escapeHtml(line);
      h = h.replace(/`([^`]+)`/g, "<code>$1</code>");
      h = h.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
      h = h.replace(/(^|[^*])\*([^*]+)\*/g, "$1<em>$2</em>");
      const head = line.match(/^(#{1,3})\s+(.*)$/);
      if (head) {
        const lvl = head[1].length;
        html += `<h${lvl}>${escapeHtml(head[2])}</h${lvl}>`;
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
function appendMessage(role, text, metrics) {
  if (empty) empty.style.display = "none";
  const msg = document.createElement("div");
  msg.className = `msg ${role}`;
  const label = role === "user" ? "You" : role === "error" ? "Error" : "Assistant";
  const bubble =
    role === "assistant" ? renderMarkdown(text) : `<div>${escapeHtml(text)}</div>`;
  msg.innerHTML = `<div class="role">${label}</div><div class="bubble">${bubble}</div>`;
  if (metrics) msg.appendChild(metricsRow(metrics));
  transcript.appendChild(msg);
  transcript.scrollTop = transcript.scrollHeight;
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

// ── Providers ───────────────────────────────────────────────────────────
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

// ── Send ────────────────────────────────────────────────────────────────
function setBusy(busy) {
  sendBtn.disabled = busy;
  sendBtn.textContent = busy ? "…" : "Send";
}

async function send() {
  const text = input.value.trim();
  if (!text || sendBtn.disabled) return;
  appendMessage("user", text);
  input.value = "";
  input.style.height = "auto";
  setBusy(true);
  try {
    const turn = await invoke("send_chat", {
      provider: providerSel.value || null,
      model: null,
      system: systemInput.value || null,
      maxOutputTokens: maxtokInput.value ? parseInt(maxtokInput.value, 10) : null,
      userText: text,
    });
    appendMessage("assistant", turn.text, turn.metrics);
  } catch (e) {
    appendMessage("error", String(e));
  } finally {
    setBusy(false);
  }
}

sendBtn.addEventListener("click", send);
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

loadProviders();
