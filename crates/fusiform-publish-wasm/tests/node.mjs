import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { webcrypto } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import {
  initSync, envelope_signing_bytes, head_signing_bytes, index_apply_change,
  index_from_snapshot, publish_step, upstream_hash,
} from '../../../dist/publish-wasm/fusiform_publish_wasm.js';

const read = (path) => readFileSync(new URL(path, import.meta.url));
const releaseFixture = (name) => read(`../../fusiform-release/fixtures/release-format-v1/${name}`);
const bytes = (value) => Buffer.from(JSON.stringify(value));

initSync({ module: new WebAssembly.Module(read('../../../dist/publish-wasm/fusiform_publish_wasm_bg.wasm')) });
const snapshot = releaseFixture('snapshot.json');
const { envelope } = JSON.parse(snapshot);
const change = JSON.parse(releaseFixture('change.json')).change;
const { head } = JSON.parse(releaseFixture('head.json'));
const evidence = read('../fixtures/evidence.json');
const models = read('../fixtures/models-dev.json');

test('recover from the release snapshot, publish and replay the new change', () => {
  const prior = index_from_snapshot(snapshot);
  assert.ok(prior instanceof Uint8Array);
  assert.equal(upstream_hash(prior), envelope.upstream_hash);

  const [wire, next] = publish_step(models, prior, evidence);
  assert.ok(wire instanceof Uint8Array);
  assert.ok(next instanceof Uint8Array);
  const summary = JSON.parse(Buffer.from(wire));
  assert.deepEqual(Object.keys(summary), ['conflicts', 'model_count', 'ops', 'upstream_hash']);
  assert.deepEqual(wire, Uint8Array.from(bytes(summary)));
  assert.equal(summary.model_count, 2);
  assert.ok(summary.ops.some(({ op, fact }) => op === 'set' && fact === 'rate.input'));
  assert.equal(upstream_hash(next), summary.upstream_hash);
  const replayed = index_apply_change(prior, bytes({ upstream: summary.ops, curation_set: null }));
  assert.equal(upstream_hash(replayed), summary.upstream_hash);
  assert.notEqual(summary.upstream_hash, envelope.upstream_hash);
  const [repeat] = publish_step(models, next, evidence);
  assert.deepEqual(JSON.parse(Buffer.from(repeat)).ops, []);
});

function assertRefusal(result, reason) {
  const [wire, index] = result;
  assert.ok(wire instanceof Uint8Array);
  assert.equal(index, null);
  const summary = JSON.parse(Buffer.from(wire));
  assert.deepEqual(summary, {
    conflicts: [], ops: [], refusal: { class: reason, detail: summary.refusal?.detail },
  });
  assert.match(summary.refusal.detail, /\S/);
}

function assertError(action, reason) {
  assert.throws(action, (err) => {
    assert.ok(err instanceof Error);
    const { class: klass, detail, ...extra } = JSON.parse(err.message);
    assert.equal(klass, reason);
    assert.match(detail, /\S/);
    assert.deepEqual(extra, {});
    return true;
  });
}

test('publish domain refusals have no replacement index, hash or model count', () => {
  const prior = index_from_snapshot(snapshot);
  assertRefusal(publish_step(models, new Uint8Array(), evidence), 'empty_index');
  assertRefusal(publish_step(Buffer.from('{'), prior, evidence), 'parse');
  const hundred = Object.fromEntries(Array.from({ length: 100 }, (_, i) => [`model-${i}`, {}]));
  const [, largeIndex] = publish_step(bytes({ alpha: { models: hundred } }), prior, evidence);
  assert.ok(largeIndex instanceof Uint8Array);
  assertRefusal(publish_step(models, largeIndex, evidence), 'implausible');
});

test('boundary errors throw JSON; a late change refusal leaves the input alone', () => {
  const prior = index_from_snapshot(snapshot);
  const original = prior.slice();
  const invalid = Buffer.from('{');
  assertError(() => publish_step(models, invalid, evidence), 'invalid_index');
  assertError(() => publish_step(models, prior, bytes({})), 'invalid_evidence');
  const wrongSchema = JSON.parse(evidence);
  wrongSchema['output-audio-per-token'].schema = 'input-audio-per-token/v1';
  assertError(() => publish_step(models, prior, bytes(wrongSchema)), 'invalid_evidence');
  assertError(() => index_from_snapshot(invalid), 'invalid_snapshot');
  const tampered = JSON.parse(snapshot);
  tampered.state.upstream.zeta.models.mini['limit.context'] += 1;
  assertError(() => index_from_snapshot(bytes(tampered)), 'snapshot_hash_mismatch');
  assertError(() => index_apply_change(prior, invalid), 'invalid_change');
  const wrongPredecessor = structuredClone(change);
  wrongPredecessor.upstream[0].value = 'Must not stick';
  assertError(() => index_apply_change(prior, bytes(wrongPredecessor)), 'absent_fact');
  const op = change.upstream[0];
  assertError(() => index_apply_change(prior, bytes({ ...change, upstream: [op, op] })), 'unsorted_or_duplicate');
  assertError(() => index_apply_change(prior, bytes({ ...change, upstream: [{ ...op, provider: 'absent' }] })), 'unknown_provider');
  const removals = ['existence', 'rate.input'].map((fact) => ({ op: 'remove', provider: 'alpha', model: '-', fact }));
  assertError(() => index_apply_change(prior, bytes({ ...change, upstream: removals })), 'orphan_provider_facts');
  assert.deepEqual(prior, original);
  assert.equal(upstream_hash(prior), envelope.upstream_hash);
  assertError(() => upstream_hash(invalid), 'invalid_index');
  assertError(() => envelope_signing_bytes(invalid), 'invalid_envelope');
  assertError(() => head_signing_bytes(invalid), 'invalid_head');
});

test('native Rust matches WASM hashes and verifies Node WebCrypto signatures', async () => {
  // RFC 8410 PKCS#8 prefix and the same public test seed as the release fixtures.
  const pkcs8 = Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), Buffer.alloc(32, 42)]);
  const key = await webcrypto.subtle.importKey('pkcs8', pkcs8, 'Ed25519', false, ['sign']);
  const sign = async (message) => ({
    kid: 'release-format-v1-test-key',
    sig: Buffer.from(await webcrypto.subtle.sign('Ed25519', key, message)).toString('base64url'),
  });
  const envelopeBytes = envelope_signing_bytes(Buffer.from(JSON.stringify(envelope, null, 2)));
  const changed = { ...envelope, published_at_ms: envelope.published_at_ms + 1 };
  const changedBytes = envelope_signing_bytes(bytes(changed));
  assert.equal(changedBytes.length, envelopeBytes.length);
  assert.equal(envelopeBytes.filter((byte, i) => byte !== changedBytes[i]).length, 1);
  const previous = index_from_snapshot(snapshot);
  const [, next] = publish_step(models, previous, evidence);
  const result = spawnSync('cargo', ['run', '--quiet', '--locked', '-p', 'fusiform-publish-wasm', '--example', 'check_node'], {
    cwd: new URL('../../../', import.meta.url),
    input: bytes({
      recovered_hash: upstream_hash(previous),
      published_hash: upstream_hash(next),
      envelope_signature: await sign(envelopeBytes),
      changed_envelope: JSON.parse(Buffer.from(changedBytes)),
      head_signature: await sign(head_signing_bytes(bytes(head))),
    }),
    encoding: 'utf8',
  });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
});
