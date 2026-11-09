// `init` installs nothing until every piece of it was built, so an `init` that failed can be
// called again. crates/tars-py/python/tests/test_init_retry.py is the Python half.
//
// `init` is process-global; Node's test runner gives this file its own process.

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { init, isInitialized, roleProvider } from '../index.js';

// The registry build resolves every provider's default model; this one names no series.
const BROKEN = `
[providers.broken]
type = "openai"
auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
default_model = "no-such-series@latest"
`;

const GOOD = `
[providers.mock_a]
type = "mock"
canned_response = "ok"

[roles.critic]
provider = "mock_a"
model = "m"
`;

function home(config) {
  const dir = mkdtempSync(join(tmpdir(), 'tars-node-init-retry-'));
  writeFileSync(join(dir, 'config.toml'), config);
  return dir;
}

test('an init whose registry does not build installs nothing, and a later init succeeds', () => {
  assert.throws(
    () => init(home(BROKEN)),
    (e) => e.code === 'TarsRegistryError' && /no-such-series/.test(e.message),
  );
  assert.equal(isInitialized(), false);

  init(home(GOOD));
  assert.equal(isInitialized(), true);
  assert.equal(roleProvider('critic'), 'mock_a');
});
