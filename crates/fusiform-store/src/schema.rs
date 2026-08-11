//! The schema, as migrations.
//!
//! Two tables carry the whole model, and the split between them is the design's
//! central idea: **every poll is recorded, and only a change opens an era.**
//!
//! - `observation` — one row per poll, including the ones that changed nothing
//!   and the ones that failed. Without the unchanged rows an era boundary is a
//!   bare instant with no lower bound; with them, "the change happened between
//!   these two instants" is arithmetic rather than a disclaimer.
//! - `era` — one row per (fact, value) interval, pure append, no `valid_to`.
//!   A closing timestamp written at close time is a second chance to be wrong
//!   about the same fact, and the next era's boundary already carries it.
//!
//! What is deliberately absent:
//!
//! - **No `valid_to` column.** An era ends where the next one begins. Storing
//!   the end separately means two rows must agree, and eventually they will not.
//! - **No `is_current` flag.** Currency is `MAX(boundary_at)` for a fact, which
//!   cannot disagree with itself. A flag needs a write on every change and is
//!   wrong from the moment one of those writes is missed.
//! - **No update or delete, anywhere.** A retirement is an era row with a
//!   tombstone value, and a correction is an era row with `Corrected` extent.
//!   The wrong value stays visible, because a consumer that derived something
//!   from it needs to find it.

use cortexkit_store::Migration;

/// The migration namespace for the catalog domain.
///
/// Named rather than defaulted: the store crate tracks applied migrations per
/// `(namespace, version)`, so a second domain added later gets an independent
/// chain instead of entangling its history with this one.
pub const NAMESPACE: &str = "catalog";

/// The ordered migration chain.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    statements: SCHEMA_V1,
}];

const SCHEMA_V1: &str = r#"
-- One row per poll. Every poll, including failures and 304s.
--
-- `outcome` is the closed vocabulary from the domain: changed / unchanged /
-- not_modified / failed / seeded. A failed poll observed NOTHING, so it must
-- never be used as the near edge of an observation window; the distinction is
-- stored rather than derived because deriving it later requires knowing what a
-- historical failure meant.
--
-- `seeded` is not a poll. It records the instant the embedded snapshot was
-- FETCHED by the refresh script, so the first real fetch that disagrees has an
-- honest left edge to bound its window against. Without it that fetch has no
-- prior observation and must be written as another seed boundary, which claims
-- the store came into existence twice.
CREATE TABLE observation (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    source            TEXT    NOT NULL,
    observed_at_ms    INTEGER NOT NULL,
    outcome           TEXT    NOT NULL,
    -- Populated only for `failed`, naming the coarse class a consumer branches
    -- on. Producer detail travels in `detail` for diagnostics only.
    failure_class     TEXT,
    detail            TEXT,
    -- The hash consumers are notified on: over the normalized catalog, so a
    -- reformatting upstream does not manufacture a change event.
    normalized_hash   TEXT,
    -- The hash of the raw bytes. Operator-only drift signal, split from the
    -- normalized hash BY AUDIENCE: this one moves when the upstream reformats,
    -- which is a fact an operator wants and a consumer must never be woken for.
    raw_hash          TEXT,
    -- The upstream's ETag, kept so the next poll can be conditional.
    etag              TEXT,
    -- Wall-clock duration of the fetch, for operator diagnostics.
    duration_ms       INTEGER
) STRICT;

CREATE INDEX observation_by_source_time
    ON observation (source, observed_at_ms DESC);

-- The most recent observation that CONFIRMED current values, per source.
-- Failed polls are excluded by the partial index rather than by a query
-- predicate, so a caller cannot forget the distinction.
CREATE INDEX observation_confirming
    ON observation (source, observed_at_ms DESC)
    WHERE outcome IN ('changed', 'unchanged', 'not_modified', 'seeded');
-- This list is the third statement of one rule (the domain's
-- CONFIRMING_OUTCOMES and the query built from it are the others). DDL cannot
-- be built from a Rust constant, so `the_schema_index_matches_the_domain_list`
-- holds the two together; without it, adding an outcome updates the query and
-- silently leaves this index behind.

-- One row per era: a fact held a value over an interval.
--
-- `fact_key` identifies WHAT the era is about, in the served contract's
-- vocabulary rather than storage's. `value_json` is the value it held.
CREATE TABLE era (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    source                TEXT    NOT NULL,
    provider_id           TEXT    NOT NULL,
    model_id              TEXT    NOT NULL,
    fact_key              TEXT    NOT NULL,
    value_json            TEXT    NOT NULL,

    -- When this era's value became true, as best the boundary kind allows.
    boundary_at_ms        INTEGER NOT NULL,
    -- How that instant was determined: observed / asserted / seed / corrected.
    -- Load-bearing, not descriptive: a source that publishes real effective
    -- dates must not be degraded to fusiform's polling cadence, and a source
    -- polled blind must never be dressed up as if it published dates.
    boundary_kind         TEXT    NOT NULL,

    -- The previous CONFIRMING observation of this source, so the window
    -- (prior_observation_at_ms, boundary_at_ms] is arithmetic.
    --
    -- NULL for every kind except `observed`, and that is a constraint below
    -- rather than a convention: a seed has no prior observation, an asserted
    -- boundary needs no window because the source stated the instant, and a
    -- correction is about fusiform's own record rather than an upstream change.
    prior_observation_at_ms INTEGER,

    -- The observation that opened this era. NULL for seed rows, which are not
    -- observations of anything.
    observation_id        INTEGER REFERENCES observation(id),

    -- Correction extent, present only when boundary_kind = 'corrected'.
    -- `affected_from` is a LOWER BOUND: when the true start of the bad region
    -- is unknown it goes to the earliest plausible instant, never a best guess,
    -- because an under-inclusive partition asserts that bad facts outside it
    -- are fine.
    corrected_fields_json   TEXT,
    affected_from_ms        INTEGER,
    affected_until_ms       INTEGER,
    correction_reason       TEXT,

    -- A boundary kind outside the vocabulary is a bug, not a new feature.
    CHECK (boundary_kind IN ('observed', 'asserted', 'seed', 'corrected')),

    -- An observation window belongs only to an observed boundary. Any other
    -- kind carrying one would be claiming a window it did not measure.
    CHECK (
        (boundary_kind = 'observed'  AND prior_observation_at_ms IS NOT NULL)
        OR
        (boundary_kind <> 'observed' AND prior_observation_at_ms IS NULL)
    ),

    -- A window must have positive width and must not run backwards.
    CHECK (
        prior_observation_at_ms IS NULL
        OR prior_observation_at_ms < boundary_at_ms
    ),

    -- Correction extent travels with a correction and never without one. The
    -- four columns are all-or-nothing: an extent missing its interval cannot
    -- partition anything, which is the only thing an extent is for.
    CHECK (
        (boundary_kind =  'corrected'
            AND corrected_fields_json IS NOT NULL
            AND affected_from_ms      IS NOT NULL
            AND affected_until_ms     IS NOT NULL
            AND correction_reason     IS NOT NULL)
        OR
        (boundary_kind <> 'corrected'
            AND corrected_fields_json IS NULL
            AND affected_from_ms      IS NULL
            AND affected_until_ms     IS NULL
            AND correction_reason     IS NULL)
    ),

    -- An interval that ends before it starts describes nothing.
    CHECK (
        affected_from_ms IS NULL
        OR affected_from_ms <= affected_until_ms
    ),

    -- A seed is not an observation, so it must not name one. Enforced because
    -- the alternative -- a seed row pointing at the first real fetch -- would
    -- read as if the upstream had been observed to hold the seeded value.
    CHECK (
        (boundary_kind =  'seed' AND observation_id IS NULL)
        OR
        (boundary_kind <> 'seed')
    )
) STRICT;

-- Point-in-time lookup: the era for a fact at instant T is the one with the
-- greatest boundary_at_ms not exceeding T.
CREATE INDEX era_point_in_time
    ON era (source, provider_id, model_id, fact_key, boundary_at_ms DESC);

-- Two eras for the same fact at the same instant would make a point-in-time
-- read ambiguous, and the arbitrary tie-break would be silent.
CREATE UNIQUE INDEX era_one_per_fact_per_instant
    ON era (source, provider_id, model_id, fact_key, boundary_at_ms);

-- The monotonic catalog version consumers refuse to go backwards on.
--
-- One row, enforced by the CHECK. A counter in a table rather than
-- MAX(id)+1 over eras because it must survive a restore: an engram restore
-- rewinds row ids, and a rewound version makes every consumer correctly refuse
-- every subsequent push.
CREATE TABLE catalog_version (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    version         INTEGER NOT NULL,
    -- The observation that produced this version, so a version is always
    -- traceable to the poll that justified it.
    observation_id  INTEGER REFERENCES observation(id),
    updated_at_ms   INTEGER NOT NULL
) STRICT;

INSERT INTO catalog_version (id, version, observation_id, updated_at_ms)
VALUES (1, 0, NULL, 0);
"#;
