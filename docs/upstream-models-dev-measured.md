# models.dev — measured shape

Direct measurement, not recollection. Fetched `https://models.dev/api.json`
on 2026-08-11 ~09:58 UTC. Every number below came from that payload or from
the response headers; the commands are reproducible with two `curl` calls and
a Python pass.

This document exists because fusiform's normalizer is designed against the
upstream's real shape, and because several fleet assumptions about that shape
turned out to be wrong.

## Transport

| Property | Value |
| --- | --- |
| Size, uncompressed | 3,628,293 bytes |
| Size, `Accept-Encoding: gzip` | 354,901 bytes (10.2x) |
| `content-type` | `application/json` |
| `etag` | `"4fb6410caa99a4dac15a0c7351ab0840"` |
| `cache-control` | `public, max-age=0, must-revalidate` |
| `last-modified` | absent |
| Conditional GET | `If-None-Match` → **304, 0 bytes** |

Two fetches 21 seconds apart returned byte-identical payloads
(`sha256 4fb6410c…`, matching the ETag's md5) with the same ETag. That rules
out per-request nondeterminism — no random key ordering, no re-serialization
churn within a cache window. It does **not** establish stability across a
rebuild of their site; only a longer sample can.

**Consequence:** change detection is cheap. A conditional GET costs zero body
bytes when nothing changed, and a full fetch costs ~355 KB gzipped. Polling
frequently is nearly free, which matters because every era boundary fusiform
records is an observation boundary whose width is its poll interval.

## Population

| Count | Value |
| --- | --- |
| Providers | 183 |
| Models (provider × model rows) | 6,253 |
| Distinct model ids | 2,957 |
| Models with no `cost` object | 420 |

Top-level JSON is an object keyed by provider id; each provider holds a
`models` object keyed by model id. Both keys are redundant with the `id`
field inside the object — 0 mismatches across all 183 providers and 6,253
models, so the nesting key can be treated as authoritative.

Model ids are **not** globally unique: `openai/gpt-oss-120b` appears under 28
different providers, `glm-5.2` under 26. The identity of a row is
`(provider_id, model_id)`, never `model_id` alone. Anything fusiform keys on
a bare model id will silently fuse 28 distinct offerings — with distinct
prices — into one.

## Provider object

Seven fields, all present on all 183 providers except `api`:

| Field | Type | Present |
| --- | --- | --- |
| `id` | string | 183 |
| `name` | string | 183 |
| `doc` | string (URL) | 183 |
| `env` | array of string | 183 |
| `npm` | string | 183 |
| `models` | object | 183 |
| `api` | string (URL) | 156 |

```json
{
  "id": "anthropic",
  "env": ["ANTHROPIC_API_KEY"],
  "npm": "@ai-sdk/anthropic",
  "name": "Anthropic",
  "doc": "https://docs.anthropic.com/en/docs/about-claude/models"
}
```

## Model object

Field frequency out of 6,253 rows:

| Field | Count | Notes |
| --- | --- | --- |
| `id`, `name`, `description` | 6,253 | always present |
| `attachment` | 6,253 | bool: 3,330 true / 2,923 false |
| `reasoning` | 6,253 | bool: 4,184 true / 2,069 false |
| `tool_call` | 6,253 | bool: 5,145 true / 1,108 false |
| `release_date` | 6,253 | date string |
| `last_updated` | 6,253 | date string |
| `modalities` | 6,253 | `{input: [...], output: [...]}` |
| `open_weights` | 6,253 | bool: 2,629 true / 3,624 false |
| `limit` | 6,253 | `{context, output, input?}` |
| `cost` | 5,833 | **absent on 420** |
| `temperature` | 5,791 | bool: 4,527 true / 1,264 false |
| `family` | 5,610 | string |
| `reasoning_options` | 4,184 | array of option objects |
| `structured_output` | 3,919 | bool: 3,070 true / 849 false |
| `knowledge` | 3,338 | date string |
| `interleaved` | 726 | bool or `{field: "..."}` |
| `status` | 241 | `deprecated` 185 / `beta` 55 / `alpha` 1 |
| `provider` | 229 | per-model transport override — see below |
| `experimental` | 38 | mode-keyed alternates — see below |

Verbatim, a rich entry:

```json
{
  "id": "claude-sonnet-4-5",
  "name": "Claude Sonnet 4.5 (latest)",
  "description": "Balanced Claude model for coding, analysis, agent workflows, and cost control",
  "family": "claude-sonnet",
  "attachment": true,
  "reasoning": true,
  "reasoning_options": [{ "type": "budget_tokens", "min": 1024 }],
  "tool_call": true,
  "structured_output": true,
  "temperature": true,
  "knowledge": "2025-07-31",
  "release_date": "2025-09-29",
  "last_updated": "2025-09-29",
  "modalities": { "input": ["text", "image", "pdf"], "output": ["text"] },
  "open_weights": false,
  "limit": { "context": 1000000, "output": 64000 },
  "cost": { "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75 }
}
```

### Dates carry mixed granularity

`release_date` and `last_updated` are `YYYY-MM-DD` on most rows but `YYYY-MM`
on 190 and 215 rows respectively. `knowledge` is `YYYY-MM` on 2,121 rows and
`YYYY-MM-DD` on 1,217. A parser that assumes full dates rejects ~5% of rows.

`last_updated` is an upstream-asserted date, but it describes **the model**,
not its price. It is not an effective-from for a rate and must never be
promoted into one.

### Modality vocabulary

Closed vocabularies in the current payload:

- `modalities.input`: `text` (6,223), `image` (3,350), `pdf` (1,320),
  `video` (797), `audio` (468)
- `modalities.output`: `text` (6,067), `image` (164), `audio` (75),
  `video` (64), `pdf` (2)

Non-text output models already exist here — 164 image, 75 audio, 64 video.
They are not hypothetical.

## Cost — where the upstream's model breaks

Cost keys and their frequency across the 5,833 priced rows:

| Key | Count | Shape |
| --- | --- | --- |
| `input` | 5,833 | number |
| `output` | 5,833 | number |
| `cache_read` | 3,625 | number |
| `cache_write` | 1,196 | number |
| `tiers` | 320 | array of tier objects |
| `context_over_200k` | 288 | nested cost object |
| `reasoning` | 107 | number |
| `input_audio` | 77 | number |
| `output_audio` | 18 | number |

Units are **implied, never stated**: no currency field exists anywhere in the
payload, and no per-what field either. `"input": 3` for Claude Sonnet 4.5
means 3 USD per million input tokens by convention alone.

### Rates are selected by context size, today

320 models carry `tiers` and 288 carry `context_over_200k`:

```json
{
  "input": 2, "output": 12, "cache_read": 0.2,
  "tiers": [{
    "input": 4, "output": 18, "cache_read": 0.4,
    "tier": { "type": "context", "size": 200000 }
  }],
  "context_over_200k": { "input": 4, "output": 18, "cache_read": 0.4 }
}
```

The rate depends on how large the request was. This settles an open question
between fusiform and astrocyte: a descriptive property participates in rate
selection **already**, not hypothetically. A pricing consumer cannot resolve
a rate from a flat `(model, token_class)` key.

`context_over_200k` and `tiers` are two encodings of the same fact, and both
appear on the same rows. `context_over_200k` is the older, threshold-hardcoded
form. Fusiform normalizes to the general tier form and never serves both.

### Audio rates share a namespace with token rates

`input_audio` / `output_audio` sit alongside `input` / `output` in one flat
object, with no unit distinguishing them. On `gemini-flash-latest`:

```json
{ "input": 1.5, "output": 9, "cache_read": 0.15, "input_audio": 1.5 }
```

Whether `input_audio` is per audio token, per second, or per minute is not
stated. This is the concrete instance of the charge-basis argument: the
upstream fuses units into one namespace and expects the consumer to know.
Fusiform must resolve it per provider from documentation and stamp the basis
explicitly, or mark the rate unpriced with `unknown charge basis` rather than
guess.

### Zero is used where unknown is meant

1,423 cost entries are exactly `0`, including `glm-4.5-flash` `input: 0` and
`cache_write: 0` across the whole zhipuai family. Zero negative rates.

Some of these are genuinely free; some are almost certainly "not published".
The upstream cannot distinguish them and neither can fusiform from the payload
alone. Combined with the 420 rows carrying no `cost` object at all, this is
astrocyte's absent/zero/unknown trichotomy showing up in real data, and it
means a fetched `0` cannot be promoted to a priced zero without a
per-provider judgment.

### Float artifacts are present and current

151 numbers in the raw payload carry six or more decimal places. Confirmed
IEEE-754 artifacts among them:

```
0.024999999999999998
0.39999999999999997
1.5999999999999999
```

Astrocyte measured 145 of these in their July snapshot; the count is 151
today, so this is a standing property of the feed, not a one-off. Some
long decimals are legitimate (`0.003625`, `0.103788`) and must survive.

Cost values arrive as **both** JSON ints and floats for the same key —
`input` is an int on 1,620 rows and a float on 4,213. A parser typed to one
or the other fails on ~26% of rows.

**Design consequence:** parse to a decimal type, scale to integer minor units
with an explicit exponent, round half-even at that boundary, reject values
that round to zero when a nonzero was published — and hash the *normalized*
integers, never the raw bytes. Then artifact churn cannot manufacture a diff.

## Wire-affecting fields — the ones fusiform must refuse

Two fields in this payload determine how a request is *rendered*, not what
exists.

`provider.npm`, per provider and overridden per model on 229 rows:

```json
{ "npm": "@ai-sdk/openai-compatible", "api": "https://api.cohere.ai/compatibility/v1" }
```

Under provider `opencode-go`, model `qwen3.7-plus` declares
`{"npm": "@ai-sdk/anthropic"}`. That is a renderer selection: an upstream edit
to that string re-routes a model's wire family. It is exactly the class of
change broca's swap guard refuses, and exactly what the charter means by
"wire-family resolution stays with consumers, forever."

`experimental`, on 38 rows, goes further and carries literal request bytes:

```json
{
  "modes": {
    "fast": {
      "cost": { "input": 30, "output": 150, "cache_read": 3, "cache_write": 37.5 },
      "provider": {
        "body": { "speed": "fast" },
        "headers": { "anthropic-beta": "fast-mode-2026-02-01" }
      }
    }
  }
}
```

Headers and body parameters, editable by an upstream contributor, that would
land verbatim in an outbound provider request.

**Rule, non-negotiable:** fusiform parses these fields, records them in raw
provenance, and **never serves them as catalog facts**. They are evidence
about the upstream, not instructions to a consumer. Broca's `family.rs` stays
hand-tabled; if broca ever wants to *diff* its table against the upstream's
claim, that is a reconciliation report for a human, never an input to a swap.

Note that `experimental.modes.*.cost` embeds a second pricing plane inside a
wire-affecting field. Fusiform's v1 does not serve mode-conditional rates; a
model with modes is served at its base rate and the modes are recorded in raw
provenance only.

## What this upstream cannot express

- **No currency.** USD is convention. A non-USD provider cannot be represented
  correctly here at all.
- **No effective dates on prices.** `last_updated` is about the model. Every
  rate era fusiform derives from this source is an observation boundary.
- **No charge basis.** Per-token is assumed; the audio keys silently violate
  it.
- **No image, video, or per-second pricing.** The 164 image-output and 64
  video-output models carry no cost that can express per-image or per-second
  billing. `stabilityai/stablediffusionxl` carries `"limit": {"context": 200,
  "output": 0}` — a token limit of zero on a model that does not emit tokens.

The last point is the measured form of the charter's domain argument: this
upstream is LLM-shaped, non-LLM rows already exist inside it, and they are
wrong. A second source is required for any real non-LLM coverage, and the
schema must carry charge basis explicitly before that source arrives.
