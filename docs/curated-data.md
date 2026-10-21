# Curated data

Most of what fusiform serves is observed: fetched from models.dev and stored
as eras. A small set of facts is curated by hand in files under
`crates/fusiform-module/data/`, compiled into the binary, and reviewed on a
schedule. This document lists those files, what each may claim, and the gates
that keep them honest.

| File | Holds | Applied |
| --- | --- | --- |
| `model-creators.json` | which provider published an open-weight family | at read time |
| `model-aliases.json` | route-specific ids and the published row each one is | at read time, current reads only |
| `curated-models.json` | provider rows models.dev does not carry | at read time, current reads only |
| `canonical-links.json` | sourced lab identities missing from upstream provider rows | at read time, current reads only |
| `billing-planes.json` | how a provider bills a login method | at read time, current reads only |
| `plan-prices.json` | subscription list prices | written to `plan_price_era` at start |
| `models-dev-seed.json` | a copy of the upstream catalog for offline first boot | written as seed eras into an empty store |
| `engram-catalog.json` | the backup enrollment descriptor | written beside `store.db` at start; not otherwise read |

Billing planes and plan prices have their own documents,
[`billing-planes.md`](billing-planes.md) and [`plan-prices.md`](plan-prices.md).
The seed and the enrollment descriptor are covered in
[`architecture.md`](architecture.md) and
[`backup-and-restore.md`](backup-and-restore.md). Unit tests named below are in
the source file of the same name under `crates/fusiform-module/src/`.

## 1. Rules every curated file follows

1. **Curated facts are never stored as observations.** Creators, aliases,
   curated rows, canonical links and billing planes are applied when a read is served, so a
   point-in-time read replays what the upstream published and never today's
   curation (see [`data-model.md`](data-model.md) §8).
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

Serving rules are in [`wire-contract.md`](wire-contract.md) §5.5.

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
| `auth_methods` | a non-empty allowlist of distinct login methods from `apikey`, `chatgpt`, `oauth`, `antigravity` |
| `facts` | fact key to value; keys must be in `SERVED_FACTS` |
| `source_ref` | the evidence for the row: each page read, with the sentences it states quoted verbatim |
| `established_by` | optional; who read the sources. Not served |
| `established_at_ms`, `review_by_ms` | the review date must be exactly 60 days after establishment |

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
`planted_duplicate_or_malformed_held_entry_is_rejected`); the shipped entries
load (`shipped_held_entries_load_and_keep_their_rows`). Serving is tested in
`route/curated_held_tests.rs`, including the control that removing a held
entry serves the row as curated again
(`releasing_a_held_entry_serves_the_row_as_curated_again`), and for a held id
the store holds as a withdrawn upstream row in
`curated_withdrawn_tests::a_held_row_leaves_the_withdrawn_upstream_row_to_report_itself`.
The serving tests in `system_one_tests` and `curated_withdrawn_tests` that
cover these rows as served plant the shipped file with its `held` array
removed.

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
Names, release dates and other provenance stay in `source_ref` rather than
being invented as new fact keys.

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
   no edit to the file. Point-in-time reads still serve only stored history,
   so at an instant when upstream listed the model they serve its upstream
   row. `catalog.history` serves the stored eras for any fact the upstream row
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
5. Point-in-time reads never serve curated rows
   (`point_in_time_reads_never_serve_curated_rows`).
6. `catalog.history` returns no eras and a `curated` object naming the source;
   `catalog.status` counts are unaffected
   (`curated_history_names_the_source_without_storing_eras_or_changing_status`).
7. A provider with curated rows is a known provider on every read, even when
   models.dev does not list it. A named read of a model it does not carry is
   refused `no_coverage` as an unknown model, and a point-in-time provider read
   answers empty rather than refusing the provider
   (`system_one_tests::an_unknown_typesafe_model_is_refused_as_an_unknown_model`,
   `system_one_tests::point_in_time_reads_never_serve_system_one_rows`).

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

## 7. When a review gate fires

Plan prices, quota values, billing planes and rules, aliases, canonical links
and curated model rows each carry a review date, and a test fails once any date has
passed. These are time-triggered: they fail on a day nobody changed anything,
and CI's scheduled run exists so they fire then. The plan-price, billing-plane
and seed gates' failure messages begin "NOT A BUILD FAILURE" and name what
to re-read.

To clear one:

1. Open the row's `source_ref` and read it again (and `lab_source_ref` for a canonical link).
2. If the claim changed, update it (or remove the row).
3. Whether or not it changed, record who looked (`established_by` where the
   file has it), when (`established_at_ms`), and a new `review_by_ms`: 60 days
   later for billing planes, aliases, canonical links and curated model rows; the file's stated
   interval for plan prices.
4. Commit the file. Confirming a claim unchanged is a real result; the diff is
   the provenance change alone.

To refresh the seed, run `scripts/refresh-seed.sh` and commit both seed files.

Extending a date without reading the source is possible, as with any gate; the
commit then records who made that decision.
