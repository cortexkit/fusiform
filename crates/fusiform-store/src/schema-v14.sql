ALTER TABLE quality_grade_era ADD COLUMN graded_effort TEXT
    CHECK (graded_effort IS NULL OR (withdrawn = 0 AND kind IS NULL
        AND grade IS NULL AND elo IS NOT NULL AND eq IS NOT NULL AND graded_effort <> ''));

ALTER TABLE plan_price_era ADD COLUMN quota_value_json TEXT CHECK (quota_value_json IS NULL OR json_valid(quota_value_json));
ALTER TABLE plan_price_era ADD COLUMN service_tiers_json TEXT CHECK (service_tiers_json IS NULL OR json_valid(service_tiers_json));
ALTER TABLE plan_price_era ADD COLUMN concurrency_json TEXT CHECK (concurrency_json IS NULL OR json_valid(concurrency_json));

CREATE TABLE effort_grade_era (
    id             INTEGER PRIMARY KEY,
    model          TEXT NOT NULL,
    effort         TEXT NOT NULL CHECK (effort <> ''),
    elo            TEXT,
    eq             TEXT,
    withdrawn      INTEGER NOT NULL CHECK (withdrawn IN (0, 1)),
    graded_by      TEXT NOT NULL,
    reason         TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    CHECK ((withdrawn = 1 AND elo IS NULL AND eq IS NULL)
        OR (withdrawn = 0 AND elo IS NOT NULL AND eq IS NOT NULL))
) STRICT;
CREATE INDEX effort_grade_era_key ON effort_grade_era(model, effort, recorded_at_ms, id);

CREATE TABLE workload_effort_era (
    id             INTEGER PRIMARY KEY,
    model          TEXT NOT NULL,
    workload       TEXT NOT NULL CHECK (workload IN ('reason', 'code', 'synthesize', 'lookup', 'quick')),
    effort_default TEXT CHECK (effort_default <> ''),
    effort_minimum TEXT CHECK (effort_minimum <> ''),
    withdrawn      INTEGER NOT NULL CHECK (withdrawn IN (0, 1)),
    set_by         TEXT NOT NULL,
    reason         TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    CHECK ((withdrawn = 1 AND effort_default IS NULL AND effort_minimum IS NULL)
        OR (withdrawn = 0 AND effort_default IS NOT NULL))
) STRICT;
CREATE INDEX workload_effort_era_key ON workload_effort_era(model, workload, recorded_at_ms, id);

CREATE TABLE override_grade_era (
    id             INTEGER PRIMARY KEY,
    model          TEXT NOT NULL,
    elo            TEXT,
    eq             TEXT,
    withdrawn      INTEGER NOT NULL CHECK (withdrawn IN (0, 1)),
    graded_by      TEXT NOT NULL,
    reason         TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    grade          TEXT,
    kind           TEXT CHECK (kind IS NULL OR kind IN ('transcription', 'speech', 'image', 'video', 'search')),
    graded_effort  TEXT CHECK (graded_effort <> ''),
    CHECK ((withdrawn = 1 AND elo IS NULL AND eq IS NULL AND grade IS NULL AND graded_effort IS NULL)
        OR (withdrawn = 0 AND elo IS NOT NULL AND eq IS NOT NULL AND grade IS NULL AND kind IS NULL)
        OR (withdrawn = 0 AND elo IS NULL AND eq IS NULL AND grade IS NOT NULL AND kind IS NOT NULL
            AND graded_effort IS NULL))
) STRICT;
CREATE INDEX override_grade_era_key ON override_grade_era(model, kind, recorded_at_ms, id);

CREATE TABLE override_effort_grade_era (
    id             INTEGER PRIMARY KEY,
    model          TEXT NOT NULL,
    effort         TEXT NOT NULL CHECK (effort <> ''),
    elo            TEXT,
    eq             TEXT,
    withdrawn      INTEGER NOT NULL CHECK (withdrawn IN (0, 1)),
    graded_by      TEXT NOT NULL,
    reason         TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    CHECK ((withdrawn = 1 AND elo IS NULL AND eq IS NULL)
        OR (withdrawn = 0 AND elo IS NOT NULL AND eq IS NOT NULL))
) STRICT;
CREATE INDEX override_effort_grade_era_key ON override_effort_grade_era(model, effort, recorded_at_ms, id);

CREATE TABLE override_workload_effort_era (
    id             INTEGER PRIMARY KEY,
    model          TEXT NOT NULL,
    workload       TEXT NOT NULL CHECK (workload IN ('reason', 'code', 'synthesize', 'lookup', 'quick')),
    effort_default TEXT CHECK (effort_default <> ''),
    effort_minimum TEXT CHECK (effort_minimum <> ''),
    withdrawn      INTEGER NOT NULL CHECK (withdrawn IN (0, 1)),
    set_by         TEXT NOT NULL,
    reason         TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    CHECK ((withdrawn = 1 AND effort_default IS NULL AND effort_minimum IS NULL)
        OR (withdrawn = 0 AND effort_default IS NOT NULL))
) STRICT;
CREATE INDEX override_workload_effort_era_key ON override_workload_effort_era(model, workload, recorded_at_ms, id);

CREATE TABLE override_mark_era (
    id             INTEGER PRIMARY KEY,
    model          TEXT NOT NULL,
    decommissioned INTEGER CHECK (decommissioned IN (0, 1)),
    marked_by      TEXT NOT NULL,
    reason         TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX override_mark_era_key ON override_mark_era(model, recorded_at_ms, id);

CREATE TABLE store_release (
    release        INTEGER PRIMARY KEY CHECK (release > 0),
    state_hash     TEXT NOT NULL CHECK (length(state_hash) = 64),
    curation_hash  TEXT NOT NULL CHECK (length(curation_hash) = 64),
    applied_at_ms  INTEGER NOT NULL
) STRICT;

CREATE TABLE store_document_era (
    id              INTEGER PRIMARY KEY,
    document        TEXT NOT NULL,
    document_json   TEXT CHECK (document_json IS NULL OR json_valid(document_json)),
    recorded_at_ms  INTEGER NOT NULL,
    CHECK (document NOT IN ('plan-prices', 'curated-models', 'fact-overrides', 'cache-lifetimes'))
) STRICT;
CREATE INDEX store_document_era_key ON store_document_era(document, recorded_at_ms, id);
