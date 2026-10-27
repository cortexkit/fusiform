# Data model

Fusiform keeps one SQLite database per host, opened through `cortexkit-store`
from the storage descriptor the supervisor sends in its connection handshake
(see [`module-surface.md`](module-surface.md)). This document describes what
that database holds and the rules every write follows. The served shape of
the same data is in [`wire-contract.md`](wire-contract.md).

Test names below are libtest paths inside each crate's integration target
(`--test it`), for example `history::a_restore_cannot_rewind_the_catalog_version`
in `crates/fusiform-store/tests/it/history.rs`.

## 1. Tables

Tables, as shipped: `observation`, `era`, `catalog_version`,
`observation_artifact_event`, `plan_price_era`, `curated_model_era`,
`cortexkit_schema_version`, `cortexkit_fence`.

- `observation`, `era`, `catalog_version`, `observation_artifact_event`,
  `plan_price_era` and `curated_model_era` are fusiform's own, created by the
  migration chain in `crates/fusiform-store/src/schema.rs` under the namespace
  `catalog`.
- `cortexkit_schema_version` belongs to the store crate and records which
  migrations have been applied.
- `cortexkit_fence` belongs to the store crate and is created on the first
  fenced write, not at open. A store that has been opened and never written to
  does not have it, and that is not a fault.

The list above is held against a real store in both directions by
`schema_doc::the_design_note_names_the_tables_that_exist`: a table the store
creates and this list omits fails it, and so does a name here that the store
does not create.

### 1.1 Migrations

| Version | Adds |
| --- | --- |
| 1 | `observation`, `era`, `catalog_version`, and the point-in-time and uniqueness indexes on `era` |
| 2 | a partial index over corrected eras, so a read can ask cheaply whether a correction covers its instant |
| 3 | an index on `era.observation_id`, which `catalog.status` uses to count what each poll wrote |
| 4 | a first artifact table, replaced by version 5 |
| 5 | `observation_artifact_event`, an event log of artifact marks and retractions |
| 6 | `plan_price_era` |
| 7 | `curated_model_era` |

A store whose recorded schema version is newer than the running binary's chain
is refused at open, and the error names the store's version
(`schema_doc::a_store_written_by_a_newer_binary_is_refused`). Migrations 4, 6
and 7 are tested against stores that already hold rows
(`schema_doc::the_artifact_table_arrives_on_a_populated_store`,
`schema_doc::the_plan_price_table_arrives_on_a_populated_store`,
`schema_doc::the_curated_model_table_arrives_on_a_populated_store`).

## 2. Observations

One `observation` row is written per poll, whatever happened. Columns:

| Column | Meaning |
| --- | --- |
| `source` | the upstream; `models.dev` is the only source |
| `observed_at_ms` | when the poll happened |
| `outcome` | `changed`, `unchanged`, `not_modified`, `failed` or `seeded` |
| `failure_class` | set only for `failed`: `network`, `http_status`, `parse` or `implausible` |
| `detail` | producer detail for diagnostics; consumers do not branch on it |
| `normalized_hash` | hash of the normalized catalog; moves only when a served fact moves |
| `raw_hash` | hash of the fetched bytes; moves when the upstream reformats |
| `etag` | the upstream's validator, sent as `If-None-Match` on the next poll |
| `duration_ms` | how long the fetch took |

The rules:

1. A `failed` poll observed nothing. It never confirms current values and is
   never the near edge of an observation window
   (`history::a_failed_poll_does_not_narrow_the_window`). A `not_modified`
   poll does confirm them (`history::a_not_modified_response_does_narrow_the_window`).
2. `seeded` is not a poll. It records the instant the embedded snapshot was
   fetched, so the first real fetch that disagrees has a left edge for its
   window (`bootstrap::the_seeded_observation_is_stamped_when_the_snapshot_was_fetched`).
3. The confirming outcomes are listed once in the domain and once in a partial
   index; `seed_boundary::the_schema_index_matches_the_domain_list` holds the
   two together.
4. The failure class round-trips through its stored word, and an unknown word
   reads back as no class rather than a guess
   (`failure_class_roundtrip::an_unknown_word_reports_no_class_rather_than_guessing`).
5. Reformatted bytes with the same normalized content are not a change
   (`tick::reformatted_bytes_do_not_produce_a_change`).

## 3. Eras

An `era` row states that one fact held one value from an instant onward. The
fact is identified by `(source, provider_id, model_id, fact_key)`; the value is
`value_json`, in the served vocabulary (see
[`wire-contract.md`](wire-contract.md) §4).

### 3.1 Append-only

The store never updates or deletes a row in `era`, `observation`,
`observation_artifact_event`, `plan_price_era` or `curated_model_era`.

- There is no `valid_to` column. An era ends where the next era for the same
  fact begins.
- There is no `is_current` flag. The value in force at instant T is the era
  with the greatest `boundary_at_ms` not exceeding T.
- A model that disappears gets an `existence` era with value `"absent"`; a
  rate that disappears gets an era whose value is unpriced with reason
  `missing_rate`; a limit or capability that disappears gets `null`. The old
  value stays readable
  (`ingest::a_disappeared_model_is_tombstoned_and_stays_readable`,
  `ingest::a_withdrawn_rate_is_tombstoned_rather_than_left_current`,
  `ingest::a_withdrawn_limit_becomes_null`).
- A correction is a new era with boundary kind `corrected` (§5), never an
  edit.
- Two eras for one fact at the same instant are refused by a unique index
  (`history::one_fact_cannot_have_two_eras_at_the_same_instant`), and a batch
  of eras is written all or nothing
  (`history::a_batch_of_eras_is_all_or_nothing`).
- Every write goes through the store crate's single-writer fence, which
  checks that this process still holds the writer lease before committing, so
  a superseded writer cannot write
  (`every_write_is_fenced::no_write_escapes_the_fence`).

### 3.2 Boundary kinds

`boundary_kind` is a closed vocabulary enforced by a `CHECK` constraint:

| Kind | Meaning | `prior_observation_at_ms` |
| --- | --- | --- |
| `observed` | a poll saw the value change | required: the previous confirming observation |
| `asserted` | the source stated the effective instant | absent |
| `seed` | the store started from the embedded snapshot | absent; `observation_id` must also be absent |
| `corrected` | fusiform's own record was wrong over a window | absent; the four correction columns of §5 are required |

For an `observed` era the change happened somewhere in
`(prior_observation_at_ms, boundary_at_ms]`, and that window has positive
width (`history::an_observation_window_is_never_zero_width`). A seed era claims
no window (`seed_boundary::a_seed_boundary_claims_no_window`). The schema and
the encoder agree on the vocabulary in both directions
(`boundary_vocabulary::every_kind_the_encoder_produces_is_accepted_by_the_schema`,
`boundary_vocabulary::the_schema_accepts_no_kind_the_encoder_cannot_produce`).

### 3.3 What opens an era

Ingest diffs the normalized document against the values in force and writes
only facts whose value changed
(`ingest::re_ingesting_the_same_document_writes_nothing`,
`ingest::one_rate_change_opens_exactly_one_era`). Values are compared as
parsed JSON, not as text, and with every field fusiform adds removed first.
That removal is one function, `upstream_claim_of` in
`crates/fusiform-store/src/ingest.rs`, used by both the diff and the catalog
digest so the two always agree about what counts as a change. A field that
fusiform attaches to a stored value, rather than one the upstream published,
must be listed there or it will open eras and move the digest
(`ingest::a_producer_annotation_moves_neither_the_digest_nor_the_diff`,
`ingest::the_digest_moves_exactly_when_the_diff_does`).

Order matters only where the upstream's order is meaningful: reordering a
modality list is not a change, reordering reasoning option values is
(`ingest::reordering_a_modality_array_is_not_a_change`,
`ingest::reordering_reasoning_option_values_is_a_change`).

### 3.4 Point-in-time reads

A read at instant T serves, per fact, the era with the greatest
`boundary_at_ms <= T`. The boundary is inclusive: a read at the boundary
instant gets the new value
(`serve::a_read_at_a_boundary_instant_returns_the_new_value`). Facts resolve
independently (`serve::facts_that_moved_at_different_times_resolve_independently`).

When T falls strictly inside a later era's observation window, the served
value may already have changed. The read still serves it and reports the
bracket as `uncertain` on the wire. Both bracket endpoints are themselves
certain instants
(`uncertain_reads::the_bracket_comes_from_the_next_era_and_excludes_both_endpoints`),
and the current value is never uncertain
(`uncertain_reads::the_current_value_is_never_stale`).

## 4. The catalog version

`catalog_version` is a single row. Each poll that writes eras advances it to
`max(now_ms, current + 1)` after the eras are durable. Because the next value
is at least the wall clock, restoring an older copy of the database cannot make
the version go backwards (`history::a_restore_cannot_rewind_the_catalog_version`,
`store_durability::a_restore_rewinds_the_row_counts_but_not_the_catalog_version`),
and a clock that steps backwards still yields a larger value
(`history::the_catalog_version_survives_a_backwards_clock`). A failed poll does
not advance it (`tick::a_failure_does_not_advance_the_version`). A freshly
seeded store reports the snapshot's fetch instant as its version
(`bootstrap::a_seeded_store_serves_the_snapshots_vintage_as_its_version`).

The version answers "has anything changed since I last looked". It does not
identify a snapshot; see [`wire-contract.md`](wire-contract.md) §3.

## 5. Corrections

A correction records that fusiform's own record of one fact was wrong over a
window. It is an era with `boundary_kind = 'corrected'` carrying four columns
that are required together and forbidden otherwise:

| Column | Meaning |
| --- | --- |
| `corrected_fields_json` | the field ids the correction names |
| `affected_from_ms` | start of the bad window; a lower bound, set to the earliest plausible instant when the true start is unknown |
| `affected_until_ms` | when the fix landed and fusiform stopped recording the bad value |
| `correction_reason` | a reference to the finding document (a defect report) that explains why the record was wrong |

The rules:

1. A correction carries no value. The value in force after the window was
   written by an ordinary poll once the fix landed
   (`correct::a_correction_does_not_move_the_current_value`).
2. A read at an instant inside `[affected_from_ms, affected_until_ms]`, both
   ends included, refuses that fact instead of serving the recorded value
   (`corrections::a_read_inside_a_corrected_interval_refuses`,
   `corrections::both_endpoints_are_inside_the_corrected_interval`). The
   catalog read omits it and names it as withheld
   (`corrections::the_bulk_read_omits_a_corrected_fact`).
3. A correction is scoped to the fact it names
   (`corrections::a_correction_is_scoped_to_its_fact`), and the recorded value
   is still reachable through history (`corrections::the_recorded_value_is_still_reachable_deliberately`).
4. A correction cannot reach past the instant it is recorded, so it can never
   refuse a read of the present
   (`correction_never_denies_now::a_correction_cannot_refuse_a_read_at_now`).
5. The planner refuses, collecting every reason at once
   (`correct::all_refusals_are_collected`): a fact with no history
   (`correct::correcting_a_fact_with_no_history_is_refused`), a window that
   contains the value currently in force
   (`correct::a_window_containing_the_current_value_is_refused`), a value that
   did not change across the window
   (`correct::a_correction_with_nothing_to_repair_is_refused`), an identical
   correction recorded twice
   (`correct::an_identical_correction_is_refused_the_second_time`), and a store
   that changed between planning and applying
   (`correct::a_poll_between_plan_and_apply_is_refused`).

How an operator writes one is in [`operations.md`](operations.md).

## 6. Artifact marks

Some polls write eras that record fusiform changing its own representation
rather than the upstream changing its data. An operator can mark such a poll;
the mark is a row in `observation_artifact_event`:

| Column | Meaning |
| --- | --- |
| `observation_id` | the marked poll |
| `action` | `marked` or `retracted` (closed by a `CHECK`) |
| `reason` | a reference to the evidence that the poll was, or was not, a representation change |
| `recorded_at_ms` | when the event was recorded |

The current state of a poll is its newest event. A mark excludes the poll's
restating eras (eras whose value states the same upstream claim as the one
already in force) from the `last_changed_at_ms` derivation; a genuine change
written by the same poll still counts
(`ingest::a_real_change_inside_a_marked_poll_is_kept`). A retraction appends
an event rather than removing the mark, so the history of the claim survives
(`ingest::a_retracted_mark_stops_excluding_and_stays_on_the_record`). A mark
changes no era and no served value.

## 7. Plan prices

`plan_price_era` holds the curated subscription prices served by
`plan.prices`. It is a separate table from `era`, keyed `(provider_id, tier)`.
The full contract is in [`plan-prices.md`](plan-prices.md); the storage rules
are:

1. `boundary_kind` is always `asserted`: every row carries a date the vendor
   stated, with no fetch behind it. Any other kind is refused by a `CHECK`.
2. `boundary_at_ms` is the vendor's effective date. There is no column for
   when the row was committed, so a price recorded late is indistinguishable
   from one recorded on time.
3. A row is either priced (`minor_units`, `exponent`, `currency` and `period`
   all present, `refusal_reason` absent) or a refusal (all four absent,
   `refusal_reason` present). Every other combination is refused by `CHECK`
   constraints (`schema_doc::the_plan_price_table_arrives_on_a_populated_store`).
4. There is no unique key on `(provider_id, tier, boundary_at_ms)`. A
   correction of a mistyped price is a new row at the same boundary, and the
   row in force at T is the newest `id` among rows with `boundary_at_ms <= T`.
5. The curated file is applied on every start. Re-applying an unchanged file
   writes nothing, and a change of provenance alone is not a change
   (`schema_doc::re_applying_the_curated_file_is_a_no_op`).
6. A key that disappears from the file is tombstoned with a refusal row whose
   reason says the key is no longer carried, rather than left serving its old
   price (`schema_doc::a_key_dropped_from_the_file_is_tombstoned`).

## 7a. Curated model rows

`curated_model_era` holds the history of the hand-curated model rows in
`crates/fusiform-module/data/curated-models.json` (rows for models models.dev
does not list), so a point-in-time `catalog.get` can serve the curated claim
that was in force at its instant. Current reads do not read this table: they
serve the compiled file. The rules:

1. A row is keyed `(provider_id, model_id, boundary_at_ms)` and holds the
   whole claim: `facts_json` (the row's facts as one JSON object, in the served
   vocabulary) and `auth_methods_json` (the login allowlist), with its
   provenance (`source_ref`, `established_by`, `established_at_ms`,
   `review_by_ms`). A tombstone has no facts, no allowlist and no review date,
   and carries a `tombstone_reason` instead. `CHECK` constraints refuse every
   other combination (`schema_doc::the_curated_model_table_arrives_on_a_populated_store`).
2. The file is applied on every start (`CatalogStore::append_curated_models`).
   Each served row is compared with the newest stored row for its key on the
   facts and the allowlist (as a set) only, never on provenance, so re-reading
   a page and re-dating the row writes nothing.
3. Dating: a key's first row is dated at the file row's `established_at_ms`,
   its curation date. Every later row, a changed claim or a tombstone, is
   dated at the start of the build that carries it, the instant fusiform began
   serving the new value. `established_at_ms` cannot date a later change: it
   records when the row was curated or last re-read, not when a fact on it
   changed. A start whose clock is behind the key's newest boundary writes at
   that boundary instead, so a key's history never runs backwards.
4. A key stored but no longer served by the file, because the row was removed
   or is held, is tombstoned, once. A held row with no stored history is
   tombstoned too, so releasing it later dates its first claim at the release.
5. There is no unique key, as in `plan_price_era`: rows can share a boundary
   (rule 3), and the row in force at T is the one with the greatest
   `boundary_at_ms` not exceeding T, the newest `id` breaking a tie.
6. The shipped file (five rows, none held) writes five rows on a store's
   first start, and an unchanged file writes nothing after that. These rules
   are tested in `crates/fusiform-module/src/route/curated_history_tests.rs`.

## 8. What is not stored

On upstream rows, `model.name` and `provider.name` are stored per model as ordinary
eras, not derived at serve time. A rename opens a new era; a missing or empty name
is absent from the catalog, with a null withdrawal era if previously published.
The `provider.` namespace describes the offering's provider, not the creator
of the weights. Curated model names and alias labels are not written to era
history. Alias labels are served only on current reads; curated model rows are
recorded in their own table (§7a). Provider labels for both are joined from
upstream model eras (on a point-in-time read of a curated row, the label in
force at that instant), or sourced in curation for a provider absent upstream.

- Billing-plane rules, curated model rows, aliases and open-weight rate
  inheritance are applied when a read is served and are never written as eras
  (`ingest::ingested_eras_never_carry_serve_only_billing_states`,
  `route::billing_plane_tests::plane_views_never_change_stored_rows_or_catalog_version`
  in `crates/fusiform-module/src/route.rs`). A point-in-time read therefore
  replays what the upstream published, not a rule fusiform holds today. The
  one curated addition to a point-in-time read is a curated model row, served
  from `curated_model_era` as it stood at that instant (§7a).
- The currency provenance of a rate is attached when it is served, not stored
  (`served_vocabulary::a_stored_rate_carries_no_annotation_fusiform_invented`).
- Request header/body overrides and experimental mode selectors are never
  stored as facts. Published client packages, endpoints and request shapes are
  stored verbatim as `wire.*` claims; renderer mapping remains the consumer's
  (`served_vocabulary::unreliable_and_renderer_selecting_fields_are_not_facts`).

## 9. Concurrency

The database runs in WAL mode so reads proceed while the poll loop writes
(`journal_mode::the_store_runs_in_wal_so_reads_survive_writes`). One process
holds the single-writer lease; a second instance cannot take it
(`store_durability::a_second_instance_cannot_steal_the_writer_lease`). Reads
use separate read-only connections that refuse writes and do not wait for each
other (unit tests in `crates/fusiform-store/src/tests/read_connections.rs`).
