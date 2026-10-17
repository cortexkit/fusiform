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
   curated rows and billing planes are applied when a read is served, so a
   point-in-time read replays what the upstream published and never today's
   curation (see [`data-model.md`](data-model.md) §8).
2. **An upstream row wins.** When models.dev publishes the same identity, the
   observed row is served and the curated claim is not.
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
not under that provider.

| Field | Meaning |
| --- | --- |
| `provider_id`, `model_id` | the served identity |
| `auth_methods` | a non-empty allowlist of distinct login methods from `apikey`, `chatgpt`, `oauth`, `antigravity` |
| `facts` | fact key to value; keys must be in `SERVED_FACTS` |
| `source_ref` | the evidence for the row |
| `established_at_ms`, `review_by_ms` | the review date must be exactly 60 days after establishment |

Load rules: a fact key outside `SERVED_FACTS` is rejected
(`curated_models.rs`: `planted_unknown_fact_is_rejected`); a login-restricted
row may not carry any `rate.*` fact, because a subscription row must not claim
an API list price (`planted_subscription_price_is_rejected`); duplicates and
malformed rows are rejects (`duplicate_and_malformed_rows_are_counted_as_rejects`).
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
3. Any stored upstream observation of the same identity, including a
   retirement, supersedes the row and its disclosure
   (`a_stored_upstream_row_supersedes_curated_facts_and_disclosure`).
4. Provider and fact filters apply; a named read keeps the row and its
   disclosure even when the fact filter matches nothing, and a bulk read drops
   it (`curated_rows_respect_fact_and_provider_filters`).
5. Point-in-time reads never serve curated rows
   (`point_in_time_reads_never_serve_curated_rows`).
6. `catalog.history` returns no eras and a `curated` object naming the source;
   `catalog.status` counts are unaffected
   (`curated_history_names_the_source_without_storing_eras_or_changing_status`).

The consuming router must refuse a credential whose login method is not in
`auth_methods`. A model absent from `curated` carries no login restriction.

When models.dev starts publishing the same identity, the suite fails with
"upstream now publishes this row; remove the curated row"
(`upstream_now_publishes_this_row_remove_the_curated_row`), checked against the
embedded seed.

## 5. The embedded seed (`models-dev-seed.json`)

A verbatim copy of `https://models.dev/api.json`, with
`models-dev-seed.meta.json` recording the fetch instant, URL, SHA-256 and
counts. On first boot an empty store is seeded from it, so a fresh install with
no network answers reads (`bootstrap::a_fresh_install_with_no_network_answers_a_read`).
The snapshot and its metadata must agree
(`bootstrap::the_snapshot_and_its_metadata_agree`), and the suite fails when
the snapshot is older than 14 days (`seed.rs`: `the_embedded_seed_is_not_stale`).
Refresh it with `scripts/refresh-seed.sh`. The seed is models.dev's data under
its MIT license; see `THIRD-PARTY-NOTICES.md`.

## 6. When a review gate fires

Plan prices, quota values, billing planes and rules, aliases and curated
model rows each carry a review date, and a test fails once any date has
passed. These are time-triggered: they fail on a day nobody changed anything,
and CI's scheduled run exists so they fire then. The plan-price, billing-plane
and seed gates' failure messages begin "NOT A BUILD FAILURE" and name what
to re-read.

To clear one:

1. Open the row's `source_ref` and read it again.
2. If the claim changed, update it (or remove the row).
3. Whether or not it changed, record who looked (`established_by` where the
   file has it), when (`established_at_ms`), and a new `review_by_ms`: 60 days
   later for billing planes, aliases and curated model rows; the file's stated
   interval for plan prices.
4. Commit the file. Confirming a claim unchanged is a real result; the diff is
   the provenance change alone.

To refresh the seed, run `scripts/refresh-seed.sh` and commit both seed files.

Extending a date without reading the source is possible, as with any gate; the
commit then records who made that decision.
