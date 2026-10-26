# Contributing

## The gate

A change must pass all of these, run from the repository root. CI runs the
same commands (`.github/workflows/ci.yml`).

```sh
cargo fmt --all -- --check
scripts/no-outside-path-deps.sh
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
scripts/check-wire-crate-version.sh origin/master
```

- `cargo fmt --check` prints nothing on success.
- `scripts/no-outside-path-deps.sh` refuses a path dependency that points
  outside the repository; the repository must build from its own checkout.
- `--locked` makes cargo refuse rather than silently rewrite `Cargo.lock`. A
  dependency change is its own commit with its own `cargo update`.
- `scripts/check-wire-crate-version.sh` fails when `fusiform-protocol`'s source
  changed in anything other than comments and blank lines since the base ref
  and its version did not move. It compares commits, so commit before running
  it; it warns when the crate has uncommitted changes.

`cargo c` and `cargo t` are shorter aliases for the clippy and test commands.
`.githooks/pre-push` refuses a push whose committed `Cargo.lock` differs from
the working tree's, whose formatting fails, or whose lock does not resolve.

Some tests fail on a date rather than on a change: review dates on curated data
and the age of the embedded seed. A failure that begins "NOT A BUILD FAILURE"
is one of these; see [`docs/curated-data.md`](docs/curated-data.md) §6.

## Commits

- One logical change per commit, with a message that says what changed and
  why. A dependency or lock update goes in its own commit.
- If a change makes an existing test fail, do not rewrite the test to match
  without saying why in the commit message: the test is a claim that a
  property matters.
- A bug fix comes with a test that fails without the fix; show that once,
  then keep the test. A check that can only pass proves nothing.
- A change to `fusiform-protocol` that alters what it compiles to bumps its
  version in `crates/fusiform-protocol/Cargo.toml`.

## Where code goes

| Change | Crate or file |
| --- | --- |
| a new upstream field, a parsing rule, money arithmetic | `fusiform-core` |
| a table, a migration, a write path, a read query | `fusiform-store` (migrations in `src/schema.rs`, append-only) |
| a request or response field, a fact key, a refusal code | `fusiform-protocol` |
| a tool, a serve-time rule, the poll loop, health | `fusiform-module` |
| curated creators, aliases, model rows, billing planes, plan prices | `crates/fusiform-module/data/*.json` |
| an operator command | `fusiform-cli` |
| a helper shared by tests | `fusiform-testkit` (dev-dependency only) |

Integration tests live in each crate's `tests/it/` directory and build into one
binary per crate (`--test it`).

When a change moves a fact a document states, update the document in the same
commit. Two documents are held against the code by tests: the table list in
`docs/data-model.md` and the tool list in `docs/module-surface.md`.
