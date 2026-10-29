# Standing rules for workers in fusiform

These apply to every delivery. They exist so a worker does not stop to ask about
routine steps; anything not listed under "pre-approved" still comes to the head.

## Build, test, gate

- Compile and test only on the remote Linux build server (`runon: "linux,16c"`
  for the whole suite, smaller for one crate). Never compile on the Mac.
- Before delivering, run `scripts/gate.sh`. It runs exactly what CI runs: a
  local phase in seconds, then clippy and the full test suite as one remote
  job. Quote its final line (`GATE_OK <rev>`) in the delivery. A delivery
  without it is not finished.
- A bug fix carries one test, shown once to fail without the fix. Say how you
  showed it.

## Pre-approved: do these without asking

- Regenerate the golden fixture (`UPDATE_GOLDEN=1` on the remote runner) when
  every changed line is explained by your change. State the explanation in the
  delivery, and stop and report any line you cannot explain.
- Bump `fusiform-protocol`'s minor version for a wire addition, together with
  the one matching version line in the root `Cargo.lock`. Nothing else in the
  lock changes.
- Add a test-only seam that mirrors an existing one, such as
  `canonical_links::plant`. Keep it out of production paths.
- Update the store write fence's expected count when you add a write site, and
  name the site.
- Update docs that describe the code you changed.

## Always ask first

- A store migration, or anything that changes what an existing stored or
  served value means.
- Removing or renaming a served fact, field or tool.
- Exempting, narrowing or loosening any fence: golden coverage, review-date
  gates, the write fence, the served-vocabulary checks.
- A curated value without a quoted first-party source.
- Any change under `.github/workflows/`.

## Text in the repository

- The repository is public. No agent seat names, task ids or local paths in
  committed text.
- Comments explain the reason for a reader with no context.
