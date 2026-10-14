# Mutation proofs

A guard is a check (a constraint, a refusal, a test-enforced rule) that keeps
some property true. It is only worth something if its test fails when the
rule it guards is broken. `mutations.toml` at the repository root records that proof for each
standing guard, and `ckdev-mutate` replays it.

## 1. The catalogue

Each `[[control]]` row breaks one rule and names the tests that must go red:

| Field | Meaning |
| --- | --- |
| `id` | a stable name for the row |
| `guards` | the rule, in one sentence |
| `file` | the source file to mutate |
| `old`, `new` | exact text to replace; `old` must match exactly once in `file` |
| `edits` | additional edits for a rule that needs more than one; empty in every current row |
| `test_file` | where the guarding tests live |
| `runner`, `package`, `target` | how to run them, for example `cargo`, `fusiform-store`, `--test it` |
| `expect_red` | the tests that must fail, by full libtest path |
| `only`, `select` | which tests the replay runs; see `ckdev-mutate prove --help` |
| `timeout_s`, `build_timeout_s` | limits for the test run and the build |
| `hub`, `hub_targets` | optional: the row's mutation legitimately reddens other test targets too, with the reason and the targets named |
| `equivalent`, `equivalent_guard` | optional: the mutant cannot change behaviour, with the reason and the code that makes it so |

Test paths follow the crate layout. Integration tests build into one binary per
crate, so a test in `crates/fusiform-store/tests/it/serve.rs` is
`serve::<name>` under `--test it`. A unit test in
`crates/fusiform-module/src/route.rs` is `route::<module>::<name>` in the
library target.

## 2. Commands

`ckdev-mutate` is the `cortexkit-mutate` crate from the CortexKit commons
repository. CI installs it at a pinned commit (see `.github/workflows/ci.yml`):

```sh
cargo install --locked --git https://github.com/cortexkit/commons \
  --rev <commit pinned in ci.yml> cortexkit-mutate
```

| Command | What it does |
| --- | --- |
| `ckdev-mutate check` | validates every row's anchor, files and test names without editing anything |
| `ckdev-mutate run --only <id>` | replays one row |
| `ckdev-mutate run --diff <base>` | replays the rows whose files changed since `<base>` |
| `ckdev-mutate run --all` | replays every row |
| `ckdev-mutate run --all --broad` | replays every row against every test target of its package |
| `ckdev-mutate prove …` | proves a new row and appends it only when the named tests go red |
| `ckdev-mutate explore …` | runs a whole package against one mutant and lists every red test |

`check` is part of the local gate (see `CONTRIBUTING.md`). An edit to guarded
code can make a row's `old` text stop matching; `check` then fails by row id
until the row is updated.

## 3. In CI

The `mutations` job in `.github/workflows/ci.yml` runs `ckdev-mutate check`
and then:

- on a push to `master`: every row;
- on pull requests and pushes to `train/**`: the rows the change touches,
  against the merge base;
- on the nightly schedule: every row with `--broad`. A catch outside the
  expected target is graded `CAUGHT_BROADLY` unless the row is marked `hub`,
  which declares that the mutated code is shared and is expected to redden
  other targets too.

The run's report is uploaded as the `mutations` artifact.

## 4. Adding a row

A new guard gets a row, not a proof written into a commit message. Prove it and
let the runner append the row:

```sh
ckdev-mutate prove --id point-in-time-boundary-inclusive-outer \
  --guards "a point-in-time read AT a boundary instant serves that era" \
  --file crates/fusiform-store/src/serve.rs \
  --old 'AND e.boundary_at_ms <= ?2' --new 'AND e.boundary_at_ms < ?2' \
  --test-file crates/fusiform-store/tests/it/serve.rs \
  --package fusiform-store --target='--test it' \
  --expect-red serve::a_read_at_a_boundary_instant_returns_the_new_value
```

If the guard sits in a function a caller must reach, add a second row that
removes the call.

When nobody knows yet which test should catch a mutant, use `explore`. It runs
the whole package (or the workspace with `--workspace`), lists every test that
went red or explains a survivor, and `--append` writes the row on a catch:

```sh
ckdev-mutate explore --package fusiform-module \
  --file crates/fusiform-module/src/fetch.rs \
  --old 'blake3::hash(bytes)' --new 'blake3::hash(&[])'
```

## 5. What a passing replay means

A row that replays green proves that its named tests fail when that exact text
changes. It does not prove the tests catch every way the rule could break, and
it says nothing about rules that have no row.
