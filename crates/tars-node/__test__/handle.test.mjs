// Smoke test for the process-global spine (Doc 12 §6, post scope-facade):
// init → pipeline(role, ctx) / provider(role, ctx) → complete.
//
//   node --test __test__/handle.test.mjs
//
// The provider is `mock` — no network. We stand up a throwaway `$TARS_HOME`
// with a config that declares a mock provider + a `[roles]` map, so
// `init(home)` builds the global registry + role table from it, and the free
// `pipeline(role, ctx)` / `provider(role, ctx)` functions resolve roles against it.
//
// The config and the binding's registry are process-global OnceLocks; Node's
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

// The tenant every handle in this file is bound to.
const TENANT = 'test-tenant';

const HOME = mkdtempSync(join(tmpdir(), 'tars-node-home-'));
writeFileSync(
  join(HOME, 'config.toml'),
  `
[providers.mock]
type = "mock"
canned_response = "handle spine ok"

[roles.critic]
provider = "mock"
model = "mock-model"

[roles.fixer]
provider = "mock"
model = "mock-model"
`,
);

test('init(home) loads the global config from a custom tars home', () => {
  assert.equal(isInitialized(), false);
  init(HOME);
  assert.equal(isInitialized(), true);
  // Not idempotent: the process already runs against a config this call did
  // not provide.
  assert.throws(() => init(HOME), (e) => e.code === 'TarsConfigError');
});

test('tarsHome(home) echoes the resolved home', () => {
  assert.equal(tarsHome(HOME), HOME);
});

test('pipeline(role).complete drives through the mock', async () => {
  const pipe = pipeline('critic', { tenant: TENANT });
  const r = await pipe.complete({ model: 'mock-model', user: 'review this' });
  assert.match(r.text, /handle spine ok/);
  assert.equal(typeof r.usage.inputTokens, 'number');
});

test('provider(role) layer-1 also completes through the mock', async () => {
  const prov = provider('fixer', { tenant: TENANT });
  assert.equal(prov.role, 'fixer');
  const r = await prov.complete({ model: 'mock-model', user: 'fix this' });
  assert.match(r.text, /handle spine ok/);
});

test('pipeline(role, ctx) binds an explicit context and still completes', async () => {
  const scoped = pipeline('critic', { tenant: TENANT, session: '01900000-0000-7000-8000-000000000abc', tags: ['dogfood'] });
  const r = await scoped.complete({ model: 'mock-model', user: 'review this' });
  assert.match(r.text, /handle spine ok/);
});

// b/122: a handle has no tenant of its own — resolving one without the
// caller's is refused, naming what is missing.
test('pipeline / provider refuse a ctx without a tenant', () => {
  assert.throws(() => pipeline('critic'), (e) => e.code === 'TarsNoTenant' && /tenant/.test(e.message));
  assert.throws(() => provider('fixer'), (e) => e.code === 'TarsNoTenant');
  assert.throws(() => pipeline('critic', { session: 's' }), /tenant/);
  assert.throws(() => provider('fixer', { session: 's' }), /tenant/);
  assert.throws(
    () => pipeline('critic', { tenant: '' }),
    (e) => e.code === 'TarsInvalidTenant' && /ctx\.tenant/.test(e.message),
  );
});

// b/136: an empty session / trace is refused by name at the boundary — not a
// panic in the id constructor, and not read as "absent".
test('pipeline / provider refuse an empty session or trace, naming the field', () => {
  assert.throws(
    () => pipeline('critic', { tenant: TENANT, session: '' }),
    (e) => e.code === 'TarsInvalidSession' && /ctx\.session/.test(e.message),
  );
  assert.throws(
    () => provider('fixer', { tenant: TENANT, session: '' }),
    (e) => e.code === 'TarsInvalidSession' && /ctx\.session/.test(e.message),
  );
  assert.throws(
    () => pipeline('critic', { tenant: TENANT, trace: '' }),
    (e) => e.code === 'TarsInvalidTrace' && /ctx\.trace/.test(e.message),
  );
  assert.throws(
    () => provider('fixer', { tenant: TENANT, trace: '' }),
    (e) => e.code === 'TarsInvalidTrace' && /ctx\.trace/.test(e.message),
  );
});
