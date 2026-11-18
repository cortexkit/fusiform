## Verify and vendor

In this directory, run `sha256sum -c SHA256SUMS` (macOS:
`shasum -a 256 -c SHA256SUMS`). Vendor `fusiform_publish_wasm_bg.wasm` and
`fusiform_publish_wasm.js` from the same verified artifact, without modifying
their bytes. The manifest checks integrity; get it from a trusted CI run.

In a Cloudflare Worker, configure `.wasm` imports as compiled WebAssembly
modules and initialize the glue once per isolate:

```js
import module from './fusiform_publish_wasm_bg.wasm';
import { initSync, publish_step } from './fusiform_publish_wasm.js';

initSync({ module });
```

## Byte API

All byte arguments and byte results are `Uint8Array`. JSON is UTF-8.
Release numbers and millisecond timestamps passed as arguments are `bigint`.

| Export | Inputs | Result |
| --- | --- | --- |
| `publish_step` | Complete models.dev response, persisted index, audio evidence | `[summaryBytes, newIndexBytes]` on success; `[summaryBytes, null]` on a refusal |
| `build_release` | `summary`, raw curation set, parent envelope (zero bytes for release 1), release number, publication time in ms, upstream observation time in ms | Canonical unsigned `{envelope, change}` or an in-band refusal |
| `snapshot_bytes` | models.dev, audio evidence, envelope, raw curation set, release signatures, history file list, history signatures, pinned release public key | Canonical signed snapshot or an in-band refusal |
| `snapshot_bytes_from_state` | Full `state.json`, envelope, release signatures, history file list, history signatures, pinned release public key | Canonical signed snapshot from an imported state or an in-band refusal |
| `history_day_bytes` | UTC day as UTF-8 `YYYY-MM-DD`, then signed release objects as JSON Lines | Canonical `{day, releases}` with signatures retained, or an in-band refusal |
| `canonical_json` | Any JSON value | Canonical bytes with curation number spellings preserved |
| `index_from_snapshot` | Complete `snapshot.json` | Opaque index. Reads `state.upstream` one value at a time and checks the envelope's upstream hash; does not verify signatures or curation. |
| `index_apply_change` | Index and the release's bare `change` object | New index after strict, atomic upstream replay. Curation does not affect index contents. |
| `upstream_hash` | Index | Validated upstream hash as a lowercase hex string |
| `envelope_signing_bytes` | Bare release `envelope` object | Exact Ed25519 message bytes checked by Rust `verify_envelope` |
| `head_signing_bytes` | Bare `head` body | Exact Ed25519 message bytes checked by Rust `verify_head` |
| `history_attestation_signing_bytes` | Bare history attestation body | Exact Ed25519 message bytes checked by Rust `verify_history_attestation` |

The publish summary is canonical JSON. Success has `ops`, `upstream_hash`,
`model_count` and `conflicts`. Each conflict identifies `provider`, `model`,
`fact` and `values` (distinct storage JSON strings for the omitted fact).
A refusal has just `ops: []`, `conflicts: []` and `refusal: {class, detail}`;
its classes are `empty_index`, `parse`, and `implausible`. Keep the previous
index on refusal. A zero-length index is not a bootstrap operation.

The evidence is canonical JSON `{"input-audio-per-token": <doc>,
"output-audio-per-token": <doc>}`. Use the two full documents from the selected
curation set's `vendor_facts`, not just the provider IDs. See the publisher
WASM crate's `fixtures/evidence.json` for an example from the embedded data.

The artifact builders return `{"refusal": {"class": ..., "detail": ...}}`
on failure, never a partial artifact. Other API errors (including malformed
indexes or evidence passed to `publish_step`) throw `Error` with a JSON
message `{ "class": ..., "detail": ... }`. Branch on `class`, not `detail`.

Pass the curation set as stored, without parsing and reserializing it in
JavaScript. Sign `envelope_signing_bytes` and attach `signatures` to the unsigned
release without reserializing the `change` through JavaScript numbers. An
unchanged curation version must have the parent's curation hash; if it matches
and there are also no upstream operations, there is nothing to publish.
The signed head's `latest_snapshot` must be between `1` and `release`
(inclusive); it locates `snapshots/<latest_snapshot>.json` without probing.

## Release 1 import

Run the native importer against a SQLite backup-API copy, not the live store:

```sh
cargo run --locked -p fusiform-publish --example import-release -- \
    backup.db new-output-dir <published-at-ms>
```

It writes canonical unsigned `release-1.json`, `state.json` and
`history/base.json`. The host signs the release envelope and the history
attestation over the hash of the exact `history/base.json` bytes, and passes
the unmodified `state.json` to `snapshot_bytes_from_state` to build snapshot 1.
The importer does not take a signing key. Imported legacy grades omit
`graded_effort`, which was not recorded in the SQLite store.

## History attestation

A day file requires a nonempty run of consecutive releases published on the
given UTC day. Release 1 belongs in the separately generated `history/base.json`.
The day builder preserves signatures; it does not verify them.

Pass history files as an ordered JSON array of `{"file": "base.json", "sha256": "…"}`
entries. The host selects the files that cover the snapshot's release and
hashes their exact bytes. Sign `history_attestation_signing_bytes` for
`{"release": n, "state_hash": "…", "history": [...]}` with the release key
and pass the resulting `{kid, sig}` as `history_signatures`. The pinned key
document is `{kid, public_key}`, where `public_key` is the raw 32-byte Ed25519
key encoded in unpadded base64url. The snapshot builder verifies both
signatures, verifies upstream and curation against the envelope, and places
the ordered list only in `history_attestation.body.history`.

For recovery, the host owns checking the snapshot signatures and curation,
replaying verified changes up to the current head, and comparing the recovered
`upstream_hash` with that release's upstream hash before polling. A history
consumer also compares each downloaded file's exact bytes with the attested
hash. Treat index bytes as opaque and only persist a replacement for a
successfully published head.

## Snapshot memory

`snapshot_bytes` is the heaviest call. On Linux with Node 24, the full seed
(6,735,471 input bytes) produced a 9,708,042-byte snapshot. Peak WASM linear
memory was 51.2 MiB; the host input and output buffers add about 15.7 MiB.
Node's whole-process peak RSS was 105.8 MiB, up from 40.1 MiB before the call.
Node RSS includes the runtime and does not measure a Worker isolate's limit.

To reproduce, build the WASM, run `cargo run --locked -p fusiform-publish
--example measure_seed -- prepare <work-dir>`, then run `node
crates/fusiform-publish-wasm/examples/measure.mjs <work-dir> snapshot` in a
fresh Node process.
