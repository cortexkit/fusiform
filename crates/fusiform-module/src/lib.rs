#![forbid(unsafe_code)]

//! The ck-fusiform daemon's logic, as a library.
//!
//! The binary is a thin `main` over this crate rather than the other way
//! around, so integration tests link the same code the daemon runs. The
//! alternative — a binary-only crate with tests pulling modules in by path —
//! compiles every module twice, once per target, and each copy sees the other's
//! exports as unused.

pub mod creators;
pub mod fetch;
pub mod health;
pub mod loop_;
pub mod overlay;
pub mod route;
pub mod seed;
pub mod signals;

use subc_protocol::manifest::{
    Concurrency, ExecutionMode, ModuleManifest, ProviderRole, Tool, TrustTier,
};
use subc_protocol::PROTOCOL_VERSION;

/// The module id fusiform registers under.
pub const MODULE_ID: &str = "fusiform";

/// The module's manifest: identity, the tools it serves, and its bindings.
///
/// Lives in the library rather than the binary so a test can assert the
/// manifest and the dispatch table against each other. A tool declared here and
/// not dispatched is advertised and broken; a tool dispatched and not declared
/// is unreachable. Neither shows up as a failure without something holding both
/// lists at once.
pub fn manifest() -> ModuleManifest {
    // Builder rather than a struct literal: `ModuleManifest` is
    // `#[non_exhaustive]`, so a field added upstream no longer breaks every
    // construction site in the fleet.
    //
    // Each optional is bound to a local rather than written inline, so that the
    // reasoning above it stays attached to the value it explains. Those comments
    // are the record of decisions this seat was corrected into; a migration that
    // compiled while dropping them would look complete and would not be.

    // One declared behaviour: the catalog poll loop.
    //
    // `Some(vec![...])` rather than `None` because the list was examined
    // rather than skipped — the wire distinguishes un-adopted from
    // examined-and-none, and fusiform has exactly one thing to declare.
    //
    // `Literal` rather than `Derived` because the interval genuinely IS a
    // compile-time constant here: `POLL_INTERVAL` is not read from config
    // and not adjusted at runtime, so a literal is the truth rather than a
    // convenient stand-in. The store-driven part of the loop is which ETAG
    // it sends, not when it wakes.
    //
    // `Observe` is the whole point of this module — a poll that changed the
    // upstream would be a defect, not a feature.
    let self_signals = Some(vec![subc_protocol::manifest::SelfSignalDeclaration {
        name: "catalog_poll".to_string(),
        kind: subc_protocol::manifest::SelfSignalKind::Poller,
        effect: subc_protocol::manifest::SelfSignalEffect::Observe,
        anchored_to: subc_protocol::manifest::SignalAnchor::FixedInterval,
        cadence: Some(subc_protocol::manifest::SignalCadence::Literal {
            interval_ms: crate::loop_::POLL_INTERVAL_MS as u64,
        }),
        domain: Some("models.dev".to_string()),
        note: Some(
            "Conditional GET against the upstream catalog; a 304 writes no \
                 eras and a changed body writes only the facts that moved."
                .to_string(),
        ),
    }]);

    // Populated only from a value the packaging path injected, never from
    // whatever a compiler happened to see.
    //
    // `BUILD_REV` reads `CK_BUILD_REV` through `option_env!`, which captures
    // the environment of WHOEVER COMPILED THE CRATE — so reading it
    // unconditionally would mint a provenance claim out of an accident of
    // the build environment. That is the `version_line` defect generalized:
    // both binaries once reported the wire crate's version because a macro
    // evaluated at its definition site rather than its caller's.
    //
    // `release-build.sh` sets it and REFUSES A DIRTY TREE outright, so there
    // is no best-effort case here to stamp: a fusiform binary either carries
    // a revision its bytes can defend, or carries none. `unknown` is the
    // sentinel an ordinary `cargo build` leaves behind, and it maps to
    // absence rather than to a string that looks like an answer.
    let provenance = (fusiform_protocol::BUILD_REV != "unknown").then(|| {
        subc_protocol::manifest::ManifestProvenance {
            build_git_sha: Some(fusiform_protocol::BUILD_REV.to_string()),
            build_lock_digest: None,
            // Absent for the same definition-site reason: this crate can
            // only see its OWN CARGO_PKG_VERSION, and reporting that as the
            // wire crate's version is precisely the `version_line` defect.
            // The protocol crate would have to export its own constant, and
            // that is a wire change rather than a manifest one.
            wire_crate_version: None,
            store_schema_version: None,
        }
    });

    let provides = vec![ProviderRole::ToolProvider {
        tools: vec![
            Tool {
                name: route::TOOL_GET.to_string(),
                description: Some(
                    "Read the model catalog: what models exist, what they can do, what \
                         they cost. Optionally at a past instant."
                        .to_string(),
                ),
                execution_mode: ExecutionMode::Pure,
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string"},
                        "at_ms": {"type": "integer"},
                        "fact_prefixes": {"type": "array", "items": {"type": "string"}},
                        "include_retired": {"type": "boolean"},
                        "provider_id": {"type": "string"},
                        "model_id": {"type": "string"}
                    },
                    "additionalProperties": false
                }),
            },
            Tool {
                name: route::TOOL_HISTORY.to_string(),
                description: Some(
                    "Every recorded era for one fact, with the observation window each \
                         change was seen in."
                        .to_string(),
                ),
                execution_mode: ExecutionMode::Pure,
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string"},
                        "provider_id": {"type": "string"},
                        "model_id": {"type": "string"},
                        "fact_key": {"type": "string"}
                    },
                    "required": ["provider_id", "model_id", "fact_key"],
                    "additionalProperties": false
                }),
            },
            Tool {
                name: route::TOOL_STATUS.to_string(),
                description: Some(
                    "What fusiform has been doing: recent polls including the ones that \
                         changed nothing, catalog version, and row counts."
                        .to_string(),
                ),
                execution_mode: ExecutionMode::Pure,
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string"},
                        "polls": {"type": "integer"}
                    },
                    "additionalProperties": false
                }),
            },
            Tool {
                name: route::TOOL_CORRECT.to_string(),
                description: Some(
                    "Record that fusiform's own record was wrong over a past window. \
                         Marks the window so reads inside it refuse; never changes what the \
                         catalog currently says. Previews unless dry_run is false."
                        .to_string(),
                ),
                // The only tool here that writes. Declared honestly so the
                // daemon can fence it: calling it Pure would tell the
                // supervisor a write is safe to replay.
                execution_mode: ExecutionMode::Mutating,
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string"},
                        "provider_id": {"type": "string"},
                        "model_id": {"type": "string"},
                        "fields": {"type": "array", "items": {"type": "object"}},
                        "affected_from_ms": {"type": "integer"},
                        "affected_until_ms": {"type": "integer"},
                        "reason": {"type": "string"},
                        "dry_run": {"type": "boolean"}
                    },
                    // No wildcard form: a correction makes reads inside its
                    // window refuse, so one naming every model would be a
                    // catalog kill switch. Both ids are required.
                    "required": [
                        "provider_id", "model_id", "fields",
                        "affected_from_ms", "affected_until_ms", "reason"
                    ],
                    "additionalProperties": false
                }),
            },
            Tool {
                name: route::TOOL_MARK_ARTIFACT.to_string(),
                description: Some(
                    "Record that one poll's eras describe fusiform changing its own \
                         representation rather than the upstream changing its data, so \
                         last_changed_at stops reporting that poll as a change. Previews \
                         unless dry_run is false."
                        .to_string(),
                ),
                // The second writer. Declared Mutating for the same reason as
                // a correction: telling the supervisor a write is Pure would
                // make it safe to replay, and replaying a mark is how one poll
                // becomes several.
                execution_mode: ExecutionMode::Mutating,
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "observation_id": {"type": "integer"},
                        "reason": {"type": "string"},
                        "dry_run": {"type": "boolean"}
                    },
                    // One observation, named. No pattern form and no range: a
                    // mark excludes eras from every derivation that honours it
                    // and leaves nothing behind to argue with, so a rule that
                    // selects several polls is a rule that eventually selects
                    // the wrong one.
                    "required": ["observation_id", "reason"],
                    "additionalProperties": false
                }),
            },
            Tool {
                name: route::TOOL_RETRACT_ARTIFACT.to_string(),
                description: Some(
                    "Take back an artifact mark, so last_changed_at counts that \
                         poll's eras again. The mark stays on the record and a \
                         retraction follows it. Previews unless dry_run is false."
                        .to_string(),
                ),
                // The third writer, and Mutating for the same reason: a replayed
                // retraction would append a second withdrawal of a claim already
                // withdrawn.
                execution_mode: ExecutionMode::Mutating,
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "observation_id": {"type": "integer"},
                        "reason": {"type": "string"},
                        "dry_run": {"type": "boolean"}
                    },
                    "required": ["observation_id", "reason"],
                    "additionalProperties": false
                }),
            },
        ],
        // EMPTY, and that is a claim rather than an omission.
        //
        // Ruled by SUBC from BindIdentity's own doc: identity_scope is the set
        // of bind keys a provider PARTITIONS its answers or state by, not the
        // set it accepts — every bind carries all of them regardless. So an
        // empty vec says "the answer does not depend on the caller", which is
        // exactly true here and is the property this catalog exists to have.
        //
        // Two projects asking what models exist must get the same answer. The
        // catalog describes the world, not a workspace. Declaring [Project,
        // Session] said the opposite, in the same direction as the storage
        // binding deleted in dcdf804 — and for the same reason: a value chosen
        // to fill a field, before anyone had written down what the field meant.
        //
        // The doc that settles it is db9ff1af, written after this seat asked
        // rather than guessed.
        identity_scope: Vec::new(),
        concurrency: Concurrency::ModuleManaged,
        // False, and it was true here for eleven commits while nothing in
        // this repository ever called push. A manifest is a claim the
        // daemon and every operator reads; declaring a capability fusiform
        // does not exercise is the same defect as a comment describing code
        // that is not there, with a wider audience.
        //
        // Fusiform is pull-only. The transport now exists — subc-client-rs
        // 0.3.0 surfaces push frames to consumers — but fusiform emits
        // none, so this stays false until it does.
        emits_push: false,
        sub_supervises: true,
    }];

    // `consumes` is the builder's default. Fusiform requests no consumer roles,
    // and an explicit empty vec would assert nothing the default does not.
    ModuleManifest::builder(MODULE_ID.to_string(), env!("CARGO_PKG_VERSION").to_string())
        .protocol_ver(PROTOCOL_VERSION)
        // Declared because it is TRUE, not because a signature demanded it.
        //
        // Under 0.18 the compiler kept this field present. Under 0.19 it is an
        // optional setter, so nothing defends it but the test below — measured,
        // not assumed: deleting this line built clean and passed the whole
        // binary. An unread value that a reader is invited to tidy away is one
        // tidy from gone with every gate green.
        .trust_tier(Some(TrustTier::FirstParty))
        // BINDINGS DELETED, and this is the case 0.19 was made for.
        //
        // The block declared `StorageScope::Project`, and the comment beside it
        // admitted the problem: that is the enum's only variant, it decides
        // nothing (the daemon resolves every module to one database at
        // <data_home>/cortexkit/<module_id>/store.db under `isolation: module`
        // regardless), and it READS as though this catalog were partitioned per
        // project. For fusiform that reading is false — the catalog describes
        // the world, so two projects asking what models exist must get the same
        // answer.
        //
        // So the only value the type could express was one this module would
        // have to footnote to stop it being wrong. Absent is the honest form,
        // and a footnote is not a substitute for not making the claim.
        //
        // Nothing is lost operationally: the StorageDescriptor arrives from the
        // daemon in HELLO_ACK, constructed by its own policy, never derived
        // from anything a module claims here.
        // No capability claim until one has been through the owner round.
        //
        // The field is decode-optional and construct-required, so the honest
        // pre-review value is None rather than a plausible string. A capability is a
        // claim other modules route on, and a claim minted here to fill a field
        // would be indistinguishable from one that was reviewed.
        .capabilities(None)
        .self_signals(self_signals)
        .provenance(provenance)
        .provides(provides)
        .build()
}
