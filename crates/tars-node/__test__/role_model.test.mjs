// b/132: a role-built handle runs on the model its caller names, as tars-py's
// role path does (crates/tars-py/python/tests/test_role_model.py holds the
// Python half of this contract against the same cassette, role config and
// calls). The role deliberately names a model the cassette never recorded, so
// a handle that bound the role's model instead of the caller's MISSes.
//
// `init` is process-global; Node's test runner gives this file its own process.

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { init, pipeline, provider } from '../index.js';

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const CASSETTE = join(REPO_ROOT, 'examples/cassettes/schema-validation.cassette.json');
const TENANT = 'test-tenant';

// The model the cassette recorded, and one it did not: the role's.
const CALLER_MODEL = 'qwen/qwen3-coder-30b';
const ROLE_MODEL = 'role-model-never-recorded';

// The exact request the cassette recorded (see ab_cassette.test.mjs).
const CALL = {
  system: 'You output ONLY a JSON object. No prose, no code fence.',
  user:
    'Rate the severity (0-10 integer) of this bug and summarize it in ' +
    'one sentence. Return an object with keys `severity` and `summary`.\n\n' +
    'BUG: unwrap() on a None in the request handler panics the whole ' +
    'worker on malformed input.',
  maxOutputTokens: 200,
};

/**
 * The reply the cassette recorded for a request keyed to `model` — the
 * expectation both bindings assert.
 * @param {string} model
 * @returns {string}
 */
function recordedReply(model) {
  const { recordings } = JSON.parse(readFileSync(CASSETTE, 'utf8'));
  const hit = Object.values(recordings).find((r) => r.request.startsWith(`model=${model}\0`));
  assert.ok(hit, `the cassette has no recording keyed to ${model}`);
  return hit.events
    .filter((e) => e.type === 'delta')
    .map((e) => e.text)
    .join('');
}

const HOME = mkdtempSync(join(tmpdir(), 'tars-node-role-model-'));
writeFileSync(
  join(HOME, 'config.toml'),
  `
[providers.cassette_schema]
type = "cassette"
path = ${JSON.stringify(CASSETTE)}

[roles.critic]
provider = "cassette_schema"
model = "${ROLE_MODEL}"
`,
);
init(HOME);

test('pipeline(role).complete({ model }) runs on the caller\'s model, not the role\'s', async () => {
  const pipe = pipeline('critic', { tenant: TENANT });
  const r = await pipe.complete({ model: CALLER_MODEL, ...CALL });
  assert.equal(r.text, recordedReply(CALLER_MODEL));
});

test('provider(role).complete({ model }) runs on the caller\'s model, not the role\'s', async () => {
  const prov = provider('critic', { tenant: TENANT });
  const r = await prov.complete({ model: CALLER_MODEL, ...CALL });
  assert.equal(r.text, recordedReply(CALLER_MODEL));
});

test('a role-built handle refuses a call that names no model', async () => {
  const pipe = pipeline('critic', { tenant: TENANT });
  const prov = provider('critic', { tenant: TENANT });
  await assert.rejects(async () => pipe.complete({ ...CALL }), /model/);
  await assert.rejects(async () => prov.complete({ ...CALL }), /model/);
});
