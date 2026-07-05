// Getting a schema-validated result out of tars from Node/TypeScript.
//
// The Node binding is thinner than Python: it exposes `responseSchema`
// (decode-time enforcement) but NOT in-pipeline output validators. So the
// pattern is:
//
//   1. `responseSchema`  — hand the JSON Schema to the provider's
//      structured-output mode; a strict-capable provider forces
//      conforming JSON, so `result.text` is clean by construction.
//   2. parse + shape-check in JS — JSON.parse(result.text) then validate
//      the shape yourself (plain JS here; swap in zod / ajv for a real
//      schema check). This is the Node analogue of Python's validator,
//      just run in your own code after the call instead of inside the
//      pipeline.
//
// By default it talks to a **cassette** provider (record-once /
// replay-forever, see examples/tars.toml), so it runs deterministically
// without a live model. Run from the repo root — the cassette path is
// cwd-relative.
//
//   # replay from the committed cassette (no live model):
//   node examples/node/schema-validation.mjs
//
//   # (re)record against live LM Studio, then it replays forever:
//   TARS_CASSETTE_RECORD=1 node examples/node/schema-validation.mjs
//
// Override with TARS_EXAMPLE_PROVIDER / TARS_EXAMPLE_CONFIG.

import { Pipeline } from '../../crates/tars-node/index.js';

const CONFIG = process.env.TARS_EXAMPLE_CONFIG ?? 'examples/tars.toml';
const PROVIDER = process.env.TARS_EXAMPLE_PROVIDER ?? 'cassette_schema';
const MODEL = 'qwen/qwen3-coder-30b';

const SCHEMA = {
  type: 'object',
  properties: {
    severity: { type: 'integer' },
    summary: { type: 'string' },
  },
  required: ['severity', 'summary'],
};

// Plain-JS structural check. Returns an error string, or null if `data`
// satisfies SCHEMA. (Swap for zod / ajv when you want a real validator.)
function checkShape(data) {
  if (typeof data !== 'object' || data === null || Array.isArray(data)) {
    return `expected an object, got ${Array.isArray(data) ? 'array' : typeof data}`;
  }
  for (const field of SCHEMA.required) {
    if (!(field in data)) return `missing required field '${field}'`;
  }
  if (!Number.isInteger(data.severity)) return 'severity must be an integer';
  if (typeof data.summary !== 'string') return 'summary must be a string';
  return null;
}

async function main() {
  const pipe = Pipeline.fromConfigPath(CONFIG, PROVIDER);

  // `responseSchema` (decode-time enforcement) needs a strict-capable
  // provider (Gemini / OpenAI / Anthropic); a local LM Studio model may
  // reject `response_format`, so this demo relies on a JSON-forcing prompt
  // + the JS shape check below. Against a cloud provider, add
  // `responseSchema: SCHEMA` here.
  const result = await pipe.complete({
    model: MODEL,
    system: 'You output ONLY a JSON object. No prose, no code fence.',
    user:
      'Rate the severity (0-10 integer) of this bug and summarize it in ' +
      'one sentence. Return an object with keys `severity` and `summary`.\n\n' +
      'BUG: unwrap() on a None in the request handler panics the whole ' +
      'worker on malformed input.',
    maxOutputTokens: 200,
  });

  console.log('── raw result.text ──');
  console.log(result.text);

  // Parse + shape-check — the Node-side "validator".
  let data;
  try {
    data = JSON.parse(result.text);
  } catch (e) {
    console.error(`reject: not JSON: ${e.message}; raw=${result.text.slice(0, 120)}`);
    process.exit(1);
  }
  const err = checkShape(data);
  if (err !== null) {
    console.error(`reject: ${err}; raw=${result.text.slice(0, 120)}`);
    process.exit(1);
  }

  console.log('── strong-typed-ish local value ──');
  console.log(`severity=${data.severity}  summary=${JSON.stringify(data.summary)}`);

  // Show the reject path on a deliberately-wrong shape (no extra call).
  const bad = checkShape(JSON.parse('{"severity": "high"}'));
  console.log('── shape check on bad value ──');
  console.log(`reason: ${bad}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
