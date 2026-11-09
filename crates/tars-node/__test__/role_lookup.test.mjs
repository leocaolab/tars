// b/138: `roleModel(role)` / `roleProvider(role)` read the `[roles]` mapping
// as tars-py's `role_model` / `role_provider` do.
// crates/tars-py/python/tests/test_role_lookup.py holds the Python half of this
// contract: the same config, the same roles, the same expected values.
//
// `init` is process-global; Node's test runner gives this file its own process.

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { init, roleModel, roleProvider } from '../index.js';

// Two providers, so a role that read the wrong entry would show it.
const CONFIG = `
[providers.mock_a]
type = "mock"
canned_response = "ok"

[providers.mock_b]
type = "mock"
canned_response = "ok"

[roles.critic]
provider = "mock_a"
model = "critic-model-1"

[roles.fixer]
provider = "mock_b"
model = "fixer-model-2"

[roles.drafter]
provider = "mock_a"
model = "no-such-series@latest"
`;

const HOME = mkdtempSync(join(tmpdir(), 'tars-node-role-lookup-'));
writeFileSync(join(HOME, 'config.toml'), CONFIG);
init(HOME);

test('roleProvider(role) is the provider id the role names', () => {
  assert.equal(roleProvider('critic'), 'mock_a');
  assert.equal(roleProvider('fixer'), 'mock_b');
});

test('roleModel(role) is the model the role names', () => {
  assert.equal(roleModel('critic'), 'critic-model-1');
  assert.equal(roleModel('fixer'), 'fixer-model-2');
});

test('roleModel(role) resolves a <series>@latest spec, refusing one that names no series', () => {
  assert.throws(
    () => roleModel('drafter'),
    (e) => e.code === 'TarsRegistryError' && /no-such-series/.test(e.message),
  );
});

test('an unmapped role is refused as TarsUnknownRole, naming it', () => {
  for (const lookup of [roleModel, roleProvider]) {
    assert.throws(
      () => lookup('no_such_role'),
      (e) => e.code === 'TarsUnknownRole' && /no_such_role/.test(e.message),
    );
  }
});
