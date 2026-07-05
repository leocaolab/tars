// Cassette-backed regression test — the A/B code-change axis as a *test*,
// not a CLI command. The LLM is pinned by a committed cassette
// (examples/tars.toml → `cassette_schema`), so this asserts on a real
// model's reply deterministically with NO live provider — it runs in CI.
//
// Bless when the pinned reply should change: re-record with
// TARS_CASSETTE_RECORD=1 (needs LM Studio once), commit the new cassette;
// the git diff of the .json is the review surface.
//
//   node --test crates/tars-node/__test__/ab_cassette.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { Pipeline } from '../index.js';

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');

// Must match the recorded request byte-for-byte to replay.
const SYSTEM = 'You output ONLY a JSON object. No prose, no code fence.';
const USER =
  'Rate the severity (0-10 integer) of this bug and summarize it in ' +
  'one sentence. Return an object with keys `severity` and `summary`.\n\n' +
  'BUG: unwrap() on a None in the request handler panics the whole ' +
  'worker on malformed input.';

async function completePinned() {
  // Cassette paths in tars.toml are cwd-relative → run from repo root.
  process.chdir(REPO_ROOT);
  const pipe = Pipeline.fromConfigPath('examples/tars.toml', 'cassette_schema');
  return pipe.complete({
    model: 'qwen/qwen3-coder-30b',
    system: SYSTEM,
    user: USER,
    maxOutputTokens: 200,
  });
}

test('pinned reply is schema-valid', async () => {
  const data = JSON.parse((await completePinned()).text);
  assert.equal(typeof data.severity, 'number');
  assert.ok(Number.isInteger(data.severity));
  assert.equal(typeof data.summary, 'string');
});

test('pinned reply is deterministic (byte-identical replay)', async () => {
  const a = (await completePinned()).text;
  const b = (await completePinned()).text;
  assert.equal(a, b);
});

test('severity bucket snapshot (bless by updating on intended change)', async () => {
  const severity = JSON.parse((await completePinned()).text).severity;
  const bucket = (s) => (s >= 9 ? 'critical' : s >= 7 ? 'high' : 'moderate');
  assert.equal(bucket(severity), 'high'); // pinned severity is 8
});
