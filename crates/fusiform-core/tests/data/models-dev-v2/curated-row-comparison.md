# Curated rows that the models.dev v2 snapshot carries

**For a decision before replacing the embedded v1 catalog.** The saved v2
catalog carries 14 of the 22 rows in
`crates/fusiform-module/data/curated-models.json`, matched by exact provider
and model ID. All 14 v2 A/B comparisons are **equal**. That does not mean the
cutover is neutral: an upstream row takes precedence over the entire curated
fallback, not just the facts that upstream happens to publish.

## Comparison method

The inputs were the [saved v2 snapshot and v1 fixture](README.md) and the
curation tables at `2aa8a1ce92be5847b052422379fee7b63a0521a6`. None of the
14 identities was upstream in the v1 fixture, and none was held in the
shipped curated table.

A Linux probe used the existing `loop_/models_dev_v2.rs` test helpers to
seed a SQLite store from the saved v1 fixture, read each current model via
`catalog.get`, and apply the full saved v2 snapshot through the guarded
apply path with the production audio charge evidence. On the same v2 store,
the probe read each model with the shipped table and then with only that
model's row removed via `curated_models::plant`. It compared the whole named
reply except `resolved_at_ms`, both without an `auth_method` filter and with
`auth_method: apikey`. The 14 pairs matched in both request forms. The mode,
tier, and unit decisions below come from those served facts; to attribute
omissions, the probe also normalized each corresponding v2 model separately
with the same evidence and inspected its raw cost schedule and findings.

The **v1 → v2** comparison below is a separate measurement: v1 plus the
active curated row versus v2 with that row yielding to upstream. It shows what
readers see at the switch, whether or not the fallback itself is deleted.
Keeping the row by exemption would not preserve its facts on current reads
while upstream publishes that ID. All 14 IDs also lose the `curated` reply
metadata (the source and login allowlist of the displaced claim).

## Price schedules

Every listed key is under `rate.`. Amounts are USD. Bare roles and
`.mode.<name>` roles are per million tokens of that role. `S($amount)` is
`priced` with `unit_provenance: {"kind":"stated"}`; `P($amount)` is `priced`
with `unit_provenance: {"kind":"assumed_by_policy","policy":"models-dev-usd-v1"}`.
Both use wire exponent 9 unless
`e0` or `e2` is shown; wire `units` is the given decimal times 10 to that
exponent. `per N` is the explicit wire denominator for the named unit.
For every `S` entry with an explicit denominator the wire also includes
`increment` and `minimum_quantity`, both `{"kind":"unknown"}`; these fields
are absent in the `P` entries. `Zero` is `{"state":"stated_zero"}`; `—` is
an absent key, not a zero or an `unpriced` value.

For every ID, the right column is the **same with and without the curated
row**. The table is the union of all served rate keys in the two snapshots.
The next section accounts for additional raw prices that v2 does not serve.
All other roles, units and named modes have no published price in these rows;
there are no `above_context` tier prices in either snapshot for any of them.

| Provider / model | Rate key suffix | Saved v1 + curated | Saved v2, either arm |
| --- | --- | --- | --- |
| `cloudflare-workers-ai/@cf/cloudflare/clairic` | `input` | S($0.10) | P($0.10) |
| | `output` | Zero | Zero |
| `cloudflare-workers-ai/@cf/cloudflare/clef` | `input` | S($0.24) | P($0.24) |
| | `output` | Zero | Zero |
| `cloudflare-workers-ai/@cf/cloudflare/clef-flash` | `input` | S($0.038) | P($0.038) |
| | `output` | Zero | Zero |
| `elevenlabs/eleven_flash_v2_5` | `input.per_character` | S($0.04, e2; per 1,000) | P($40; per 1,000,000) |
| `google/gemini-3.5-transcribe` | `input` | — | P($2) |
| | `input_audio` | S($2) | P($2) |
| | `output` | S($12) | P($12) |
| `google/gemini-3.5-transcribe-live` | `input` | — | P($3.50) |
| | `input_audio` | S($3.50) | P($3.50) |
| | `output` | S($21) | P($21) |
| `google/gemini-3.8-flash-tts` | `input` | S($0.50, e2) | **— (refused)** |
| | `input.mode.flex` | — | P($0.25) |
| | `input.mode.priority` | — | P($0.90) |
| | `output.mode.flex` | — | P($4.50) |
| | `output.mode.priority` | — | P($16.20) |
| | `output_audio` | S($9, e0) | **— (refused)** |
| | `output_audio.mode.flex` | — | P($4.50) |
| | `output_audio.mode.priority` | — | P($16.20) |
| `google/gemini-omni-1.2-flash` | `input` | S($1.50, e2) | P($1.50) |
| | `output` | S($9, e0) | P($9) |
| | `output_video` | S($12, e0) | P($12) |
| `openai/gpt-image-2.6-flare` | `cache_read` | S($1.25, e2) | P($1.25) |
| | `input` | S($5, e0) | P($5) |
| | `input_image` | S($8, e0) | P($8) |
| | `output_image` | S($30, e0) | P($30) |
| `openai/gpt-live-transcribe` | `input_audio.per_second` | S($0.017; per 60) | P($0.017; per 60) |
| `openai/gpt-realtime-2.1` | `cache_read` | S($0.40) | P($0.40) |
| | `cached_input_audio` | S($0.40) | **— (unestablished charge basis)** |
| | `input` | S($4) | P($4) |
| | `input_audio` | S($32) | **Unpriced: `unknown_charge_basis`** |
| | `input_image` | — | P($5) |
| | `output` | S($24) | P($24) |
| | `output_audio` | S($64) | **Unpriced: `unknown_charge_basis`** |
| `openai/gpt-transcribe` | `input_audio.per_second` | S($0.0045; per 60) | P($0.0045; per 60) |
| `typesafe/jev-1.13.0` | `input` | S($0.042) | P($0.042) |
| | `output` | Zero | Zero |
| `typesafe/jev-1.14.0` | `input` | S($0.042) | P($0.042) |
| | `output` | Zero | Zero |

The ElevenLabs amount and denominator both change by a factor of 1,000;
the price per character stays the same. The two minute-billed OpenAI rows
also keep their prices per unit, but no longer carry the explicit unknown
rounding and minimum terms. The new Gemini transcription `input` prices are
text-token roles separate from the audio-token prices, even though those
snapshots describe audio-only input.

### Published in v2 but not served as a price

- **`google/gemini-3.8-flash-tts`:** v2 publishes base text input $0.50,
  text output $9 and audio output $9 per million tokens, plus a 2× multiplier
  for `date gte 2027-01-01`. That predicate is refused; the finding is
  `unsupported condition date (gte)`, scope `BaseAndTierRates`. It leaves all
  three base keys absent in both v2 arms (text output was also absent before
  the switch). This ID has no prior upstream rate to retain. The `flex` (0.5×)
  and `priority` (1.8×) mode prices still serve for all three roles.
- **`openai/gpt-realtime-2.1`:** v2 publishes audio input $32, cached audio
  input $0.40 and audio output $64 per million tokens, but none has production
  charge-unit evidence for this ID. The parser reports
  `UnattributableChargeUnit` for all three; `rate.input_audio` and
  `rate.output_audio` serve `{"state":"unpriced","reason":"unknown_charge_basis"}`
  and `rate.cached_input_audio` is absent. v2 also publishes cached image
  input $0.50 per million tokens; no cached-image rate role is admitted, so
  that price is not served in either arm (and was not served before).
- **`openai/gpt-image-2.6-flare`:** v2 publishes cached image input $2 per
  million tokens; no cached-image rate role is admitted, so that price is
  not served in either arm (and was not served before). Text output has no
  published price; it was and remains absent.

No other collision has a refused or unserved upstream price. In particular,
none of these 14 publishes a cost using an unadmitted unit. The cached-image
exceptions use an admitted unit (tokens) but have no admitted role. Missing
rates outside these exceptions are unpublished, not refused.

## Existing non-price facts at the switch

`—` means the key is no longer served. Each entry is **saved v1 + curated →
served v2**; existing model facts not listed here or in the price table are
unchanged. The newly served facts are listed separately below.

`wire.api` is a served fact synthesized from endpoint evidence. For OpenAI
and Google, the URL stays the same but its `from` becomes `model` instead
of `curated`: `https://api.openai.com/v1` and
`https://generativelanguage.googleapis.com/v1beta`, respectively. Where
listed, a `wire.family` change is distinct from the endpoint URL change.

| Provider / model | v2 A/B | Changed or lost non-price facts at cutover |
| --- | --- | --- |
| `cloudflare-workers-ai/@cf/cloudflare/clairic` | Equal | **Lost** `model.kind`: `"extraction"` → — |
| `cloudflare-workers-ai/@cf/cloudflare/clef` | Equal | **Lost** `model.kind`: `"decision"` → — |
| `cloudflare-workers-ai/@cf/cloudflare/clef-flash` | Equal | **Lost** `model.kind`: `"decision"` → —; `model.name`: `"Clef-flash"` → `"Clef Flash"` |
| `elevenlabs/eleven_flash_v2_5` | Equal | **Changed** `wire.api`: `{"from":"curated","value":"https://api.elevenlabs.io/v1"}` → `{"from":"model","value":"https://api.elevenlabs.io"}`; **lost** `wire.family`: `"elevenlabs.text_to_speech"` → — |
| `google/gemini-3.5-transcribe` | Equal | **Changed** `limit.audio_tokens_per_second`: `25` → `32`; `wire.api`: provenance only |
| `google/gemini-3.5-transcribe-live` | Equal | **Changed** `limit.audio_tokens_per_second`: `25` → `32`; `wire.api`: provenance only |
| `google/gemini-3.8-flash-tts` | Equal | **Lost** `limit.output_audio_tokens_per_second`: `25` → —; `wire.api`: provenance only |
| `google/gemini-omni-1.2-flash` | Equal | **Lost** `capability.aspect_ratios`: `["9:16","16:9"]` → —; `capability.output_audio`: `true` → —; `limit.output_duration_ms`: `15000` → —; `limit.output_video_tokens_per_second.resolution.360p`: `1931` → —, `.720p`: `5792` → —, `.1080p`: `8688` → —, `.4k`: `17376` → —; **changed** `wire.family`: `"google.interactions"` → `"google.generate_content"`; `wire.api`: provenance only |
| `openai/gpt-image-2.6-flare` | Equal | **Changed** `wire.family`: `"openai.images"` → `"openai.responses"`; **lost** `limit.temperature_min`: `0` → —, `limit.temperature_max`: `2` → —; `wire.api`: provenance only |
| `openai/gpt-live-transcribe` | Equal | **Lost** `limit.temperature_min`: `0` → —, `limit.temperature_max`: `2` → —; `wire.api`: provenance only |
| `openai/gpt-realtime-2.1` | Equal | `wire.api`: provenance only |
| `openai/gpt-transcribe` | Equal | `model.name`: `"GPT Transcribe"` → `"GPT-Transcribe"`; `wire.api`: provenance only |
| `typesafe/jev-1.13.0` | Equal | **Lost** `model.kind`: `"decision"` → — |
| `typesafe/jev-1.14.0` | Equal | **Lost** `model.kind`: `"decision"` → — |

### What v2 does not directly express

- The three Cloudflare and two TypeSafe rows lose the curated `model.kind`.
  v2 `type` instead produces `model.type` with the same string; it does not
  restore `model.kind`. `existence: present` is synthesized from the presence
  of each ID. `capability.attachment: true` for the Cloudflare rows and
  `openai/gpt-realtime-2.1` survives via normalization of the uploaded media
  modalities, not a separate upstream attachment flag.
- Both Gemini transcription rows lose their model-specific audio conversion.
  No v2 field supplies it; the unchanged provider-level audio evidence
  (`input-audio-per-token.json`) supplies 32 tokens/s instead. The TTS output
  conversion and Omni aspect ratios, native-audio flag, clip duration and all
  four resolution-specific video conversions have no values in those v2 rows
  and no replacement layer serves them.
- No v2 row carries `wire.api` or `wire.family` as Fusiform facts. v2 has API
  declarations with `base_url`; for ElevenLabs the raw speech base is
  `https://api.elevenlabs.io/v1` but the normalized `wire.api` omits `/v1`.
  The independent SDK-to-wire-family rules preserve `google.generate_content`
  for the TTS row, but fall back to `google.generate_content` for Omni and
  `openai.responses` for Flare, even though the v2 protocol entries include
  `interactions` and `images`, respectively. ElevenLabs has no replacement
  wire-family value.
- The temperature bounds lost by Flare and GPT-Live-Transcribe came from
  separate temperature-range curation while the capability was unspecified;
  the v2 rows explicitly say `capability.temperature: false`. Separate
  attachment-limit curation still serves `limit.images_per_request: 3600`
  for Omni and `1500` for Flare and GPT-Realtime-2.1. Those limits are not
  supplied by v2 or by the three displaced model rows themselves.

## Newly served non-price facts

Every row gains `model.status: null` and `model.open_weights` (`true` for
Cloudflare, `false` otherwise). Every row except GPT-Realtime-2.1 also gains
`model.canonical_id`: `null` for Cloudflare, the full `provider/model` ID
for the others. All gain `model.family` as shown below. In this table, an
empty cell adds no fact; `null` is an added JSON null, not an absent key.
The limits column lists only new `limit.context` and `limit.output` keys,
respectively. New `wire.api` and `wire.npm` values all have `from: model`.

| Model (within provider above) | `model.family` | New limits (context / output) | Other new model or wire facts |
| --- | --- | --- | --- |
| `@cf/cloudflare/clairic` | `null` | — / `65536` | `model.type: "extraction"`; Cloudflare `wire.api` below |
| `@cf/cloudflare/clef` | `"clef"` | — / `65536` | `model.type: "decision"`; Cloudflare `wire.api` below |
| `@cf/cloudflare/clef-flash` | `"clef-flash"` | — / `24576` | `model.type: "decision"`; Cloudflare `wire.api` below |
| `eleven_flash_v2_5` | `"elevenlabs-flash"` | `null` / `null` | `wire.npm: "@ai-sdk/elevenlabs"` |
| `gemini-3.5-transcribe` | `"gemini-transcribe"` | `98304` / `32768` | `wire.family: "google.generate_content"`; `wire.npm: "@ai-sdk/google"` |
| `gemini-3.5-transcribe-live` | `"gemini-transcribe"` | `131072` / `65536` | `wire.family: "google.generate_content"`; `wire.npm: "@ai-sdk/google"` |
| `gemini-3.8-flash-tts` | `"gemini-tts-flash"` | | `wire.npm: "@ai-sdk/google"` |
| `gemini-omni-1.2-flash` | `"gemini-omni-flash"` | `131072` / `65536` | `wire.npm: "@ai-sdk/google"` |
| `gpt-image-2.6-flare` | `"gpt-image"` | `null` / `null` | `wire.npm: "@ai-sdk/openai"` |
| `gpt-live-transcribe` | `"gpt-transcribe"` | `null` / `null` | |
| `gpt-realtime-2.1` | `"gpt-realtime"` | | `wire.family: "openai.responses"`; `wire.npm: "@ai-sdk/openai"` |
| `gpt-transcribe` | `"gpt-transcribe"` | `null` / `null` | `wire.family: "openai.responses"`; `wire.npm: "@ai-sdk/openai"` |
| `jev-1.13.0` | `"jev"` | `64000` / `null` | `model.type: "decision"`; TypeSafe `wire.api` below; `wire.npm: "@ai-sdk/typesafe-ai"` |
| `jev-1.14.0` | `"jev"` | `64000` / `null` | `model.type: "decision"`; TypeSafe `wire.api` below; `wire.npm: "@ai-sdk/typesafe-ai"` |

The new Cloudflare `wire.api` value is
`https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/ai/run`.
The new TypeSafe `wire.api` value is `https://api.typesafe.ai/v1`.

New facts under `capability.`:

| Model(s) | Boolean `true` | Boolean `false` | Other new capabilities |
| --- | --- | --- | --- |
| All three Cloudflare rows | `structured_output`, `temperature` | `reasoning`, `tool_call` | `input_modalities: ["image","text"]`, `output_modalities: ["text"]`, `reasoning_options: null` |
| `eleven_flash_v2_5` | | `attachment`, `reasoning`, `temperature`, `tool_call` | `reasoning_options: null` |
| Both Gemini 3.5 transcription rows; `gpt-transcribe` | `attachment`, `temperature` | `reasoning`, `tool_call` | `reasoning_options: null` |
| `gemini-3.8-flash-tts` | `temperature` | `attachment`, `reasoning`, `structured_output`, `tool_call` | `reasoning_options: null` |
| `gemini-omni-1.2-flash` | `attachment`, `reasoning`, `temperature` | `tool_call` | `reasoning_options: []` |
| `gpt-image-2.6-flare`, `gpt-live-transcribe` | `attachment` | `reasoning`, `temperature`, `tool_call` | `reasoning_options: null` |
| `gpt-realtime-2.1` | | | None |
| Both TypeSafe rows | `attachment`, `structured_output` | `reasoning`, `temperature`, `tool_call` | `input_modalities: ["image","text"]`, `output_modalities: ["text"]`, `reasoning_options: null` |

No removal or exemption is enacted by this report.
