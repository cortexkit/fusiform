-- SQLite cannot widen a CHECK in place. Preserve the insertion ids, which
-- break ties in grade history, and copy the exact stored number tokens.
CREATE TABLE quality_grade_era_v15 (
    id             INTEGER PRIMARY KEY,
    model          TEXT NOT NULL,
    elo            TEXT,
    eq             TEXT,
    withdrawn      INTEGER NOT NULL CHECK (withdrawn IN (0, 1)),
    graded_by      TEXT NOT NULL,
    reason         TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    grade          TEXT,
    kind           TEXT CHECK (kind IS NULL OR kind IN ('transcription', 'speech', 'image', 'video', 'search', 'decision')),
    graded_effort  TEXT CHECK (graded_effort IS NULL OR (withdrawn = 0 AND kind IS NULL
        AND grade IS NULL AND elo IS NOT NULL AND eq IS NOT NULL AND graded_effort <> '')),
    CHECK ((withdrawn = 1 AND elo IS NULL AND eq IS NULL AND grade IS NULL)
        OR (withdrawn = 0 AND elo IS NOT NULL AND eq IS NOT NULL AND grade IS NULL AND kind IS NULL)
        OR (withdrawn = 0 AND elo IS NULL AND eq IS NULL AND grade IS NOT NULL AND kind IS NOT NULL))
) STRICT;
INSERT INTO quality_grade_era_v15 (id, model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms, grade, kind, graded_effort)
SELECT id, model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms, grade, kind, graded_effort FROM quality_grade_era;
DROP TABLE quality_grade_era;
ALTER TABLE quality_grade_era_v15 RENAME TO quality_grade_era;
CREATE INDEX quality_grade_era_key ON quality_grade_era(model, kind, id);

-- In store mode, operator grades are written to the local override table.
CREATE TABLE override_grade_era_v15 (
    id             INTEGER PRIMARY KEY,
    model          TEXT NOT NULL,
    elo            TEXT,
    eq             TEXT,
    withdrawn      INTEGER NOT NULL CHECK (withdrawn IN (0, 1)),
    graded_by      TEXT NOT NULL,
    reason         TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    grade          TEXT,
    kind           TEXT CHECK (kind IS NULL OR kind IN ('transcription', 'speech', 'image', 'video', 'search', 'decision')),
    graded_effort  TEXT CHECK (graded_effort <> ''),
    CHECK ((withdrawn = 1 AND elo IS NULL AND eq IS NULL AND grade IS NULL AND graded_effort IS NULL)
        OR (withdrawn = 0 AND elo IS NOT NULL AND eq IS NOT NULL AND grade IS NULL AND kind IS NULL)
        OR (withdrawn = 0 AND elo IS NULL AND eq IS NULL AND grade IS NOT NULL AND kind IS NOT NULL
            AND graded_effort IS NULL))
) STRICT;
INSERT INTO override_grade_era_v15 (id, model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms, grade, kind, graded_effort)
SELECT id, model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms, grade, kind, graded_effort FROM override_grade_era;
DROP TABLE override_grade_era;
ALTER TABLE override_grade_era_v15 RENAME TO override_grade_era;
CREATE INDEX override_grade_era_key ON override_grade_era(model, kind, recorded_at_ms, id);
