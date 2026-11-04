# Standing rules for workers in fusiform

These apply to every delivery. They exist so a worker does not stop to ask about
routine steps; anything not listed under "pre-approved" still comes to the head.

## Build, test, gate

- Compile and test only on the remote Linux build server. Request the CPUs
  the job uses: `linux,2c` for one test binary or a filtered run, `linux,4c`
  for clippy on the workspace. A job holds every CPU it requests for its whole
  run, so a larger request only makes other jobs wait. Never compile on the Mac.
- Run only filtered tests: the tests for the files and behaviour you changed
  (for example `cargo test --locked -p fusiform-module --test it fact_overrides`),
  plus `cargo fmt --check` and `cargo clippy --locked --workspace --all-targets
  -- -D warnings`. Quote each command and its exit code in the delivery.
- Never run the whole workspace suite or `scripts/gate.sh`. The head runs the
  full gate once, over merged deliveries.
- A failure that looks unrelated to your change: rerun that one test by name on
  the base commit, never the suite, and report both results.
- A filtered run that takes longer than about 15 minutes means the filter is
  too broad; narrow it rather than waiting on it. Keep the default build
  directory; a separate `CARGO_TARGET_DIR` forces a cold build every time.
- A refusal that says nothing ran is safe to retry straight away, at a
  smaller CPU size if capacity was the reason.
- A bug fix carries one test, shown once to fail without the fix. Say how you
  showed it.

## Pre-approved: do these without asking

- Regenerate the golden fixture (`UPDATE_GOLDEN=1` on the remote runner, with
  `return_paths: ["crates/fusiform-module/fixtures/served-payloads.json"]`,
  because remote Linux jobs can't write tracked files otherwise and the
  rewrite would be lost) when every changed line is explained by your change. State the explanation in the
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
