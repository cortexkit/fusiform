#![forbid(unsafe_code)]

//! Fusiform's served wire schema.
//!
//! These are the types a consumer compiles against to call `catalog.get`,
//! `catalog.history` and `catalog.status`. They are the contract; everything
//! else in fusiform is implementation.
//!
//! # Why this crate exists, and why it holds nothing else
//!
//! The fleet's cross-repo payload rule is that one definition is consumed by
//! both sides of a wire. SUBC ruled that a module-owned published crate is the
//! house pattern for that — `subc-protocol` is the precedent, owned by the
//! daemon's repo and compiled against by every client — because a served schema
//! is a DECLARATION authored by the producer. Producer ownership makes drift
//! unauthorable: the schema and the crate move in one commit.
//!
//! It depends on serde and nothing else, and that is a contract rather than a
//! coincidence. A consumer's condition for depending on a module-owned crate
//! was that it must not drag in the module's internals — a client, a runtime, a
//! database driver — because then the location stops mattering and the coupling
//! is real. `deps.rs` asserts the tree stays this shape.
//!
//! # What is NOT here
//!
//! The domain types. `SourceId`, `Timestamp`, `FactKey`, `Amount` and the rest
//! live in `fusiform-core` and `fusiform-store`, and a consumer never needs
//! them: every field below is a `String`, an `i64`, a `BTreeMap` or a
//! `serde_json::Value`. That is not a simplification for the wire — it is what
//! the wire always was, and moving these types here only made it visible.

use std::collections::BTreeMap;

pub mod money;
pub mod refusal;

pub use refusal::{
    RefusalKind, CODE_BAD_REQUEST, CODE_NO_COVERAGE, CODE_REFUSED, CODE_UNAVAILABLE,
};

pub use money::{
    Amount, CurrencyCode, Floor, InvalidCurrencyCode, PolicyId, RateValue, UnitProvenance,
    UnpricedReason, NANO_EXPONENT,
};

use serde::{Deserialize, Serialize};

/// The tools fusiform serves.
pub const TOOL_GET: &str = "catalog.get";
pub const TOOL_HISTORY: &str = "catalog.history";
pub const TOOL_STATUS: &str = "catalog.status";
/// The only tool that writes.
pub const TOOL_CORRECT: &str = "catalog.correct";

/// Every tool name, so a dispatch and a manifest cannot disagree about which
/// tools exist.
/// Record that a poll's eras describe fusiform changing its own representation
/// rather than the upstream changing its data.
///
/// An operator write, like [`TOOL_CORRECT`], and bounded the same way: one
/// named observation, previewed by default, no wildcard.
pub const TOOL_MARK_ARTIFACT: &str = "catalog.mark_artifact";

/// Take back a mark.
///
/// Separate from [`TOOL_MARK_ARTIFACT`] rather than a flag on it, because a
/// retraction is a different claim with a different reason, and a boolean would
/// let one be mistaken for the other in a call log.
pub const TOOL_RETRACT_ARTIFACT: &str = "catalog.retract_artifact";

pub const TOOLS: &[&str] = &[
    TOOL_GET,
    TOOL_HISTORY,
    TOOL_STATUS,
    TOOL_CORRECT,
    TOOL_MARK_ARTIFACT,
    TOOL_RETRACT_ARTIFACT,
];

/// Take back a mark on one poll.
///
/// The mark stays in the log and a retraction event follows it, so an operator
/// asking why `last_changed_at` moved sees that a claim was made and withdrawn
/// rather than finding nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetractArtifactRequest {
    pub observation_id: i64,
    /// Why the mark is being withdrawn. Required for the same reason the mark
    /// requires one: a retraction with no reason is an unexplained change to
    /// what every consumer of `last_changed_at` reads.
    pub reason: String,
    #[serde(default = "crate::default_true")]
    pub dry_run: bool,
}

/// What retracting would do, or did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetractArtifactResponse {
    pub observation_id: i64,
    pub committed: bool,
    /// Whether a live mark existed to take back.
    ///
    /// Reported rather than folded into `committed`, so a caller can tell "I
    /// withdrew it" from "there was nothing there". Both are successful calls
    /// and only one of them changed anything.
    pub had_live_mark: bool,
}

/// Mark one poll as a representation change of fusiform's own making.
///
/// # Why this exists
///
/// On 2026-08-16 a serialization change wrote 17,455 eras with identical claims
/// on both sides. Nothing distinguishes them from real price movements in the
/// era table, so `last_changed_at` reports that instant for every priced fact
/// in the catalog — and a consumer calibrating on it reads the whole catalog as
/// freshly maintained, which hides abandoned rows rather than exposing them.
///
/// The mark is how an operator says which poll that was. It is an operator
/// action rather than something fusiform infers, because a mistaken mark
/// deletes real history from every derivation that honours it and is
/// unfalsifiable afterwards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkArtifactRequest {
    /// The observation to mark. Named explicitly and never matched by pattern:
    /// ids are per-store, and a rule that selects several polls is a rule that
    /// eventually selects the wrong one.
    pub observation_id: i64,
    /// A pointer to the evidence, not a summary of it — a findings path, an
    /// incident record. An operator reading this later should meet the account
    /// rather than someone's recollection of it.
    pub reason: String,
    /// Preview unless explicitly false. Defaults to preview so an omitted flag
    /// cannot write.
    #[serde(default = "crate::default_true")]
    pub dry_run: bool,
}

/// What marking would do, or did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkArtifactResponse {
    pub observation_id: i64,
    pub committed: bool,
    /// Eras of this poll that RESTATE the claim already in force — the ones the
    /// mark would exclude.
    pub restating_eras: i64,
    /// Every era this poll wrote.
    ///
    /// Served beside `restating_eras` rather than as a ratio, so an operator
    /// sees the DIFFERENCE. On the 2026-08-16 poll it is 17,454 of 17,455: the
    /// one era that is not a restatement is a genuine upstream change that
    /// landed in the same tick, and it survives the mark. A single number would
    /// have hidden exactly the case that decided the design.
    pub total_eras: i64,
    /// The instant the poll was observed, so an operator can check the id names
    /// the poll they meant before committing.
    pub observed_at_ms: i64,
}

/// A `catalog.get` request.
///
/// Every field is optional with a stated default, so the empty object `{}` is a
/// valid request for the current catalog. That matters for an operator typing a
/// call by hand and for a consumer that wants everything.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogGetRequest {
    /// Which upstream to read. Defaults to `models.dev`, the only source today.
    ///
    /// Unknown fields are rejected by `deny_unknown_fields` above, and the
    /// source name is matched explicitly rather than parsed leniently. So a
    /// misspelled field or an unsupported source produces an error instead of a
    /// silently different answer.
    #[serde(default)]
    pub source: Option<String>,

    /// Resolve the catalog at this instant, in epoch milliseconds.
    ///
    /// Absent means now — and the response records which instant "now" resolved
    /// to, so the same read is repeatable.
    #[serde(default)]
    pub at_ms: Option<i64>,

    /// Only return facts whose key starts with one of these prefixes.
    ///
    /// Absent means every fact. Measured on a live 6,253-model document: the
    /// full fact set is 3.0 MB of JSON, the capability plane 0.64 MB, rates
    /// 1.55 MB.
    #[serde(default)]
    pub fact_prefixes: Option<Vec<String>>,

    /// Include models the catalog has recorded as retired.
    ///
    /// Defaults to false: a retired model is not part of "what models exist",
    /// and returning it by default would have every consumer write the same
    /// filter. An audit read asks for it.
    #[serde(default)]
    pub include_retired: bool,

    /// Return only this model. `provider_id` must accompany it.
    #[serde(default)]
    pub model_id: Option<String>,

    /// Return only models from this provider.
    #[serde(default)]
    pub provider_id: Option<String>,
}

/// A `catalog.get` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogGetResponse {
    pub source: String,
    /// The instant this snapshot resolves to, always concrete.
    ///
    /// # This is the field that tells a shrunken catalog from a historical one
    ///
    /// A response whose identity set is SMALLER than the one you hold has two
    /// causes, and they need opposite responses:
    ///
    /// - **You asked for the past.** `resolved_at_ms` is the instant you sent.
    ///   Fewer models is correct — models arrive daily, so any past instant has
    ///   fewer than now.
    /// - **Fusiform's store went backwards** (an engram restore returns it to an
    ///   older generation). `resolved_at_ms` is approximately NOW, and the
    ///   missing models are missing from the current catalog.
    ///
    /// `catalog_version` cannot tell you which. It is `max(now_ms, current + 1)`
    /// — derived so a restore cannot rewind it — so after a restore the version
    /// goes UP while the content goes BACK, and a point-in-time read carries the
    /// current high version with an older identity set. Both cases present as
    /// "version rose, identities shrank".
    ///
    /// **So a completeness check must read this field, not just the version.**
    /// A consumer that refuses on a shrunken set without checking whether it
    /// asked for the past will refuse its own historical query; one that refuses
    /// without noticing `resolved_at_ms ≈ now` cannot tell a restore from a
    /// routine read, and a refusal that never installs is a refusal that never
    /// clears.
    ///
    /// Recorded because the field has carried this discriminator since the first
    /// response and nothing said so — I described the two cases as
    /// indistinguishable to a consumer, having compared the versions and not the
    /// rest of the response. Detectability is not detection: a property nobody
    /// is told to check is a property of the source, not of the system.
    pub resolved_at_ms: i64,
    /// The monotonic catalog version at the time of the read.
    ///
    /// Carried on a pull as well as a push, so a consumer can tell whether a
    /// read it just made is newer than the last push it applied without
    /// correlating two different surfaces.
    ///
    /// # This is the ONLY field safe to hold as a high-water mark
    ///
    /// It is monotonic ACROSS RESTORES, which no other number here is. The
    /// value is derived as `max(now_ms, current + 1)`, so a whole-database
    /// restore that rewinds the stored counter cannot rewind the next value
    /// issued — wall-clock time does not go backwards when a file is replaced.
    /// Measured, not asserted: a restore rewound this from 9000 to 1000 and the
    /// next version issued was a current millisecond timestamp.
    ///
    /// Other numbers in these responses look like they could serve the same
    /// purpose and cannot. `era_count` rises monotonically in normal operation
    /// and is a plain row count, so a restore rewinds it to whatever the backup
    /// held; a consumer using it to detect change would see the catalog go
    /// backwards and then repeat values it had already processed. Same for
    /// `model_count`.
    ///
    /// The distinction is not visible in the payload — both are integers that
    /// only ever went up in every observation a consumer has made — which is
    /// why it is recorded here rather than left to be inferred.
    ///
    /// # NOT A SNAPSHOT IDENTIFIER
    ///
    /// This answers "has anything changed since I last looked". It does not
    /// answer "which catalog did I read", and a consumer that needs to REPLAY
    /// a past read — a routing decision reproduced against the catalog as it
    /// stood at decision time — must embed [`CatalogGetResponse::resolved_at_ms`]
    /// instead.
    ///
    /// The reason is that this field describes the store at READ time, not the
    /// snapshot resolved. Measured on the live module 2026-08-15: a read at
    /// `at_ms = 1786500000000` returned `resolved_at_ms` 1786500000000 and
    /// `catalog_version` 1786796896973 — today's watermark, on a response
    /// carrying values from three days earlier.
    ///
    /// So the round trip is `resolved_at_ms` out, `at_ms` back in:
    ///
    /// ```text
    /// decision time:  read catalog.get      -> record resolved_at_ms = T
    /// replay:         catalog.get {at_ms:T} -> the values in force at T
    /// ```
    ///
    /// Two caveats a replaying consumer needs, and neither is visible in the
    /// number itself:
    ///
    /// - A replayed read may carry `uncertain` entries. Fusiform polls on a
    ///   cadence, so an instant between two polls has a value that was in
    ///   force at the last observation and may have changed before the next
    ///   one. The bracket says which window is unobserved rather than
    ///   pretending the instant was measured.
    /// - A replayed read is NOT overridden. Serve-time corrections apply to
    ///   the current view only, so replaying a decision returns what the
    ///   upstream published, which is what the decision was actually made on.
    ///   The `overridden` array names any divergence on a current read.
    ///
    /// # Auditing a consumer for this: look at WRITES, not reads
    ///
    /// BROCA's, after checking their own uses and finding both correct. The
    /// misuse is invisible at the read site — every read of this field looks
    /// the same, and comparing two versions is the field's whole purpose. What
    /// distinguishes correct from broken is whether the value is PERSISTED and
    /// later treated as "the catalog we had".
    ///
    /// So the audit is: find every place the field is written down — a record,
    /// a ledger row, a decision log — and ask what reads it back. Comparison
    /// sites are fine by construction; storage sites are where the defect
    /// lives.
    ///
    /// Their own immunity turned out to be structural rather than careful, and
    /// the shape is worth knowing: BROCA forbids `at_ms` on refresh requests
    /// outright, with a test asserting the request shape omits it. That guard
    /// exists for an unrelated reason — a historical read would resolve an
    /// identity set they no longer serve — and it happens to make this hazard
    /// unconstructible, because every read is current and the watermark
    /// therefore advances monotonically with read time. A request-shape
    /// prohibition can confer immunity to hazards its author never considered,
    /// which is only findable by checking where the field is read rather than
    /// reasoning from the design.
    pub catalog_version: i64,
    /// Model identity to fact map. The identity is `provider_id/model_id`, the
    /// pair that makes a model unique — measured, 6,253 model rows carry only
    /// 2,957 distinct ids.
    ///
    /// The inner key is a FACT KEY, from the closed set in [`SERVED_FACTS`].
    /// Read that before deciding what to do with a value: it records which
    /// facts can change a consumer's request bytes, which are advisory, and
    /// which fusiform deliberately never serves.
    pub models: BTreeMap<String, BTreeMap<String, serde_json::Value>>,

    /// Facts this read withheld because their record is known bad.
    ///
    /// Omitted from the wire when empty, which is the ordinary case, so this
    /// costs nothing until it matters.
    ///
    /// A withheld fact is absent from `models`, and without this list that
    /// absence is indistinguishable from a fact the upstream never published.
    /// A consumer pricing a model would read "no rate" and could not tell it
    /// from "the rate we recorded is wrong" — the second demands a different
    /// action and is the one that costs money.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<WithheldFactWire>,

    /// Facts whose value is included but may already have been superseded at
    /// the read instant.
    ///
    /// **Always empty for a read of the current catalog**, so a consumer
    /// tracking today's models never sees this. It appears on historical reads,
    /// where the recorded value sits inside a later era's observation window —
    /// fusiform confirmed the value at `superseded_after`, saw it changed at
    /// `superseded_by`, and did not look in between.
    ///
    /// Distinct from `withheld` in the direction that matters: the value IS in
    /// `models`. This qualifies an answer rather than refusing one, because
    /// fusiform knows something about the instant and dropping the value would
    /// discard information it holds. A caller can act on a qualified answer and
    /// cannot act on a refusal.
    ///
    /// What to do with it depends on the width. At the normal 30-minute cadence
    /// the bracket is 30 minutes and most consumers will ignore it. A bracket of
    /// days means nobody looked for days, and a ledger repricing usage inside it
    /// is repricing against a value that may not have been in force.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncertain: Vec<UncertainFactWire>,
    /// Facts whose served value differs from what the upstream published.
    ///
    /// # The served value is fusiform's judgment, not the upstream's word
    ///
    /// models.dev publishes a 1,000,000-token context window for
    /// `anthropic/claude-sonnet-4-5`; Anthropic's own documentation names that
    /// model as its example of the 200k class. Fusiform serves 200,000, because
    /// a consumer trusting 1M sends five times the real ceiling into a hard 400
    /// — the Anthropic wall is prompt-only, so nothing truncates.
    ///
    /// Every entry names the upstream value, the served value, and the
    /// AUTHORITY behind the change. The authority is load-bearing: a divergence
    /// an operator cannot answer "says who" about makes distrusting the
    /// correction their cheapest move.
    ///
    /// **Always empty on a point-in-time read.** A correction records what a
    /// source says NOW and carries no validity interval, so applying it to a
    /// past instant would manufacture a claim about a window nobody observed.
    /// History serves what the upstream published, which is the true statement
    /// about the record.
    ///
    /// A consumer ignoring this list gets the safe value; one reading it can
    /// audit. Both are correct, and the default is the safe one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overridden: Vec<OverriddenFactWire>,
}

/// One fact whose served value differs from the upstream's.
///
/// # Not to be confused with `CorrectedFact`, which is a different claim
///
/// `CorrectedFact` says FUSIFORM'S OWN RECORD was wrong over a past window —
/// a defect in this module's reading, marked so historical queries refuse.
///
/// This says THE UPSTREAM'S PUBLISHED VALUE is wrong right now, and fusiform is
/// serving something else. The upstream's record is intact and history still
/// reports it; only the current served view differs.
///
/// The names had to diverge because a consumer meeting both in one crate would
/// reasonably assume they were related. They are not: one is an admission, the
/// other is an override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverriddenFactWire {
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: String,
    /// What the upstream published, verbatim, as JSON text.
    pub upstream_value: String,
    /// What fusiform serves instead.
    pub served_value: String,
    /// WHO says so, and specific enough to go and read.
    pub authority: String,
}

/// A fact whose recorded value at the read instant may already have ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UncertainFactWire {
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: String,
    /// The last instant fusiform confirmed this value.
    pub superseded_after_ms: i64,
    /// The instant fusiform observed it had changed.
    ///
    /// The change happened somewhere in `(superseded_after_ms,
    /// superseded_by_ms]`. Both ends are real observation instants rather than
    /// estimates, so the bracket is arithmetic a consumer can act on.
    pub superseded_by_ms: i64,
}

/// The commit this binary was built from, or `"unknown"`.
///
/// # Why a version number is not enough
///
/// `CARGO_PKG_VERSION` answers "is this fusiform" and never "which fusiform".
/// It has not moved in this project's lifetime, and the deploy ladder's other
/// identity — LC_UUID — is PATH-DEPENDENT: the same commit built in the main
/// tree and in a git worktree produces different UUIDs (measured by CKCRED,
/// 2026-08-12). LC_UUID proves two FILES match, which is what a placement
/// needs, and cannot name a commit, which is what an incident needs.
///
/// This gap was not hypothetical here. Fusiform ran for several hours on a
/// binary nine code commits behind the tree, and what exposed it was noticing
/// that health metrics lacked counters added since — not any identity probe,
/// because none of them could answer the question.
///
/// Read from the environment at compile time rather than from a `build.rs`
/// shelling out to git: a build script reading HEAD reruns on every commit and
/// rebuilds the crate graph behind it. `scripts/release-build.sh` sets
/// `CK_BUILD_REV`; an ordinary `cargo build` leaves it unset and gets
/// `"unknown"`, which is the honest answer — a dev build IS of unknown
/// provenance, and stamping a possibly-dirty tree's HEAD would assert
/// otherwise.
///
/// Convention adopted from CKCRED via SUBC.
///
/// # It names WHOEVER built this crate, not fusiform
///
/// `option_env!` reads the environment of the build that compiled THIS crate.
/// For fusiform's own binaries that is fusiform's release build, which is the
/// intent. For a consumer who compiles `fusiform-protocol` into their binary,
/// it is THEIR build — so this constant would report their rev under a name
/// that reads like fusiform's.
///
/// That is the same definition-site hazard that put the wire crate's version on
/// both binaries an hour ago, surviving in a second place because a constant
/// looks less like a call than a macro does. Verified rather than assumed:
/// cargo does re-track the variable, so `CK_BUILD_REV=AAAA` then `=BBBB` with
/// no source change produces two different binaries.
///
/// So a consumer must not read this as "which fusiform served me". The rev of
/// the module that answered a request is not available from a type a consumer
/// compiled themselves; it comes from `ck-fusiform --version` on the running
/// binary, or from the module's own report. This constant is honest only about
/// the build it is compiled into.
pub const BUILD_REV: &str = match option_env!("CK_BUILD_REV") {
    Some(rev) => rev,
    None => "unknown",
};

/// The version line both fusiform binaries print.
///
/// One function so the daemon and the CLI cannot drift into two spellings of
/// the same identity — a forensic comparing them would have to know which
/// format each used.
///
/// The caller passes its OWN version. The first draft of this called
/// `env!("CARGO_PKG_VERSION")` here, which expands at the DEFINITION site, so
/// both binaries reported the wire crate's version as their own — a correct
/// number in the wrong role, and the more confusing kind of wrong because it
/// moves and looks plausible.
///
/// Both numbers are printed, because they answer different questions. The
/// binary version says which build; the schema version says what a consumer
/// can expect on the wire, and that is the one a consumer's compatibility
/// question is actually about.
pub fn version_line(binary: &str, binary_version: &str) -> String {
    format!(
        "{binary} {binary_version} ({BUILD_REV}) schema {}",
        env!("CARGO_PKG_VERSION")
    )
}

/// Every fact fusiform serves, and what a consumer may do with it.
///
/// # Why this is in the wire crate rather than in fusiform's documentation
///
/// The set of served facts is checkable from the payload — a consumer can
/// enumerate the keys. What is NOT checkable from the payload is which of them
/// may change the bytes a consumer sends to a provider, and that distinction
/// decides whether a wrong value is a cosmetic defect or a silent behaviour
/// change.
///
/// It lived in fusiform's charter and in one conversation with the consumer who
/// named the three byte-affecting fields. Neither is reachable from the artifact
/// a consumer compiles against, which makes it a relationship recorded nowhere a
/// check can see — the failure mode ASTRO and I isolated on 2026-08-12, where
/// every available verification confirms the shapes and none can find the error,
/// because what is missing is not a wrong value but an unrecorded meaning.
///
/// # The classes
///
/// **Byte-affecting.** A consumer legitimately renders these into a request, so
/// a wrong value changes what goes on the wire to a provider with nothing
/// failing. BROCA named these from their own source: `limits.context` drives
/// transform pressure, `limits.output` is rendered as a request parameter, and
/// `capability.reasoning` gates their reasoning policy — a false value strips
/// thinking blocks and the model simply stops reasoning, silently.
///
/// **Money.** Rates price real usage. Absent, zero and unknown are three
/// distinct states here and must not be collapsed: an unpriced fact is a
/// refusal to state a rate, not a rate of zero.
///
/// **Advisory.** Descriptive. A consumer may display or filter on these; a
/// wrong value is visible rather than silent.
///
/// # The classes are not symmetrically evidenced, and the difference matters
///
/// `ByteAffecting` is a fact about a CONSUMER'S RENDERER, not about the value.
/// The three entries carrying it are there because BROCA read them out of their
/// own source and said so. Nothing in fusiform can verify or refute that, and
/// nothing in fusiform can discover a fourth.
///
/// So `Advisory` here means "no consumer has told fusiform this reaches their
/// wire" — not "fusiform has established that it does not". The two read
/// identically in this table and are very different claims.
///
/// **If you consume this catalog and render a field marked `Advisory` into a
/// provider request, that entry is wrong and fusiform cannot find out any other
/// way.** Say so; it will be reclassified and the class carries your
/// attribution.
///
/// That instruction has already worked in the other direction: `limit.output`
/// was `ByteAffecting` on my belief that a consumer rendered it as a request
/// parameter, until BROCA enumerated their render path and found zero reads of
/// it. A wrong classification is not always over-cautious.
///
/// # `null` means UNKNOWN, and coercing it to `false` is the failure this
/// distinction exists to prevent
///
/// A capability served as `null` means the upstream published no flag.
/// Fusiform types these `Option<bool>` deliberately: manufacturing `false` from
/// silence is a positive claim about a model, made from an absence of
/// information.
///
/// The hazard is measured rather than hypothetical. BROCA's parse reads
/// `entry.get(k).and_then(Value::as_bool).unwrap_or(false)`, and `as_bool()`
/// returns `None` on a JSON null — so absent, explicitly false, and null all
/// collapse to `false`, which routes a model to `ReasoningPolicy::None` and
/// strips thinking blocks from a model that supports them. Nothing fails.
///
/// **That parse is no longer on BROCA's serving path.** It reads their VENDORED
/// snapshot, which their catalog cutover took off the path entirely. The
/// runtime index now treats an absent key as a TYPED ERROR rather than a
/// default: `Some(Bool)` is known, `Some(Null)` is Unknown, absent fails the
/// whole catalog load. Fail-closed where the vendored path was fail-quiet.
///
/// # What this correction cost, and the rule it earned
///
/// This comment previously said the defect was "dormant only because models.dev
/// happens to publish the reasoning key for every model today; they pinned a
/// test that fails if a refresh ever lands one without it."
///
/// Every clause was true. The upstream does publish it — re-measured
/// 2026-08-13, 6,292 models, zero missing. The test exists and is
/// non-vacuous. **And it guards a road nobody drives anymore.** Nobody edited
/// the test, nobody edited the snapshot: A CUTOVER RETIRED A TRIPWIRE WITHOUT
/// TOUCHING IT, so the claim became IRRELEVANT rather than false.
///
/// A claim citing another system's code usually degrades loudly, because they
/// would notice editing it. This one degraded silently because the code stayed
/// exactly as true as ever and simply stopped being on the path. So checking
/// "is this about code or about data" is not enough — the third question is
/// **is the code it cites still on the path the data takes today**, which only
/// somebody who remembers the cutover can answer.
///
/// The rule survives the correction: **a consumer must carry unknown through to
/// the point that decides what to do about it, rather than resolving it at the
/// parse boundary where the only available default is a guess.** BROCA's
/// runtime index now does exactly that.
///
/// The three collapsed cases are not one defect with three inputs, and the
/// difference decides what a fix has to do:
///
/// | input | result | verdict |
/// |---|---|---|
/// | key absent | `false` | a default, defensible |
/// | key `false` | `false` | correct |
/// | key `null` | `false` | **a producer's deliberate uncertainty overwritten with a confident claim** |
///
/// Only the third has someone upstream doing work specifically to prevent it.
/// That is why the fix is not "handle null" but "carry unknown to the point
/// that can decide" — a parse-level default cannot know whether a missing flag
/// should mean no-reasoning or refuse-to-serve, and should not pretend to.
///
/// Fusiform emits `null` for BOTH an absent key and an explicit null, since
/// both spell "the upstream said nothing". Pinned by
/// `an_absent_capability_and_an_explicit_null_are_both_unknown`. Note the
/// contrast with the modality lists, where an absent block and an empty list
/// are DIFFERENT claims: `[]` states the model accepts no modality at all,
/// which is a positive assertion manufactured from silence.
///
/// # Limits: a published ZERO also arrives as `null`, and that is deliberate
///
/// `limit.context` and `limit.output` collapse an upstream zero into `null`
/// alongside an absent key. **A consumer cannot distinguish "the upstream
/// published 0" from "the upstream published nothing"**, and should not try:
/// no model accepts zero tokens, so a published zero is the upstream's spelling
/// of "not stated" rather than a capacity.
///
/// Per FIELD rather than per model. Measured 2026-08-11: 90 models mix real and
/// zero limits — `alibaba-token-plan/qwen-image-2.0` publishes an 8,192 context
/// limit beside a zero output cap, and a row-level rule would discard the real
/// one.
///
/// This is stated here because it is invisible from the wire and unrecoverable
/// downstream. A consumer fixing its own zero-versus-absent handling cannot get
/// the distinction back, because it was collapsed before the response was
/// built. Measured example: `privatemode-ai/whisper-large-v3` published
/// `context: 0` and was corrected to `448` on 2026-08-13 — during that window
/// fusiform served `null`, and "the upstream said zero" was not reconstructible.
///
/// If a consumer needs the distinction — for a drift alarm, or to catch an
/// upstream regression — it is a wire change to request, not a value to infer.
/// Fusiform will not emit a bare `0` and leave every consumer to decide what it
/// means, since that is the shape that produces absorbing-default defects.
///
/// # What fusiform never serves, and why it is the highest-stakes entry here
///
/// Renderer-selection fields — `provider.npm`, per-model provider overrides,
/// and `experimental` — are parsed, flagged, and never emitted. Fusiform says
/// WHAT exists; it never says HOW to speak to it.
/// `crates/fusiform-store/tests/served_vocabulary.rs` fails if one appears.
///
/// **This is not merely an omission, and BROCA supplied the reason from their
/// own render path.** The field the rule excludes is `provider.wire_family`,
/// which SELECTS THE RENDERER — so it does not affect one field of a request,
/// it decides every byte of it. Every other entry in this table is a value a
/// consumer may render; that one would be fusiform choosing how a consumer
/// speaks. It stays hand-tabled on the consumer's side permanently.
///
/// So the most consequential classification in this table is about a field that
/// is not in it.
pub const SERVED_FACTS: &[ServedFact] = &[
    ServedFact {
        key: "existence",
        class: FactClass::Advisory,
        note: "present, absent, or retired. Absence is a withdrawal, not a gap.",
    },
    ServedFact {
        key: "limit.context",
        class: FactClass::ByteAffecting,
        note: "NEVER rendered into a request. It is the pressure signal for a \
               consumer's prompt-reduction transform, so a wrong value changes \
               what gets compacted, which changes the prompt, which changes \
               every byte. Indirect and total — a reader who greps for it in a \
               request body will not find it and may wrongly reclassify it. \
               NULL means the limit is UNKNOWN and must not be defaulted to a \
               number: a consumer that reads null as zero applies maximum \
               compaction pressure to a model whose capacity it does not know.",
    },
    ServedFact {
        key: "limit.output",
        class: FactClass::Advisory,
        note: "the model's maximum output. NOT the request's output cap: BROCA \
               takes that from the caller's max_tokens, never from a catalog. \
               Reclassified 2026-08-12 after they enumerated their render path \
               and found zero reads of it.",
    },
    ServedFact {
        key: "capability.reasoning",
        class: FactClass::ByteAffecting,
        note: "gates a reasoning policy; a wrong false silently strips thinking \
               blocks and a capable model stops reasoning with nothing failing. \
               null means UNKNOWN and a consumer must not coerce it to false — \
               see the note on FactClass.",
    },
    ServedFact {
        key: "capability.tool_call",
        class: FactClass::Advisory,
        note: "whether the model accepts tool definitions",
    },
    ServedFact {
        key: "capability.attachment",
        class: FactClass::Advisory,
        note: "whether the model accepts attachments",
    },
    ServedFact {
        key: "capability.input_modalities",
        class: FactClass::Advisory,
        note: "null when unpublished, which is not the same as an empty list",
    },
    ServedFact {
        key: "capability.output_modalities",
        class: FactClass::Advisory,
        note: "null when unpublished, which is not the same as an empty list",
    },
    ServedFact {
        key: "model.family",
        class: FactClass::Advisory,
        note: "the weights family the upstream names, identical across every \
               provider serving the same weights. A JOIN KEY rather than a \
               description: it is what relates one provider's row to another's, \
               so a wrong value relates a row to the wrong siblings. null when \
               unpublished",
    },
    ServedFact {
        key: "model.open_weights",
        class: FactClass::Advisory,
        note: "whether the weights are published. Marks the population where \
               one provider's price says something about another's row, since \
               a closed model served by two providers is two distinct \
               offerings. null when unpublished, which is not false",
    },
    ServedFact {
        key: "rate.input",
        class: FactClass::Money,
        note: "per million input tokens",
    },
    ServedFact {
        key: "rate.output",
        class: FactClass::Money,
        note: "per million output tokens",
    },
    ServedFact {
        key: "rate.cache_read",
        class: FactClass::Money,
        note: "per million cached-read tokens",
    },
    ServedFact {
        key: "rate.cache_write",
        class: FactClass::Money,
        note: "per million cache-write tokens",
    },
    ServedFact {
        key: "rate.reasoning",
        class: FactClass::Money,
        note: "per million reasoning tokens",
    },
];

/// One served fact and what a consumer may do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServedFact {
    /// The exact key as it appears in [`CatalogGetResponse::models`].
    ///
    /// A tiered rate appends `.above_context.<threshold>` — the threshold comes
    /// from the upstream and is not fusiform's to enumerate, so tiered keys are
    /// matched by prefix rather than listed.
    ///
    /// # What a tiered key states, and what it does NOT
    ///
    /// STATED: the threshold is a CONTEXT size. The upstream tags each tier
    /// with its own discriminator and every one of 370 tier rows measured
    /// 2026-08-15 carries `type: "context"`; normalization refuses any other
    /// type rather than assuming. So `rate.input.above_context.200000` means
    /// "the input rate that applies when context exceeds 200,000", not "the
    /// rate for input tokens beyond the 200,000th".
    ///
    /// NOT STATED: whether crossing the threshold reprices the WHOLE request
    /// or only the excess. The upstream publishes a complete rate set per tier
    /// — input, output, cache_read together — rather than a marginal
    /// increment, which is suggestive and is not evidence. Nothing in the
    /// catalog says which scheme a provider bills under, and fusiform will not
    /// infer one from the shape of a JSON object.
    ///
    /// THE CONSEQUENCE, because this is a money contract and the two readings
    /// are far apart. A 250,000-token request against a model whose tier rate
    /// is 2x the base:
    ///
    /// ```text
    /// whole-request repricing:  250k @ 2x  = 500k base-units
    /// marginal:                 200k @ 1x + 50k @ 2x = 300k base-units
    /// ```
    ///
    /// A 40% difference on the same request and the same catalog. A consumer
    /// pricing usage must establish the scheme per provider from the
    /// provider's own published terms; this catalog carries the threshold and
    /// the rates, and deliberately does not carry a claim it cannot source.
    ///
    /// Anthropic publishes prompt-size bands ("prompts <= 200K" versus
    /// "prompts > 200K"), which is whole-request repricing for THAT provider.
    /// Generalising it to the other tiered providers would be the same error
    /// as letting one vendor's documentation adjudicate another's serving
    /// path — the bill belongs to whoever charges it.
    pub key: &'static str,
    pub class: FactClass,
    /// What the value means, in one line, for a consumer deciding what to do
    /// with it.
    pub note: &'static str,
}

/// What a wrong value in this fact would do to a consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactClass {
    /// A consumer may render this into a provider request. A wrong value
    /// changes the bytes sent, with nothing failing.
    ByteAffecting,
    /// A rate. Prices real usage; absent, zero and unknown are distinct.
    Money,
    /// Descriptive. A wrong value is visible rather than silent.
    Advisory,
}

/// A fact a read refused to answer, on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WithheldFactWire {
    /// `provider_id/model_id`, the same identity spelling `models` uses.
    pub model: String,
    pub fact_key: String,
    /// Every correction covering the read instant, oldest first.
    pub corrections: Vec<CorrectionDetail>,
}

/// What a served tool call produced.
///
/// One enum rather than three entry points, so the daemon handler dispatches
/// once and every tool shares the same envelope unwrap and the same error
/// mapping. A second handler path is a second place for those to drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ToolResponse {
    Catalog(CatalogGetResponse),
    History(HistoryResponse),
    Status(StatusResponse),
    Correct(CorrectResponse),
    MarkArtifact(MarkArtifactResponse),
    RetractArtifact(RetractArtifactResponse),
}

/// A `catalog.history` request: every era for one fact.
///
/// The fact is named explicitly rather than defaulting to "all facts for this
/// model", because a model's full history is thousands of rows and an operator
/// asking a question has one fact in mind.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRequest {
    #[serde(default)]
    pub source: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    /// A key in the served vocabulary: `rate.input`, `limit.context`,
    /// `capability.reasoning`, `existence`.
    pub fact_key: String,
}

/// What a correction says, as an operator reads it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectionDetail {
    /// The interval whose recorded values are known bad. Closed at both ends.
    pub affected_from_ms: i64,
    pub affected_until_ms: i64,
    /// Why the record was wrong, in fusiform's own words.
    pub reason: String,
    /// The `FieldId` list the correction names, as stored.
    pub fields: serde_json::Value,
}

/// One era, as an operator reads it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEra {
    pub value: serde_json::Value,
    pub boundary_at_ms: i64,
    /// observed / asserted / seed / corrected.
    pub boundary_kind: String,
    /// The extent and reason, present only on a `corrected` boundary.
    ///
    /// Without this an operator sees `kind: "corrected"` and cannot tell which
    /// interval was affected or why — the cause recorded in the store with no
    /// path to the surface anyone reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correction: Option<CorrectionDetail>,
    /// The other edge of the observation window.
    ///
    /// Present only on an observed boundary. Its absence is the honest answer
    /// for a seed or a correction: neither is an observation of an upstream
    /// change, so neither has a window to report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_from_ms: Option<i64>,
}

/// A `catalog.history` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryResponse {
    pub source: String,
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: String,
    /// Oldest first: a history is read forwards.
    pub eras: Vec<HistoryEra>,
    /// When this fact's value last GENUINELY changed, in epoch milliseconds.
    ///
    /// # Why a consumer cannot compute this from `eras`
    ///
    /// The newest era's boundary is not the answer. Some eras record fusiform
    /// changing its own representation rather than the upstream changing its
    /// data — on 2026-08-16 one poll wrote 17,455 such eras with identical
    /// values on both sides. Derived naively, every priced fact in the catalog
    /// reports that instant, so a consumer calibrating on it reads the WHOLE
    /// CATALOG as freshly maintained. That hides abandoned rows rather than
    /// exposing them, which is the opposite of what the field is for.
    ///
    /// Which polls those were is not visible in this list and is not something
    /// a consumer could be expected to know, so the exclusion is applied here,
    /// per era, before the answer is served.
    ///
    /// # What it does NOT mean
    ///
    /// Not "when fusiform last looked" — see the observation windows on each
    /// era for that. Not a staleness verdict either: 116 providers in this
    /// catalog have never changed a rate, so an old instant is the normal state
    /// for most of it and means only what it says.
    ///
    /// `None` when the fact has no era at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_changed_at_ms: Option<i64>,
    /// The override in force on this fact right now, when there is one.
    ///
    /// # Why history carries this and does NOT apply it
    ///
    /// The eras below are what the upstream published, and they stay that way:
    /// an override is a serve-time judgment about the CURRENT view, not a
    /// correction to fusiform's record of what was said. Rewriting them would
    /// forge the record — the thing this store exists to prevent.
    ///
    /// But an operator reading history for `limit.context` on a model whose
    /// value is overridden was seeing the upstream's number with nothing to
    /// reconcile it against what `catalog.get` serves. Two surfaces
    /// disagreeing, both correct, with no cross-reference — which reads as one
    /// of them being wrong.
    ///
    /// Absent when no override applies, which is the ordinary case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden: Option<OverriddenFactWire>,
}

/// A `catalog.status` request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    #[serde(default)]
    pub source: Option<String>,
    /// How many recent polls to include. Defaults to 10.
    #[serde(default)]
    pub polls: Option<u32>,
}

/// One recorded poll.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusPoll {
    pub observed_at_ms: i64,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,

    /// What this poll changed, omitted when it changed nothing.
    ///
    /// A 304, an unchanged document and a failure all write no eras, so the
    /// absence is the ordinary case and costs nothing on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<PollChanges>,
}

/// What one poll changed.
///
/// # Why an era count alone is not enough
///
/// Measured over 11 hours of live polling: 72% of era churn was models ARRIVING
/// AND LEAVING rather than facts changing, because an arriving model writes one
/// era per fact it has. One real poll wrote 96 eras of which 19 were genuine
/// changes; another wrote 61 for 15.
///
/// So an era count misleads in a consistent direction, and misleads most on the
/// busiest polls — exactly the ones an operator looks at. A model arriving is
/// the upstream publishing something new; a fact changing on a known model is
/// the upstream REVISING something, and only the second is what a consumer's
/// cache or a ledger acts on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollChanges {
    /// Total eras written. Kept because it is what the store actually did, and
    /// a reader comparing it against the parts can see the arithmetic.
    pub eras: i64,
    pub models_arrived: i64,
    pub models_withdrawn: i64,
    /// Facts that moved on a model already known.
    pub facts_changed: i64,
}

/// A `catalog.status` response: what fusiform has been doing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusResponse {
    pub source: String,
    pub catalog_version: i64,
    pub model_count: usize,
    /// How many polls have EVER failed, and when the last one was.
    ///
    /// # Why a total, when the recent polls are right there
    ///
    /// The poll list is windowed — ten by default. The live store's only failed
    /// poll sits about forty polls back, so an operator asking "what has
    /// fusiform been doing" sees ten clean rows and no reason to look further.
    /// It is reachable with `--polls 60`, and nothing tells them to pass it.
    ///
    /// The health surface carries the same pair, and this is not redundancy:
    /// health answers "is this module OK" for a supervisor, status answers
    /// "what has this module been doing" for a person. A history visible only
    /// in the first is one most readers never see.
    ///
    /// `None` when nothing has ever failed. Omitted from the wire when absent,
    /// so a module too old to report it degrades to silence rather than to a
    /// false clean bill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failures: Option<FailureHistoryWire>,
    /// Facts fusiform is currently serving against what the upstream publishes.
    ///
    /// # Why this belongs on the status surface and not only on a read
    ///
    /// `catalog.get` reports the overrides that apply to the rows it returned.
    /// An operator asking "what is fusiform doing" is asking a different
    /// question, and the answer includes "deliberately disagreeing with the
    /// upstream about two facts" — which they would otherwise learn only by
    /// happening to read one of those two models.
    ///
    /// Empty in the ordinary case, and omitted from the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overridden: Vec<OverriddenFactWire>,
    /// How many of those models carry at least one rate.
    ///
    /// Measured on the live store: 420 of 6,293 present models carry no rate at
    /// all, because the upstream publishes no cost object for them. That is 6.7%
    /// of the catalog, and the model total cannot express it — an operator
    /// reading "6,293 models" has no way to know a fifteenth of them cannot be
    /// priced.
    ///
    /// Not a defect being reported. A model with no cost object correctly stores
    /// no rate rows, and a consumer asking for one gets no coverage rather than
    /// a fabricated zero. This number exists so the coverage is visible without
    /// querying for it, because absent and free are different states and only
    /// one of them is safe to bill.
    ///
    /// Optional on the wire: a module predating this field sends none, and the
    /// deployment asymmetry makes that a certainty rather than a risk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_priced: Option<usize>,
    pub era_count: i64,
    /// The most recent polls, newest first.
    ///
    /// Failures and 304s included. An operator asking what fusiform has been
    /// doing needs the polls that changed nothing most of all: a source
    /// returning 304 for a week and a source failing for a week are
    /// indistinguishable from the catalog alone.
    pub recent_polls: Vec<StatusPoll>,
}

impl CatalogGetResponse {
    pub fn model_count(&self) -> usize {
        self.models.len()
    }

    pub fn fact_count(&self) -> usize {
        self.models.values().map(|f| f.len()).sum()
    }
}

/// Failed polls across the store's whole history.
///
/// Read from the observation table rather than from a process counter, so it
/// survives both a recovery and a restart. A process-scoped total would reset
/// at exactly the moment someone is looking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailureHistoryWire {
    /// How many polls have failed, ever. Never zero when this is present.
    pub ever: i64,
    /// When the most recent failure was.
    pub last_at_ms: i64,
    /// How many polls a caller must ask for to reach that failure.
    ///
    /// Derived per response rather than fixed, because the answer changes with
    /// every poll. The operator hint used to be a hardcoded `--polls 60`,
    /// which was true when it was written and false by 2026-08-17: the
    /// recorded failure was 196 polls back, so anyone following the hint ran a
    /// costly query AND did not see the thing it pointed at.
    ///
    /// `None` from a module that predates this field, which a renderer must
    /// treat as "no number to offer" rather than as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polls_back: Option<i64>,
    /// What kind the most recent failure was: `network`, `http_status`,
    /// `parse`, or `implausible`.
    ///
    /// # Why this is here as well as on health
    ///
    /// Health's `last_failure_class` is stamped by the current failure streak
    /// and clears with it, which is correct for "what is failing now" and null
    /// for a failure that healed. Measured on the live module the day the
    /// durable count shipped: `failures_ever: 1, last_failure_age_ms:
    /// 84628131, last_failure_class: null` — something went wrong, 23 hours
    /// ago, kind unknown.
    ///
    /// The class is what decides where an operator looks. `network` sends them
    /// to the upstream, `parse` to the payload, `implausible` to the shrink
    /// guard. A count and an age without it says something went wrong and not
    /// what.
    ///
    /// `None` when the stored word is one this build does not recognise, which
    /// reports no class rather than guessing one — a wrong cause sends an
    /// operator somewhere specific and wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_class: Option<String>,
}

/// A `catalog.correct` request: record that fusiform's own record was wrong.
///
/// # Why this carries no value, and no wildcard
///
/// The operator supplies a DIAGNOSIS — which model, which facts, what window,
/// and why. They supply no value: `affected_until` is when the fix deployed and
/// fusiform stopped recording the bad value, so a later poll has already
/// written the right one. A correction marks a window; it never edits a value.
///
/// And it names exactly one model. There is no "all models" or "every fact"
/// form, deliberately: a correction makes reads inside its window refuse, so a
/// wildcard correction is a catalog kill switch. Fusiform cannot tell who is
/// calling — `RequestCtx` carries no consumer identity — so the blast radius is
/// bounded by what the request can express rather than by who may send it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CorrectRequest {
    #[serde(default)]
    pub source: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    /// The `FieldId` values this correction names, as the served vocabulary
    /// spells them: `{"field":"rate","class":"input"}`, `{"field":"limit",
    /// "limit":"context"}`, `{"field":"existence"}`.
    pub fields: Vec<serde_json::Value>,
    /// Start of the bad window. A LOWER BOUND: when the true start is not known
    /// precisely it goes to the earliest plausible instant, never the best
    /// guess. An over-inclusive partition costs review time; an under-inclusive
    /// one leaves bad facts outside a partition asserting they are fine.
    pub affected_from_ms: i64,
    /// End of the bad window: when the fix deployed and fusiform stopped
    /// recording the bad value.
    pub affected_until_ms: i64,
    /// Why the record was wrong. Names a finding document, so an audit is
    /// repeatable rather than dependent on prose.
    pub reason: String,
    /// Resolve and report without writing.
    ///
    /// Defaults to true. A write that happens because a flag was forgotten is
    /// the wrong default for the only command in this module that changes what
    /// the catalog says about the past.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

fn default_true() -> bool {
    true
}

/// What a correction did, or would do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectResponse {
    /// False when this was a dry run.
    pub written: bool,
    /// One entry per fact, each carrying the value that stays in force.
    pub facts: Vec<CorrectedFact>,
    pub affected_from_ms: i64,
    pub affected_until_ms: i64,
    pub reason: String,
}

/// One fact a correction marked, or would mark.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectedFact {
    pub fact_key: String,
    /// The value that remains in force, read from the store rather than
    /// supplied. Shown so an operator can see what the catalog will still say.
    pub value: serde_json::Value,
    /// When the era carrying that value was established.
    pub current_since_ms: i64,
}
