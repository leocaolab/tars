// The unknown-role path needs a global config with MORE THAN ONE provider and
// NO `default` tier — otherwise the sole-provider fallback (rule 5) or the
// default-tier fallback (rule 4) would absorb an unmapped role and mask it.
//
// `init` / `ProviderRegistry::global` are process-global OnceLocks; Node's test
// runner isolates each *file* in its own process, so this file installs its own
// two-provider config independently of `handle.test.mjs` (whose single-provider
// config would make sole-provider fallback swallow every role).
//
//   node --test __test__/unknown_role.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { init, Workspaces } from '../index.js';

const HOME = mkdtempSync(join(tmpdir(), 'tars-node-unknown-home-'));
writeFileSync(
  join(HOME, 'config.toml'),
  `
[providers.mock1]
type = "mock"
canned_response = "ok"

[providers.mock2]
type = "mock"
canned_response = "ok"

[roles]
critic = "mock1"
`,
);

const WORKSPACE = mkdtempSync(join(tmpdir(), 'tars-node-unknown-ws-'));

test('an unmapped role throws with a discriminable .code = TarsUnknownRole', () => {
  init(HOME);
  const ws = new Workspaces('arc');
  const handle = ws.open(WORKSPACE);

  // Mapped role still resolves through the flat [roles] map.
  assert.equal(handle.provider('critic').role, 'critic');

  // Unmapped: not in [roles], not a tier, not a literal provider id, no
  // `default` tier, and >1 provider so no sole-provider fallback → typed error.
  try {
    handle.pipeline('no_such_role');
    assert.fail('expected pipeline() to throw for an unmapped role');
  } catch (e) {
    assert.equal(e.code, 'TarsUnknownRole');
  }
  ws.closeAll();
});
