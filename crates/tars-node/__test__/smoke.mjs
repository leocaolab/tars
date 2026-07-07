// Smoke test for tars-node M2 — verifies the napi marshalling
// round-trip + real Pipeline construction through a `mock` provider.
//
// Runs under Node's builtin test runner: `node --test __test__/`.
//
// Coverage:
//   1. The native addon loads (no "module did not self-register").
//   2. `hello()` round-trips a string through napi.
//   3. `Pipeline.fromStr(toml, providerId)` constructs over the inline
//      TOML — proves the real config-load + provider-registry +
//      pipeline-builder chain works end-to-end without an external
//      config file.
//   4. `Pipeline.complete({...})` returns a Promise that resolves to
//      a `{ text, usage, model, stopReason }` shape — proves async +
//      struct marshalling + the LlmService drive loop work.
//
// The provider is `mock` — `tars_provider::backends::mock::MockProvider`
// returns a canned response without touching the network. To exercise
// a real provider, swap the `type` + add the auth block.

import test from 'node:test';
import assert from 'node:assert/strict';
import { hello, Pipeline } from '../index.js';

const MOCK_CONFIG = `
[providers.demo]
type = "mock"
canned_response = "tars-node smoke test ok"
`;

test('hello() round-trips a string through napi', () => {
    assert.equal(hello('world'), 'tars-node says hi, world');
});

test('Pipeline.fromStr(toml, providerId) builds against the mock provider', () => {
    const p = Pipeline.fromStr(MOCK_CONFIG, 'demo');
    assert.equal(p.id, 'demo');
});

test('Pipeline.complete() drives through the LlmService and returns the result shape', async () => {
    const p = Pipeline.fromStr(MOCK_CONFIG, 'demo');
    const r = await p.complete({
        model: 'mock-model',
        user: 'hello world',
        maxOutputTokens: 100,
    });
    // Mock provider's canned response surfaces in `text`.
    assert.match(r.text, /smoke test ok/);
    // model field surfaces post-routing — for mock it's the literal
    // model name we sent in.
    assert.ok(r.model);
    // Usage object exists and is numeric.
    assert.equal(typeof r.usage.inputTokens, 'number');
    assert.equal(typeof r.usage.outputTokens, 'number');
});

test('Pipeline.complete() rejects on both `user` and `messages` set', async () => {
    const p = Pipeline.fromStr(MOCK_CONFIG, 'demo');
    await assert.rejects(
        () =>
            p.complete({
                model: 'mock-model',
                user: 'hi',
                messages: [{ role: 'user', content: 'hi' }],
            }),
        /either `user`.*or `messages`/,
    );
});
