// Smoke test for the handle-based spine (Doc 12 §7): init → Workspaces.open →
// pipeline(role) / provider(role) → complete → context → close.
//
//   node --test __test__/handle.test.mjs
//
// The provider is `mock` — no network. We stand up a throwaway `$TARS_HOME`
// with a config that declares a mock provider + a `[roles]` map, so
// `init(home)` builds the global registry + role table from it, and a fresh
// temp dir is opened as a workspace (no `.arc` marker / `.git` → opened as its
// own root; `for_workspace` bootstraps `<root>/.arc/tars/`).
//
// Config::load + ProviderRegistry::global are process-global OnceLocks; Node's
// test runner isolates each *file* in its own process, so this file's `init`
// starts from a clean slate.

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import {
  init,
  isInitialized,
  tarsHome,
  Workspaces,
  TarsHandle,
} from '../index.js';

const HOME = mkdtempSync(join(tmpdir(), 'tars-node-home-'));
writeFileSync(
  join(HOME, 'config.toml'),
  `
[providers.mock]
type = "mock"
canned_response = "handle spine ok"

[roles]
critic = "mock"
fixer  = "mock"
`,
);

const WORKSPACE = mkdtempSync(join(tmpdir(), 'tars-node-ws-'));

test('init(home) loads the global config from a custom tars home', () => {
  assert.equal(isInitialized(), false);
  init(HOME);
  assert.equal(isInitialized(), true);
  // Idempotent: second call is a no-op, not an error.
  init(HOME);
});

test('tarsHome(home) echoes the resolved home', () => {
  assert.equal(tarsHome(HOME), HOME);
});

test('Workspaces.open → pipeline(role).complete drives through the mock', async () => {
  const ws = new Workspaces('arc');
  const handle = ws.open(WORKSPACE);
  assert.equal(handle.tool, 'arc');

  const pipe = handle.pipeline('critic');
  const r = await pipe.complete({ model: 'mock-model', user: 'review this' });
  assert.match(r.text, /handle spine ok/);
  assert.equal(typeof r.usage.inputTokens, 'number');

  // Re-open is cached: roots() reports exactly the one canonical root.
  const roots = ws.roots();
  assert.equal(roots.length, 1);

  const closed = ws.close(WORKSPACE);
  assert.equal(closed, true);
  assert.equal(ws.roots().length, 0);
});

test('provider(role) layer-1 also completes through the mock', async () => {
  const ws = new Workspaces('arc');
  const handle = ws.open(WORKSPACE);
  const prov = handle.provider('fixer');
  assert.equal(prov.role, 'fixer');
  const r = await prov.complete({ model: 'mock-model', user: 'fix this' });
  assert.match(r.text, /handle spine ok/);
  ws.closeAll();
});

test('context(ctx) yields a bound handle that still completes', async () => {
  const ws = new Workspaces('arc');
  const handle = ws.open(WORKSPACE);
  const scoped = handle.context({ session: 'sess-abc', tags: ['dogfood'] });
  const r = await scoped.pipeline('critic').complete({
    model: 'mock-model',
    user: 'review this',
  });
  assert.match(r.text, /handle spine ok/);
  ws.closeAll();
});

test('TarsHandle.standalone opens without a workspace', async () => {
  const handle = TarsHandle.standalone('arc', 'sess-standalone');
  const r = await handle
    .pipeline('critic')
    .complete({ model: 'mock-model', user: 'hi' });
  assert.match(r.text, /handle spine ok/);
  handle.close();
});

test('opening a missing path rejects with a discriminable .code', () => {
  const ws = new Workspaces('arc');
  try {
    ws.open('/no/such/path/exists/here');
    assert.fail('expected open() to throw');
  } catch (e) {
    // Typed code, not one opaque string (Doc 12 §7.3).
    assert.equal(e.code, 'TarsIoError');
  }
});
