# models.dev — measured shape

Direct measurement, not recollection. Fetched `https://models.dev/api.json`
on 2026-08-11 at 09:58, 09:59 and 10:34 UTC. Every number below came from
those payloads or their response headers.

Reproduce with:

```sh
curl -sS -D headers.txt -o api.json https://models.dev/api.json
curl -sS -o /dev/null -D - -H 'If-None-Match: "<etag from headers.txt>"' \
     https://models.dev/api.json -w 'http=%{http_code} size=%{size_download}\n'
curl -sS -o /dev/null -H 'Accept-Encoding: gzip' https://models.dev/api.json \
     -w 'gzipped=%{size_download}\n'
```

Counts below come from `json.load(open('api.json'))` and iterating
`{provider_id: {..., "models": {model_id: {...}}}}`; each section names the
field it counted, so any figure can be re-derived from a fresh payload.

**Every count here is dated, and it moves in BOTH directions.** Providers and
models are added and removed on the upstream's own schedule — measured over 18
days, +821 models and −391. So a stale count from this document is neither a
floor nor a ceiling, and must not be treated as either. Re-measure rather than
extrapolate; the reproduction above costs one request.

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
(`sha256 4fb6410c…`, matching the ETag's md5) with the same ETag. Both were
`cf-cache-status: HIT`, so this observes that a cached response is stable
within a cache window; it does **not** rule out per-request nondeterminism at
the origin, and it says nothing about stability across a rebuild. It is one
negative observation over one interval.

A third fetch 36 minutes later returned a different payload (`etag
"9436dd07..."`, 3,628,254 bytes). See "Observed churn" below: the content
changed, and what changed is the most consequential field in the document.

**Consequence:** change detection is cheap in bandwidth. An unchanged poll
costs one round trip and zero body bytes; a changed one costs ~355 KB
gzipped. That is the whole measured basis — it does not account for the
upstream's tolerance for polling frequency, which is a courtesy question and
not a measured one. Bandwidth matters here because every era boundary
fusiform records is an observation boundary whose width is its poll interval,
so cadence buys precision rather than throughput.

## Observed churn: a wire family re-routed in 36 minutes

Three fetches on 2026-08-11: 09:58, 09:59 (byte-identical), 10:34.

Between 09:58 and 10:34 the payload changed. Model population was static at
6,253 with zero added and zero removed. A full field-by-field diff across all
183 providers and 6,253 models found **exactly one change**:

```
model  opencode-go/deepseek-v4-flash
field  provider
09:58  {"npm": "@ai-sdk/anthropic"}
10:34  null
```

The per-model `provider` override was deleted. The provider-level default is
`@ai-sdk/openai-compatible`, unchanged across both fetches. So in 36 minutes,
with no other field touched anywhere in the document, this model's renderer
moved from the Anthropic wire family to the OpenAI-compatible one.

Three properties of this event matter more than the event itself:

1. **It is a renderer re-route delivered as data** — precisely the class the
   charter forbids fusiform from serving, observed within the first hour of
   measuring the feed rather than argued as a risk. A consumer sourcing
   wire-family selection from this field would have silently changed its
   outbound request shape for this model, mid-day, with no signal.
2. **`last_updated` did not move.** It reads `2026-07-31` in both fetches
   while the model's `provider` override was deleted. That is one observed
   case of the field failing to track a change — enough to establish that
   `last_updated` is not a reliable change indicator, not enough to
   characterize how often it fails. Fusiform's conclusion (treat it as
   descriptive prose, never as a change signal or era boundary) is a design
   choice made on that evidence, argued in the design note.
3. **Nothing else moved.** No price, no capability, no limit. A change
   detector that ignores quarantined fields reports "nothing changed" for this
   fetch, which is correct for consumers and wrong for the operator. This is
   the concrete case the two-hash split exists for: `raw_hash` moves,
   `normalized_hash` holds.

## Population

| Count | Value |
| --- | --- |
| Providers | 183 |
| Models (provider × model rows) | 6,253 |
| Distinct model ids | 2,957 |
| Models with no `cost` object | 420 |

Top-level JSON is an object keyed by provider id; each provider holds a
`models` object keyed by model id. Both keys duplicate the `id` field inside
the object, and they agreed on every row measured — 0 mismatches across 183
providers and 6,253 models. Fusiform keys on the nesting key and **validates**
the inner `id` against it, treating a mismatch as a normalizer error rather
than assuming the agreement holds. A property observed across one payload is
not a guarantee the upstream offers.

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

`release_date` and `last_updated` are present on all 6,253 rows and are
`YYYY-MM` rather than `YYYY-MM-DD` on 190 (3.0%) and 215 (3.4%) of them.
`knowledge` is present on 3,338 rows and is `YYYY-MM` on 2,121 of those
— **63.5% of the rows that have it**. So a parser that assumes full dates
rejects a few percent of `release_date`/`last_updated` values and most
`knowledge` values.

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

The published rate therefore depends on request size, which means a request
property participates in rate selection in the data as it exists today — not
as a future possibility. A pricing consumer cannot resolve a rate from a flat
`(model, token_class)` key against this payload.

(What a consumer *does* with that is its own decision. The measurement
establishes that the data is tier-shaped; it does not establish any consumer's
selection semantics.)

**Measured:** 288 models carry a `context_over_200k` object alongside `tiers`,
and every one of the 288 also carries `tiers`. On the rows sampled, the two
carry the same rates.

**Inferred, not measured:** that `context_over_200k` is an older,
threshold-hardcoded encoding of the same fact. Co-occurrence and matching
values are consistent with that reading but do not establish it, and the
upstream documents neither field. Fusiform's handling of the discrepancy case
is in the design note; this document records only that both fields exist,
always together, and agree where compared.

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

Some long decimals are legitimate (`0.003625`, `0.103788`) and must survive;
only values with a 15+ digit fractional tail are IEEE-754 artifacts. Counted
across four snapshots spanning 23 days:

| Snapshot | ≥6 decimals | ≥15 decimals (artifacts) |
| --- | --- | --- |
| 2026-07-19 (astrocyte's staged file) | 202 | 145 |
| 2026-07-24 (broca's vendored) | 151 | 72 |
| 2026-08-11 09:58 | 151 | 72 |
| 2026-08-11 10:34 | 151 | 72 |

Artifacts are present in every snapshot measured, and their count moved (145
→ 72) as the underlying data changed. Four observations over 23 days is not
proof of a permanent property, but it is enough to say the feed has carried
artifacts continuously over that span and a parser must handle them rather
than treat them as an incident.

Cost values arrive as **both** JSON ints and floats for the same key. Of the
5,833 rows carrying a `cost.input`, it is a JSON int on 1,620 (27.8%) and a
float on 4,213 (72.2%). A parser typed to `f64` alone would reject 27.8% of
them; one typed to an integer type alone would reject 72.2%. Neither JSON
number type covers this field.

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

Measured on the 10:34 payload: 38 models carry `experimental`, holding 42 mode
variants. All 42 carry a `provider` block (42 with `body`, 16 with `headers`);
38 carry a `cost` block. So the field fuses request bytes and a second pricing
plane into one structure — and the pricing half is material: **32 of 42 modes
price input differently from the model's base rate**, at multipliers of 2.0,
2.5, 6.0 and 6.67. `gmicloud/anthropic/claude-opus-4.7` bills $4.50/Mtok base
and $30/Mtok in `fast` mode.

These two fields are why fusiform's design quarantines renderer-selection data
rather than serving it. The rule, its reasoning, and its test are in
`docs/design/schema-and-store.md` §6, which is the authoritative statement;
this document records only what the fields contain.

`experimental.modes.*.cost` embeds a second pricing plane inside a
wire-affecting field. What fusiform does about that, and the fleet's measured
exposure to it, are in `docs/findings/2026-08-11-experimental-mode-rates.md`.

## What this upstream cannot express

- **No currency.** USD is convention. A non-USD provider cannot be represented
  correctly here at all.
- **No effective dates on prices.** `last_updated` is about the model. Every
  rate era fusiform derives from this source is an observation boundary.
- **No charge basis.** Per-token is assumed; the audio keys silently violate
  it.
- **No image, video, or per-second pricing.** 299 models emit something other
  than text; 164 of them carry a `cost` object, and every rate in it is
  token-shaped. There is no field that can express per-image, per-second, or
  per-minute billing.

  The token-shaped schema visibly breaks on these rows. **186 models declare
  `limit.output == 0`**, **124 declare `limit.context == 0`**, and **5 declare
  `limit.input == 0`**. `poe/google/veo-3` (video output) carries
  `{"context": 480, "output": 0}`; the five input-zero rows are OpenAI image
  models zeroing all three fields.

  A token limit of zero is not a measurement of anything — a model accepting
  zero context or emitting zero output cannot be called. A consumer treating
  it as a limit concludes the model can produce no output at all.

  **The zeros are not confined to non-token models, and they are not
  whole-row.** Two independent groupings of the 200 models carrying at least
  one zero limit field:

  By output modality: 148 non-text-only, 13 mixed, **39 text-only**.
  `poe/cerebras/qwen3-32b-cs` emits text and declares
  `{"context": 0, "output": 0}`. So modality does not predict the zero.

  By field pattern: 110 declare *every* limit field zero, **90 mix real values
  with zeros**. `alibaba-token-plan/qwen-image-2.0` declares
  `{"context": 8192, "output": 0}`; `greenpt/green-s` declares
  `{"context": 0, "output": 8192}`. So the zero is a per-field property, not a
  per-row one, and a rule applied at row level discards real values.

The last point is what the measurement establishes: this upstream is
LLM-shaped, non-LLM rows already exist inside it, and their pricing and limit
fields cannot express how those models bill. The design conclusion fusiform
draws from it — that real non-LLM coverage needs a second source, and the
schema must carry charge basis explicitly before one arrives — is an
inference from that evidence, argued in `docs/design/schema-and-store.md`,
not something the payload states.
