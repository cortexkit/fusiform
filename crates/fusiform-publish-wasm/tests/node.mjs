import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, webcrypto } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as wasm from '../../../dist/publish-wasm/fusiform_publish_wasm.js';

import {
  build_release, canonical_json, envelope_signing_bytes, head_signing_bytes,
  history_attestation_signing_bytes, history_day_bytes, index_apply_change,
  index_from_snapshot, initSync, publish_step, snapshot_bytes, upstream_hash,
} from '../../../dist/publish-wasm/fusiform_publish_wasm.js';

const read = (path) => readFileSync(new URL(path, import.meta.url));
const releaseFixture = (name) => read(`../../fusiform-release/fixtures/release-format-v1/${name}`);
const bytes = (value) => Buffer.from(JSON.stringify(value));
const sha256 = (wire) => createHash('sha256').update(wire).digest('hex');
const start = 1795046400000n;
const day = Buffer.from('2026-11-19');
const curation = read('../fixtures/curation.json');

initSync({ module: new WebAssembly.Module(read('../../../dist/publish-wasm/fusiform_publish_wasm_bg.wasm')) });
const snapshot = releaseFixture('snapshot.json');
const { envelope } = JSON.parse(snapshot);
const observedAt = BigInt(envelope.upstream_observed_at_ms);
const change = JSON.parse(releaseFixture('change.json')).change;
const { head } = JSON.parse(releaseFixture('head.json'));
const evidence = read('../fixtures/evidence.json');
const models = read('../fixtures/models-dev.json');

test('recover from the release snapshot, publish and replay the new change', () => {
  const prior = index_from_snapshot(snapshot);
  assert.ok(prior instanceof Uint8Array);
  assert.equal(upstream_hash(prior), envelope.upstream_hash);

  const [wire, next] = publish_step(models, prior, evidence, observedAt);
  assert.ok(wire instanceof Uint8Array);
  assert.ok(next instanceof Uint8Array);
  const summary = JSON.parse(Buffer.from(wire));
  assert.deepEqual(Object.keys(summary), ['base_upstream_hash', 'conflicts', 'findings', 'held_fact_count', 'model_count', 'next_date_boundary_ms', 'ops', 'upstream_hash']);
  assert.equal(summary.base_upstream_hash, envelope.upstream_hash);
  assert.deepEqual(wire, Uint8Array.from(bytes(summary)));
  assert.equal(summary.model_count, 2);
  assert.deepEqual(summary.findings, []);
  assert.equal(summary.held_fact_count, 0);
  assert.equal(summary.next_date_boundary_ms, null);
  assert.ok(summary.ops.some(({ op, fact }) => op === 'set' && fact === 'rate.input'));
  assert.equal(upstream_hash(next), summary.upstream_hash);
  const replayed = index_apply_change(prior, bytes({ upstream: summary.ops, curation_set: null }));
  assert.equal(upstream_hash(replayed), summary.upstream_hash);
  assert.notEqual(summary.upstream_hash, envelope.upstream_hash);
  const [repeat] = publish_step(models, next, evidence, observedAt);
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
  assertRefusal(publish_step(models, new Uint8Array(), evidence, observedAt), 'empty_index');
  assertRefusal(publish_step(Buffer.from('{'), prior, evidence, observedAt), 'parse');
  const hundred = Object.fromEntries(Array.from({ length: 100 }, (_, i) => [`model-${i}`, { type: 'chat', api: {}, capabilities: {} }]));
  const [, largeIndex] = publish_step(bytes({ alpha: { models: hundred } }), prior, evidence, observedAt);
  assert.ok(largeIndex instanceof Uint8Array);
  assertRefusal(publish_step(models, largeIndex, evidence, observedAt), 'implausible');
});

test('boundary errors throw JSON; a late change refusal leaves the input alone', () => {
  const prior = index_from_snapshot(snapshot);
  const original = prior.slice();
  const invalid = Buffer.from('{');
  assertError(() => publish_step(models, invalid, evidence, observedAt), 'invalid_index');
  assertError(() => publish_step(models, prior, bytes({}), observedAt), 'invalid_evidence');
  const wrongSchema = JSON.parse(evidence);
  wrongSchema['output-audio-per-token'].schema = 'input-audio-per-token/v1';
  assertError(() => publish_step(models, prior, bytes(wrongSchema), observedAt), 'invalid_evidence');
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
  const [, next] = publish_step(models, previous, evidence, observedAt);
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

function emptyIndex() {
  // Use recovery rather than making JavaScript depend on the opaque index format.
  return index_from_snapshot(bytes({
    envelope: { ...envelope, release: 1, parent: null, upstream_hash: sha256(Buffer.alloc(0)) },
    signatures: { kid: '', sig: '' }, state: { curation_set: null, upstream: {} },
  }));
}

async function signedRelease(sign, summary, raw, previous, number, time, observed) {
  const unsigned = Buffer.from(build_release(summary, raw, previous, number, time, observed));
  const body = JSON.parse(unsigned).envelope;
  assert.ok(body, unsigned.toString());
  assert.equal(BigInt(body.upstream_observed_at_ms), observed);
  const env = envelope_signing_bytes(bytes(body));
  const signatures = await sign(env);
  // Do not round-trip the release/change through JS numbers when adding a signature.
  const wire = Buffer.from(canonical_json(Buffer.concat([
    unsigned.subarray(0, -1), Buffer.from(',"signatures":'), bytes(signatures), Buffer.from('}'),
  ])));
  return { wire, body, env, signatures };
}

async function signedHistory(sign, release, base, daily) {
  const history = [
    { file: 'base.json', sha256: sha256(base) },
    { file: 'days/2026-11-19.json', sha256: sha256(daily) },
  ];
  const attestation = { release: release.body.release, state_hash: release.body.state_hash, history };
  const historySignature = await sign(history_attestation_signing_bytes(bytes(attestation)));
  return { history, historySignature };
}

function snapshotState(wire) {
  // Splice the last top-level field without rounding its JSON numbers.
  return wire.subarray(wire.indexOf(Buffer.from(',"state":')) + 9, -1);
}

test('rebuild the frozen release-format chain bytes from historical facts', async () => {
  const { sign, publicKey } = await testKey();
  let prior = emptyIndex();
  let parent = Buffer.alloc(0);
  let release;
  for (const number of [1, 2]) {
    const expected = releaseFixture(`chain/release-${number}.json`);
    const frozen = JSON.parse(expected);
    const next = index_apply_change(prior, bytes({ upstream: frozen.change.upstream, curation_set: null }));
    const summary = bytes({ base_upstream_hash: upstream_hash(prior), ops: frozen.change.upstream,
      upstream_hash: upstream_hash(next) });
    release = await signedRelease(sign, summary, curation, parent, BigInt(number),
      BigInt(frozen.envelope.published_at_ms), BigInt(frozen.envelope.upstream_observed_at_ms));
    assert.deepEqual(release.wire, expected);
    parent = release.env;
    prior = next;
  }
  const dayFile = Buffer.from(history_day_bytes(day, release.wire));
  assert.deepEqual(dayFile, releaseFixture('chain/days/2026-11-19.json'));
  const expected = releaseFixture('chain/snapshot.json');
  const { history, historySignature } = await signedHistory(sign, release, releaseFixture('chain/base.json'), dayFile);
  const actual = wasm.snapshot_bytes_from_state(snapshotState(expected), release.env,
    bytes(release.signatures), bytes(history), bytes(historySignature), publicKey);
  assert.deepEqual(Buffer.from(actual), expected);
});

test('build and sign the live v2 release chain, history and snapshot for the Rust consumer', async () => {
  const { sign, publicKey } = await testKey();
  const renamed = JSON.parse(models);
  renamed.zeta.models.mini.name = 'Mini 2';
  const changedModels = bytes(renamed);
  const [genesis, after1] = publish_step(models, emptyIndex(), evidence, start - 2n);
  const [changed, after2] = publish_step(changedModels, after1, evidence, start - 1n);
  const [unchanged] = publish_step(changedModels, after2, evidence, start + 86399998n);
  const newCuration = Buffer.from(curation.toString().replace('"version":1', '"version":2'));
  const r1 = await signedRelease(sign, genesis, curation, Buffer.alloc(0), 1n, start - 1n, start - 2n);
  const r2 = await signedRelease(sign, changed, curation, r1.env, 2n, start, start - 1n);
  const r3 = await signedRelease(sign, unchanged, newCuration, r2.env, 3n, start + 86399999n, start + 86399998n);
  const genesisOps = JSON.parse(r1.wire).change.upstream;
  const modelTypes = genesisOps.filter(({ fact }) => fact === 'model.type');
  assert.deepEqual(modelTypes, [
    { op: 'set', provider: 'alpha', model: '-', fact: 'model.type', value: 'chat' },
    { op: 'set', provider: 'zeta', model: 'mini', fact: 'model.type', value: 'chat' },
  ]);
  assert.deepEqual(genesisOps.filter(({ fact }) => fact !== 'model.type'),
    JSON.parse(releaseFixture('chain/release-1.json')).change.upstream);
  assert.deepEqual(JSON.parse(r2.wire).change.upstream,
    [{ op: 'set', provider: 'zeta', model: 'mini', fact: 'model.name', value: 'Mini 2' }]);
  assert.deepEqual(JSON.parse(r3.wire).change.upstream, []);
  const dayFile = Buffer.from(history_day_bytes(day, r2.wire));
  const fullDay = Buffer.from(history_day_bytes(day, Buffer.concat([r2.wire, Buffer.from('\n'), r3.wire])));
  assert.deepEqual(JSON.parse(fullDay).releases.map((r) => r.envelope.release), [2, 3]);

  const base = JSON.parse(releaseFixture('chain/base.json'));
  const eraTemplate = base.tables.era[0];
  base.tables.era = genesisOps.map(({ provider, model, fact, value }, i) => ({
    ...eraTemplate, id: i + 1, provider_id: provider, model_id: model, fact_key: fact,
    value_json: JSON.stringify(value), boundary_at_ms: r1.body.upstream_observed_at_ms,
  }));
  const baseFile = Buffer.from(canonical_json(bytes(base)));
  const { history, historySignature } = await signedHistory(sign, r2, baseFile, dayFile);
  const snapshot = Buffer.from(snapshot_bytes(changedModels, evidence, r2.env, curation,
    bytes(r2.signatures), bytes(history), bytes(historySignature), publicKey));
  assert.deepEqual(Buffer.from(wasm.snapshot_bytes_from_state(snapshotState(snapshot), r2.env,
    bytes(r2.signatures), bytes(history), bytes(historySignature), publicKey)), snapshot);
  assert.equal(upstream_hash(index_from_snapshot(snapshot)), upstream_hash(after2));
  const refusal = (wire) => JSON.parse(Buffer.from(wire)).refusal.class;
  assert.equal(refusal(build_release(unchanged, curation, r2.env, 3n, start + 86399999n, start + 86399998n)), 'nothing_to_publish');
  assert.equal(refusal(build_release(unchanged, curation, r1.env, 3n, start + 86399999n, start + 86399998n)), 'invalid_parent');
  assert.equal(refusal(history_day_bytes(day, r1.wire)), 'history_import_release');
  assert.equal(refusal(snapshot_bytes(changedModels, evidence, r2.env, curation,
    bytes(r2.signatures), bytes(history.toReversed()), bytes(historySignature), publicKey)), 'invalid_history_signature');

  checkInRust({
    releases: [r1, r2, r3].map((r) => r.wire.toString()),
    snapshot: snapshot.toString(),
    base: baseFile.toString(),
    day: dayFile.toString(),
  }, 'chain');
});

test('snapshot from state streams the signed upstream and rejects tampering', async () => {
  const { publicKey, sign } = await testKey();
  const expected = releaseFixture('chain/snapshot.json');
  const { envelope, state, signatures, history_attestation: attestation } = JSON.parse(expected);
  const build = (input, releaseSignatures = signatures, historySignatures = attestation.signatures) =>
    Buffer.from(wasm.snapshot_bytes_from_state(input, bytes(envelope), bytes(releaseSignatures),
      bytes(attestation.body.history), bytes(historySignatures), publicKey));
  const stateWire = snapshotState(expected);
  assert.deepEqual(build(stateWire), expected);

  const refusal = (wire) => JSON.parse(wire).refusal.class;
  const changedUpstream = Buffer.from(stateWire.toString().replace('"existence":"present"', '"existence":"withdrawn"'));
  assert.equal(refusal(build(changedUpstream)), 'snapshot_hash_mismatch');
  assert.equal(refusal(build(bytes({ ...state, curation_set: { ...state.curation_set, version: 99 } }))), 'snapshot_curation_mismatch');
  assert.equal(refusal(build(stateWire, attestation.signatures)), 'invalid_release_signature');
  assert.equal(refusal(build(stateWire, signatures, signatures)), 'invalid_history_signature');

  // The other fixture has provider-level facts. They hash before model facts
  // but serialize after models in a canonical provider object.
  const providerFixture = JSON.parse(releaseFixture('snapshot.json'));
  const body = { release: providerFixture.envelope.release, state_hash: providerFixture.envelope.state_hash,
    history: attestation.body.history };
  const providerHistorySignature = await sign(history_attestation_signing_bytes(bytes(body)));
  const providerSnapshot = wasm.snapshot_bytes_from_state(bytes(providerFixture.state),
    bytes(providerFixture.envelope), bytes(providerFixture.signatures), bytes(body.history),
    bytes(providerHistorySignature), publicKey);
  assert.deepEqual(providerSnapshot, canonical_json(bytes({ ...providerFixture,
    history_attestation: { body, signatures: providerHistorySignature } })));
});
