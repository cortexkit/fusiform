import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { initSync, publish_step } from '../../../dist/publish-wasm/fusiform_publish_wasm.js';

const read = (path) => readFileSync(new URL(path, import.meta.url));
const models = JSON.parse(read('../../fusiform-module/data/models-dev-seed.json'));
const evidence = read('../fixtures/evidence.json');
const bytes = (value) => Buffer.from(JSON.stringify(value));
const { memory } = initSync({ module: new WebAssembly.Module(read('../../../dist/publish-wasm/fusiform_publish_wasm_bg.wasm')) });

if (process.argv[2] === 'prepare') {
  const empty = Buffer.concat([Buffer.from('CKFI\x01'), createHash('sha256').digest(), Buffer.alloc(8)]);
  const [, index] = publish_step(bytes(models), empty, evidence);
  assert.ok(index instanceof Uint8Array);
  process.stdout.write(index);
} else {
  test('a growth poll does not retain a second JSON tree for the diff', () => {
    // Prepare in another process: a Wasm instance keeps the pages it allocated
    // on a prior poll, so reusing it would hide this call's memory high-water.
    const prepare = spawnSync(process.execPath, [fileURLToPath(import.meta.url), 'prepare'], {
      maxBuffer: 32 * 1024 * 1024,
    });
    assert.equal(prepare.status, 0, prepare.error?.message ?? prepare.stderr.toString());
    for (const { models: rows } of Object.values(models)) {
      for (const [id, model] of Object.entries(rows)) rows[`${id}~copy-1`] = model;
    }
    const [summary, index] = publish_step(bytes(models), prepare.stdout, evidence);
    const decoded = JSON.parse(Buffer.from(summary));
    assert.ok(index instanceof Uint8Array, JSON.stringify(decoded.refusal));
    assert.equal(decoded.model_count, 20_020);
    assert.equal(decoded.ops.length, 219_401);
    // This threshold detects a regression toward the 316 MB full-tree encoder;
    // it does not claim the poll fits within a Worker's memory budget.
    assert.ok(memory.buffer.byteLength < 240 * 1024 * 1024,
      `growth poll used ${memory.buffer.byteLength} bytes of Wasm linear memory`);
  });
}
