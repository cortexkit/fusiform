# fusiform

The CortexKit AI-model capability catalog: one structured data plane describing
what AI models exist, what they can do, and what they cost — across every
modality (LLM, image generation, video generation, audio, embeddings), across
every provider.

Named for the fusiform gyrus: the brain's recognition-and-categorization
region, the part that knows what things are and what their properties are.

Runs as `ck-fusiform`, a supervised module of the subc daemon. See
`docs/charter.md` for scope, constraints, and the fleet context.

## Building

**This repo does not build from a clone of itself alone.** It takes four
CortexKit siblings as path dependencies, expected as directories beside it:

```
cortexkit/
  fusiform/      <- this repo
  subconscious/  <- subc-client-rs, subc-protocol, subc-transport, subc-core
  commons/       <- cortexkit-paths, cortexkit-store, cortexkit-store-types
  engram/        <- engram-core (dev-dependency, enrollment test only)
```

Cargo loads every workspace manifest before honouring `-p` or `--exclude`, so
one missing sibling gates everything: no partial build, no single-crate build,
no test run. Measured from a fresh clone with no siblings — `cargo build -p
fusiform-cli` and `cargo build --workspace --exclude fusiform-module` both
fail at manifest load, before any compilation.

**The failures come in a ladder, and only the second rung is a wall:**

| # | missing dep | repo | |
|---|---|---|---|
| 1 | `cortexkit-store` | commons | **public** — clone it and you pass |
| 2 | `subc-client-rs` | subconscious | **private**, and a *regular* dependency |
| 3 | `engram-core` | engram | **private**, but only a *dev*-dependency |

Worth knowing before you start: the first error names commons, which anyone
can fix, so the first rung gives no hint that a permission wall waits behind
it. Cargo reports a directory it could not find, so *"you cannot see this
repository"* arrives spelled as *"this path is wrong"*.

Issue #1 was filed against rung 3 for that reason — the dev-dependency is the
one that looks unusual, and removing it would move the failure to rung 2 rather
than clearing it.

`engram-core` is a **dev**-dependency and powers one test
(`crates/fusiform-module/tests/enrollment.rs`), which validates the backup
enrollment descriptor against engram's own parser rather than a hand-written
schema copy. The other three are regular dependencies and are not optional.

Scripts: `scripts/release-build.sh` builds with the commit stamped in and runs
anywhere. `scripts/stage.sh` signs with an Apple Developer identity and is
macOS-only; it refuses immediately elsewhere rather than failing partway.
