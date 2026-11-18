ALTER TABLE quality_grade_era ADD COLUMN graded_effort TEXT
    CHECK (graded_effort IS NULL OR (withdrawn = 0 AND kind IS NULL
        AND grade IS NULL AND elo IS NOT NULL AND eq IS NOT NULL AND graded_effort <> ''));

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
