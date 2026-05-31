// Smoke test for tars-node v0.1 scaffold.
//
// Runs under Node's builtin test runner: `node --test __test__/`.
// What it verifies is deliberately narrow — that the napi
// build / load / marshal round-trip is healthy:
//
//   1. The native addon loads (no "module did not self-register").
//   2. The `hello` export is callable, returns a String.
//   3. The `Pipeline` factory + getter round-trip a String.
//   4. `Pipeline.complete(...)` returns a Promise that resolves to
//      the v0.1 stub shape — confirming async + struct marshalling
//      across the boundary.
//
// What it does NOT verify: that a real LLM call works. That lands
// when v0.1.1 wires the actual Pipeline construction (right now
// `complete()` returns a synthetic echo).

import test from 'node:test';
import assert from 'node:assert/strict';
import { hello, Pipeline } from '../index.js';

test('hello() round-trips a string through napi', () => {
    assert.equal(hello('world'), 'tars-node says hi, world');
});

test('Pipeline.fromConfigPath stores the path verbatim (v0.1 stub)', () => {
    const p = Pipeline.fromConfigPath('/tmp/test.toml');
    assert.equal(p.configPath, '/tmp/test.toml');
});

test('Pipeline.complete() resolves with the stub shape', async () => {
    const p = Pipeline.fromConfigPath('/tmp/test.toml');
    const r = await p.complete({
        model: 'mock-model',
        user: 'hello world',
        responseSchemaStrict: true,
    });
    // Stub echoes the user text + model into the result.text so the
    // round-trip is verifiable without a real provider.
    assert.match(r.text, /tars-node v0\.1 stub/);
    assert.match(r.text, /mock-model/);
    assert.match(r.text, /hello world/);
    assert.equal(r.model, 'mock-model');
    assert.equal(r.stopReason, 'end_turn');
    // Usage object exists and is zeroed in v0.1.
    assert.equal(r.usage.inputTokens, 0);
    assert.equal(r.usage.outputTokens, 0);
});
