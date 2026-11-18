import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, webcrypto } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import {
  build_release, canonical_json, envelope_signing_bytes, head_signing_bytes,
  history_attestation_signing_bytes, history_day_bytes, index_apply_change,
  index_from_snapshot, initSync, publish_step, snapshot_bytes, upstream_hash,
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
  const { sign } = await testKey();
  const envelopeBytes = envelope_signing_bytes(Buffer.from(JSON.stringify(envelope, null, 2)));
  const changed = { ...envelope, published_at_ms: envelope.published_at_ms + 1 };
  const changedBytes = envelope_signing_bytes(bytes(changed));
  assert.equal(changedBytes.length, envelopeBytes.length);
  assert.equal(envelopeBytes.filter((byte, i) => byte !== changedBytes[i]).length, 1);
  const previous = index_from_snapshot(snapshot);
  const [, next] = publish_step(models, previous, evidence);
  checkInRust({
    recovered_hash: upstream_hash(previous),
    published_hash: upstream_hash(next),
    envelope_signature: await sign(envelopeBytes),
    changed_envelope: JSON.parse(Buffer.from(changedBytes)),
    head_signature: await sign(head_signing_bytes(bytes(head))),
  });
});

async function testKey() {
  // RFC 8410 PKCS#8 prefix and the same public test seed as the release fixtures.
  const pkcs8 = Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), Buffer.alloc(32, 42)]);
  const key = await webcrypto.subtle.importKey('pkcs8', pkcs8, 'Ed25519', true, ['sign']);
  const { x } = await webcrypto.subtle.exportKey('jwk', key);
  const kid = 'release-format-v1-test-key';
  return {
    publicKey: bytes({ kid, public_key: x }),
    sign: async (message) => ({ kid, sig: Buffer.from(await webcrypto.subtle.sign('Ed25519', key, message)).toString('base64url') }),
  };
}

function checkInRust(input, ...args) {
  const result = spawnSync('cargo', ['run', '--quiet', '--locked', '-p', 'fusiform-publish-wasm', '--example', 'check_node', '--', ...args], {
    cwd: new URL('../../../', import.meta.url), input: bytes(input), encoding: 'utf8',
  });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
}

test('build and sign the release chain, history and snapshot for the Rust consumer', async () => {
  const { sign, publicKey } = await testKey();
  const start = 1795046400000n;
  const day = Buffer.from('2026-11-19');
  const sha256 = (wire) => createHash('sha256').update(wire).digest('hex');
  const curation = read('../fixtures/curation.json');
  const renamed = JSON.parse(models);
  renamed.zeta.models.mini.name = 'Mini 2';
  const changedModels = bytes(renamed);
  // The index has no facts before the import. Use the recovery API instead of
  // having JavaScript know how to encode an opaque index.
  const emptyIndex = index_from_snapshot(bytes({
    envelope: { ...envelope, release: 1, parent: null, upstream_hash: sha256(Buffer.alloc(0)) },
    signatures: { kid: '', sig: '' }, state: { curation_set: null, upstream: {} },
  }));
  const [genesis, after1] = publish_step(models, emptyIndex, evidence);
  const [changed, after2] = publish_step(changedModels, after1, evidence);
  const [unchanged] = publish_step(changedModels, after2, evidence);

  async function release(summary, raw, previous, number, time) {
    const unsigned = Buffer.from(build_release(summary, raw, previous, number, time, time - 1n));
    const body = JSON.parse(unsigned).envelope;
    assert.ok(body, unsigned.toString());
    const env = envelope_signing_bytes(bytes(body));
    const signatures = await sign(env);
    // Do not round-trip the release/change through JS numbers when adding a signature.
    const wire = Buffer.from(canonical_json(Buffer.concat([
      unsigned.subarray(0, -1), Buffer.from(',"signatures":'), bytes(signatures), Buffer.from('}'),
    ])));
    return { wire, body, env, signatures };
  }
  const r1 = await release(genesis, curation, Buffer.alloc(0), 1n, start - 1n);
  const r2 = await release(changed, curation, r1.env, 2n, start);
  const newCuration = Buffer.from(curation.toString().replace('"version":1', '"version":2'));
  const r3 = await release(unchanged, newCuration, r2.env, 3n, start + 86399999n);
  assert.deepEqual(r1.wire, releaseFixture('chain/base.json'));
  assert.deepEqual(r2.wire, releaseFixture('chain/release-2.json'));
  const dayFile = Buffer.from(history_day_bytes(day, r2.wire));
  assert.deepEqual(dayFile, releaseFixture('chain/days/2026-11-19.json'));
  const fullDay = Buffer.from(history_day_bytes(day, Buffer.concat([r2.wire, Buffer.from('\n'), r3.wire])));
  assert.deepEqual(JSON.parse(fullDay).releases.map((r) => r.envelope.release), [2, 3]);

  const history = [
    { file: 'base.json', sha256: sha256(r1.wire) },
    { file: 'days/2026-11-19.json', sha256: sha256(dayFile) },
  ];
  const attestation = { release: r2.body.release, state_hash: r2.body.state_hash, history };
  const historySignature = await sign(history_attestation_signing_bytes(bytes(attestation)));
  const snapshot = Buffer.from(snapshot_bytes(changedModels, evidence, r2.env, curation,
    bytes(r2.signatures), bytes(history), bytes(historySignature), publicKey));
  assert.deepEqual(snapshot, releaseFixture('chain/snapshot.json'));
  assert.equal(upstream_hash(index_from_snapshot(snapshot)), upstream_hash(after2));
  const refusal = (wire) => JSON.parse(Buffer.from(wire)).refusal.class;
  assert.equal(refusal(build_release(unchanged, curation, r2.env, 3n, start, start)), 'nothing_to_publish');
  assert.equal(refusal(build_release(unchanged, curation, r1.env, 3n, start, start)), 'invalid_parent');
  assert.equal(refusal(history_day_bytes(day, r1.wire)), 'history_import_release');
  assert.equal(refusal(snapshot_bytes(changedModels, evidence, r2.env, curation,
    bytes(r2.signatures), bytes(history.toReversed()), bytes(historySignature), publicKey)), 'invalid_history_signature');

  checkInRust({
    releases: [r1, r2, r3].map((r) => r.wire.toString()),
    snapshot: snapshot.toString(),
    day: dayFile.toString(),
  }, 'chain');
});
