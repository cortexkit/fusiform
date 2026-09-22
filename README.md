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

**One sibling is a permission wall. It used to be two.**

| # | missing dep | repo | visibility |
|---|---|---|---|
| 1 | `cortexkit-store` | commons | **public** — clone it and you pass |
| 2 | `subc-client-rs` | subconscious | **public** — clone it and you pass |
| 3 | `engram-core` | engram | **private**, and a *dev*-dependency |

Measured 2026-09-22 with `gh api repos/cortexkit/<name> --jq .visibility`,
because this table was wrong for weeks and nothing here could have caught it.

Until some point after 2026-08-14, subconscious was private, and this section
said so — it called rung 2 "the hard wall" and rung 3 merely the one that
*looks* unusual. That reversed after subconscious went public, and a reader
following rung 2's advice would have gone looking for permission they already
had. The repository state changed under a recorded measurement, which is the
failure mode a dated claim has and an undated one hides.

So issue #1's disposition no longer holds either. It was declined on the
grounds that removing the dev-dependency would move the failure to rung 2
rather than clear it. Rung 2 is gone, so removing rung 3 now clears the ladder,
and issues #6 and #7 re-raise it correctly against the changed state.

`engram-core` is a **dev**-dependency and powers one test file
(`crates/fusiform-module/tests/enrollment.rs`, 5 tests), which validates the
backup enrollment descriptor against engram's own parser rather than a
hand-written schema copy. The other three are regular dependencies.

**A dev-dependency still gates a release build**, which is the part that
surprises people: Cargo resolves the whole workspace graph before honouring a
target, so a missing dev-dep manifest fails `cargo build --release -p
fusiform-module` and `cargo metadata`, not only `cargo test`.

And it cannot be feature-gated away. Both obvious shapes were measured here on
2026-09-22 and both fail:

```
optional dev-dependency    cargo refuses the manifest outright:
                           "dev-dependencies are not allowed to be optional"
optional PATH dependency   still loads the source with the feature OFF:
                           "failed to load source for dependency"
```

The only shape that keeps a path out of the graph is keeping the crate that
declares it out of the workspace (`[workspace] exclude`), which resolves
cleanly — also measured. Noted so the next person to propose a feature flag
reads the refusal rather than discovering it.

Scripts: `scripts/release-build.sh` builds with the commit stamped in and runs
anywhere. `scripts/stage.sh` signs with an Apple Developer identity and is
macOS-only; it refuses immediately elsewhere rather than failing partway.
