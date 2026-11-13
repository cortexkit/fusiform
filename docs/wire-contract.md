# Wire contract

What fusiform serves, field by field. The types live in the
`fusiform-protocol` crate (`crates/fusiform-protocol/src/lib.rs`, `money.rs`,
`refusal.rs`); a consumer compiles against that crate rather than copying the
shapes. This document says what each field means, what is guaranteed, what is
not, and which test proves each rule.

Test names are libtest paths inside each crate's integration target
(`--test it`) unless a source file is named, in which case they are unit tests
in that file. Billing planes, plan prices and curated rows have their own
documents: [`billing-planes.md`](billing-planes.md),
[`plan-prices.md`](plan-prices.md), [`curated-data.md`](curated-data.md).

## 1. Calling a tool

A call is a JSON object `{"name": "<tool>", "arguments": {...}}`
(`route::the_tool_call_envelope_is_unwrapped`).

1. `arguments` absent or `null` means no arguments. An empty request body is a
   `catalog.get` with no arguments, which returns the current catalog
   (`route::an_empty_request_returns_the_current_catalog`).
2. An unknown tool name is refused with `bad_request` naming the served tools
   (`route::an_unknown_tool_is_refused`).
3. A call that names a tool preset is refused with `bad_request`; fusiform
   serves one surface and no presets
   (`scope_and_preset::a_named_preset_is_refused_and_an_absent_one_is_served`).
4. The requests for `catalog.get`, `catalog.history`, `catalog.status` and
   `catalog.correct`, `quality.grade`, `quality.withdraw` and `quality.grades`
   are declared to reject unknown fields, so a misspelled
   field is an error rather than a silently different answer. The test covers
   `catalog.get` (`route::a_malformed_request_is_refused_rather_than_defaulted`).
   The requests for `catalog.mark_artifact`, `catalog.retract_artifact` and
   `plan.prices` do not reject unknown fields; they are ignored there.
5. `source` is optional on `catalog.get`, `catalog.history`, `catalog.status`
   and `catalog.correct`, and defaults to `models.dev`, the only accepted
   value. Any other value, including `seed`, is `bad_request`
   (`route::every_accepted_source_can_match_a_row`).

The tools are listed in [`module-surface.md`](module-surface.md).

## 2. Refusals

A refused call returns an error with a `code` and a human-readable `message`.
Consumers branch on the code, never on the message. `RefusalKind::of(code)`
maps a code to what the consumer should do
(`refusal_kinds::every_code_maps_to_its_own_kind`):

| Code | Kind | Meaning | Retry? |
| --- | --- | --- | --- |
| `bad_request` | `malformed` | the request could not be read: bad JSON, a missing field, an incomplete identity, an unknown tool, source or `auth_method` | no; fix the request |
| `no_coverage` | `outside_coverage` | the request was well formed and fusiform has no catalog to answer it: unknown provider or model, an instant before the record begins, an undeclared billing plane, an unknown observation id | no; accept the answer |
| `unavailable` | `retryable` | fusiform cannot answer now: no store is open, or a store read failed | yes |
| `refused` | `declined` | a write fusiform declines because performing it would be wrong (a correction the planner rejects) | no; reconsider the request |
| `catalog_changed` | `restart_from_first_page` | a paged read's continuation arrived after the catalog changed since its first page | no; restart the paged read from page one |
| any other code | `unrecognized` | a code this build of the protocol crate does not know | no |

Guarantees:

1. `no_coverage` and `bad_request` are distinct, so a correct question about
   something fusiform does not cover is never reported as a caller defect
   (`crates/fusiform-protocol/src/refusal.rs`: `coverage_and_malformed_are_distinguishable`).
2. An unrecognised code is not retryable
   (`crates/fusiform-protocol/src/refusal.rs`: `an_unknown_code_is_not_retryable`).
3. The module host can also return the code `internal` when a request panics
   or a response fails to serialize
   (`request_serving::panicking_request_returns_internal_error`). It is not one
   of the four protocol codes and classifies as `unrecognized`.

## 3. Time and versions

Every `catalog.get` response carries two numbers that look alike and answer
different questions (`catalog.status` carries only the second).

| Field | Answers | Use it for |
| --- | --- | --- |
| `resolved_at_ms` | which instant this response describes | replaying a read: send it back as `at_ms` |
| `catalog_version` | whether the store has changed since you last looked | a high-water mark that only moves forward |

1. `resolved_at_ms` is always concrete. A read without `at_ms` records the
   instant "now" resolved to, and sending that instant back as `at_ms` returns
   the same values (`served_corrections::a_read_can_be_replayed_by_the_instant_it_resolved`).
2. `catalog_version` is `max(now_ms, previous + 1)` at each poll that writes,
   each committed quality grade or withdrawal, and each start of the module
   process. The start covers data compiled into the build (aliases, curated
   rows, overrides, billing planes), which changes only with a deploy
   (`history::a_process_start_advances_the_catalog_version_without_a_poll`).
   Previews and the one-time grade import do not advance it. It never goes backwards, even across a restore of the
   database ([`data-model.md`](data-model.md) §4). It describes the store at
   read time: a point-in-time read of old values carries today's version. It
   is not a snapshot identifier.
3. Row counts such as `era_count` and `model_count` are not monotonic across a
   restore and must not be used as high-water marks.
4. A point-in-time read serves, per fact, the era in force at `at_ms`, with
   the boundary instant inclusive
   (`route::a_point_in_time_request_is_honoured`,
   `serve::a_read_at_a_boundary_instant_returns_the_new_value`).
5. A read at an instant before the store's earliest observation is refused
   with `no_coverage` rather than answered with an empty catalog
   (`served_corrections::a_read_before_the_record_is_refused_rather_than_answered_empty`).
   `catalog.status` reports that bound as `history_begins_at_ms`, the same
   value the refusal uses
   (`served_corrections::status_reports_the_bound_the_pre_history_refusal_enforces`).
   The bound can move later after a restore and never moves earlier.
6. A historical read and a current one can be told apart by
   `resolved_at_ms`, which matters when a historical read returns fewer models
   than a current one (`served_corrections::a_historical_read_is_distinguishable_from_a_current_one`).
7. A curated model row (one models.dev does not list; see
   [`curated-data.md`](curated-data.md) §4) is served on a point-in-time read
   from the store's history of the curated file: the curated claim in force at
   `at_ms`, in the same shape as a current read, with a `curated` disclosure
   from that claim's provenance. A claim's first era begins at the row's
   curation date and a later change begins at the start of the build that
   carried it. Where models.dev had the model present at `at_ms`, its upstream
   row is served instead, as on a current read. A named read before the row's
   first era, or after fusiform stopped serving the row, is refused
   `no_coverage` with a message saying which. A re-dated row's provenance is
   not a change, so a disclosure on a replayed read can carry the source and
   review date recorded with the claim rather than today's.

## 4. Facts

`catalog.get` returns `models`: a map from `provider_id/model_id` to a map of
fact key to value. The identity is the pair, because one model id appears under
many providers (`crates/fusiform-core/src/lib.rs`:
`model_identity_is_the_pair_not_the_id`).

### 4.1 The served fact keys

The closed set is `SERVED_FACTS` in the protocol crate, also served as
`served_facts` by `catalog.status`. Each key has a class saying what a wrong
value would do to a consumer:

| Key | Class | Value |
| --- | --- | --- |
| `existence` | advisory | `"present"` or `"absent"` |
| `model.name` | advisory | human display name as a JSON string; otherwise no fact |
| `model.lineage` | advisory | curated routing diversity and cost-fallback group; current reads only; otherwise no fact (§5.10) |
| `provider.name` | advisory | the row's provider display name as a JSON string; otherwise no fact |
| `quality.elo` | advisory | operator-assigned general quality score as an exact JSON number; routing selection, not request bytes; current reads only (§5.9) |
| `quality.eq` | advisory | operator-assigned writing and judgment score as an exact JSON number; routing selection, not request bytes; current reads only (§5.9) |
| `limit.context` | byte-affecting | integer token count, or `null` |
| `limit.output` | byte-affecting | integer token count, or `null`; a sourced current-read override is disclosed in `fact_overrides` (§5.8) |
| `limit.audio_tokens_per_second` | advisory | integer audio tokens per second of input audio, curated from the provider's documentation; provider defaults join only current rows serving `rate.input_audio`; a model-specific curated row may store its own sourced figure, also available in that row's history; otherwise no fact |
| `limit.images_per_request` | advisory | documented integer image count per request, only on first-party image-input rows; current reads only; absent means unknown, never unlimited |
| `limit.image_bytes` | advisory | integer raw-file bytes per image, only on first-party image-input rows; current reads only; absent means unknown, never unlimited |
| `limit.image_long_edge_px` | advisory | integer image long edge in pixels with no count condition; check the conditional family for stricter bounds; only on first-party image-input rows, never aliases or gateways; current reads only; absent means unknown, never unlimited |
| `limit.image_long_edge_px.above_images.<n>` | advisory | stricter integer image long edge in pixels when more than n image and document blocks occur across the whole request, history included; n is a positive decimal integer without leading zeros; only on first-party image-input rows, never aliases or gateways; current reads only; absent means no stated condition; a reader unable to count must take the smaller of the base and conditional values |
| `limit.document_pages` | advisory | integer pages per document, only on first-party PDF-input rows; current reads only; absent means unknown, never unlimited |
| `limit.document_bytes` | advisory | integer raw-file bytes per document, only on first-party PDF-input rows; current reads only; absent means unknown, never unlimited |
| `capability.reasoning` | byte-affecting | `true`, `false` or `null` |
| `capability.reasoning_options` | byte-affecting | the upstream's `reasoning_options` array verbatim, or `null` |
| `capability.structured_output` | advisory | `true` or `false` only when published by models.dev; otherwise no fact |
| `capability.tool_call` | advisory | `true`, `false` or `null` |
| `capability.server_side_fallback` | advisory | `true` or `false` on current reads of the `anthropic` rows Anthropic's documentation covers, from sourced additions disclosed in `fact_overrides` (§5.8); otherwise no fact, which means not stated, never `false` |
| `capability.mid_conversation_system` | advisory | `true` or `false` for persistent `role: "system"` messages inside `messages`, without a beta header, not the beta turn-scoped `clear_at` variant; sourced current-read-only additions on documented `anthropic` rows, disclosed in `fact_overrides` (§5.8), never stored or inherited by resellers, Bedrock, Vertex or Code Assist aliases; any upstream value wins; otherwise no fact, which means not stated, never `false` |
| `capability.cache_lifetimes` | advisory | prompt-cache lifetimes the model offers, shortest first: durations (`"5m"`, `"30m"`, `"1h"`, `"12h"`, `"24h"`) or OpenAI's `"in_memory"`; curated, first-party `openai` and `anthropic` rows only, current reads only; otherwise no fact ([`curated-data.md`](curated-data.md)) |
| `capability.attachment` | advisory | `true`, `false` or `null` |
| `capability.input_modalities` | advisory | list of modality strings, or `null` |
| `capability.output_modalities` | advisory | list of modality strings, or `null` |
| `model.family` | advisory | upstream family string, or `null` |
| `model.open_weights` | advisory | `true`, `false` or `null` |
| `model.canonical_id` | advisory | upstream canonical model id, or `null` |
| `model.status` | advisory | upstream lifecycle label verbatim, or `null`; on current reads, `"deprecated"` may come from a sourced override of the stored `null`, disclosed in `fact_overrides` (§5.8) when the vendor deprecates a model models.dev has not yet marked |
| `model.type` | advisory | models.dev's own sparse type label, verbatim from its open-ended vocabulary; absent or null upstream means no fact: unlabelled, never chat or text; distinct from curated `model.kind` |
| `model.kind` | advisory | `"decision"` or `"extraction"`, on curated rows only; otherwise no fact (§4.6) |
| `wire.npm` | advisory | `{"value": "<published client package>", "from": "model" or "provider"}`; otherwise no fact |
| `wire.api` | advisory | `{"value": "<base URL>", "from": "model", "provider" or "curated"}`; otherwise no fact; corrections/withholding disclosed in `wire_corrections` |
| `wire.shape` | advisory | `{"value": "<published request shape>", "from": "model"}`; otherwise no fact |
| `wire.family` | byte-affecting | verified request protocol token; current reads only; otherwise the request protocol is not verified |
| `route.reasoning_wire_ids` | byte-affecting | curated effort → backend model id object on live aliases only; otherwise no fact |
| `route.reasoning_default` | byte-affecting | default effort selected when the alias is requested without a tier suffix; a key in `route.reasoning_wire_ids`; otherwise no fact |
| `successor` | advisory | sourced replacement `provider/model` identity on retired aliases; otherwise no fact |
| `rate.input` | money | rate value (§4.3), per million input tokens |
| `rate.output` | money | per million output tokens |
| `rate.cache_read` | money | per million cached-read tokens |
| `rate.cache_write` | money | per million cache-write tokens |
| `rate.reasoning` | money | per million reasoning tokens |
| `rate.input_audio` | money | per million audio input tokens, only for providers whose docs establish per-token audio billing |

Names are for consumer display only, never identifiers. Names on upstream rows are
stored as ordinary eras, so renames are preserved and `at_ms` reads return the
name recorded then. Missing or empty names create no fact; casing and whitespace
are otherwise unchanged, including whitespace-only labels. A withdrawn name opens a null era in history but is
absent from catalog reads. `provider.` is its own fact namespace, selectable with
`fact_prefixes: ["provider."]`, and billing planes preserve it like other
non-rate facts. It identifies the row's provider, not the model's creator.
Curated models carry vendor-sourced `model.name`; their provider name comes from
the stored upstream provider claims, or is sourced in the curated row when the
provider is absent upstream. Aliases never inherit the target's `model.name` or
`provider.name`: they use their own `model_name` in `model-aliases.json` when
stated, with the row's source citation, and their own provider's upstream name.
Curated and alias rows, including these provider-label joins, are current-only,
with one exception: a curated model row is served on `at_ms` reads from its
recorded history, with the provider label in force at that instant (§3 item
7). Retired aliases carry only their retirement
facts, not live labels. Name eras can be corrected independently through
`{"field":"model","attribute":"name"}` and `{"field":"provider_name"}`.

Except for explicitly disclosed corrections and a curated row's own sourced
endpoint, `wire.npm`, `wire.api` and `wire.shape` are models.dev's claims,
not fusiform's transport policy.
For each field independently, a model's `provider.npm` or `provider.api` wins
over its provider's `npm` or `api` default; `from` names the level that supplied
the value. `provider.shape` exists only on models. Missing fields create **no
fact**: absence means unknown, never an inferred default. Strings pass through
verbatim, including empty strings, URL spelling and trailing slashes. `wire.api`
is a base URL, not a request protocol token. Consumers own runner support and
transport quirks. Alias rows do not
inherit these backend-specific claims (§5.5), and they are not projected onto
curated rows (which may state their own sourced endpoint).

`wire.family` is a fact about the wire: this route's request protocol is
verified, from its client package or the provider's own docs. It is not a
statement that any client or runner can send it. Routing matches a runner pair's
`wire_api` member against this `wire.family` fact; runners declare their own
protocol and auth-method support elsewhere. When the fact is absent, **the
request protocol is not verified**, never unsupported and never a fallback to
another grammar.

The only tokens are `anthropic.messages`, `openai.responses`,
`openai.chat_completions`, `openai.chatgpt_responses`,
`google.generate_content`, `google.code_assist`, `aws.bedrock_converse` and
`elevenlabs.text_to_speech`, `openai.images` and `google.interactions`.
`openai.images` is verified from the Flare model's
[supported image endpoints](https://platform.openai.com/docs/models/gpt-image-2.6-flare)
(`/v1/images/generations` and `/v1/images/edits`, not Responses).
`google.interactions` is verified from the
[Omni guide](https://ai.google.dev/gemini-api/docs/omni)
(`POST /v1beta/interactions`). The ElevenLabs token is verified from its
[text-to-speech endpoint](https://elevenlabs.io/docs/api-reference/text-to-speech/convert),
`POST /v1/text-to-speech/{voice_id}` with `xi-api-key` authentication.
For plain upstream rows, the compiled-in `wire-families.json` matches the exact
`wire.npm` package and `wire.shape` (including absence). Today's verification is
broca's pinned `wire_family` table at
[`d75a77db7249d4eb094ba667d1fa0d649e2e1682`](https://github.com/cortexkit/broca/blob/d75a77db7249d4eb094ba667d1fa0d649e2e1682/crates/broca-catalog/src/family.rs#L19-L40).
A new rule is added when someone verifies a package's protocol, citing the
package source or the provider's docs. Unknown packages or shapes produce no
fact. A row whose endpoint is withheld by a wire correction also gets no
`wire.family`, so protocol matching cannot admit that unaddressable route.

Present Code Assist aliases use their own curated `wire_family` in
`model-aliases.json`, verified from the pinned plugin source, never the target's
protocol. Retired aliases get none. A billing-plane read may replace the row's
value with the optional `wire_family` in `billing-planes.json`: `openai/chatgpt`
uses `openai.chatgpt_responses`, and `meta/oauth` uses `openai.responses`.
Other planes inherit the row's value. Curated plane-only and System One rows
have no package-derived protocol; a plane's verified value can still apply.
These derived facts are computed only on current upstream reads, never stored, and omitted on
`at_ms` reads. `fact_prefixes: ["wire.family"]` and `["wire."]` select them.

Curated rows may state their own sourced `wire.api` (`from: curated`)
and verified `wire.family`, without inheriting a provider default or a
package-derived rule. The endpoint must be HTTPS and the family must be an
admitted verified token. These declarations belong to the recorded curated
claim and may therefore also appear on its historical reads. A curated row
without declarations still carries neither fact; `wire.npm` and `wire.shape`
are never inferred or copied onto it. Gemini Flash TTS uses
`google.generate_content`, verified from the model page's supported
[GenerateContent method](https://ai.google.dev/gemini-api/docs/models/gemini-3.8-flash-tts).

`rate.output_audio` is USD per million output-audio tokens, not seconds or text
tokens. Upstream audio output passes a separate sourced unit gate from input
audio. `limit.output_audio_tokens_per_second` is a model's documented output
conversion, distinct from the input-only `limit.audio_tokens_per_second`.
ElevenLabs Flash uses `rate.input.per_character`: $0.04 per 1,000 characters,
with unknown `increment` and `minimum_quantity`, in the same unit-billed shape
as transcription's `rate.input_audio.per_second`.

Image and video token prices use `rate.input_image`, `rate.output_image` and
`rate.output_video` (USD per million tokens of the named modality, never per
image or second). An unstated modality price remains absent, not zero or the
text price. Gemini Omni's output conversion keys are
`limit.output_video_tokens_per_second.resolution.<resolution>` for `360p`,
`720p`, `1080p` and `4k`. The last two include upsampling. The explicit
resolution selector follows the same dotted selector convention as rate
context and TTL selectors; it does not select a different video price.

Current reads correct OpenCode Go's `wire.api` to the per-model base in
`@earendil-works/pi-ai` 1.4.0's catalog, verified against its provider code at
`earendil-works/pi@74f46864`, including the version path that its SDK adds.
Pi's route list is treated as complete for OpenCode Go: every OpenCode Go
model it does not route, including models upstream adds later, has no
`wire.api` on current reads, whatever upstream publishes for it.
Current upstream rows of `openai`, `anthropic`, `google` (Gemini API, not Vertex),
and `xai` also receive a sourced provider default when neither the model nor
the provider publishes an observed `wire.api`. Observed endpoints always win.
Aliases and curated model rows receive no default.
Each applied or withheld endpoint is disclosed in response-level
`wire_corrections`: `provider`, `model` (separate identity components), `field`
(`wire.api`), `withheld`, `source_ref` (`repo`, `commit`, `file`),
`established_by`, `established_at_ms`, and `review_by_ms` (60 days later).
A disclosure withheld because the model is absent from a complete route list
also carries `notes`, naming the source and commit that do not route it (for
OpenCode Go, `"not routed by pi at 74f46864"`); other disclosures omit it.
The array is omitted when empty and follows provider paging and fact-prefix
filters, including disclosure-only pages when an endpoint is withheld.
`wire.npm` and `wire.shape` are not corrected. Neither corrections nor
withholding change stored facts or `catalog.history`; `at_ms` reads retain the
uncorrected upstream claims.

The `rate.input_audio` rule: models.dev publishes `cost.input_audio` beside
the token rates with no unit, so the key alone establishes nothing. It is
served only for a provider listed in
`crates/fusiform-module/data/input-audio-per-token.json`, whose own pricing
page, quoted in that file, prices audio input per million tokens (see
[`curated-data.md`](curated-data.md) §8); a listed provider may exclude single
models its page does not support. Today only `google` is listed. Elsewhere
the key stays `unknown_charge_basis` internally and no fact is served: **an
absent `rate.input_audio` means the unit is not established, never that audio
is free.** Only the unconditional rate is served; a provider that prices audio
inside a context tier serves no audio rate, and a mode that prices audio is
refused whole, as before
(`models_dev_normalization::an_evidenced_provider_prices_input_audio_per_token`,
`models_dev_normalization::evidence_for_one_provider_does_not_attribute_another`,
`models_dev_normalization::tiered_audio_is_not_served_even_with_evidence`).
When the same provider row documents audio tokens per second, current reads
also serve `limit.audio_tokens_per_second` on rows carrying `rate.input_audio`,
including aliases borrowing that rate. Today Google's documented value is `32`.
The conversion is never stored or included in `at_ms` reads. Selecting only
the limit key still evaluates the rate gate and does not expose the internally
fetched rate.

The rate is stored as eras like every other models.dev rate, so `at_ms` reads
before it was first stored lack it, and a curated alias carries it marked
`inherited_from` with basis `alias`, like its other rates
(`input_audio_rates::a_point_in_time_read_before_the_fact_existed_lacks_it`,
`input_audio_rates::the_alias_carries_its_targets_input_audio_marked`). Added
in `fusiform-protocol` 0.40.0.

- **byte-affecting**: a consumer renders the value into a provider request,
  directly or indirectly, so a wrong value changes request bytes with nothing
  failing. This class records what consumers have reported about their own
  renderers; fusiform cannot verify it.
- **money**: a rate. Absent, stated zero and unpriced are distinct (§4.3).
- **advisory**: descriptive. A wrong value is visible rather than silent.

Structured output is a model-level claim only: it is omitted when models.dev
has no key, and it does not establish that any particular route enforces JSON
schema. Route behavior may differ, so alias projection does not carry this fact.

`model.type` follows the upstream's vocabulary rather than fusiform's curated
`model.kind` (`decision`/`extraction`). Unknown and empty strings are preserved,
not mapped to a closed set or inferred from modalities. Like `model.status`,
the label is included on an alias when the target publishes it. Curated rows are
served whole from `curated-models.json`, so they do not inherit either label
from an upstream row; no `model.type` labels are added to that table.

The table matches what the producer emits, in both directions
(`wire_fact_table::the_wire_fact_table_matches_what_the_producer_emits`,
`served_vocabulary::the_served_fact_set_is_exactly_this`), and every key
reaches the golden fixture
(`golden_payload::every_served_fact_key_reaches_the_fixture`).

Two families of keys extend the rate keys and are matched by prefix, because
their suffix comes from the upstream:

- **Context tiers**: `rate.<class>.above_context.<threshold>`, the rate that
  applies when the request's context exceeds the threshold
  (`ingest::a_tiered_rate_has_its_own_fact_key`). The catalog does not say
  whether crossing the threshold reprices the whole request or only the
  excess; that is the provider's billing rule.
- **Modes**: `rate.<class>.mode.<name>`, the rate in one of the upstream's
  named request modes, with the name restricted to `[a-z0-9_-]+`
  (`mode_rates::a_rate_read_returns_mode_rates_shaped_like_base_rates`). A
  class the mode does not price has no key, which means unpriced in that mode,
  never the base rate (`mode_rates::a_dimension_the_mode_omits_stays_absent`).
  The request bytes that select a mode are never served
  (`mode_rates::a_modes_request_bytes_are_never_served`).

### 4.2 Absent, null and zero

These are different states and fusiform never converts one into another.

1. **An absent key** means the upstream published nothing for that fact, or
   the read's filters excluded it, or the fact was withheld (§5.3). For a rate
   it is a statement about publication, never about price: an absent rate
   does not mean free (`served_corrections::an_ordinary_read_preserves_unpublished_rate_absence`).
2. **`null`** means unknown. A capability the upstream left out and one it
   set to `null` are both served as `null`
   (`models_dev_normalization::an_absent_capability_and_an_explicit_null_are_both_unknown`).
   Consumers must not coerce `null` to `false` or to a number.
3. **Limits**: an upstream limit of `0` is served as `null`, the same as an
   absent limit, because no model accepts zero tokens
   (`models_dev_normalization::a_zero_limit_and_an_absent_limit_are_both_unknown`).
   A consumer cannot recover whether the upstream wrote `0`.
4. **Modality lists**: `null` (unpublished) and `[]` (published as empty) are
   different claims (`models_dev_normalization::a_missing_modality_block_is_unknown_rather_than_empty`).
5. **Reasoning options**: `null` (unpublished), `[]` (the upstream states no
   options) and a populated list are served distinctly, and the list is
   verbatim, order included
   (`reasoning_options::absent_empty_and_populated_are_served_distinctly`,
   `reasoning_options::a_populated_list_round_trips_verbatim`).
6. **Rates**: a published `0` is `stated_zero`, never a priced zero and never
   absent (`models_dev_normalization::a_stated_zero_rate_is_distinct_from_no_rate`).
7. A stored value that fails to parse is served as its raw string, never as
   `null` (`crates/fusiform-module/src/route.rs`: `wire_value_tests::an_unparseable_row_never_becomes_null`).

### 4.3 Rate values

A rate is a JSON object tagged by `state` (`RateValue` in
`crates/fusiform-protocol/src/money.rs`):

| `state` | Fields | Meaning |
| --- | --- | --- |
| `priced` | `units`, `exponent`, `currency`, `unit_provenance`, `floor`, optional `per`, `increment`, `minimum_quantity`, `inherited_from` | an exact amount: `units × 10^-exponent` of `currency`; per million tokens unless the key names a billing unit |
| `stated_zero` | optional `inherited_from` | the source stated exactly zero; whether that is a real price is the consumer's policy |
| `unpriced` | `reason`, optional `inherited_from` | no rate can be stated, for the reason given |
| `billed_as` | `key` | on a billing-plane read, this class is billed at the rate of the full fact key named (see [`billing-planes.md`](billing-planes.md)) |

- Amounts are integers. Upstream decimals are converted exactly at the parse
  boundary and never become floats (`crates/fusiform-core/src/money.rs`:
  `a_nonzero_rate_never_becomes_free`, `plain_decimals_scale_exactly`).
- `unpriced.reason` is one of `missing_rate` (the source describes the model
  and publishes no rate for this class), `no_catalog_coverage`,
  `unknown_charge_basis` (a rate exists and what it charges for cannot be
  established), `not_established` (a token-class billing rule is not established),
  or `no_api_card` (a cross-provider quota plane has no present API row at the
  model's exact canonical id in its declared price provider). The last two are
  billing-plane reads only; stored history never carries them. `no_api_card`
  is introduced in `fusiform-protocol` 0.37.0.
- `unit_provenance.kind` is `stated`, `assumed_by_policy` (with `policy`) or
  `unknown`. models.dev publishes no currency, so its rates are USD under the
  policy `models-dev-usd-v1`, and every priced rate says so, including rates
  stored before the field existed
  (`served_corrections::a_rate_stored_before_the_field_existed_still_names_its_policy`,
  `served_corrections::only_one_currency_policy_has_ever_existed`).
- `floor.kind` is `unknown`, `none` or `minimum` (with `units`). Every rate
  fusiform serves today has `floor` `unknown`: nobody has established whether
  a minimum charge applies.
- Unit-billed keys use `rate.<role>.per_<unit>[.<selector>.<value>]...`, with
  units `second`, `character`, `image` or `request`. They are per-model keys,
  not enumerated base keys in `SERVED_FACTS`, like context-tier keys. Curated
  admission allows only `rate.input_audio.per_second` today; new keys require
  deliberate admission after the spend-pricing consumer recognises them.
- A cache-write price per cache lifetime is served as
  `rate.cache_write.ttl.<lifetime>`, and on a context tier as
  `rate.cache_write.above_context.<tokens>.ttl.<lifetime>`. These are token
  rates (per million tokens) on the rows that list
  `capability.cache_lifetimes`, current reads only. Each lifetime the model
  offers has its own key, its default included, so a write measured at a
  lifetime prices only at that key. Plain `rate.cache_write` keeps its meaning,
  the vendor's default lifetime. They are absent on the `(openai, chatgpt)`
  billing plane, the ChatGPT subscription login, which takes no cache
  lifetime ([`billing-planes.md`](billing-planes.md)).
- A unit-billed price states `per`, the positive quantity of the key's unit
  bought by the amount. `openai/gpt-transcribe` serves 4,500,000 × 10^-9 USD
  per 60 seconds, with no token rates. No `per_` key segment means the
  existing per-million-token meaning; token-rate bytes are unchanged.
- Since protocol 0.52.0, `increment.kind` is `unknown`, `none` or `quantum`
  (with `quantity`); it describes rounding billed usage up to a multiple.
  `minimum_quantity.kind` is `unknown`, `none` or `minimum` (with `quantity`);
  it is a minimum usage quantity, not the monetary `floor`. Unknown means
  unestablished, not no rounding or no minimum. Missing fields decode as
  unknown. Unknown terms are omitted on serialization only when `per` is
  absent, preserving existing token-rate bytes; unit rates state both terms
  even when unknown. Curated unit rates must explicitly carry all three fields.
- Every state the producer writes decodes with the protocol crate's type
  (`wire_types_decode_the_wire::every_served_rate_decodes_with_the_protocol_type`,
  `wire_types_decode_the_wire::the_fixture_carries_every_rate_state`).

### 4.4 `inherited_from`

A rate carrying `inherited_from` is not the serving provider's own statement.
The object names `provider_id`, `family`, `basis`, and, under `alias` or a
cross-provider `quota_proxy`, the target `model_id`. A consumer must not read
it as the cost of the route.

`family` is a required string. Exact-row aliases and cross-provider quota cards
use the existing `""` sentinel when the target family is absent or null; this
does not state a family name. Their target identity is established independently
of family. Open-weight inheritance instead requires the serving row's family
to find its curated creator, and skips inheritance if that family is missing.

| `basis` | When | Rule |
| --- | --- | --- |
| `open_weights` | an open-weight row whose provider published no rate at all, for a family whose creator is curated | the creator's published rate is served, marked (`crates/fusiform-module/src/route.rs`: `inheritance_tests::an_unpriced_open_weight_row_inherits_and_says_whose_price_it_is`) |
| `alias` | a curated alias id (§5.5) | the target row's rates are served, marked with the target's `model_id` |
| `quota_proxy` | a subscription billing plane | an API list price is served as a relative weight, not an amount billed; cross-provider planes name the canonical target provider and `model_id` (see [`billing-planes.md`](billing-planes.md)) |

The Kimi coding plane (`kimi-code-plan-global`, `apikey`) replaces its own zeros
with every published non-mode rate on the exact `moonshotai/<id>` canonical
row, base and tiered alike. Mode rates are stripped on every quota-proxy plane.
It marks each rate with that API row's provider, model and
family under `quota_proxy`. No canonical id, another prefix, or an absent API
row produces `unpriced` / `no_api_card`, never the plan's zero or a similar-name
price. A read without `auth_method`, other planes, and stored upstream claims
are unchanged; `at_ms` plane reads remain refused
(`billing_plane_tests::kimi_plan_borrows_only_exact_canonical_api_cards_without_changing_storage`
in `crates/fusiform-module/src/route.rs`,
`golden_payload::kimi_plane_cases_pin_borrowed_cards_and_missing_api_coverage`).

Open-weight inheritance rules, each with its test in `crates/fusiform-module/src/route.rs`:

1. A closed-weight row never inherits
   (`inheritance_tests::a_closed_weight_row_does_not_inherit`).
2. A provider that published any rate keeps its own card; a partial card is
   not topped up (`inheritance_tests::a_provider_that_published_any_rate_keeps_its_own_card`,
   `inheritance_tests::a_partial_card_is_not_topped_up_from_the_creator`).
3. A published rate is left exactly as published
   (`inheritance_tests::a_published_rate_is_left_exactly_as_published`).
4. Inheritance never supplies a mode rate
   (`inheritance_tests::inheritance_never_supplies_a_mode_rate`).
5. A point-in-time read does not inherit
   (`inheritance_tests::a_point_in_time_read_does_not_inherit`).
6. A family with no curated creator inherits nothing
   (`inheritance_tests::an_uncurated_family_inherits_nothing`).

The marker survives decoding in every rate state
(`wire_types_decode_the_wire::a_borrowed_marker_survives_the_published_type_in_every_state`).

### 4.5 Never served

Request header and body overrides are never served: the per-model `provider`
override block and the request-selecting parts of `experimental` (the
parameters and headers that switch a mode on)
(`served_vocabulary::unreliable_and_renderer_selecting_fields_are_not_facts`,
`served_vocabulary::mode_request_bytes_never_reach_a_fact`). The upstream's
endpoint claims are served instead as `wire.npm`, `wire.api` and `wire.shape`
(§4.1), never under their upstream names, and `wire.family` states a verified
request protocol. None of them supplies header or body overrides; how to encode
a request stays with the consumer.

### 4.6 `model.kind`

`model.kind` says what kind of work a model does. The values are the closed
set `MODEL_KINDS` in the protocol crate:

| Value | Meaning |
| --- | --- |
| `decision` | the model returns a typed choice, with probabilities, among options the request defines |
| `extraction` | the model returns typed values it reads, computes, estimates or forecasts from its input |

Neither kind writes conversational replies.

1. Only curated rows state it ([`curated-data.md`](curated-data.md) §4).
   models.dev publishes no such field and fusiform never infers one, so every
   upstream row lacks the key
   (`wire_fact_table::the_wire_fact_table_matches_what_the_producer_emits`).
2. **Absent means not stated.** It never means `chat`, and a consumer must not
   default it to `chat`.
3. **A consumer must refuse chat work on any row whose `model.kind` is present
   and is not `chat`.** A decision or extraction model given a chat prompt does
   not answer it.
4. A value outside `MODEL_KINDS` is rejected when the curated file loads, so it
   is never served
   (`curated_models::planted_model_kind_outside_the_vocabulary_is_rejected`).
   A new value arrives only with a new `fusiform-protocol` version; `model.kind`
   is introduced in 0.39.0.
5. It is advisory: it decides which work a model receives, not the bytes of a
   request.

## 5. `catalog.get`

### 5.1 Request

Every field is optional; `{}` reads the current catalog.

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `source` | string | `models.dev` | the upstream |
| `at_ms` | integer | now | resolve at this instant (epoch milliseconds) |
| `fact_prefixes` | array of strings | every fact | return only facts whose key starts with one of these |
| `include_retired` | boolean | `false` | include models whose `existence` is `absent` |
| `provider_id` | string | none | only this provider |
| `model_id` | string | none | only this model; requires `provider_id` |
| `auth_method` | string | none | select a billing plane: `apikey`, `chatgpt`, `oauth`, `antigravity`, or `gateway` (Anthropic only); requires `provider_id`, current-only; see [`billing-planes.md`](billing-planes.md) |
| `paged` | boolean | `false` | read whole providers in bounded pages; no provider/model/plane selector |
| `page_token` | string | none | opaque continuation from `next_page_token`; requires `paged: true` |

`model_id` without `provider_id` is `bad_request`
(`route::a_model_id_without_a_provider_is_refused`).

### 5.2 Response

| Field | Present | Meaning |
| --- | --- | --- |
| `source` | always | the upstream read |
| `resolved_at_ms` | always | §3 |
| `catalog_version` | always | §3 (`route::the_response_carries_the_catalog_version`) |
| `models` | always | §4 |
| `next_page_token` | on a non-final paged response | opaque continuation for the next page; absent on all unpaged reads |
| `withheld` | when non-empty | §5.3 |
| `retired` | when non-empty | §5.4 |
| `uncertain` | when non-empty | §5.4 |
| `aliased` | when non-empty | §5.5 |
| `curated` | when non-empty | §5.6 |
| `wire_corrections` | when non-empty | sourced current-read corrections/withholding of `wire.api` (§4.1) |
| `fact_overrides` | when non-empty | sourced current-read values served in place of the upstream's (§5.8) |
| `plane` | on a billing-plane read | [`billing-planes.md`](billing-planes.md) |
| `billing_rules` | on a billing-plane read, when non-empty | [`billing-planes.md`](billing-planes.md) |

Every response field reaches the golden fixture
(`golden_payload::every_read_response_field_reaches_the_fixture`), and the
served bytes match it (`golden_payload::the_served_payloads_match_the_golden_fixture`).
A read without `auth_method` carries no plane fields
(`golden_payload::every_non_plane_golden_case_has_no_plane_bytes`).

#### Whole-catalog paging

Send `{"paged":true}` for page one. Repeat the same filters (including `at_ms`
if supplied) with `page_token` set to the previous `next_page_token` until that
field is absent. Providers are ordered by ascending provider id (the model map
is a `BTreeMap` keyed by `provider/model`); a provider is never split. Pages
greedily include whole providers while the **actual compact serialized response**,
including disclosures and the token, fits 3 MiB (3 × 1024 × 1024 bytes). This
leaves envelope/framing headroom below the supervisor's 4 MiB per-module-reply
frame limit. A single provider exceeding 3 MiB is returned alone, never dropped;
a provider exceeding 4 MiB would require model-level paging.

All model-scoped disclosures (`withheld`, `uncertain`, `curated`, `aliased`,
`retired`, `billing_rules`, `wire_corrections`, `fact_overrides`) are scoped to that page's providers. Providers with
only withheld facts still have a page. Bulk reads do not populate `retired`,
and paging cannot select a billing plane, so `plane`/`billing_rules` are absent.
`source`, `catalog_version`, and **page one's `resolved_at_ms`** repeat on every
page. Merge the model maps and concatenate disclosures to reconstruct the read.

The opaque token's current versioned encoding is
`v1:<catalog_version>:<resolved_at_ms>:<last_provider_id>`. The two numbers are
canonical decimal i64 values; only the first three colons are separators.
Consumers must forward the token unchanged, not construct it. A malformed token,
a token without `paged: true`, or paging combined with `provider_id`, `model_id`,
or `auth_method` is `bad_request`.

If the store's current version differs from the token's, the continuation is
refused with its own code, `catalog_changed`, so a reader can recognise it
without matching message text. Retrying the same token cannot repair it.
Discard partial pages and begin a new read from page one. An unchanged version means unchanged stored content,
so subsequent pages truthfully report the first page's instant. Current reads
remain current reads internally: converting them into `at_ms` reads would skip
serve-time inheritance, aliases, and curated rows and change their values.
Historical paging is supported by repeating the explicit `at_ms` on every page.

**Unpaged whole-catalog reads:** an unpaged read with no `provider_id` whose
compact reply would exceed 4 MiB, the supervisor's limit for one reply frame,
is refused with `bad_request` naming `"paged": true`. It is never truncated,
because a partial catalog would make the missing models look retired. A reply
exactly at the limit is served whole, and provider-scoped reads are never
checked
(`route::paging_tests::an_unpaged_whole_catalog_over_the_frame_limit_is_refused_never_truncated`).
`ck models get` without `--provider` pages automatically, restarting at most
three times on snapshot refusals or version changes, and merges before JSON
output or rendering. Provider-scoped CLI reads remain unpaged.

Tests: `route::paging_tests::pages_cover_whole_providers_in_order_and_scope_disclosures`,
`route::paging_tests::disclosure_only_providers_survive_and_oversized_providers_are_not_split`,
`route::paging_tests::continuation_after_era_write_is_refused`,
`route::paging_tests::invalid_paging_requests_are_bad_requests`,
`route::paging_tests::embedded_seed_paging_measurement_and_unpaged_compatibility`,
`paging_tests::paging_loop_merges_and_restarts_on_version_change`, and
`paging_tests::paging_loop_restarts_snapshot_refusals_and_bounds_restarts`.
The golden case `catalog.get — first provider page` carries a real continuation.

### 5.3 Refusals and withheld facts

1. An unknown provider is `no_coverage`, on both the bulk and the named path,
   rather than an empty catalog
   (`route::an_unknown_provider_is_refused_rather_than_answered_with_zero`,
   `route::an_unknown_provider_is_refused_on_the_single_model_path_too`).
   Provider ids are models.dev's own slugs.
2. An unknown model under a known provider is `no_coverage`, and the message
   says the provider exists (`route::an_unknown_model_names_the_provider_as_known`).
3. A known model that is retired, or did not yet exist at `at_ms`, is an
   answer, not a refusal
   (`route::a_known_but_retired_model_answers_rather_than_refusing`,
   `route::a_known_model_before_it_existed_answers_rather_than_refusing`).
4. A fact covered by a correction at the read instant is left out of `models`
   and named in `withheld`, with every covering correction's window, reason and
   fields (`route::a_corrected_fact_is_named_as_withheld_on_the_wire`,
   `route::the_single_model_read_also_names_a_withheld_fact`). A model whose
   facts are all withheld is still listed
   (`route::a_model_whose_facts_are_all_withheld_is_still_discoverable`).

### 5.4 Retired and uncertain

- **`retired`**: on a named read (`provider_id` and `model_id`) of a retired
  model without `include_retired`, the model is named here with
  `retired_at_ms`, the instant fusiform first observed it gone
  (`route::a_named_read_of_a_retired_model_says_so`). A bulk read omits retired
  models without listing them (`route::retired_models_are_excluded_by_default`).
  A retired alias id and a curated row the file holds are reported the same
  way on current reads, with the instant fusiform stopped serving them, and a
  retired-inclusive read lists them marked `existence: absent` (see
  [`curated-data.md`](curated-data.md) §4, "Held rows").
  A retired alias with a sourced replacement also carries `successor`, a full
  `provider/model` identity: on the absent row for `include_retired` reads, or
  on its named `retired` entry otherwise. The row fact respects fact-prefix
  filters. No successor is inferred when none is curated.
- **`uncertain`**: on a historical read, a fact whose value is served but sits
  inside a later era's observation window is listed with
  `superseded_after_ms` (the last instant the value was confirmed) and
  `superseded_by_ms` (the instant it was seen changed). The change happened in
  `(superseded_after_ms, superseded_by_ms]`. Always empty on a current read
  (`route::a_historical_read_reports_uncertainty_on_the_wire`).

### 5.5 Aliases

Some routes expose a model under an id models.dev does not carry. A curated
alias (see [`curated-data.md`](curated-data.md)) serves the target row's facts
under the alias id and names it in `aliased` with `target_provider_id`,
`target_model_id` and `source_ref`. The target's `capability.structured_output`
claim is not projected: whether JSON Schema is enforced depends on the route,
which a model-level fact cannot establish. The target's `wire.*` claims are
also omitted because an alias reaches a different backend. Rules, tested in
`crates/fusiform-module/src/route.rs`:

1. Current reads only; a point-in-time read never aliases
   (`alias_tests::a_point_in_time_read_never_aliases`).
2. A real upstream row for the alias id always wins
   (`alias_tests::a_real_row_for_the_alias_id_wins`).
3. Rates through an alias carry `inherited_from` with basis `alias`
   (`alias_tests::a_named_alias_read_serves_the_target_under_the_alias_key`),
   including mode rates (`mode_rates::an_alias_carries_its_targets_mode_rates_marked`).
4. Structured-output claims are omitted from aliases while remaining on the
   target row (`alias_tests::structured_output_is_not_projected_onto_an_alias`).
   The same holds for `wire.npm`, `wire.api` and `wire.shape`
   (`alias_tests::wire_facts_are_not_projected_onto_an_alias`).
   Route facts never project from the target either. When the alias table
   establishes tier routes, only that alias receives
   `route.reasoning_wire_ids` and `route.reasoning_default`. Every effort must
   appear in the target's served `capability.reasoning_options`. Uncurated
   aliases carry neither fact; fact-prefix filters apply normally.
5. A retired or absent target serves nothing
   (`alias_tests::a_retired_target_serves_nothing`,
   `alias_tests::an_absent_target_serves_nothing_and_names_it`).

### 5.6 Curated rows

`curated` lists rows served from `data/curated-models.json` rather than from
stored upstream observations. Each entry has `model`, `auth_methods`,
`source_ref` and `review_by_ms`. `auth_methods` is an allowlist: a consumer
must refuse a credential whose login method is not listed. A model absent
from `curated` carries no such restriction. A curated row may be the
only thing fusiform serves under its provider (`typesafe`, which models.dev
does not list): such a provider is known to every read, so a named read of a
model it does not carry is refused as an unknown model, not an unknown
provider (`route::system_one_tests::an_unknown_typesafe_model_is_refused_as_an_unknown_model`).
On an `at_ms` read, each entry comes from the curated claim in force at that
instant (§3 item 7).
Rules are in [`curated-data.md`](curated-data.md) §4.

### 5.7 Canonical links

`canonical_links` discloses sourced lab identities supplied where the upstream
row's `model.canonical_id` is null or absent. It is an array, omitted when empty,
whose entries have `model`, `canonical_id`, `source_ref`, `quote`,
`lab_source_ref`, `lab_quote` and `review_by_ms`. For example, a current
`ollama-cloud/kimi-k2.6` read serves `model.canonical_id: "moonshotai/kimi-k2.6"`
and names that route, value, the Ollama library sentence and the models.dev lab
entry in the disclosure. It is a curated identity claim, not an observation.

Any non-null upstream identity wins without a disclosure, even if it disagrees.
`at_ms` reads never apply links. Named, bulk and paged current reads all fill
only null or absent identities and disclose each applied link. Fact-prefix
filters still apply: `model.` and `model.canonical_id`
include the value and disclosure; `rate.` alone includes neither. Neither the
stored fact nor its history changes. Sources and review rules are in
[`curated-data.md`](curated-data.md) §5.

### 5.8 Fact overrides

`fact_overrides` discloses each fact whose served value is a sourced curated
value rather than the upstream's. It is an array, omitted when empty, whose
entries have `model` (the `provider/model` entry in `models` carrying the
value), `provider_id` and `model_id` (the row the override is declared for),
`fact_key`, `value` (what `models` serves), `replaces` (the upstream value it
replaces, or `null` for an addition), `reason`, `source_ref`, `quote`,
optional `notes`, `established_by`, `established_at_ms` and `review_by_ms`.

An entry is either a REPLACEMENT, whose `replaces` is the upstream value it
corrects, or an ADDITION, whose `replaces` is always present and `null`: a
fact models.dev publishes no value for, sourced from the vendor's own
documentation. The added facts are `capability.server_side_fallback` and
`capability.mid_conversation_system`.

For example, a current `google/gemini-4-argon` read serves
`limit.output: 65536` where models.dev publishes 32,768, Google's cap for a
non-streaming request; Google's streaming cap is 65,536. The disclosure names
both values and quotes Google's model page and latest-model guide
(`golden_payload`: `catalog.get — sourced output override`). A current
`anthropic/claude-opus-5-5` read serves `capability.server_side_fallback:
true`, meaning the Claude API accepts `fallbacks: "default"` with the
`server-side-fallback-2026-07-01` beta header for it, and discloses an
addition quoting Anthropic's refusals-and-fallback page
(`golden_payload`: `catalog.get — sourced server-side fallback addition`).
`anthropic/claude-haiku-5-5` serves `false`, which Anthropic states.
`capability.mid_conversation_system` describes only persistent `role: "system"`
messages inside `messages`, without a beta header, not the beta turn-scoped
`clear_at` variant. Supported `anthropic` rows serve `true`, Sonnet 5 and
Opus 4.7 serve `false`, and absence on any other row means not stated. Its
additions use the same dated-history and upstream-wins rules.

1. **Compiled current reads, stored historical reads.** Current reads use the
    compiled file unchanged. Every boot diffs its claims into
    `fact_override_era`: the first era starts at `established_at_ms`; changes
    and removals start at boot, clamped to the latest boundary if the clock is
    behind. Provenance-only edits open no era and retain stored provenance.
    `at_ms` reads apply and disclose the claim in force, or leave upstream
    untouched before the first era and after a tombstone. `catalog.history`
    still lists upstream eras only. For example, the golden case
    `catalog.get — dated Flash Image split rates` serves $3/M text output and
    $60/M image output at 2026-11-11 23:36:33Z from the dated override history
    (`fact_overrides::dated_flash_image_overrides_split_text_and_image_after_establishment`).
2. **Only while upstream publishes `replaces`.** When the stored upstream
   value is anything else, including the override's own value, or the fact is
   absent or withheld by a correction, the upstream value is served and
   nothing is disclosed. An override can never hide a later upstream change
   (`fact_overrides::an_upstream_value_other_than_replaces_is_served_as_published`).
   An addition applies only while the stored upstream row has no such fact:
   once upstream records the key, with any value including `null`, or a
    correction withholds it, the addition stops applying
    (`fact_overrides::an_upstream_value_for_the_key_suppresses_the_addition`).
    On an `at_ms` read both guards use the stored upstream at that instant,
    never today's upstream (`fact_overrides::dated_override_guards_use_upstream_at_the_requested_instant`).
3. **Named, bulk and paged reads alike**, and billing-plane (`auth_method`)
   reads, which are current-only and change only rates
   (`fact_overrides::a_current_read_serves_the_override_and_discloses_it`,
   `fact_overrides::a_current_read_serves_the_documented_server_side_fallback`).
   Paging keeps each entry with the provider of its `model`.
4. **Current aliases carry it.** Aliases remain current-only. An alias entry is built from its target's served
   facts, so it serves the overridden value too, and its disclosure has the
   alias id in `model` and the target in `provider_id`/`model_id`
   (`fact_overrides::the_antigravity_alias_serves_the_overridden_value`).
   `capability.server_side_fallback` and `capability.mid_conversation_system`
   are exceptions: they describe what the Claude API accepts, so an alias route
   reaching the model another way never carries them
   (`fact_overrides::the_antigravity_route_to_opus_does_not_carry_server_side_fallback`).
   A curated row (§5.6) is fusiform's own data and replaces no upstream value,
   so no override applies to it.
5. **Filters apply.** A fact-prefix filter that excludes the fact excludes the
   disclosure too
   (`fact_overrides::a_filter_excluding_the_fact_serves_and_discloses_nothing`).
   A filter naming an added fact still reaches the rows it is added to, even
   though the store holds no fact under that key.

Rules for the data are in [`curated-data.md`](curated-data.md) §10.

### 5.9 Operator quality scores

Current `catalog.get` reads join the operator's current store grades to each
row's served `model.canonical_id`. A row without a canonical id uses its own
`provider/model` key. Several providers serving the same canonical model share
one grade. Alias rows receive grades only through a matching served canonical
id, never through a target's provider/model key. Billing-plane reads keep the
plain read's grades for the same row.

Chat rows (absent `model.type` or `chat`) receive `quality.elo` (general quality)
and `quality.eq` (writing and judgment). Rows with a set non-chat `model.type`
(image, speech, transcription, video, embedding, reranking, realtime) receive
only `quality.grade`. Higher grades are better, and `quality.grade` is comparable
only among rows with the same `model.type`. All three dimensions are
advisory judgments for routing selection, not upstream measurements or
instructions changing request bytes. Withdrawn grades are absent facts. Numbers
retain their decimal representation without a binary-float conversion.
`fact_prefixes` applies normally (`["quality."]` selects all quality dimensions).

When at least one quality fact survives in the response, `quality_scores`
appears once at response level:

```json
{
  "source": "models.dev",
  "resolved_at_ms": 1000,
  "catalog_version": 1,
  "models": {
    "synthetic-reseller/model": {"quality.elo": 1284, "quality.eq": 1599.8}
  },
  "quality_scores": {"graded_by": "operator", "graded_at_ms": 123}
}
```

The example uses synthetic grades. `graded_at_ms` is the newest `recorded_at_ms`
among grades actually served in this response, in epoch milliseconds. An
unrelated grade write does not redate another model's grade. There is no
per-model attribution duplication. The object
is omitted for an empty grade table, unmatched rows, filters excluding quality,
and pages with no quality facts. Grades have their own append-only history;
point-in-time (`at_ms`) catalog reads still carry no quality facts or attribution,
and `catalog.history` remains upstream fact history. See
[`curated-data.md`](curated-data.md) §11 for storage and operator commands.

#### Operator grade tools

`quality.grade {model, elo, eq, reason, dry_run?: true}` (chat) or
`quality.grade {model, grade, reason, dry_run?: true}` (non-chat), and
`quality.withdraw {model, reason, dry_run?: true}` are Mutating tools that preview
by default. Only explicit `dry_run: false` appends a row. Each committed row
advances `catalog_version` in the same transaction, using the poll-write rule
`max(now_ms, current + 1)`; previews and the migration import do not advance it.
This version is also returned by `catalog.status`, so polling consumers and
cached catalog page plans notice live grade changes.

`model` names a canonical id carried by a current served row, or the row's own
`provider/model` id when it has no canonical id. An unknown grade key refuses
`bad_request`, naming the key and the unavailable `model.type`. The dimensions
must match every affected row's served type: `grade` on chat, `elo` or `eq` on
non-chat, mixed dimensions, and incomplete grades refuse `bad_request`, naming
the row's `model.type` and required dimensions. All numbers must be finite and
positive; invalid numbers and empty reasons refuse `bad_request`. Withdrawals
without a current grade refuse `no_coverage`. Withdrawing a previously graded
key remains possible after its last served row disappears.

Both write responses have `dry_run`, `model`, `current` (the current grade, or
null), `new` (the proposed or committed record), `affected_count`, and
`affected_ids` (up to ten served row ids in lexical order). A grade record has
`model`, `elo`, `eq`, `withdrawn`, `graded_by`, `reason`, and `recorded_at_ms`.
Chat records carry numeric `elo` and `eq` and omit `grade`, retaining their
existing wire shape. Non-chat records add numeric `grade` and set `elo` and `eq`
null. Withdrawal records set `withdrawn: true`, both chat numbers null, and omit
`grade`.

CLI equivalents are `ck models grade <model> --elo N --eq N --reason TEXT`
and `ck models grade <model> --grade N --reason TEXT`; both preview unless
`--commit` is supplied. `ck models grades` and `withdraw-grade` handle either
kind of judgment.

`quality.grades {model?: key}` is Pure. With no key it returns
`{grades: [...]}` containing every current, non-withdrawn grade ordered by key.
With a key it returns that key's full history newest first, withdrawals
included; no history refuses `no_coverage`. Request and response types live in
`fusiform-protocol`.

### 5.10 Model lineage

`model.lineage` is a curated per-row classification for routing panel diversity
(one seat per lineage) and same-lineage cost fallback. Its closed string set is
`claude`, `gpt`, `gemini`, `grok`, `qwen`, `kimi`, `glm`, `deepseek`, `minimax`,
`mistral`, `llama`. An unclassified row has no fact. The string `other` is never
served; consumers may treat absence as their own other group.

The compiled-in `model-lineage.json` applies exact model overrides first, then
an exact models.dev `model.family` mapping. Only a row without a family may
fall back to the lab prefix of its served `model.canonical_id`. If both fields
are absent, a final map of first-party providers may establish lineage; a
reseller id or model name never does. A present but unmapped family blocks both
fallbacks. Gemma belongs to `gemini`; Muse belongs to `llama` under routing's
current grouping. These are routing policy groups, not claims of identical
weights or request formats.

Aliases use their own exact override or their own served canonical identity,
never a target's exact-model override or copied family. The fact is computed
on current `catalog.get` reads, including billing-plane reads, before the final
`fact_prefixes` filter (`model.lineage` and `model.` both select it). It is never
stored, never present on `at_ms` reads, and adds no history or disclosure field.
See [curated data §12](curated-data.md#12-model-lineage-model-lineagejson).

## 6. `catalog.history`

Every era for one fact, oldest first.

Request: `provider_id`, `model_id` and `fact_key` (all required), optional
`source`.

Response: `source`, `provider_id`, `model_id`, `fact_key`, `eras`, and the
optional `last_changed_at_ms`, `inherited_from` and `curated`.

Each era carries `value`, `boundary_at_ms`, `boundary_kind` (`observed`,
`asserted`, `seed` or `corrected`), `window_from_ms` on observed boundaries
only, and `correction` (`affected_from_ms`, `affected_until_ms`, `reason`,
`fields`) on corrected boundaries only
(`route::history_returns_eras_with_their_windows`,
`route::history_carries_the_correction_extent`).

1. `last_changed_at_ms` is when the value last genuinely changed. It ignores
   eras from polls marked as artifacts, so it can be older than the newest
   boundary (`route::the_served_last_change_ignores_an_artifact_poll`). It is
   absent when the fact has no era.
2. An empty `eras` list says which of its causes applies: an unknown provider
   or model is `no_coverage`; a known model with nothing recorded for the key
   returns an empty list (`route::an_empty_history_names_which_cause_applies`).
3. A rate served by open-weight inheritance has no eras on the serving row;
   the response names the origin in `inherited_from`
   (`crates/fusiform-module/src/route.rs`: `inheritance_tests::the_history_route_serves_the_origin`).
4. An alias id has no eras; `inherited_from` names the target row with basis
   `alias` (`crates/fusiform-module/src/route.rs`: `alias_tests::history_of_an_alias_names_the_target`).
5. A curated row has no eras; `curated` names its source
   (`crates/fusiform-module/src/route.rs`: `curated_tests::curated_history_names_the_source_without_storing_eras_or_changing_status`).
   The curated row's own history is recorded separately and served through
   `catalog.get` with `at_ms` (§3 item 7), not through `catalog.history`.

## 7. `catalog.status`

What fusiform has been doing. Request: optional `source`, optional `polls`
(how many recent polls; default 10).

| Field | Meaning |
| --- | --- |
| `catalog_version` | §3; a cursor to compare, not a snapshot identity |
| `model_count` | stored upstream models present now; curated rows and aliases are not counted |
| `models_priced` | how many of those carry at least one rate (`route::status_reports_how_many_models_can_be_priced`) |
| `served_facts` | every key fusiform can serve, whether or not any model carries it (`route::status_names_served_keys_no_model_in_this_store_carries`) |
| `history_begins_at_ms` | the earliest instant a point-in-time read accepts; absent on an empty store |
| `era_count` | total stored eras |
| `failures` | absent when no poll has ever failed; otherwise `ever`, `last_at_ms`, `polls_back` and `last_class` (`served_corrections::a_failure_older_than_the_poll_window_is_still_reported`) |
| `recent_polls` | newest first, including 304s and failures (`route::status_reports_quiet_polls_and_failures`) |

Each poll carries `observed_at_ms`, `outcome`, and when set `failure_class`,
`detail`, `duration_ms`, and `changes` (`eras`, `models_arrived`,
`models_withdrawn`, `facts_changed`). `changes` is absent for a poll that
wrote nothing (`route::status_reports_what_each_poll_changed`,
`route::a_poll_that_wrote_nothing_reports_no_changes`). Optional fields absent
from an older module's response still decode
(`deps::a_status_response_without_the_newest_fields_still_parses`).

## 8. Writes

Three tools write. All three preview by default: `dry_run` defaults to `true`,
so an omitted flag cannot write (`deps::an_omitted_dry_run_previews_rather_than_writes`).

### 8.1 `catalog.correct`

Request: `provider_id`, `model_id`, `fields`, `affected_from_ms`,
`affected_until_ms`, `reason` (all required), optional `source`, optional
`dry_run`. `fields` are field ids as the served vocabulary spells them, for
example `{"field":"rate","class":"input"}`, `{"field":"limit","limit":"context"}`,
`{"field":"existence"}`.

Response: `written`, `facts` (each `fact_key`, the `value` that stays in force,
`current_since_ms`), `affected_from_ms`, `affected_until_ms`, `reason`.

1. A correction names exactly one model; there is no wildcard
   (`correct_route::a_correction_cannot_name_all_models`).
2. A blank reason, an empty field list or an unrecognised field is
   `bad_request` (`correct_route::a_correction_without_a_reason_is_refused`,
   `correct_route::an_empty_field_list_is_refused`,
   `correct_route::an_unrecognised_field_is_refused_rather_than_dropped`).
3. A correction the store's planner rejects (the cases in
   [`data-model.md`](data-model.md) §5, rule 5) is `refused`, with every reason
   joined in the message
   (`correct_route::a_window_containing_the_present_is_refused_with_instructions`).
4. The preview and the commit agree
   (`correct_route::the_dry_run_and_the_commit_agree`), and a committed
   correction makes reads inside the window withhold the fact
   (`correct_route::a_committed_correction_makes_the_window_refuse`).

The storage rules are in [`data-model.md`](data-model.md) §5.

### 8.2 `catalog.mark_artifact` and `catalog.retract_artifact`

Request: `observation_id`, `reason` (non-blank), optional `dry_run`.

- Mark response: `observation_id`, `committed`, `restating_eras` (eras of that
  poll that restate the claim already in force), `total_eras`,
  `observed_at_ms` (`route::marking_previews_before_it_writes`).
- Retract response: `observation_id`, `committed`, `had_live_mark`. Nothing is
  written when there is no live mark.
- An observation id the store does not hold is `no_coverage`
  (`route::marking_an_unknown_observation_is_not_a_bad_request`).

The storage rules are in [`data-model.md`](data-model.md) §6.

## 9. `plan.prices`

Curated subscription list prices, current reads only. See
[`plan-prices.md`](plan-prices.md) for the complete row contract.

Each priced row may carry `service_tiers`, independent of `quota_value`:

- Absent means unknown (never curated), **not** that the plan has no tiers.
- `state: "established"` carries `tiers`, `source_ref`, `established_by`,
  `established_at_ms` and `review_by_ms`.
- Each tier has `tier` (`fast` or `ultrafast`, a closed vocabulary),
  `included_usage_rate` and `credit_rate`. Both rates are exact decimals
  `{units, exponent}`, meaning `units / 10^exponent` relative to Standard.
  For example, Fast consumes included usage at `{units: 25, exponent: 1}`
  (2.5x), and purchased credits after the allowance at `{units: 2, exponent: 0}`
  (2x). No floats are used.
- `state: "not_established"` instead carries `refusal_reason` and `review_by_ms`.

Standard is implicit and never listed. Tier names match model rows'
`rate.<dim>.mode.<name>` suffixes; which models offer a tier stays on those
rows, not here. Plan entitlements are joined at read time, never stored as
plan-price eras. Their own review dates can expire independently of the price.

Each priced row may also carry `concurrency`, a simultaneous-request cap for
the subscription account, not a requests-per-time rate. **Absent means unknown,
never unlimited and never zero.** The object carries a positive integer `limit`,
`scope` (`account` for all models together, or `account_model` for each model
separately), optional `per_model` model-key-to-positive-integer overrides,
`source_ref`, a verbatim first-party `quote`, `established_at_ms` and `review_by_ms`.
`per_model` is valid only with `account_model`; omitted overrides use `limit`.
Like service tiers, concurrency is joined by `(provider_id, tier)` at each
current read, omitted for tombstones, and never stored or given an era. Its
independent review is due after 60 days. Consumers choose their own settings
overrides; the catalog does not infer a cap from usage or rate limits.

## 10. Versioning

1. The wire types are the `fusiform-protocol` crate, whose version moves with
   any change that is not comments or blank lines. CI enforces this with
   `scripts/check-wire-crate-version.sh origin/master`.
2. The crate depends on `serde` and `serde_json` only
   (`deps::the_wire_crate_stays_free_of_the_modules_internals`).
3. Every response type round-trips through serde
   (`deps::every_response_type_round_trips`), and a response from before the
   billing-plane fields existed still decodes
   (`billing_planes::a_pre_plane_response_decodes_and_omits_the_new_fields_on_encode`).
4. Fusiform emits no pushes. Every consumer pulls.
