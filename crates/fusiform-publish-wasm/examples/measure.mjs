// Prepare the inputs with `cargo run --locked -p fusiform-publish --example
// measure_seed -- prepare <work-dir>`. Run each mode in a fresh Node process
// so retained linear-memory pages from recovery cannot affect publish results.
import assert from 'node:assert/strict';
import { webcrypto } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

import {
  history_attestation_signing_bytes, initSync, index_from_snapshot, publish_step, snapshot_bytes,
} from '../../../dist/publish-wasm/fusiform_publish_wasm.js';

const [workDir, mode] = process.argv.slice(2);
assert.ok(workDir && ['recovery', 'publish', 'snapshot'].includes(mode), 'expected work-dir and recovery, publish or snapshot');
const read = (name) => readFileSync(join(workDir, name));
const wasm = readFileSync(new URL('../../../dist/publish-wasm/fusiform_publish_wasm_bg.wasm', import.meta.url));
const { memory } = initSync({ module: new WebAssembly.Module(wasm) });

function measure(label, call) {
  const start = performance.now();
  const result = call();
  const wallMs = performance.now() - start;
  console.log(`${label}: wall_ms=${wallMs.toFixed(1)} wasm_peak_bytes=${memory.buffer.byteLength} node_peak_rss_kib=${process.resourceUsage().maxRSS}`);
  return result;
}

if (mode === 'recovery') {
  const input = read('snapshot.json');
  const recovered = measure('index_from_snapshot', () => index_from_snapshot(input));
  assert.deepEqual(Buffer.from(recovered), read('index'));
} else if (mode === 'publish') {
  const index = read('index');
  const evidence = readFileSync(new URL('../fixtures/evidence.json', import.meta.url));
  const input = read('one.json');
  const [wire, next] = measure('publish_step', () => publish_step(input, index, evidence));
  const summary = JSON.parse(Buffer.from(wire));
  assert.equal(summary.ops.length, 1);
  assert.equal(summary.ops[0].fact, 'model.name');
  assert.ok(next instanceof Uint8Array);
  console.log(`models=${summary.model_count} ops=${summary.ops.length} upstream_hash=${summary.upstream_hash}`);
} else {
  const data = (name) => readFileSync(new URL(`../../fusiform-module/data/${name}.json`, import.meta.url));
  const models = data('models-dev-seed');
  const evidence = Buffer.concat([
    Buffer.from('{"input-audio-per-token":'), data('input-audio-per-token'),
    Buffer.from(',"output-audio-per-token":'), data('output-audio-per-token'), Buffer.from('}'),
  ]);
  const envelope = read('snapshot-envelope.json');
  const { release, state_hash } = JSON.parse(envelope);
  const curation = read('snapshot-curation.json');
  const signatures = read('snapshot-signatures.json');
  const { kid } = JSON.parse(signatures);
  const bytes = (value) => Buffer.from(JSON.stringify(value));
  const history = [];
  const pkcs8 = Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), Buffer.alloc(32, 42)]);
  const key = await webcrypto.subtle.importKey('pkcs8', pkcs8, 'Ed25519', true, ['sign']);
  const { x: public_key } = await webcrypto.subtle.exportKey('jwk', key);
  const preimage = history_attestation_signing_bytes(bytes({ release, state_hash, history }));
  const sig = Buffer.from(await webcrypto.subtle.sign('Ed25519', key, preimage)).toString('base64url');
  console.log(`snapshot_input_bytes=${models.length} node_before_rss_kib=${process.resourceUsage().maxRSS}`);
  const snapshot = measure('snapshot_bytes', () => snapshot_bytes(models, evidence, envelope, curation,
    signatures, bytes(history), bytes({ kid, sig }), bytes({ kid, public_key })));
  // Compare the recovered snapshot index with the expected index after measuring.
  assert.deepEqual(Buffer.from(index_from_snapshot(snapshot)), read('index'));
  console.log(`snapshot_bytes=${snapshot.length}`);
}
