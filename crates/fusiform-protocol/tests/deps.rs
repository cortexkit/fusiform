//! The dependency tree is part of the contract.
//!
//! A consumer's condition for depending on a module-owned crate was that it must
//! not drag in the module's internals. The exact objection, worth keeping in the
//! file that enforces it: "if the replacement drags in a client, a tokio
//! runtime, or a database driver, then depending on it does couple me to your
//! internals regardless of where it lives — and I would refuse that."
//!
//! That consumer has already declined a crate on this ground, mirroring three
//! types by hand rather than take a dependency that pulled in cryptographic and
//! network libraries for three struct definitions. So this is a stated
//! requirement with precedent, not a style preference.
//!
//! The failure this prevents is gradual and invisible from inside. Nobody adds
//! tokio to a schema crate deliberately; it arrives because someone needed one
//! helper from a crate that happens to pull it, the build still works, and the
//! coupling is only discovered by the consumer who then refuses the dependency.

use std::process::Command;

/// Every crate this one pulls in, transitively.
fn dependency_tree() -> String {
    let out = Command::new("cargo")
        .args([
            "tree",
            "-p",
            "fusiform-protocol",
            "--edges",
            "normal",
            "--prefix",
            "none",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree runs");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("cargo tree emits utf8")
}

/// No runtime, no client, no database, no fusiform internals.
///
/// Named crates rather than a count, so the failure message says WHAT arrived
/// rather than that a number moved.
#[test]
fn the_wire_crate_stays_free_of_the_modules_internals() {
    let tree = dependency_tree();

    // Each entry is a crate whose presence would defeat the crate's purpose,
    // with the reason it would.
    let forbidden: &[(&str, &str)] = &[
        (
            "tokio",
            "an async runtime: a consumer would inherit fusiform's execution model",
        ),
        (
            "reqwest",
            "an HTTP client: the fetch layer is fusiform's, not a consumer's",
        ),
        (
            "rusqlite",
            "a database driver: the store is fusiform's internal representation",
        ),
        ("libsqlite3-sys", "SQLite itself, via a driver"),
        ("cortexkit-store", "fusiform's managed storage layer"),
        (
            "subc-client-rs",
            "the subc SDK: a consumer has its own transport",
        ),
        ("subc-core", "the subc daemon"),
        (
            "fusiform-core",
            "fusiform's domain types, which are not the contract",
        ),
        (
            "fusiform-store",
            "fusiform's store, which is not the contract",
        ),
        ("ring", "a cryptography library"),
        ("rustls", "a TLS stack"),
    ];

    let mut found = Vec::new();
    for (crate_name, why) in forbidden {
        // Match at a line start with a following space, so `serde` does not
        // match `serde_json` and a substring cannot produce a false positive.
        let present = tree
            .lines()
            .any(|l| l.split_whitespace().next() == Some(crate_name));
        if present {
            found.push(format!("  {crate_name} — {why}"));
        }
    }

    assert!(
        found.is_empty(),
        "the served-schema crate pulled in the module's internals:\n{}\n\n\
         full tree:\n{tree}",
        found.join("\n")
    );
}

/// The whole tree is small enough to read.
///
/// The named-crate test above catches the specific hazards a consumer objected
/// to. This one catches the general case: something arrives that nobody thought
/// to forbid. It fails on ANY growth, so the number is a tripwire rather than a
/// budget — a genuine new dependency updates it deliberately.
#[test]
fn the_dependency_tree_is_small_enough_to_read() {
    let tree = dependency_tree();
    let crates: Vec<&str> = tree
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|c| !c.is_empty())
        .collect();

    // serde, serde_json, and their proc-macro and numeric support: itoa,
    // memchr, proc-macro2, quote, serde_core, serde_derive, syn,
    // unicode-ident, zmij. Twelve distinct crates including this one, on 17
    // lines because `cargo tree` repeats a crate under each parent.
    //
    // The number is measured, and the first version of this test proves why
    // that matters: it said 10 and called itself "measured rather than
    // estimated". It was neither — it was a guess wearing a measurement's
    // words, in the comment of a test written to keep this crate's
    // dependencies honest. Run `cargo tree -p fusiform-protocol --edges normal
    // --prefix none` to update it.
    const EXPECTED_LINES: usize = 17;
    assert!(
        crates.len() <= EXPECTED_LINES,
        "the served-schema crate now reports {} tree lines, was {EXPECTED_LINES}:\n{tree}",
        crates.len()
    );

    // The distinct count is the number that means something; the line count
    // above is what `cargo tree` happens to print.
    let mut distinct: Vec<&str> = crates.clone();
    distinct.sort_unstable();
    distinct.dedup();
    const EXPECTED_DISTINCT: usize = 12;
    assert!(
        distinct.len() <= EXPECTED_DISTINCT,
        "the served-schema crate now pulls {} distinct crates, was {EXPECTED_DISTINCT}: {distinct:?}",
        distinct.len()
    );
}

/// Every wire type round-trips through JSON.
///
/// The contract is what crosses the wire, so a type that cannot survive a
/// round trip is broken regardless of whether it compiles. This is cheap and
/// catches a `#[serde(skip)]` added to a field a consumer needs.
#[test]
fn every_response_type_round_trips() {
    use fusiform_protocol::*;
    use std::collections::BTreeMap;

    let catalog = CatalogGetResponse {
        source: "models.dev".into(),
        resolved_at_ms: 1_786_488_349_024,
        catalog_version: 1_786_488_349_024,
        models: BTreeMap::from([(
            "anthropic/claude-sonnet-4-5".to_string(),
            BTreeMap::from([(
                "rate.input".to_string(),
                serde_json::json!({"state":"priced","currency":"USD","exponent":9,"units":3000000000i64}),
            )]),
        )]),
        withheld: vec![WithheldFactWire {
            model: "anthropic/claude-sonnet-4-5".into(),
            fact_key: "rate.output".into(),
            corrections: vec![CorrectionDetail {
                affected_from_ms: 1_000,
                affected_until_ms: 2_000,
                reason: "normalizer scaled the rate wrong".into(),
                fields: serde_json::json!([{"field":"rate","class":"output"}]),
            }],
        }],
        overridden: vec![fusiform_protocol::OverriddenFactWire {
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: "limit.context".into(),
            upstream_value: "1000000".into(),
            served_value: "200000".into(),
            authority: "https://docs.claude.com/...".into(),
        }],
        uncertain: vec![fusiform_protocol::UncertainFactWire {
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: "rate.cache_read".into(),
            superseded_after_ms: 1_786_400_000_000,
            superseded_by_ms: 1_786_488_349_024,
        }],
    };
    let text = serde_json::to_string(&catalog).unwrap();
    let back: CatalogGetResponse = serde_json::from_str(&text).unwrap();
    assert_eq!(back, catalog, "catalog.get response must round-trip");

    // The withheld list must survive, since a consumer reading a missing fact
    // depends on it to tell "unpublished" from "known bad".
    assert!(
        text.contains("withheld") && text.contains("normalizer scaled"),
        "the withheld list must reach the wire: {text}"
    );

    let history = HistoryResponse {
        source: "models.dev".into(),
        provider_id: "anthropic".into(),
        model_id: "claude-sonnet-4-5".into(),
        fact_key: "rate.input".into(),
        eras: vec![HistoryEra {
            value: serde_json::json!({"units": 3_000_000_000i64}),
            boundary_at_ms: 5_000,
            boundary_kind: "observed".into(),
            correction: None,
            window_from_ms: Some(1_000),
        }],
        overridden: None,
    };
    let back: HistoryResponse =
        serde_json::from_str(&serde_json::to_string(&history).unwrap()).unwrap();
    assert_eq!(back, history);

    let status = StatusResponse {
        failures: Some(fusiform_protocol::FailureHistoryWire {
            ever: 2,
            last_at_ms: 1_786_500_000_000,
            last_class: Some("network".into()),
        }),
        overridden: vec![],
        source: "models.dev".into(),
        catalog_version: 7,
        model_count: 6_270,
        models_priced: Some(5_850),
        era_count: 67_914,
        recent_polls: vec![StatusPoll {
            observed_at_ms: 5_000,
            outcome: "changed".into(),
            failure_class: None,
            detail: None,
            duration_ms: Some(291),
            changes: Some(fusiform_protocol::PollChanges {
                eras: 7,
                models_arrived: 2,
                models_withdrawn: 1,
                facts_changed: 1,
            }),
        }],
    };
    let back: StatusResponse =
        serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
    assert_eq!(back, status);
}

/// A status response from an older module still parses.
///
/// # Why this is a structural guarantee rather than a courtesy
///
/// The CLI and the module are deployed by DIFFERENT PARTIES — `ck-models` into
/// `~/.local/bin` is mine, `ck-fusiform` into the fleet bin dir is SUBC's — so
/// they are never placed simultaneously. A version skew window is not a risk to
/// be managed, it is a certainty of the deployment shape, and it is open for as
/// long as it takes a message to cross a seam.
///
/// Observed rather than theorised: a `ck-models` at schema 0.5.0 ran against a
/// module at 0.4.1 for the whole interval between staging and placement, and
/// rendered changed polls with no composition and no error.
///
/// So every field added to a served response must be optional, and this is the
/// test that says so. If `changes` were required, the operator's only tool
/// would fail to parse every status response from an older module — during
/// exactly the window where someone is most likely to be checking on it.
#[test]
fn a_status_response_without_the_newest_fields_still_parses() {
    // Exactly what a module predating `PollChanges` emits.
    let older = r#"{
        "source": "models.dev",
        "catalog_version": 7,
        "model_count": 6270,
        "era_count": 67914,
        "recent_polls": [
            {"observed_at_ms": 5000, "outcome": "changed", "duration_ms": 291}
        ]
    }"#;

    let parsed: fusiform_protocol::StatusResponse =
        serde_json::from_str(older).expect("a response from an older module must parse");

    assert_eq!(parsed.recent_polls.len(), 1);
    assert!(
        parsed.recent_polls[0].changes.is_none(),
        "an older module reports no composition, which is absence rather than zero"
    );
}

/// An omitted `dry_run` means PREVIEW, not write.
///
/// The field's doc comment says "Defaults to true" and names the reason: a
/// write that happens because a flag was forgotten is the wrong default for
/// the only operation that changes what the catalog says about the past.
///
/// That was prose about a serde attribute, and prose is not a fence. A request
/// missing the field — an older client, a hand-written body, a proxy that
/// drops unknown keys — would silently become a WRITE if the attribute were
/// ever dropped, and `#[serde(default)]` on a bool means `false`, so the
/// failure is one deleted line away and lands in the destructive direction.
#[test]
fn an_omitted_dry_run_previews_rather_than_writes() {
    let without: fusiform_protocol::CorrectRequest = serde_json::from_str(
        r#"{"provider_id":"anthropic","model_id":"claude-sonnet-4-5",
             "fields":["limit.context"],"affected_from_ms":1,
             "affected_until_ms":2,"reason":"docs/findings/x.md"}"#,
    )
    .expect("a request without dry_run must still parse");
    assert!(
        without.dry_run,
        "an omitted dry_run must preview. A forgotten flag becoming a write is \
         the wrong direction to fail for the only command that rewrites the \
         past."
    );

    // And the explicit values must survive, or the default masks a wire bug.
    let explicit_write: fusiform_protocol::CorrectRequest = serde_json::from_str(
        r#"{"provider_id":"a","model_id":"m","fields":["limit.context"],
             "affected_from_ms":1,"affected_until_ms":2,"reason":"r",
             "dry_run":false}"#,
    )
    .unwrap();
    assert!(
        !explicit_write.dry_run,
        "control: an explicit false must reach the module, or nothing could \
         ever be committed and the default above would look correct anyway"
    );
}
