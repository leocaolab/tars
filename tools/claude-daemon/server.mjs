// tars claude-daemon — long-lived host for @anthropic-ai/claude-agent-sdk.
//
// Two transports, same SDK glue:
//
//   node server.mjs --stdio      ← child-process mode (production)
//                                  NDJSON in on stdin, NDJSON out on stdout.
//                                  tars-rust spawns one of these per
//                                  `ClaudeSdkProvider`. Multiplexed by
//                                  request `id` so 4-concurrent critic
//                                  calls overlap inside the same SDK.
//
//   node server.mjs              ← HTTP mode (manual debugging only)
//                                  fastify on 127.0.0.1:7777. Lets you
//                                  `curl -X POST` to verify the SDK + OAuth
//                                  path. Not used by tars.
//
// In both modes the same `handleChat({prompt, system, model, max_turns})`
// does the SDK work: tools off, no dynamic system prompt, settingSources
// stripped, max_turns=1 — apples-to-apples with `claude -p`.

import { query } from '@anthropic-ai/claude-agent-sdk';
import readline from 'node:readline';

// Default maxTurns=7: history is 1 → 3 → 7. The 1→3 bump fit
// sonnet-4-5 extended thinking (`thinking_block → text_block` =
// 2 turns). 3→7 covers heavier think-iterate-refine patterns
// the L4 critic shows on dense files — under `arc auto` we saw
// "Reached maximum number of turns (3)" on a handful of 154
// files where the model wants to think → draft → re-read →
// refine before emitting. 7 covers up to 3 such rounds plus
// the final answer. Tools stay disabled below (`disallowedTools:
// ['*']`) so the model can't go agentic regardless of this
// counter.
async function handleChat({ prompt, system, model, max_turns = 7, schema, thinking: thinkingMode }) {
  if (typeof prompt !== 'string' || !prompt.length) {
    throw new Error('prompt (string) is required');
  }

  const t0 = performance.now();
  let ttfb = null;
  let text = '';
  let thinking = '';
  let structuredOutput = null;
  let usage = null;
  let resultSubtype = null;
  let actualModel = null;
  let messageCount = 0;

  const opts = {
    maxTurns: max_turns,
    // Pure LLM — no tool agency. Comparable to `claude -p`, and avoids
    // accidental file writes from a critic prompt that contains code.
    disallowedTools: ['*'],
    // Don't pollute results with project-local settings / hooks /
    // dynamic system prompt that Claude Code normally injects.
    settingSources: [],
    permissionMode: 'bypassPermissions',
  };
  if (model) opts.model = model;
  if (system) opts.systemPrompt = system;
  // Provider-level structured output: when the caller passes a JSON
  // Schema, the Agent SDK constrains the final answer to it and returns
  // it on the result message's `structured_output`. This is SEPARATE
  // from agentic tools, so it coexists with `disallowedTools: ['*']` —
  // claude_sdk gets strict schema without re-enabling tool agency.
  if (schema) opts.outputFormat = { type: 'json_schema', schema };
  // Extended thinking. `display: 'summarized'` is REQUIRED to populate the
  // thinking-block text — the default omits it (empty), which is why the
  // reasoning was a black hole: it happened + was billed, but never logged.
  // `thinkingMode` of 'off'/'disabled' turns thinking off — but note that
  // removes the reasoning CHANNEL, so the model reasons in visible text
  // instead (verbose prose / the "essay"). Default: on + summarized.
  opts.thinking =
    thinkingMode === 'off' || thinkingMode === 'disabled'
      ? { type: 'disabled' }
      : { type: 'adaptive', display: 'summarized' };

  if (process.env.CLAUDE_DAEMON_TRACE) {
    process.stderr.write(
      `trace: query() starting (prompt=${prompt.length} chars, schema=${schema ? 'yes' : 'no'})\n`,
    );
  }
  const q = query({ prompt, options: opts });

  try {
    for await (const msg of q) {
      if (ttfb === null) ttfb = performance.now() - t0;
      messageCount += 1;
      // Observability: log every SDK message as it streams, so a wedged or
      // looping daemon shows WHAT the model is doing — a `tool_use` block (the
      // model trying to call a tool despite disallowedTools:['*'] — the
      // reviewer-hang signature), a long run of thinking with no result, or
      // NOTHING at all (query stuck before the first message). Gated on
      // CLAUDE_DAEMON_TRACE so production stays silent.
      if (process.env.CLAUDE_DAEMON_TRACE) {
        let detail = '';
        const c = msg.message?.content;
        if (Array.isArray(c)) {
          const kinds = c
            .map((b) => (b?.type ?? '?') + (b?.name ? `:${b.name}` : ''))
            .join(',');
          if (kinds) detail = ` [${kinds}]`;
        }
        process.stderr.write(`trace: msg#${messageCount} type=${msg.type}${detail}\n`);
      }
      // Defend against unexpected message shapes from the SDK (e.g. content
      // arriving as a string rather than a block array, or a non-string
      // block.text). A single malformed message must not abort the stream.
      try {
        if (msg.type === 'assistant') {
          actualModel = actualModel || msg.message?.model || null;
          const content = msg.message?.content;
          if (Array.isArray(content)) {
            for (const block of content) {
              if (block?.type === 'text' && typeof block.text === 'string') {
                text += block.text;
              } else if (block?.type === 'thinking' && typeof block.thinking === 'string') {
                thinking += block.thinking;
              }
            }
          }
        } else if (msg.type === 'result') {
          usage = msg.usage ?? null;
          resultSubtype = msg.subtype ?? null;
          if (msg.structured_output != null) structuredOutput = msg.structured_output;
        }
      } catch (err) {
        process.stderr.write(
          `handleChat: skipping malformed SDK message: ${err?.message ?? err}\n`,
        );
      }
    }
  } catch (err) {
    // The SDK throws — not emits a result message — when it hits its
    // own internal walls. The most common one in arc-auto-shaped runs
    // is "Reached maximum number of turns (N)": the model's still
    // thinking when the turn counter overflows. Before this catch, that
    // throw became a hard daemon error and every accumulated `text`
    // chunk was discarded — even when the model had already emitted
    // 90% of its verdict. Worker crash, file gets zero review.
    //
    // Instead: return the partial output with `result_subtype:
    // "error_max_turns"`. The Rust client already maps that subtype to
    // `StopReason::Other` (claude_sdk.rs:543), so the consumer sees a
    // legitimate (truncated) response and can decide what to do —
    // arc's Critic parser may still extract enough verdicts to be
    // useful, and even when it can't, the call counts as a soft
    // failure (graceful) rather than a worker crash (hard).
    //
    // Only recover from max_turns specifically; everything else
    // (network errors, auth failures, SDK protocol bugs) still
    // propagates so the operator sees what's wrong.
    const errMsg = String(err?.message ?? err);
    if (errMsg.includes('Reached maximum number of turns')) {
      process.stderr.write(
        `handleChat: max_turns hit after ${messageCount} msg(s), ${text.length} text chars — returning partial result\n`,
      );
      resultSubtype = 'error_max_turns';
      // Fall through to the normal return path with whatever we
      // accumulated (text / usage / actualModel can all be partial
      // or null; the consumer handles each independently).
    } else {
      throw err;
    }
  }

  const total = performance.now() - t0;
  // When the SDK returned structured output, IT is the answer — serialize
  // it as the reply text so the Rust text path (and arc's JSON parser)
  // carry it unchanged. Falls back to the streamed assistant text.
  const replyText = structuredOutput != null ? JSON.stringify(structuredOutput) : text;
  return {
    text: replyText,
    thinking,
    result_subtype: resultSubtype,
    usage,
    model: actualModel,
    message_count: messageCount,
    durations: {
      ttfb_ms: Math.round(ttfb ?? total),
      total_ms: Math.round(total),
    },
  };
}

// ────────────────────────────────────────────────────────────────────
// Mode dispatch
// ────────────────────────────────────────────────────────────────────

const stdioMode = process.argv.includes('--stdio');

if (stdioMode) {
  // stdin: one JSON object per line — {id, prompt, system?, model?, max_turns?}.
  // stdout: one JSON object per line — {id, ...reply}  OR  {id, error, stack?}.
  // stderr: free-form diagnostics, not parsed by tars.

  process.stderr.write(`claude-daemon stdio ready (pid=${process.pid})\n`);

  const rl = readline.createInterface({ input: process.stdin });

  rl.on('line', (line) => {
    let req;
    try {
      req = JSON.parse(line);
    } catch (err) {
      // No id available — best we can do is shout on stderr; tars will
      // notice the pending request timing out.
      process.stderr.write(`stdio: bad JSON: ${err.message}\n`);
      return;
    }
    const { id } = req;
    // Without a usable id we can't route a reply; an undefined/null id would
    // produce an unaddressable response and leak a pending promise on the
    // tars side. Reject early on stderr.
    if (id == null || (typeof id !== 'string' && typeof id !== 'number')) {
      process.stderr.write(
        `stdio: dropping request with missing/invalid id (got ${typeof id})\n`,
      );
      return;
    }

    // Wrap stdout.write: if it throws (EPIPE on a dead reader, OOM), an
    // uncaught throw inside this .then/.catch becomes an unhandledRejection
    // and crashes the daemon. Degrade to a stderr log instead.
    const writeLine = (obj) => {
      try {
        process.stdout.write(JSON.stringify(obj) + '\n');
      } catch (werr) {
        process.stderr.write(
          `stdio: failed to write reply for id=${id}: ${werr?.message ?? werr}\n`,
        );
      }
    };

    // Fire-and-forget so multiple in-flight requests overlap inside
    // the SDK — each call gets its own async chain.
    handleChat(req).then((reply) => {
      writeLine({ id, ...reply });
    }).catch((err) => {
      writeLine({
        id,
        error: String(err?.message ?? err),
        stack: err?.stack,
      });
    });
  });

  rl.on('close', () => {
    process.stderr.write('claude-daemon stdio: stdin closed, exiting\n');
    process.exit(0);
  });
} else {
  // HTTP debugging mode. Lazy-imported so stdio-only deployments don't
  // need fastify installed.
  const { fastify } = await import('fastify');

  const PORT = parseInt(process.env.PORT ?? '7777', 10);
  const HOST = process.env.HOST ?? '127.0.0.1';
  const app = fastify({ logger: { level: 'info' } });

  app.get('/healthz', async () => ({ ok: true, pid: process.pid, uptime_s: process.uptime() }));

  app.post('/chat', async (req, reply) => {
    const body = req.body ?? {};
    // A missing/empty prompt is a client error, not a server fault → 400.
    if (typeof body.prompt !== 'string' || !body.prompt.length) {
      return reply.code(400).send({ error: 'prompt (string) is required' });
    }
    try {
      return await handleChat(body);
    } catch (err) {
      return reply.code(500).send({
        error: String(err?.message ?? err),
        stack: err?.stack,
      });
    }
  });

  try {
    await app.listen({ port: PORT, host: HOST });
    app.log.info(`claude-daemon HTTP debug mode: http://${HOST}:${PORT}`);
  } catch (err) {
    app.log.error(err);
    process.exit(1);
  }
}
