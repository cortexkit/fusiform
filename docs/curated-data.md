# Curated data

Most of what fusiform serves is observed: fetched from models.dev and stored
as eras. A small set of facts is curated by hand in files under
`crates/fusiform-module/data/`, compiled into the binary, and generally reviewed
on a schedule. Routing lineage is an undated classification, not a dated price
or vendor claim. This document lists those files, what each may claim, and the gates
that keep them honest.

| File | Holds | Applied |
| --- | --- | --- |
| `model-creators.json` | which provider published an open-weight family | at read time |
| `model-aliases.json` | route-specific ids and the published row each one is | at read time, current reads only |
| `curated-models.json` | provider rows models.dev does not carry | at read time on current reads; written to `curated_model_era` at start, which point-in-time reads serve (§4) |
| `canonical-links.json` | sourced lab identities missing from upstream provider rows | at read time, current reads only |
| store quality grades | operator-assigned ELO and EQ judgments for routing selection | at read time, current reads only (§11) |
| `model-lineage.json` | curated routing diversity and cost-fallback groups | computed per row at read time, current reads only (§12) |
| `wire-corrections.json` | sourced per-model endpoint corrections or withholding, and providers whose routed list is complete | at read time, current reads only (§9) |
| `wire-families.json` | verified package-and-shape request protocols | at read time, current reads only (§13) |
| `fact-overrides.json` | guarded sourced replacements or additions to upstream facts | compiled file on current reads; stored `fact_override_era` claims on point-in-time reads (§10) |
| `billing-planes.json` | how a provider bills a login method | at read time, current reads only |
| `cache-lifetimes.json` | first-party OpenAI and Anthropic prompt-cache lifetimes and exact write-price multipliers | at read time, current reads only |
| `temperature-ranges.json` | documented provider-level sampling temperature bounds | current reads of the provider's own rows only, never aliases or rows with `capability.temperature: false` |
| `input-audio-per-token.json` | providers with sourced per-token audio pricing, less excluded models, and optional documented audio tokens per second | at ingest: lets an eligible upstream `cost.input_audio` with no conditional audio price be stored as `rate.input_audio`; at read time: joins `limit.audio_tokens_per_second` onto current rows serving that rate (§8) |
| `output-audio-per-token.json` | providers with separately sourced output-audio token units and optional excluded models | at ingest: lets eligible unconditional `cost.output_audio` become `rate.output_audio`; conditional audio remains unpriced |
| `plan-prices.json` | subscription list prices, quota values and service-tier entitlements | prices written to `plan_price_era` at start; quota values and service tiers joined only on current reads |
| `models-dev-seed.json` | a copy of the upstream catalog for offline first boot | written as seed eras into an empty store |
| `curated-providers.json` | sourced provider identities models.dev does not list | test-side provider vector generation only; never joined into served model rows |
| `engram-catalog.json` | the backup enrollment descriptor | written beside `store.db` at start; not otherwise read |

Billing planes and plan prices have their own documents,
[`billing-planes.md`](billing-planes.md) and [`plan-prices.md`](plan-prices.md).

`cache-lifetimes.json` explicitly lists first-party OpenAI and Anthropic models,
their shortest-first lifetimes, quoted sources and a 60-day review deadline.
Write multipliers are exact numerator/denominator pairs relative to input;
retention-only OpenAI rows have no multipliers. On current reads the table adds
`capability.cache_lifetimes` and `rate.cache_write[.above_context.<tokens>].ttl.<lifetime>`.
No TTL prices are served for a row unless its base and every context tier's own
published short write price exactly equals input times the shortest multiplier.
The lifetime list remains available when that guard fails. The prices retain
the row's own write-rate provenance and never change stored facts or plain rates.
API-key planes carry these prices; Anthropic OAuth marks them as quota weights.
In `route::apply_billing_plane`, OpenAI ChatGPT strips TTL selectors and the
lifetime list before its cache-write-as-input rule, alongside the existing
quota-plane mode stripping. Historical reads carry neither lifetimes nor TTL prices.

The optional `service_tiers` field on a plan-price cell curates `fast` and
`ultrafast` entitlements and their exact included-usage and purchased-credit
rates relative to implicit Standard. Absent means unknown, never "no tiers".
An established field carries quoted first-party `source_ref`, establisher,
establishment date and its own review date; `not_established` carries a refusal
reason and review date. Unknown tier names, floats and missing provenance are
counted rejects without losing the independently sourced price. These fields
are never stored, so changing only service tiers writes no price era.
Only OpenAI Plus, Pro 100, Pro 200 and Pro 500 are curated for service tiers;
the other providers remain unknown. Review service tiers after 30 days because
Pro 500 and the published consumption rates are recent. Model availability
stays on model rows.

The seed and the enrollment descriptor are covered in
[`architecture.md`](architecture.md) and
[`backup-and-restore.md`](backup-and-restore.md). Unit tests named below are in
the source file of the same name under `crates/fusiform-module/src/`.

## 1. Rules every curated file follows

1. **Curated facts are never stored as observations.** Creators, aliases,
   curated rows, canonical links, quality scores, model lineage, fact overrides and billing planes are applied when a read is served, so a
   point-in-time read replays what the upstream published and never today's
    curation (see [`data-model.md`](data-model.md) §8). Curated model rows and
    fact overrides differ on point-in-time reads: their histories are recorded
    in `curated_model_era` and `fact_override_era`, never as upstream eras, and
    a point-in-time read serves the curated claims in force at its instant
    (§4, "Point-in-time reads", and §10).
   Among stored model facts the exception is `input-audio-per-token.json`
   (§8), whose pricing evidence adds no value of its own: it supplies evidence
   for interpreting an upstream `cost.input_audio` during normalization, and an
   eligible upstream price is then stored as an observed rate like any other.
   The file's optional audio-token conversion is a current-read join, never stored.
2. **An upstream row wins while upstream publishes it.** When models.dev
   currently publishes the same identity, the observed row is served and the
   curated claim is not. A curated model row also returns when upstream
   withdraws the identity (§4).
3. **Every claim carries provenance**: a source reference, and for most files
   who established it, when, and when it must be reviewed.
4. **Malformed rows are counted, not fatal.** Each loader returns the rows it
   accepted and a reason for each row it rejected. A test asserts the shipped
   file has no rejects, so a curation mistake fails the build instead of
   taking down the unrelated catalog at runtime.
5. **Absence is not a gap to fill.** A family with no creator row inherits
   nothing; a provider with no plan price is refused as uncurated. Rows exist
   because someone sourced them.

## 2. Creators (`model-creators.json`)

Schema `model-creators/v1`. Each row in `creators` names a `family`, the
`creator_provider_id` that published those weights, a `basis` sentence, and the
date it was `recorded`. The file has a single file-level `review_by` date.

The table is used for one thing: when an open-weight row's provider publishes
no rate at all, the creator's published rates are served on that row, marked
`inherited_from` with basis `open_weights` (rules in
[`wire-contract.md`](wire-contract.md) §4.4). It is never used to fill in a
partial card or a closed-weight model.

Gates: the shipped table loads without rejects
(`creators.rs`: `the_shipped_table_loads_without_rejects`); a family listed
twice is reported (`a_duplicate_family_is_reported_rather_than_resolved`); and
every curated creator publishes a price in the family it claims
(`creator_table::every_curated_creator_publishes_a_price_in_the_family_it_claims`,
in the module crate's `--test it` target).

## 3. Aliases (`model-aliases.json`)

Schema `model-aliases/v1`. A row says that an id a route exposes is a specific
published row:

| Field | Meaning |
| --- | --- |
| `provider_id`, `model_id` | the alias id |
| `target.provider_id`, `target.model_id` | the published row it is |
| `basis` | why the id is that model, in a sentence |
| `source_ref` | the file and pinned revision of the routing code that was read |
| `established_at_ms`, `review_by_ms` | when it was read; when it must be read again |
| `reasoning_wire_ids`, `reasoning_default` | optional effort → backend model id map and default effort used when no tier is specified, read from the pinned plugin |
| `wire_family` | optional verified route protocol, from the alias's pinned plugin source, never its target's package |
| `limit_output` | optional route-specific output token limit: a positive integer `value`, nonempty `source_ref` and `quote` |

How to add a row: read the routing code at a pinned revision. Include the
alias only if every tier of the id resolves to the target's own backend model
id and the target exists in the embedded seed. Record the revision in
`source_ref` and a review date 60 days after `established_at_ms`. A
look-alike id that does not qualify belongs in the declared-exclusions list in
`src/aliases.rs` with its reason, not in the file.

Gates (`aliases.rs`):

- the shipped table loads without rejects
  (`the_shipped_table_loads_without_rejects`);
- every target exists in the embedded seed, and no alias id does
  (`every_alias_target_exists_in_the_embedded_seed`,
  `no_alias_id_exists_in_the_embedded_seed`);
- no declared exclusion appears in the table
  (`no_declared_exclusion_is_in_the_table`);
- an id listed twice, an alias whose target is its own id, or an alias whose
  target is another
  alias is rejected (`a_duplicate_alias_id_is_reported_rather_than_resolved`,
  `a_chained_alias_is_refused`);
- no alias is past its review date (`no_alias_is_past_its_review_date`).

The live 3.8 Flash and Argon aliases carry tier maps for `low`, `medium` and
`high`, defaulting to `medium`. The source is each alias's own plugin pin
(Flash `44eb8fa0`, Argon `48919aa7`), not a consumer's routing table.
`reasoning_option_drift` checks every effort against the target's
`capability.reasoning_options`; `planted_tier_effort_drift_is_reported` calls
that same check with an unsupported effort. The loader rejects a missing or
unknown default and empty wire ids.

When an alias carries `reasoning_wire_ids`, its served
`capability.reasoning_options` effort values are restricted to the map's keys,
preserving the option type and the target's value order. A target can offer
more efforts than the route accepts; those extra efforts must not be offered
to a consumer of the alias. This is a current-read projection only: the target's
own reads and stored history remain unchanged. Other option types are preserved;
an effort-to-wire-id map makes no claim about budget or toggle controls.
The Gemini 3.8 Flash and Argon targets already list exactly `low`, `medium`
and `high`, so their aliases' reasoning options do not change.
The Sonnet 5.5 and Opus 5.5 aliases expose only `low`, `medium` and `high`,
defaulting to `low`, rather than the Anthropic targets' additional `xhigh` and
`max`. Both cite plugin revision `d17ca2e8`: each model's three native routes
share one `vertexModelId`, and the resolver selects `-low` for a bare id.

An alias can carry `limit_output` when its route has a sourced output limit
different from the target's API. Current reads apply it after the target's
facts and fact overrides. The existing `fact_overrides` list then discloses
exactly one `limit.output` replacement for the alias identity, with the value
the target would otherwise serve in `replaces`, the field's source and quote,
and the alias row's review dates. The target's disclosure for that fact is not
also copied to the alias. Target rows and point-in-time reads are unchanged;
point-in-time reads do not serve aliases.

Antigravity Carbon serves 262,144 output tokens, citing the captured
Antigravity CLI (agy) 1.5.5 request's `"maxOutputTokens": 262144` for `gemini-4-carbon-low` and the plugin
registry's `limit: { context: 1048576, output: 262144 }` at `cb40633d`.
The public Carbon API target and the Argon alias still serve 65,536.

Retiring an alias moves its identity to `retired`; never delete a served id.
An optional `successor` is a full `provider/model` identity, accompanied by
`successor_source_ref`. It must name a row fusiform currently serves
(`invalid_successors`, also exercised by a planted missing/unsourced successor).
The 3.6 and 3.7 Flash aliases name `google/antigravity-gemini-3.8-flash`, citing
broca `2770908`'s record of AGAUTH's two `fetchAvailableModels` captures before
2026-10-14T13:11Z (clients 1.3.3 and 1.3.5). No successor is established for
`antigravity-claude-sonnet-4-6-thinking`. On current `include_retired` reads the
absent row carries `successor`; a named read excluding the row carries it in
the `retired` disclosure instead. Point-in-time reads do not apply this list.

Serving rules are in [`wire-contract.md`](wire-contract.md) §5.5.

Every present Antigravity alias states `wire_family: "google.code_assist"`.
Retired aliases serve no protocol. This describes the request wire, not whether
a runner supports it.

## 4. Curated model rows (`curated-models.json`)

Schema `curated-models/v1`. A row supplies a provider row that models.dev's
provider catalog does not carry, for example a model a provider serves under a
versioned id while models.dev lists the model only under its creating lab,
not under that provider, or a model under a provider models.dev does not list
at all.

The shipped rows:

| Identity | Kind | Why curated |
| --- | --- | --- |
| `meta/muse-wonder-full-1.0` | not stated | models.dev lists the model only under the lab |
| `cloudflare-workers-ai/@cf/cloudflare/clef` | `decision` | models.dev's Cloudflare provider does not list it |
| `cloudflare-workers-ai/@cf/cloudflare/clef-flash` | `decision` | as above |
| `cloudflare-workers-ai/@cf/cloudflare/clairic` | `extraction` | as above |
| `typesafe/jev-1.13.0` | `decision` | models.dev does not list TypeSafe. Only the versioned id is served, never the moving alias `jev-latest` |

All four rows after `meta` are currently held: see "Held rows" below.

| Field | Meaning |
| --- | --- |
| `provider_id`, `model_id` | the served identity |
| `auth_methods` | a non-empty allowlist of distinct login methods from `apikey`, `chatgpt`, `oauth`, `antigravity`, `gateway` |
| `facts` | fact key to value; keys must be in `SERVED_FACTS` or the curated loader's explicit unit-billed key allowlist |
| `source_ref` | the evidence for the row: each page read, with the sentences it states quoted verbatim |
| `established_by` | optional; who read the sources. Not served |
| `established_at_ms`, `review_by_ms` | the review date must be exactly 60 days after establishment |

`openai/gpt-transcribe` is curated with audio/text input and text output, no
`model.kind`, and one rate: `rate.input_audio.per_second`, $0.0045 per 60
seconds. The source states no rounding, minimum or prompt/keyword-text billing
rule, so quantity terms are unknown rather than none. No token rate is served.
The loader admits this unit-billed key by name, not a pattern, only after the
spend-pricing consumer recognises it. It requires a positive `per` and explicit
`increment` and `minimum_quantity`, and refuses `per` on token keys. The full
rate, including its denominator, belongs to the curated row's stored claim in
`curated_model_era`; it never enters models.dev's upstream fact eras.

`google/gemini-3.8-flash-tts` is curated as speech with text input and audio
output: $0.50/M text input, $9/M audio output, and 25 output audio tokens per
second. Google's [pricing page](https://ai.google.dev/gemini-api/docs/pricing)
says both prices double on 2027-01-01; only the current prices are served. `elevenlabs/eleven_flash_v2_5` uses the explicitly admitted
`rate.input.per_character`, $0.04 per 1,000 characters, with unknown increment
and minimum. Its documented 40,000-character request cap is not a token limit.
Neither row borrows a text-output price or an input-audio conversion.

A curated row may declare a sourced HTTPS `wire.api` as exactly
`{value, from: "curated"}` and a verified `wire.family` token. These are the
row's own claims, disclosed through `curated`, not provider-default wire
corrections. Rows without declarations still carry no wire facts, and the
loader rejects unknown protocol tokens, non-curated endpoint origins and
`wire.npm`/`wire.shape` declarations. A recorded curated claim keeps its own
declarations on historical reads; current-read-only derivation describes the
upstream package rules, not these recorded facts.

The output-audio source gate is independent of the input gate: Google and
OpenAI pricing pages explicitly establish per-million-token output billing.
Other providers, excluded models and conditional audio prices remain
unattributed. Both source documents use the same 60-day review validation;
output tokenization is model-specific rather than a provider-wide conversion.

### Held rows

The file's top-level `held` array names rows fusiform keeps in `models` but
does not serve for now. Each entry has `provider_id` and `model_id` (which must
name a row in `models`), `held_since_ms`, and a `reason` saying why the row is
held and what lets it be served again. The held row's data stays in `models`
unchanged, so serving it again is the removal of one `held` entry, not a revert.

Shipped: the four decision and extraction rows above are held since
2026-10-21T20:30:00Z. A consumer's catalog assembly (broca's) refuses its whole
refresh when a present row lacks `limit.context`, `capability.attachment`,
`capability.reasoning` or `capability.tool_call`, and these rows leave some of
those out because the vendors do not state them. Each entry is removed once
that consumer accepts a present row without those facts and its fix is
deployed.

On current reads (no `at_ms`, no `auth_method`) a held id is reported exactly
the way a retired alias is, through the same code path in `route.rs`:

- if the store knows the id (models.dev listed it once, as it did the three
  Cloudflare ids), the store's own row is served as for any other upstream
  row: absent on a retired-inclusive read, and in `retired` with the store's
  retirement instant on a named read. No second row is added;
- otherwise a retired-inclusive read (bulk, provider-filtered or paged) lists
  the id with the single fact `existence: absent`, and a named read lists it
  in `retired` with `held_since_ms` as `retired_at_ms`;
- a read without `include_retired` does not list it, and nothing discloses
  it in `curated`. A plane read never lists it, as for a retired alias.

`catalog.history` names no curated source for a held row. A provider whose
only rows are held (`typesafe`, while Jev is held) stays a known provider: a
plain provider read answers with zero models, and a named read of a model it
never carried is still refused as an unknown model.

Why held and not deleted: a consumer that tracks identities across reads, and
asks for retired rows, reads an identity that disappears without a retired
row as a truncated catalog and refuses every refresh. Deleting alias rows did
exactly that on 2026-10-17. So an id fusiform has served is never deleted
from what it reports: a curated row is held, and an alias is moved to the
alias file's `retired` list.

Load rules: a held entry naming no row in `models`, an id held twice, and an
entry without a reason or a positive `held_since_ms` are rejects
(`planted_held_entry_naming_no_row_is_rejected`,
`planted_duplicate_or_malformed_held_entry_is_rejected`), and held entries
load (`held_entries_load_and_keep_their_rows`). These tests, and the serving
tests in
`route/curated_held_tests.rs`, plant a copy of the file that holds the four
System One rows, so they test the mechanism whatever the shipped file holds.
They include the control that removing a held
entry serves the row as curated again
(`releasing_a_held_entry_serves_the_row_as_curated_again`), and for a held id
the store holds as a withdrawn upstream row in
`curated_withdrawn_tests::a_held_row_leaves_the_withdrawn_upstream_row_to_report_itself`.
The serving tests in `system_one_tests` and `curated_withdrawn_tests` that
cover these rows as served plant the shipped file with its `held` array
removed.

The four System One rows were held from 2026-10-21 20:30Z until broca's
catalog assembly accepted rows missing a context or capability fact (broca
0.3.247, placed 2026-10-22); the shipped file now holds nothing.

Load rules:

- a fact key outside `SERVED_FACTS` is rejected
  (`curated_models.rs`: `planted_unknown_fact_is_rejected`);
- only a row whose `auth_methods` is exactly `["apikey"]` may carry `rate.*`
  facts, because a row a subscription login can read must not claim an API
  list price (`planted_subscription_price_is_rejected`);
- a curated rate is served exactly as written, so it must already be in the
  served shape: a `priced` amount with `unit_provenance` `stated` (the source
  printed the currency), or a bare `stated_zero`. A borrowed, unpriced or
  policy-assumed rate is rejected
  (`planted_rate_outside_the_served_shape_is_rejected`);
- `model.kind` must be a value in `MODEL_KINDS` (`decision` or `extraction`);
  anything else is rejected
  (`planted_model_kind_outside_the_vocabulary_is_rejected`);
- duplicates and malformed rows are rejects
  (`duplicate_and_malformed_rows_are_counted_as_rejects`).

`model.kind` says what work the model does: `decision` (a typed choice among
options the request defines) or `extraction` (typed values read, computed or
estimated from the input). Neither writes replies. Only curated rows state it;
fusiform never infers it for a models.dev row, so it is absent there, and
absent means "not stated", never `chat`. A consumer must refuse chat work on a
row whose `model.kind` is present and is not `chat`
([`wire-contract.md`](wire-contract.md) §4.6).

A value the source does not state exactly is left out rather than rounded:
TypeSafe documents Jev's context as "64k per-request / 32k per-question" and
publishes no token count, so `typesafe/jev-1.13.0` carries no
`limit.context`. Where sources conflict, the fact is left out: TypeSafe's models
page lists text input while its images page says Jev accepts images, so the
row carries no `capability.attachment`.
`model.name` is the vendor's exact human display label, with its page cited in
`source_ref`: Muse Wonder Full, Clef, Clef-flash, Clairic and Jev 1.13. The
spellings `Clef-flash` and `Jev 1.13` follow vendor prose, not a prettified model id. Provider
names come from stored upstream claims for Meta and Cloudflare Workers AI;
`typesafe` is unlisted upstream, so its row sources `provider.name: "TypeSafe"`
from https://docs.typesafe.ai/models. Release dates and other provenance remain
in `source_ref` rather than being invented as new fact keys.

Serving rules, tested in `crates/fusiform-module/src/route.rs`
(`curated_tests::…`):

1. A plain read (no `auth_method`) and a read with a listed method serve the
   row and disclose it in `curated`
   (`plain_and_oauth_reads_serve_published_curated_facts`,
   `every_served_curated_row_discloses_its_allowlist_and_source`).
2. A read with an unlisted method never serves it
   (`unlisted_login_method_never_serves_a_curated_row`).
3. Upstream wins only while models.dev currently publishes the identity: when
   the store's current row for it says `existence: present`, that row is
   served and the curated row and its disclosure are not
   (`a_present_upstream_row_supersedes_curated_facts_and_a_withdrawn_one_gives_way`).
   A row models.dev listed and later withdrew does not suppress the curated
   row. On current reads (bulk, provider-filtered, paged, plane and named,
   with or without `include_retired`) the curated row replaces the withdrawn
   upstream row whole: its facts are the curated file's, none of the stored
   row's facts or disclosures are mixed in, a named read does not list the id
   in `retired`, and a retired-inclusive read shows the one curated row. If
   models.dev lists the identity again, the upstream row is served again with
   no edit to the file. A point-in-time read asks the same rule at its own
   instant: where the store's history says upstream had the model present
   then, its upstream row is served, and otherwise the curated claim in force
   then, if any ("Point-in-time reads" below). `catalog.history` serves the stored eras for any fact the upstream row
   recorded; a fact with no eras that the curated row carries, such as
   `model.kind`, names the curated source. One predicate in `route.rs`
   (`curated_in_effect`) decides this for every route
   (`curated_withdrawn_tests::a_withdrawn_upstream_row_gives_way_to_the_curated_row`,
   `curated_withdrawn_tests::an_upstream_relisting_wins_over_the_curated_row_again`).
   This is a live case, not a hypothetical: models.dev listed
   `@cf/cloudflare/clef` and `clef-flash` for five hours on 2026-10-01 and
   `clairic` for about ten hours on 2026-10-19, then withdrew them.
4. Provider and fact filters apply; a named read keeps the row and its
   disclosure even when the fact filter matches nothing, and a bulk read drops
   it (`curated_rows_respect_fact_and_provider_filters`).
5. Point-in-time reads serve curated rows only from the store's recorded
   history of the file, never from the compiled file: a store with no history
   recorded serves none (`point_in_time_reads_serve_no_unrecorded_curated_row`).
   See "Point-in-time reads" below.
6. `catalog.history` returns no eras and a `curated` object naming the source;
   `catalog.status` counts are unaffected
   (`curated_history_names_the_source_without_storing_eras_or_changing_status`).
7. A provider with curated rows is a known provider on every read, even when
   models.dev does not list it. A named read of a model it does not carry is
   refused `no_coverage` as an unknown model, and a point-in-time provider read
   answers rather than refusing the provider
   (`system_one_tests::an_unknown_typesafe_model_is_refused_as_an_unknown_model`,
   `system_one_tests::point_in_time_reads_serve_no_unrecorded_system_one_row`).

### Point-in-time reads

A consumer pricing a call at the call's instant reads with `at_ms`, so a
curated row needs a history: the price in force when the call was made, not
today's. Every start records the file in the store's `curated_model_era` table
(rules and schema in [`data-model.md`](data-model.md) §7a):

- a row's first recorded claim is dated at its `established_at_ms`;
- a later change to its facts or `auth_methods` is dated at the start of the
  build that carries it, the instant fusiform began serving the new value.
  Re-reading a page and re-dating `established_at_ms`, `review_by_ms` or
  `source_ref` is not a change and writes nothing, and the stored disclosure
  keeps the provenance recorded with the claim;
- a row removed from the file, or held, is tombstoned at that start.

A `catalog.get` with `at_ms` serves, for each curated row, the claim in force
at that instant (the newest boundary at or before it), in the same shape as a
current read: the row's facts, its provider's upstream label in force then
when the row states none, and a `curated` disclosure from that claim's
provenance. Upstream wins at that instant, as on current reads. A named read
before the row's first recorded claim, or after its tombstone, is refused
`no_coverage` with a message saying which, and when a read would answer; a
bulk or provider read at such an instant does not list the row. Current reads
are unchanged: they serve the compiled file. Plane reads (`auth_method`)
refuse `at_ms` as before, and alias rows stay current-only.

Tested in `route/curated_history_tests.rs`: Jev's priced input and stated-zero
output with the disclosure after its first claim
(`a_read_after_the_first_era_serves_jevs_recorded_rates_and_disclosure`), the
refusal before it (`a_read_before_the_first_era_is_refused_with_the_reason`),
a changed price dated at the start that carries it
(`a_changed_curated_price_opens_an_era_dated_at_the_boot_that_carries_it`), an
unchanged or re-dated file writing nothing
(`re_applying_an_unchanged_or_only_redated_file_writes_nothing`), removal and
holding as tombstones
(`a_removed_row_is_tombstoned_and_a_read_after_it_is_refused`,
`a_row_held_from_its_first_boot_is_first_served_at_its_release`), upstream
winning at the instant
(`a_historical_read_where_upstream_had_the_model_serves_the_upstream_row`),
and current reads ignoring the stored history
(`a_current_read_serves_the_compiled_file_not_the_stored_history`). The golden
case `catalog.get — a curated row at an instant` pins the served bytes.

The decision and extraction rows (the vendors call them "System One"
models: they return typed decisions or values, never replies) are covered on
bulk, named, provider-filtered, paged and `apikey` plane reads, with
fact-prefix filters and the decoded rate states, by `route.rs`'s
`system_one_tests`, against the shipped file with nothing held.

The consuming router must refuse a credential whose login method is not in
`auth_methods`. A model absent from `curated` carries no login restriction.

When models.dev starts publishing the same identity, the suite fails with
"upstream now publishes this row; remove the curated row"
(`upstream_now_publishes_this_row_remove_the_curated_row`), checked against the
embedded seed for every row. A seed planted with each shipped identity in turn
trips it (`planted_upstream_row_triggers_the_removal_fence`).

## 5. Canonical links (`canonical-links.json`)

Each row in `links` names an existing `provider_id` / `model_id` and a
`canonical_id` in `<lab>/<id>` form. This is the models.dev lab identity, not
another provider's route id. The file fills only a null or absent
`model.canonical_id`; any non-null upstream value wins, including a different
identity. No fact is written to the store, and `at_ms` reads never apply links.

The shipped list is empty after the 2026-10-28 seed refresh. Upstream now
publishes canonical identities for `ollama-cloud/deepseek-v4-pro`,
`ollama-cloud/kimi-k2.6` and `ollama-cloud/minimax-m2.7`, so their links are no
longer needed. The seed no longer contains `ollama-cloud/deepseek-v4-flash`,
`ollama-cloud/glm-5.1`, `ollama-cloud/kimi-k2.5` or
`ollama-cloud/minimax-m2.5`, so their links were removed too. The mechanism and
its fixture-based tests remain available for future sourced claims.

Two sources establish the claim: `source_ref` is the provider's library URL,
and `quote` is its sentence identifying the exact model and variant.
`lab_source_ref` points to `https://models.dev/models.json`; `lab_quote` quotes
that entry's `id` and `name` fields, with the lab encoded in `id`. Do not derive
either identity from a route's spelling. A different size or unverified variant
must stay unlinked. `established_by` is `fusiform`, `established_at_ms` records
verification, and `review_by_ms` must be exactly 60 days later.

The response's `canonical_links` list names each applied value and both sources
(see [`wire-contract.md`](wire-contract.md) §5.7). It appears only when the
identity survives the fact-prefix filter, on named, bulk and paged reads.

Gates in `canonical_links.rs` check rejects, review dates, existence of the
linked route in the embedded seed, and absence of an upstream
`canonical_model_id`. When upstream fills the id, the failure names the link
to remove. Each scan has a planted-violation test. Route tests in
`route/canonical_link_tests.rs` cover null and absent facts, upstream wins,
historical reads, filters, disclosure and unchanged stored facts/era counts.

## Provider vectors

[`test-vectors/providers-v1.json`](../test-vectors/providers-v1.json) is the
credential vault's reviewed provider-id contract for login rows. The vault pins
the file by repository commit SHA and SHA-256; it does not call a live catalog.
Provider changes reach it only when a reviewed vault commit updates that pin.

The test-side generator reads the embedded `models-dev-seed.json` using the
existing raw models.dev types, then adds `curated-providers.json` for vendors
models.dev does not list. The hand-kept table covers model providers and web
search services so consumers can read both from one provider list. Each row has
`id`, `name`, `doc`, the first-party page actually read in `source_ref`, and
`established_at_ms` / `review_by_ms` exactly 60 days apart. A real-clock test fails
after review expires and asks for re-verification or removal. A separate test
rejects any hand-kept id already in the seed: the upstream entry wins, so remove
the hand-kept row rather than overriding it.

Curated rows may also have `kind`, whose closed set of values is currently
`["web_search"]`. `kind: "web_search"` identifies a web search service; absent
`kind` means a model provider. An unknown kind is a counted loader reject, and
the vector's byte-comparison test fails if the loader reports any rejects instead
of silently dropping a provider.
Search services do not infer or add model rows or output modalities.

The generated file has stable key order, providers sorted by `id`, pretty JSON
and a trailing newline. There is no generation timestamp.

| Field | Meaning |
| --- | --- |
| `format` | `providers-v1` |
| `snapshot_fetched_at` | the seed metadata's `fetched_at` UTC string, verbatim |
| `providers` | one entry per provider id, including providers with no model rows |
| `id`, `name`, `doc` | the provider id, published display name and documentation URL |
| `source` | `models.dev` or `hand_kept` |
| `kind` | `web_search` for curated web search services; omitted for model providers, including all seed providers |
| `env` | models.dev's provider environment-variable list verbatim, preserving order and duplicates; `[]` for hand-kept rows |
| `output_modalities` | for seed providers, the sorted, deduplicated union of their model `modalities.output` values, plus `embedding` if any model has `type: embedding`; for hand-kept model providers, the sorted, deduplicated union of `capability.output_modalities` from their served model rows; `[]` when none are recorded, and always `[]` for web search services |

The generator seeds a fresh store and pages the real `catalog.get` serving path
to collect hand-kept providers' output modalities, including curated models and
aliases. It does not guess capabilities from provider names or documentation.
A coverage fence requires every provider id in those served pages to appear in
the committed vector. The read includes retired rows so held curated models
and retired aliases cannot lose their provider identity; a row that serves only
`existence: absent` contributes no output modalities. New row-producing tables
therefore pass through the same fence without a separate list of data files.

`env: []` does not mean no credentials or no API key: hand-kept rows have no
credential-variable claim. `output_modalities: []` means no output modality is
recorded in these inputs, not that the provider has no capabilities. The vector
has no `brand` field: it makes no creator-versus-reseller grouping claim. It has
no API-key boolean: environment-variable names are not an authentication-method
classification. Absence of either field is unknown, not false. Review dates and
source evidence remain in the curated input, not in the generated contract.

Refresh the vector with the seed, **at least every 14 days**.
`scripts/refresh-seed.sh` prints the regeneration command alongside the served
golden fixture and lineage-audit commands. Run on the Linux runner:

```sh
UPDATE_GOLDEN=1 cargo test --locked -p fusiform-module --test it providers_vector::providers_match_the_committed_vector
```

For a remote runner call, set `return_paths: ["test-vectors/providers-v1.json"]`:
this copies the regenerated file from the build server back into the local
checkout; otherwise the rewrite stays remote. Review the diff before committing.
Without `UPDATE_GOLDEN`, that same test compares the
generated bytes with the committed file and fails with the regeneration command
if they differ.

## 6. The embedded seed (`models-dev-seed.json`)

A verbatim copy of `https://models.dev/api.json`, with
`models-dev-seed.meta.json` recording the fetch instant, URL, SHA-256 and
counts. On first boot an empty store is seeded from it, so a fresh install with
no network answers reads (`bootstrap::a_fresh_install_with_no_network_answers_a_read`).
The snapshot and its metadata must agree
(`bootstrap::the_snapshot_and_its_metadata_agree`), and the suite fails when
the snapshot is older than 14 days (`seed.rs`: `the_embedded_seed_is_not_stale`).
Refresh it with `scripts/refresh-seed.sh`. The seed is models.dev's data under
its MIT license; see `THIRD-PARTY-NOTICES.md`.

After each seed refresh, regenerate the golden fixture and lineage parity audit
on the remote Linux runner (`runon: "linux,2c"`):

```sh
UPDATE_GOLDEN=1 cargo test --locked -p fusiform-module --test it golden_payload::
UPDATE_LINEAGE_AUDIT=1 cargo test --locked -p fusiform-module --lib lineage_seed_parity_with_dated_routing_globs_has_only_explicit_differences
```

Set `return_paths: ["crates/fusiform-module/fixtures/served-payloads.json"]`
on the golden runner call and
`return_paths: ["crates/fusiform-module/src/route/model_lineage_routing_differences.json"]`
on the lineage runner call. The runner must return these regenerated files to
the local worktree; otherwise their writes are not retained. Review both diffs
alongside the seed and metadata before committing.

The lineage audit updates coverage counts, removes allowed rows no longer in
the seed, and adds newly classified rows where the retired routing globs said
`other`. A new disagreement with a non-`other` routing lineage still fails in
update mode, naming every unreviewed row. Each needs a hand-written reason group
in `model_lineage_routing_differences.json`; update mode never invents a reason.
Rows still in the seed whose reviewed transition changes also need review.

## 7. When a review gate fires

Plan prices, quota values, service tiers, billing planes and rules, aliases, canonical links, wire corrections, fact overrides,
input-audio sources and curated model rows each carry a review date, and a test fails once any date has
passed. These are time-triggered: they fail on a day nobody changed anything,
and CI's scheduled run exists so they fire then. The plan-price, billing-plane
and seed gates' failure messages begin "NOT A BUILD FAILURE" and name what
to re-read.

To clear one:

1. Open the row's `source_ref` and read it again (and `lab_source_ref` for a canonical link).
2. If the claim changed, update it (or remove the row).
3. Whether or not it changed, record who looked (`established_by` where the
   file has it), when (`established_at_ms`), and a new `review_by_ms`: 60 days
   later for billing planes, aliases, canonical links, wire corrections, fact overrides, input-audio sources and curated model rows; the file's stated
   interval for plan prices, and 30 days for service-tier entitlements.
4. Commit the file. Confirming a claim unchanged is a real result; the diff is
   the provenance change alone.

To refresh the seed, follow §6, including regeneration and diff review, and
commit both seed files with any regenerated fixture and audit changes.

Extending a date without reading the source is possible, as with any gate; the
commit then records who made that decision.

## 8. Input audio per token (`input-audio-per-token.json`)

Schema `input-audio-per-token/v1`. models.dev publishes `cost.input_audio`
beside the token rates, in the same "$ per million" convention, but nothing in
the payload says whether a provider bills audio per token, per second or per
minute. So the normalizer leaves the key unattributed (`unknown_charge_basis`,
counted as `UnattributableChargeUnit`, no fact served) unless the provider is
listed, the model is not excluded, and the cost block has no audio price in a
context tier or the legacy `context_over_200k` block. Eligible values are
stored and served as
`rate.input_audio`, per million audio input tokens (rules in
[`wire-contract.md`](wire-contract.md) §4.1).

Each row in `providers`:

| Field | Meaning |
| --- | --- |
| `provider_id` | the models.dev provider |
| `source_ref` | the provider's OWN pricing page, as an `https` URL |
| `quote` | verbatim text from that page stating audio input is priced per million tokens; it must name the per-million-token unit |
| `established_at_ms`, `review_by_ms` | when the page was read; exactly 60 days later |
| `excluded_models` | models of this provider whose published `input_audio` the page does not support, each with `model_id` and `reason` |
| `audio_tokens_per_second` | optional object with a positive integer default `value`, an `https` `source_ref` and a verbatim `quote` from the provider's documentation stating the conversion; optional `model_overrides` are described below; covered by the provider row's review date |

Each entry in `audio_tokens_per_second.model_overrides` has a unique `model_id`
under that provider, a positive integer `value`, `basis: "measured"`, an `https`
`source_ref` identifying the provider's billed usage counts, and a non-empty
`quote` describing the measurement. Overrides replace only the named model's
conversion; all other models retain the provider default. The embedded-seed
fence requires each override's model to exist under that provider, not merely
under another provider with the same model id.

`declined` records providers whose page was read and did not establish the
unit (`provider_id`, `source_ref`, `quote`, `reason`). It changes nothing that
is served; it keeps the decision from being remade without its source. A
provider cannot be both listed and declined.

How to add a provider: read its own pricing page, not another provider's
(the bill belongs to whoever charges it). List it only if the page prices
audio input per million tokens and nowhere contradicts that. For each of its
models whose `input_audio` differs from its `input`, check that the page's
explicit audio row gives that number; exclude any model the page does not
support, with the reason.

The pricing evidence is applied at ingest rather than at read time (§1, rule 1):
it adds no price of its own and only decides whether the upstream's number has a
known unit. Listing a provider makes the next poll write one
`rate.input_audio` era per listed model that publishes the key; removing one
withdraws those facts on the next poll. Point-in-time reads replay what was
stored then. The table is passed to `fusiform_core::normalize_models_dev_with`
as data, so the core crate does no file IO.

The optional `audio_tokens_per_second` is applied only on current reads as
`limit.audio_tokens_per_second`, a JSON integer. It is joined onto a provider's
served rows only when their facts contain `rate.input_audio`, including aliases
borrowing that rate. An alias uses its target provider and model to choose an
override, so Google's `antigravity-gemini-3.8-flash` receives the same 25 as
`gemini-3.8-flash`. Today only Google's row supplies the conversion: its audio
documentation states "Tokens: 32 tokens per second of audio (1 minute = 1,920
tokens)", which remains the default. The measured override for
`gemini-3.8-flash` is 25: 62 billed requests on 2026-11-10 with 3.5–7 s audio
clips each reported exactly `ceil(25 × duration in seconds)` audio input tokens
in the Gemini API's `usageMetadata.promptTokensDetails`. The billed count is the
authority for the charged conversion even though Google's audio page states
32; its pricing page uses 25 per second for newer speech models.
The conversion is never stored or replayed on `at_ms` reads. Selecting only
`limit.audio_tokens_per_second` still evaluates the audio-rate gate internally;
the response contains only the facts the caller selected. An absent conversion
is not a default or a zero.

Gates (`input_audio.rs`): the shipped table loads without rejects
(`shipped_sources_load_without_rejects`); malformed rows, duplicates, bad
exclusions and bad declined rows are rejected
(`malformed_and_duplicate_rows_are_counted_as_rejects`,
`malformed_fields_are_rejected`,
`audio_tokens_per_second_is_optional_and_malformed_values_are_counted_rejects`,
`audio_token_overrides_are_optional_and_malformed_values_are_rejected`,
`malformed_exclusions_and_declined_rows_are_rejected`); every listed provider
and every exclusion matches rows in the embedded seed, as does each audio token
override under its provider
(`every_source_has_input_audio_rows_in_the_embedded_seed`,
`planted_provider_without_audio_rows_is_named`,
`planted_stale_exclusion_is_named`,
`audio_token_override_naming_unknown_model_fails_seed_fence`,
`planted_audio_token_override_without_seed_model_is_named`); and no row is past its review date
(`input_audio_sources_are_within_their_review_dates`,
`planted_overdue_source_requires_rereading_or_removal`).

## Attachment limits (`attachment-limits.json`)

The compiled provider table supplies five base integer facts on current
reads: `limit.images_per_request`,
`limit.image_bytes`, `limit.image_long_edge_px`, `limit.document_pages` and
`limit.document_bytes`, plus the conditional family
`limit.image_long_edge_px.above_images.<n>`. Absent base facts mean unknown,
never unlimited; an absent conditional key means no stated condition. A vendor's
"no limit" statement therefore produces no fact. These values are never stored
or replayed on `at_ms` reads.

`limit.documents_per_request` is the name reserved for a per-request document
count, which apps reading these limits already expect. No provider's docs state
one yet, so it is not served and not registered in `SERVED_FACTS`. It can join the served vocabulary when a
vendor states a document count and a real served value can be pinned in the
golden fixture.

Moonshot's file-upload documentation states "Each file can be up to 100 MiB"
([source](https://platform.kimi.ai/docs/api/files-upload)), or 104,857,600 bytes,
but no `moonshotai` row in the embedded seed accepts PDF input. No Moonshot
attachment evidence ships until there is an eligible row. The seed fence
requires every shipped limit to reach at least one eligible model, so dormant
evidence cannot silently accumulate review obligations.

Only first-party provider ids in the table qualify. Image facts require
`image` in the served `capability.input_modalities`; document facts require
`pdf`. Gateways and resellers, including `google-vertex`, do not inherit the
backend's limits. Each published value carries a first-party `source_ref`, its
verbatim `quote`, and a review interval of exactly 60 days. Review-date and
embedded-seed fences require the evidence to stay current and applicable.
Aliases do not inherit attachment limits: their receiving backend differs
from the first-party endpoint whose documentation establishes the bound.

Byte limits describe raw files. Decimal MB means 1,000,000 bytes; MiB means
1,048,576. Anthropic's 10 MB base64-encoded image limit becomes 7,500,000 raw
bytes (10,000,000 × 3/4). Image dimensions serve the base bound without a
count condition, plus any stricter bound in
`limit.image_long_edge_px.above_images.<n>`. The condition applies when the
request contains **more than n image and document blocks across the whole
request, history included**. A reader unable to count must take the smaller
of the base and conditional values. Anthropic serves base 8000 and
`above_images.20` = 3000; DeepSeek serves base 8192 and `above_images.14` = 4096
(15 or more means more than 14).

An `image_count` rule contains the base `value` and a nonempty `conditions`
array. Each condition has a positive `above_images` threshold, a smaller
positive `value`, and its own verbatim `quote` from the limit's `source_ref`.
The limit's top-level quote supports the base value. Duplicate thresholds,
missing quotes, and count conditions on facts other than image dimensions
are rejected. Thresholds are provider data, not enumerated wire vocabulary:
the registered family matches positive decimal suffixes without leading zeros.

Anthropic's image count depends on the row's `limit.context`: exactly 200,000
tokens gives 100 images, any other known context gives 600. Its PDF page cap
is 100 below 1,000,000 context tokens and 600 at or above that. Missing or null
context gives neither context-dependent fact. Whole-request byte caps,
recommended dimensions and page-extraction truncation are not per-file limits
and are deliberately omitted. OpenAI's 50,000,000-byte per-file PDF limit also
has a 50 MB combined-request limit; the per-file fact alone does not promise
that several maximum-sized files fit in one request.

## Temperature ranges (`temperature-ranges.json`)

models.dev's optional `temperature` boolean is stored and served as
`capability.temperature`. Absence means unknown, never false. A consumer omits
temperature for false without refusing the send; for true it clamps the caller's
value to the served `limit.temperature_min`..`limit.temperature_max`, or 0..1
when no bounds are served. An unknown acceptance flag uses the 0..1 clamp.

The `temperature-ranges/v1` file has provider-level `ranges` rows: `provider_id`,
`minimum` and `maximum` JSON numbers, `source_ref` with accessed date, a verbatim
`quote`, explanatory `notes`, `established_at_ms` and `review_by_ms` exactly 60
days later. Zero is a real minimum. OpenAI, Google, DeepSeek and xAI document
0..2; Anthropic documents 0.0..1.0. xAI's sampling cap above 1 is noted but does
not narrow the accepted request range. Malformed and duplicate rows are counted
as rejects; the review gate requires re-reading overdue sources.

Ranges are applied after fact replacements, before prefix filtering, only on
current reads of that provider's own rows. No range is served on an alias or
when the served capability is false. Aliases inherit a target's false acceptance
flag because omitting temperature is safe on any route; true and absent target
flags establish no acceptance on the alias API and produce no alias fact.

Google's GenerateContent reference and text-generation guide state that Gemini
3.6 Flash and newer ignore custom temperature. Five guarded replacements in
`fact-overrides.json` correct the seed's true claims: `gemini-3.6-flash`,
`gemini-3.7-flash`, `gemini-3.8-flash`, `gemini-4-argon`, `gemini-4-carbon`, all
under `google`. The three Flash versions are at least 3.6; the other two are
Gemini 4, explicitly covered by the guide. No other Google Gemini 3.6+ row is
in the embedded seed. The correction has dated history and stops when the
stored upstream boolean moves away from true.

## 9. Wire corrections (`wire-corrections.json`)

Each `corrections` row names `provider`, `model`, `field` (currently only
`wire.api`), and either a nonempty `value` with `withheld: false`, or no value
with `withheld: true`. `source_ref` is an object containing `repo`, `commit`
and `file`; `established_by` is `fusiform`, with `established_at_ms` and a
`review_by_ms` exactly 60 days later. The file is embedded and parsed once;
malformed rows and duplicate identities are counted as rejects, never silently
resolved by ordering. `overdue_at` enforces review dates, with a planted overdue
row driving the same check as the shipped-data gate.

OpenCode Go follows the pi coding agent, not models.dev's endpoint spelling.
The sources read were `earendil-works/pi@74f46864`'s
`packages/ai/src/providers/opencode-go.ts` and the built
[`@earendil-works/pi-ai` 1.4.0 catalog](https://unpkg.com/@earendil-works/pi-ai@1.4.0/dist/providers/data/opencode-go.json),
keyed by API group and `chat:<model-id>`. Compared with models.dev on
2026-10-23, all 29 shared rows had matching API-kind/npm families and differing
base URLs: pi gives five Anthropic rows the raw base
`https://opencode.ai/inference/go`; its Anthropic SDK appends `/v1/messages`.
Fusiform serves their effective base `https://opencode.ai/inference/go/v1`,
so consumers append only `/messages`, as with observed models.dev endpoints.
All other shared rows, including Google, already include `/v1` in pi's base.

Pi's route list is complete for OpenCode Go: the file's `complete_providers`
array names `opencode-go` with the same pi source, and every OpenCode Go model
without a per-model row has `wire.api` withheld on current reads. That covers
the upstream-only models seen on 2026-10-23 (`cat-jam-free`, `minimax-m2.7`,
`minimax-m3.1-flash`, `space-bunny`, `step-5-preview-free`) and every model
upstream adds later, such as `kimi-k3.1-pro`, until pi routes it and a
per-model row is added. The disclosure is marked `withheld: true`, cites the
completeness entry's `source_ref` and dates, and carries its `notes`
(`not routed by pi at 74f46864`); per-model rows win, so a routed model keeps
its curated base. Which providers are complete is declared in the data, never
named in code, which only validates and applies the declaration. A
`complete_providers` entry has `provider`, `field` (`wire.api`), `source_ref`,
`notes`, `established_by`, `established_at_ms` and `review_by_ms`, under the
same review interval as corrections. The loader rejects an entry whose notes do
not name its source commit, a provider that also has a `*` default (which would
fill what completeness withholds), a provider with no routed per-model row, or
a per-model row of that provider citing a different source; a rejected entry
leaves the provider with its per-model rows alone. Explicit per-model
withholding is still accepted, but none ships for a complete provider because
completeness already covers it.

The current-read overlay replaces only that field with `{value, from: curated}`
or removes it when withheld, and discloses each change in `wire_corrections`
([wire contract](wire-contract.md) §4.1). It does not touch `wire.npm`,
`wire.shape`, stored eras or historical `at_ms` reads, and never applies to
alias or curated rows. Filters and paging keep
disclosures with their provider, including a withheld-only provider. Tests in
`tests/it/curated_wire.rs` drive named, bulk, filtered and paged reads plus
uncorrected history; mutation controls remove the application call, the
withholding operation and the completeness fallback independently.

Provider defaults use `model: "*"`, a nonempty `quote` from the provider's
official documentation, and a non-withheld value. The source reference records
the documentation URL and access date rather than a repository revision.
OpenAI, Anthropic, Google Gemini API (not Vertex), and xAI defaults fill only
missing observed endpoints; a model-specific correction or withholding wins.
Disclosures still name each affected model, preserving the existing protocol
and provider paging. The loader rejects defaults without a value or quote;
planted violations exercise that guard and the OpenCode Anthropic version-path
check against the normalized embedded seed's observed `wire.npm`.

## 10. Fact overrides (`fact-overrides.json`)

A row serves a sourced value for one upstream fact in place of the value
models.dev publishes, for the case where models.dev records a number that is
true for one way of calling a model and wrong for the way consumers call it.
It is not `catalog.correct`, which withholds a past window of recorded values;
an override changes what a current read serves and leaves the record alone.

Each row in `overrides` has `provider_id`, `model_id`, `fact_key`, `value`,
`replaces` (the exact upstream value it corrects, or `null` for an addition),
`reason`, `source_ref`, `quote` (the source sentences establishing the value),
optional `notes`, `established_by` (`fusiform`), `established_at_ms` and a
`review_by_ms` exactly 60 days later. `replaces` is required in both kinds, so
an addition is always written as one. Each fact key has its own rule:

| Fact key | Kind | `value` | `replaces` |
| --- | --- | --- | --- |
| `limit.context`, `limit.output` | replacement only | positive integer | a different positive integer |
| `capability.server_side_fallback` | addition only | `true` or `false` | `null` |
| `capability.mid_conversation_system` | addition only | `true` or `false` | `null` |
| `capability.temperature` | replacement only | boolean | a different published boolean |
| `model.status` | replacement of the stored `null` only | `"deprecated"` | `null` |
| `rate.output` | replacement only | sourced per-million-token rate | exact stored upstream token-rate object, without serve-time currency provenance |
| `rate.output_image` | addition only | sourced per-million-image-token rate | `null` |

No other fact may be overridden: other facts have their own curated mechanism
or serve-time derivation (other rates: inheritance and billing planes; `wire.*`: §9;
`model.canonical_id`: §5), and two mechanisms writing one fact would make the
served value depend on which ran last. Limits cannot be added: an unpublished
limit is stored as `null`, so a limit is absent only when a correction
withholds it, and an addition must never undo a withholding.
`capability.server_side_fallback` and `capability.mid_conversation_system` can
only be added, because models.dev has no such fields. Ingest stores
`model.status` on every row, as `null` when
models.dev publishes none, so a `model.status` row replaces that stored `null`
(`replaces: null`), only with `"deprecated"`: it marks a model its vendor has
deprecated while models.dev still publishes no status for it, so consumers
that skip deprecated rows stop routing to it. A status models.dev publishes is
never replaced. Whether a row is an addition follows its fact key, not a null
`replaces`. The file is embedded and parsed once; malformed rows and
duplicate `(provider_id, model_id, fact_key)` identities are counted as
rejects.

A replacement applies only while the stored
upstream value equals `replaces`. Once models.dev publishes anything else, the
row stops applying and the upstream value is served, so an override can never
hide a later upstream change. An addition applies
only while the stored upstream row has no such fact, neither served nor
withheld: once models.dev publishes the key with any value, `null` included,
upstream wins. Current reads use the compiled file. Every boot records claim
changes in `fact_override_era`; `at_ms` reads apply the claim in force and
evaluate its guard against upstream at that instant. A first claim starts at
`established_at_ms`; later changes and tombstones start at boot, clamped to
the latest boundary if the clock is behind. Provenance-only edits write
nothing and retain original stored provenance. Before the first era or after
a tombstone, upstream is served without an override. `catalog.history` still
serves upstream eras only, and every applied override is
disclosed in the response's `fact_overrides` list
([wire contract](wire-contract.md) §5.8). Alias entries are built from their
target's served facts, so an alias of an overridden row serves the overridden
value, disclosed under the alias id. `capability.server_side_fallback` and
`capability.mid_conversation_system` are not projected onto aliases, because
they describe what the Claude API accepts and an alias route reaches the model
another way. Curated model rows (§4) are
fusiform's own data and are never overridden.

Gemini 3.1 Flash Image splits its published $60/M image-output price from
$3/M text/thinking output. The `rate.output` replacement matches only the exact
upstream $60/M object; `rate.output_image` is an addition and yields if upstream
records that key. Both flow through its Antigravity alias, with borrowed-rate
markers retained in the disclosed served values. Vertex is not overridden.
The [Google pricing page](https://ai.google.dev/gemini-api/docs/pricing)
also quotes image output token counts (747 at 0.5K, 1120 at 1K,
1680 at 2K, 2520 at 4K). They are quoted in the override's source text as
supporting evidence only; fusiform serves no fact for them.

One replacement row serves `limit.output` 65,536 for `google/gemini-4-argon`.
models.dev changed it from 65,536 to 32,768 on 2026-10-13; 32,768 is Google's
cap for a non-streaming request, while a streaming request returns up to
65,536 ([model page](https://ai.google.dev/gemini-api/docs/models/gemini-4-argon),
[latest-model guide](https://ai.google.dev/gemini-api/docs/generate-content/latest-model)).
Its alias `google/antigravity-gemini-4-argon` serves 65,536 through the target,
which matches the route's own declaration in the Antigravity plugin's
[`model-registry.ts`](https://github.com/cortexkit/antigravity-auth/blob/48919aa7ae9378ea19af17e7d9da5b986395f00a/packages/core/src/model-registry.ts).

Five addition rows serve `capability.server_side_fallback` on `anthropic`
rows, from Anthropic's
[refusals and fallback page](https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback).
`true` means the Claude API accepts `fallbacks: "default"` with the
`server-side-fallback-2026-07-01` beta header for the model; an absent fact
means not stated.

| Model | Value | Basis |
| --- | --- | --- |
| `claude-fable-5`, `claude-fable-5-1`, `claude-opus-5`, `claude-opus-5-5` | `true` | The page names six models with safety classifiers, states the fallback generally for the Claude API, and excludes only Haiku 5.5; `claude-fable-5` is also its default-mode request example. The anthropic-auth plugin's [model table](https://github.com/cortexkit/anthropic-auth/blob/a9b4c35b1ed5aef217d29e6dc699d60589b0c9e2/packages/core/src/model-capabilities.ts) marks exactly these four true. No sentence names each model individually. |
| `claude-haiku-5-5` | `false` | "Claude Haiku 5.5 has no server-side fallback: with `fallbacks: "default"`, a declined request stays declined, and a list of fallback models returns a 400 error." |

`claude-sonnet-5-5` has no row. The page lists it among the models with
classifiers and shows a Sonnet 5.5 request falling back only in the
explicit-list form, while the anthropic-auth table marks it false; with the
two sources disagreeing, nothing is served for it.

Nine addition rows serve `capability.mid_conversation_system` on `anthropic`
rows only. It means support for a **persistent** `role: "system"` message inside
`messages`, without a beta header, not the beta turn-scoped `clear_at` variant.
The [mid-conversation system messages page](https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages)
explicitly lists the supported models and excludes Sonnet 5; the
[Opus 5.5 migration guide](https://platform.claude.com/docs/en/models/opus-5-5/migration-guide)
states that Opus 4.7 rejects the message with a 400 error. Each row quotes its
relevant sentence, accessed 2026-10-31, with review due 2026-12-30 (60 days).

| Model | Value |
| --- | --- |
| `claude-fable-5-1`, `claude-fable-5`, `claude-opus-5-5`, `claude-opus-4-8`, `claude-opus-5`, `claude-sonnet-5-5`, `claude-haiku-5-5` | `true` |
| `claude-sonnet-5`, `claude-opus-4-7` | `false` |

All other Anthropic rows, resellers, Bedrock, Vertex and Code Assist aliases
have no curated value; absence means not stated, never `false`.
`claude-mythos-preview` stays absent after its documented end-of-life.
The source also covers `claude-mythos-5-1` and `claude-mythos-5`, but the seed
does not carry them. They have no addition rows until the catalog carries
those ids, preserving the seed-presence gate. If models.dev ever publishes
the fact key, its value wins at the instant being read. The
`capability.mid_conversation_system` addition has dated claim history.

Gates in `fact_overrides.rs` check rejects, review dates
(`fact_overrides_are_within_their_review_dates`, with a planted overdue row in
`planted_overdue_override_fails_the_review_gate`), and that each row would
apply over the embedded seed as ingest stores it
(`overrides_replace_what_the_embedded_seed_publishes`): a seed refresh that
moves a replaced value, that starts publishing an added key, or that drops the
model names the row to re-source or remove
(`planted_upstream_move_reports_the_stale_override`,
`planted_upstream_publication_reports_the_stale_addition`). Route tests in
`tests/it/fact_overrides.rs` cover named, bulk, paged, filtered, plane, alias,
historical, upstream-moved and upstream-published reads.

## 11. Operator quality grades (store)

These grades are the operator's judgments, not upstream facts or benchmark
measurements. On chat rows, ELO describes general quality; EQ describes writing
and judgment. A set, non-chat `model.type` instead uses one `quality.grade`,
comparable only within that type, with higher values better.
They guide routing selection, not request bytes. Grades live in the store's
append-only `quality_grade_era` table and take effect immediately, without a
build or restart. A binary upgrade never overwrites an operator judgment.

Migration v8 imports the former grade file once. Its frozen snapshot is
`crates/fusiform-store/src/quality-v8.json`, reachable only from the migration:
all original decimal tokens and the file's grading time are retained, with
`graded_by: "operator"` and an import reason. Reopening a v8 store imports
nothing, including when a model has subsequently been withdrawn.

One grade belongs to the served `model.canonical_id`, shared across providers.
Only a row with no canonical id falls back to its own `provider/model` key
(which can contain slashes inside the model id). An alias gets a grade only
when its served canonical id matches; it does not borrow a target's own-key
grade. Canonical links are applied before the join. Billing-plane reads use
the same grades as plain reads. Read-pool queries select the newest row by
insertion id; a withdrawal hides the grade. Point-in-time `at_ms` catalog
reads still omit grades. Grade history is available separately.

All scores must be finite positive JSON numbers. They travel as exact
`serde_json::Number` values with `arbitrary_precision` and are stored as decimal
TEXT, never SQLite REAL. The positivity/finite check does not rewrite the
number: fractional trailing zeros survive storage and serving. A withdrawal
stores null in every numeric column. Migration v10 preserves every v8 row while
adding `grade`; a CHECK requires either the chat pair or the non-chat grade on
an active era, and no numbers on a withdrawal. No non-chat values are seeded.

Operator commands (preview by default; `--commit` appends one row):

```sh
ck models grade <model> --elo N --eq N --reason TEXT [--commit]
ck models grade <model> --grade N --reason TEXT [--commit]
ck models withdraw-grade <model> --reason TEXT [--commit]
ck models grades [--model <key>]
```

`quality.grade` validates that the key reaches a current served row and that
the dimensions match its `model.type` (absent means chat). A preview
shows the current and proposed grade and the affected row count plus up to ten
row ids. `quality.withdraw` refuses `no_coverage` without a current grade.
`quality.grades` lists all current grades; with `model`, it returns that key's
full history newest first, including times, reasons and withdrawals. A key with
no history refuses `no_coverage`. The CLI calls those tools, never the database.

Committed grades and withdrawals advance `catalog_version` atomically with the
append, using the same `max(now_ms, current + 1)` rule as poll writes. Previews
write nothing and do not advance it. This also invalidates cached page plans
and oversized-response refusals. Consumers polling `catalog.status` therefore
observe a live grade change without needing an upstream poll.

`fact_prefixes` filters `quality.elo`, `quality.eq` and `quality.grade` normally,
including `quality.`. A response containing any quality fact carries one `quality_scores`
attribution object. Its `graded_at_ms` is the newest `recorded_at_ms` among the
grades actually served in that response, not the date of an unrelated grade.
Filtered or paged responses with no quality facts omit it.

Gates cover populated-v7 migration exactness and constraints, reopen/import
idempotence, atomic grade/version writes, read-pool isolation, preview/commit,
validation and withdrawal. `route/quality_score_tests.rs` covers canonical and
own-key joins, aliases, billing planes, filters, page cache invalidation and
current-only catalog reads. Golden cases pin both the served facts and the
three operator tools. The former CSV importer and its CI test are removed;
there is no compiled-in serving loader.

## 12. Model lineage (`model-lineage.json`)

This file carries routing's grouping, applied to models.dev's own family and
canonical fields. It is classification policy rather than a published vendor
fact, so it requires neither first-party quotes nor review dates. Each map has
a short `purpose` note and an `entries` object from exact keys to one of the
closed lineage strings: `claude`, `gpt`, `gemini`, `grok`, `qwen`, `kimi`, `glm`,
`deepseek`, `minimax`, `mistral`, `llama`. Unknown rows have no `model.lineage`;
`other` is not an accepted value.

The maps apply in order:

1. `models`: exact `provider/model` overrides, including alias identities that
   cannot use the ordinary family path.
2. `families`: exact upstream `model.family` values. All 67 families belonging
   to the eleven lineages in the embedded seed are mapped; other families are
   deliberately unmapped. Related Mistral families are grouped together, Gemma
   is `gemini`, and Muse is `llama` under routing's existing policy.
3. `labs`: the lab prefix of the served `model.canonical_id`, only when no family
   is present. An unmapped family does not fall through.
4. `providers`: first-party provider ids present in the seed, only when both
   family and canonical id are absent. This does not include reseller provider
   prefixes or inspect model names.

Aliases use only their own exact entry or served canonical id: neither the
copied target family nor the target's exact-model override establishes the
alias's classification. A missing canonical identity is not replaced with the
target's provider/model key.

`model_lineage.rs` parses the embedded file once, reports malformed entries as
rejects while retaining independent valid classifications, and tests that the
shipped file has zero rejects. The route joins `model.lineage` only on current
reads, before the final fact-prefix filter and equally for plain and billing
plane views. Nothing is written to storage or served on `at_ms` reads.

`route/model_lineage_tests.rs` checks the serving paths and compares every
embedded seed row with the dated routing glob reference supplied on
2026-10-29. Its explicit `model_lineage_routing_differences.json` allowlist is
grouped by one-line reasons and pins both sides of every difference; stale or
new differences fail the test. New family classifications must be reviewed,
not inferred from names or added merely to preserve a mistaken glob match.

## 13. Request protocols (`wire-families.json`)

`wire.family` is a byte-affecting fact about a route's verified request protocol,
not a claim about runner support. Absent means **the request protocol is not
verified**, never unsupported. `wire.api` is a base URL, not this token. Runners
publish the protocols and auth methods they can drive separately.

The compiled-in table is parsed once. Each rule names exact `npm` packages,
`shapes` (`null` means the shape is absent, not a wildcard), one `wire_family`
token and a one-line `basis`. Today's rules cite broca's `wire_family` table at
[`d75a77db7249d4eb094ba667d1fa0d649e2e1682`](https://github.com/cortexkit/broca/blob/d75a77db7249d4eb094ba667d1fa0d649e2e1682/crates/broca-catalog/src/family.rs#L19-L40).
A new rule is added when someone verifies a package's protocol, citing the
package source or the provider's docs. A new or unverified package or shape
makes no fact, never a fallback to a different protocol.

Only `anthropic.messages`, `openai.responses`, `openai.chat_completions`,
`openai.chatgpt_responses`, `google.generate_content`, `google.code_assist` and
`aws.bedrock_converse` are admitted.
`openai.images` is verified from
https://platform.openai.com/docs/models/gpt-image-2.6-flare
(`/v1/images/generations` and `/v1/images/edits`, not Responses).
`google.interactions` is verified from
https://ai.google.dev/gemini-api/docs/omni
(`POST https://generativelanguage.googleapis.com/v1beta/interactions`).
`elevenlabs.text_to_speech` is also admitted, verified from
https://elevenlabs.io/docs/api-reference/text-to-speech/convert:
`POST https://api.elevenlabs.io/v1/text-to-speech/{voice_id}`, header `xi-api-key`.
The loader counts rejects for unknown
tokens, malformed rules, missing provenance and duplicate selectors; shipped
rules must have zero rejects. Alias and billing-plane loaders reject unknown
protocol tokens too.

Current upstream reads derive the fact from `wire.npm` and `wire.shape` only
after endpoint corrections. Withheld endpoints get no protocol. Present alias
rows use their own curated `wire_family` (§3); retired aliases get none.
`billing-planes.json` may set an optional per-plane `wire_family` in place of the
row's value; unspecified planes inherit it. Only these planes override today:

- `openai/chatgpt` → `openai.chatgpt_responses`. OpenAI's Codex client at
  [`708e3e2c70142c75682a375669ca60754f76976d`](https://github.com/openai/codex/blob/708e3e2c70142c75682a375669ca60754f76976d/codex-rs/model-provider-info/src/lib.rs#L85)
  defines `CHATGPT_CODEX_BASE_URL` as `https://chatgpt.com/backend-api/codex`,
  selected for `AuthMode::Chatgpt` (lines 151–159 and 467–486). Its
  [`responses.rs` lines 135–150](https://github.com/openai/codex/blob/708e3e2c70142c75682a375669ca60754f76976d/codex-rs/codex-api/src/endpoint/responses.rs#L135-L150)
  constructs `POST /responses`.
- `meta/oauth` → `openai.responses`. Broca's pinned
  [`credential_persona_route` lines 9–17](https://github.com/cortexkit/broca/blob/d75a77db7249d4eb094ba667d1fa0d649e2e1682/crates/broca-catalog/src/family.rs#L9-L17)
  selects Responses at `https://api.meta.ai/v1` for Meta subscription credentials.
  Meta's [API reference](https://dev.meta.ai/docs/api-reference) describes
  `POST /v1/responses` as “OpenAI-compatible”; its
  [Responses guide](https://dev.meta.ai/docs/protocols/responses) says “Call
  `POST /v1/responses` with a `model` and `input`.” These docs verify the protocol,
  not subscription credential eligibility; that plane's source also records a
  successful subscription-login probe.

Curated plane-only and System One rows have no package-derived protocol. The
Meta plane has an explicit verified value; no protocol is inferred for the
Cloudflare or TypeSafe curated rows from their decision/extraction kind.
For upstream rows, derived `wire.family` is never stored, appears only on current reads, is omitted on
`at_ms` reads, and respects `wire.family` and `wire.` fact-prefix filters.
`tests/it/wire_families.rs` exercises real seed witnesses, endpoint withholding,
aliases, planes, history and filters through the served route.
