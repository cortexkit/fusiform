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
   `catalog.correct` are declared to reject unknown fields, so a misspelled
   field is an error rather than a silently different answer. The test covers
   `catalog.get` (`route::a_malformed_request_is_refused_rather_than_defaulted`).
   The requests for `catalog.mark_artifact`, `catalog.retract_artifact` and
   `plan.prices` do not reject unknown fields; they are ignored there.
5. `source` is optional on `catalog.get`, `catalog.history`, `catalog.status`
   and `catalog.correct`, and defaults to `models.dev`, the only accepted
   value. Any other value, including `seed`, is `bad_request`
   (`route::every_accepted_source_can_match_a_row`).

The seven tools are listed in [`module-surface.md`](module-surface.md).

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
2. `catalog_version` is `max(now_ms, previous + 1)` at each poll that changed
   something, so it never goes backwards, even across a restore of the
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
| `limit.context` | byte-affecting | integer token count, or `null` |
| `limit.output` | byte-affecting | integer token count, or `null` |
| `capability.reasoning` | byte-affecting | `true`, `false` or `null` |
| `capability.reasoning_options` | byte-affecting | the upstream's `reasoning_options` array verbatim, or `null` |
| `capability.structured_output` | advisory | `true` or `false` only when published by models.dev; otherwise no fact |
| `capability.tool_call` | advisory | `true`, `false` or `null` |
| `capability.attachment` | advisory | `true`, `false` or `null` |
| `capability.input_modalities` | advisory | list of modality strings, or `null` |
| `capability.output_modalities` | advisory | list of modality strings, or `null` |
| `model.family` | advisory | upstream family string, or `null` |
| `model.open_weights` | advisory | `true`, `false` or `null` |
| `model.canonical_id` | advisory | upstream canonical model id, or `null` |
| `model.status` | advisory | upstream lifecycle label verbatim, or `null` |
| `model.kind` | advisory | `"decision"` or `"extraction"`, on curated rows only; otherwise no fact (§4.6) |
| `wire.npm` | advisory | `{"value": "<published client package>", "from": "model" or "provider"}`; otherwise no fact |
| `wire.api` | advisory | `{"value": "<base URL>", "from": "model", "provider" or "curated"}`; otherwise no fact; corrections/withholding disclosed in `wire_corrections` |
| `wire.shape` | advisory | `{"value": "<published request shape>", "from": "model"}`; otherwise no fact |
| `route.reasoning_wire_ids` | byte-affecting | curated effort → backend model id object on live aliases only; otherwise no fact |
| `route.reasoning_default` | byte-affecting | default effort selected when the alias is requested without a tier suffix; a key in `route.reasoning_wire_ids`; otherwise no fact |
| `successor` | advisory | sourced replacement `provider/model` identity on retired aliases; otherwise no fact |
| `rate.input` | money | rate value (§4.3), per million input tokens |
| `rate.output` | money | per million output tokens |
| `rate.cache_read` | money | per million cached-read tokens |
| `rate.cache_write` | money | per million cache-write tokens |
| `rate.reasoning` | money | per million reasoning tokens |
| `rate.input_audio` | money | per million audio input tokens, only for providers whose docs establish per-token audio billing |

Except for explicitly disclosed corrections, the `wire.*` values are models.dev's claims, not fusiform's transport policy.
For each field independently, a model's `provider.npm` or `provider.api` wins
over its provider's `npm` or `api` default; `from` names the level that supplied
the value. `provider.shape` exists only on models. Missing fields create **no
fact**: absence means unknown, never an inferred default. Strings pass through
verbatim, including empty strings, URL spelling and trailing slashes. Consumers
own package-to-renderer mapping and all transport quirks. Alias rows do not
inherit these backend-specific claims (§5.5), and curated rows carry none.

Current reads correct OpenCode Go's `wire.api` to the per-model base in
`@earendil-works/pi-ai` 1.4.0's catalog, verified against its provider code at
`earendil-works/pi@74f46864`. Rows curated as unsupported by that catalog have no `wire.api`.
Each applied or withheld endpoint is disclosed in response-level
`wire_corrections`: `provider`, `model` (separate identity components), `field`
(`wire.api`), `withheld`, `source_ref` (`repo`, `commit`, `file`),
`established_by`, `established_at_ms`, and `review_by_ms` (60 days later).
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
| `priced` | `units`, `exponent`, `currency`, `unit_provenance`, `floor`, optional `inherited_from` | an exact amount: `units × 10^-exponent` of `currency` per million tokens |
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

Fields that select how a request is rendered (`provider.npm`, per-model
provider overrides, and the request-selecting parts of `experimental`) are
parsed, quarantined and never served
(`models_dev_normalization::renderer_selecting_fields_are_quarantined`,
`served_vocabulary::unreliable_and_renderer_selecting_fields_are_not_facts`).
Fusiform says what a model is, never how to speak to it.

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
| `auth_method` | string | none | select a billing plane; see [`billing-planes.md`](billing-planes.md) |
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
`retired`, `billing_rules`, `wire_corrections`) are scoped to that page's providers. Providers with
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

Curated subscription list prices. See [`plan-prices.md`](plan-prices.md).

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
