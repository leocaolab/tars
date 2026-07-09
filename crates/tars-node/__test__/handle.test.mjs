// Smoke test for the process-global spine (Doc 12 §6, post scope-facade):
// init → pipeline(role) / provider(role) → complete → context.
//
//   node --test __test__/handle.test.mjs
//
// The provider is `mock` — no network. We stand up a throwaway `$TARS_HOME`
// with a config that declares a mock provider + a `[roles]` map, so
// `init(home)` builds the global registry + role table from it, and the free
// `pipeline(role)` / `provider(role)` functions resolve roles against it.
//
// Config::load + ProviderRegistry::global are process-global OnceLocks; Node's
// test runner isolates each *file* in its own process, so this file's `init`
// starts from a clean slate.

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import {
  init,
  isInitialized,
  tarsHome,
  pipeline,
  provider,
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

test('pipeline(role).complete drives through the mock', async () => {
  const pipe = pipeline('critic');
  const r = await pipe.complete({ model: 'mock-model', user: 'review this' });
  assert.match(r.text, /handle spine ok/);
  assert.equal(typeof r.usage.inputTokens, 'number');
});

test('provider(role) layer-1 also completes through the mock', async () => {
  const prov = provider('fixer');
  assert.equal(prov.role, 'fixer');
  const r = await prov.complete({ model: 'mock-model', user: 'fix this' });
  assert.match(r.text, /handle spine ok/);
});

test('pipeline(role, ctx) binds an explicit context and still completes', async () => {
  const scoped = pipeline('critic', { session: 'sess-abc', tags: ['dogfood'] });
  const r = await scoped.complete({ model: 'mock-model', user: 'review this' });
  assert.match(r.text, /handle spine ok/);
});
