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

| Export | Inputs | Result |
| --- | --- | --- |
| `publish_step` | Complete models.dev response, persisted index, audio evidence | `[summaryBytes, newIndexBytes]` on success; `[summaryBytes, null]` on a refusal |
| `index_from_snapshot` | Complete `snapshot.json` | Opaque index. Reads `state.upstream` one value at a time and checks the envelope's upstream hash; does not verify signatures or curation. |
| `index_apply_change` | Index and the release's bare `change` object | New index after strict, atomic upstream replay. Curation does not affect index contents. |
| `upstream_hash` | Index | Validated upstream hash as a lowercase hex string |
| `envelope_signing_bytes` | Bare release `envelope` object | Exact Ed25519 message bytes checked by Rust `verify_envelope` |
| `head_signing_bytes` | Bare `head` body | Exact Ed25519 message bytes checked by Rust `verify_head` |

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

Malformed indexes or evidence, and all errors outside `publish_step`, throw
`Error` with a JSON message `{ "class": ..., "detail": ... }`. Branch on
`class`, not the human-readable `detail`. The host owns checking signatures
and curation, replaying verified changes from the verified snapshot to the
current head, and comparing the recovered `upstream_hash` with the head
release's upstream hash before polling. Treat index bytes as opaque and only
persist a replacement for a successfully published head.
