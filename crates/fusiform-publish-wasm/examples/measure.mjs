// Prepare the inputs with `cargo run --locked -p fusiform-publish --example
// measure_seed -- prepare <work-dir>`. Run each mode in a fresh Node process
// so retained linear-memory pages from recovery cannot affect publish results.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

import {
  initSync, index_from_snapshot, publish_step,
} from '../../../dist/publish-wasm/fusiform_publish_wasm.js';

const [workDir, mode] = process.argv.slice(2);
assert.ok(workDir && ['recovery', 'publish'].includes(mode), 'expected work-dir and recovery or publish');
const read = (name) => readFileSync(join(workDir, name));
const wasm = readFileSync(new URL('../../../dist/publish-wasm/fusiform_publish_wasm_bg.wasm', import.meta.url));
const { memory } = initSync({ module: new WebAssembly.Module(wasm) });
const index = read('index');
const evidence = readFileSync(new URL('../fixtures/evidence.json', import.meta.url));
const input = read(mode === 'recovery' ? 'snapshot.json' : 'one.json');

function measure(label, call) {
  const start = performance.now();
  const result = call();
  const wallMs = performance.now() - start;
  console.log(`${label}: wall_ms=${wallMs.toFixed(1)} memory_bytes=${memory.buffer.byteLength}`);
  return result;
}

if (mode === 'recovery') {
  const recovered = measure('index_from_snapshot', () => index_from_snapshot(input));
  assert.deepEqual(Buffer.from(recovered), index);
} else {
  const [wire, next] = measure('publish_step', () => publish_step(input, index, evidence));
  const summary = JSON.parse(Buffer.from(wire));
  assert.equal(summary.ops.length, 1);
  assert.equal(summary.ops[0].fact, 'model.name');
  assert.ok(next instanceof Uint8Array);
  console.log(`models=${summary.model_count} ops=${summary.ops.length} upstream_hash=${summary.upstream_hash}`);
}
