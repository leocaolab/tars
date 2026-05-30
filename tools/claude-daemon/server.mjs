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

async function handleChat({ prompt, system, model, max_turns = 1 }) {
  if (typeof prompt !== 'string' || !prompt.length) {
    throw new Error('prompt (string) is required');
  }

  const t0 = performance.now();
  let ttfb = null;
  let text = '';
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

  const q = query({ prompt, options: opts });

  for await (const msg of q) {
    if (ttfb === null) ttfb = performance.now() - t0;
    messageCount += 1;
    if (msg.type === 'assistant') {
      actualModel = actualModel || msg.message?.model || null;
      for (const block of msg.message?.content ?? []) {
        if (block.type === 'text') text += block.text;
      }
    } else if (msg.type === 'result') {
      usage = msg.usage ?? null;
      resultSubtype = msg.subtype ?? null;
    }
  }

  const total = performance.now() - t0;
  return {
    text,
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
    // Fire-and-forget so multiple in-flight requests overlap inside
    // the SDK — each call gets its own async chain.
    handleChat(req).then((reply) => {
      process.stdout.write(JSON.stringify({ id, ...reply }) + '\n');
    }).catch((err) => {
      process.stdout.write(JSON.stringify({
        id,
        error: String(err?.message ?? err),
        stack: err?.stack,
      }) + '\n');
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
    try {
      return await handleChat(req.body ?? {});
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
