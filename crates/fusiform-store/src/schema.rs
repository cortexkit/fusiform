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
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        statements: SCHEMA_V1,
    },
    Migration {
        version: 2,
        statements: SCHEMA_V2,
    },
    Migration {
        version: 3,
        statements: SCHEMA_V3,
    },
    Migration {
        version: 4,
        statements: SCHEMA_V4,
    },
    Migration {
        version: 5,
        statements: SCHEMA_V5,
    },
    Migration {
        version: 6,
        statements: SCHEMA_V6,
    },
];

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

// Every point-in-time read asks whether a correction covers the instant it is
// resolving. Without an index for that question it is answered by walking all
// of a source's eras — measured at 9ms against 68,000 rows, paid on every read,
// to discover that corrections are almost always absent.
//
// A partial index holds ONLY corrected rows. Corrections are rare by nature:
// each one is a defect fusiform found in its own record, so the index stays
// tiny while the table grows past a hundred thousand rows. That asymmetry is
// exactly what a partial index is for.
//
// Found by running a real read against a real store. The query looked fine.
const SCHEMA_V2: &str = r#"
CREATE INDEX era_corrections
    ON era (source, affected_from_ms, affected_until_ms)
    WHERE boundary_kind = 'corrected';
"#;

// `catalog.status` derives what each poll changed by counting the eras that
// poll wrote, and without this index every count is a FULL TABLE SCAN.
//
// Measured on the live store 2026-08-16, 92,425 eras. The status route runs
// four correlated subqueries per poll — total eras, arrivals, withdrawals,
// revisions — and `EXPLAIN QUERY PLAN` reported `SCAN e` for each. So the cost
// is polls x 4 x 92,425 rows, and it showed up as clean linear scaling:
//
//     --polls 1     0.57s
//     --polls 10    3.25s     (the default)
//     --polls 60   30.00s
//
// Thirty seconds is past any sane request deadline, and the failure-history
// line in status output literally suggests `--polls 60` to see an older
// failure. The default of 10 was survivable at 3.25s until a poll tick was
// running, at which point catalog.status timed out on the channel — which is
// how this was found: the tool call failed while `ck health` stayed green,
// because health reads atomic signals and never touches the store.
//
// The composition is derived rather than stored on purpose: it is retroactive,
// so polls recorded before the feature existed still report correctly. That
// decision is worth keeping and it is what makes the index load-bearing.
const SCHEMA_V3: &str = r#"
CREATE INDEX era_by_observation ON era (observation_id);
"#;

// A poll whose eras record fusiform changing its own mind, not the upstream
// changing its data.
//
// # Why this exists as a mechanism rather than a constant
//
// On 2026-08-16 a serialization change of mine made every stored rate differ
// textually from every freshly normalized one, so the diff wrote 17,455 eras
// with identical values on both sides. Any measure derived from "when did this
// fact last change" reads those as changes, and a consumer calibrating on them
// sees the WHOLE CATALOG as freshly maintained — the direction that hides
// abandoned rows rather than exposing them.
//
// A hardcoded exclusion of that one observation would fix the instance. The
// next serialization change would arrive uncovered, and would arrive looking
// exactly like data. So the artifact is a fact ABOUT a poll, recorded once and
// honoured by every derivation.
//
// # Why a separate table rather than a column
//
// The store never updates a row in place, and these observations are already
// written. A new row asserting something about an existing one is an addition;
// altering the observation would be the store doing to its own history what
// this table exists to record.
//
// `reason` names the finding rather than describing it, so an operator meets
// the evidence rather than a summary of it.
const SCHEMA_V4: &str = r#"
CREATE TABLE observation_artifact (
    observation_id  INTEGER PRIMARY KEY REFERENCES observation(id),
    reason          TEXT    NOT NULL,
    recorded_at_ms  INTEGER NOT NULL
);
"#;

// A mark becomes an EVENT LOG, so it can be taken back without erasing that it
// was made.
//
// Marking a poll excludes its restating eras from every derivation that honours
// it, and v4 had no way to take that back. That asymmetry is what made the
// production mark a decision needing sign-off: a write with no retraction path
// has to be certain in advance, which is exactly when certainty is least
// available.
//
// The obvious fix — a `retracted_at` column on the existing row — was written
// and thrown away. It needs an UPDATE, and a later re-mark overwrites the
// original `reason`, so the sequence "marked for A, retracted, marked for B"
// loses A. That is the store's one invariant eroded for convenience: an
// operator asking why last_changed_at moved would read B and find no trace that
// A was ever claimed.
//
// So: one row per EVENT, current state derived from the newest event per
// observation. Exactly how eras work, for exactly the same reason. The old
// table is dropped rather than migrated because nothing has ever been marked in
// production — verified against the live store before writing this, rather than
// assumed from the feature's age.
const SCHEMA_V5: &str = r#"
DROP TABLE observation_artifact;

CREATE TABLE observation_artifact_event (
    id              INTEGER PRIMARY KEY,
    observation_id  INTEGER NOT NULL REFERENCES observation(id),
    -- 'marked' or 'retracted'. A CHECK rather than a comment: an unrecognised
    -- action would make the derived state silently wrong, and the vocabulary
    -- fence in boundary_vocabulary.rs exists because that already happened once
    -- with boundary_kind.
    action          TEXT    NOT NULL CHECK (action IN ('marked', 'retracted')),
    reason          TEXT    NOT NULL,
    recorded_at_ms  INTEGER NOT NULL
);

-- The derivation reads the newest event per observation, so this index is the
-- one the exclusion query depends on.
CREATE INDEX artifact_event_by_observation
    ON observation_artifact_event (observation_id, id DESC);
"#;

const SCHEMA_V6: &str = r#"
-- Subscription list prices, keyed (provider_id, tier). A separate plane from
-- the model catalog, sharing this store's clock and era machinery.
--
-- WHY NOT THE `era` TABLE: `era.model_id` is NOT NULL, so a tier row would need
-- a sentinel, and a sentinel in a key column is where ambiguity starts. Worse,
-- plan rows keyed into the model space would surface as pseudo-models on
-- `catalog.get` for every existing consumer, to serve one that reads them
-- rarely and off any hot path.
CREATE TABLE plan_price_era (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,

    -- A models.dev slug, the same vocabulary as `era.provider_id`. NOT the
    -- naming a quota source uses for the same vendor: those differ (`codex`
    -- against `openai`, `claude` against `anthropic`), and insula publishes a
    -- separate `apiProvider` field carrying these slugs precisely because of
    -- that gap.
    provider_id       TEXT    NOT NULL,

    -- The provider's own tier string, UNNORMALISED.
    --
    -- Never canonicalised, because the same string names plans an order of
    -- magnitude apart across vendors: `pro` is a low consumer tier at one and a
    -- top tier at another. Any normalisation step is a place those could be
    -- brought together, and that error is invisible downstream — it produces a
    -- healthy-looking multiplier that is wrong by 10x.
    tier              TEXT    NOT NULL,

    -- NULL means "this tier was observed and no price is published for it",
    -- which is a POSITIVE claim rather than a gap. `refusal_reason` says which
    -- kind, and the CHECK below makes the incoherent combinations
    -- unrepresentable.
    minor_units       INTEGER,
    exponent          INTEGER,
    currency          TEXT,

    -- Stated rather than assumed. A price without its period is not a price.
    period            TEXT    NOT NULL,

    -- Always `asserted` here, and the CHECK says so rather than leaving it to
    -- be inferred. Every row in this plane arrives with a date the vendor
    -- stated and no fetch behind it, so an `observed` row would be a category
    -- error rather than a data error.
    boundary_kind     TEXT    NOT NULL CHECK (boundary_kind = 'asserted'),

    -- The VENDOR'S effective date.
    --
    -- There is deliberately NO column for when this row was committed, and that
    -- absence is load-bearing: it makes a price recorded late indistinguishable
    -- from one recorded on time, so a backfilled reprice leaves no residue and
    -- every past window computes correctly the moment the row lands.
    --
    -- Adding a commit timestamp "for completeness" would destroy that property
    -- without looking like it changed anything.
    boundary_at_ms    INTEGER NOT NULL,

    -- Who sourced it and when they looked. From the first row, never
    -- retrofitted: the first ten rows are obviously true when written, which is
    -- exactly why the provenance never gets added later.
    established_by    TEXT    NOT NULL,
    established_at_ms INTEGER NOT NULL,

    -- Per ROW, never per file. A file-level date is bumped wholesale, so it
    -- goes stale as a unit and stops answering "when did anyone last look at
    -- THIS price" — the question that matters when one vendor reprices and the
    -- others do not.
    --
    -- This is a CEILING ON HOW LONG A WRONG VALUE MAY CIRCULATE, not a sampling
    -- rate. Nothing reveals a plan reprice: every other fact in this store has
    -- a fetch that would eventually contradict a stale value, and this one has
    -- the review and nothing else. The damage is retroactive, because every
    -- window in the stale period was priced against the wrong number.
    review_by_ms      INTEGER NOT NULL,

    -- The page the number came from, so a review is a click rather than a
    -- search.
    source_ref        TEXT    NOT NULL,

    refusal_reason    TEXT,

    -- A priced row carrying a refusal, and a refused row explaining nothing,
    -- are both things a careful writer would not produce and a careless one
    -- would. Stated as a constraint so they are unrepresentable rather than
    -- merely unwritten.
    CHECK ((minor_units IS NULL) = (refusal_reason IS NOT NULL)),

    -- A priced row needs all three money parts or none: a value without its
    -- exponent and currency is a number, not an amount.
    CHECK (
        (minor_units IS NULL AND exponent IS NULL AND currency IS NULL)
        OR (minor_units IS NOT NULL AND exponent IS NOT NULL AND currency IS NOT NULL)
    )
);

-- One era per (provider, tier) boundary. The same key twice at one instant is a
-- contradiction rather than a history.
CREATE UNIQUE INDEX plan_price_era_key
    ON plan_price_era(provider_id, tier, boundary_at_ms);
"#;
