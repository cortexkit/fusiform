#[path = "consumer.rs"]
mod consumer;

use std::collections::BTreeMap;

use fusiform_core::ChargeUnitEvidence;
use fusiform_publish::{
    FactIndex, SnapshotInputs, publish_step, snapshot_bytes, snapshot_bytes_from_state,
};
use fusiform_release::{
    ChangeOp, HistoryAttestationBody, HistoryFile, Release, SignatureError, SigningKey, Snapshot,
    State, Upstream, build_release, canonical_json, history_day_bytes, sha256, sign_envelope,
    sign_history_attestation, verify_envelope, verify_history_attestation,
};
use serde_json::{Value, json};

const DAY: &str = "2026-11-19";
const START_MS: i64 = 1795046400000;
const MODELS: &[u8] = include_bytes!("../../../fusiform-publish-wasm/fixtures/models-dev.json");
const CURATION: &str = include_str!("../../../fusiform-publish-wasm/fixtures/curation.json");
const KID: &str = "release-format-v1-test-key";
const PUBLIC_KEY: &[u8] = br#"{"kid":"release-format-v1-test-key","public_key":"GX9rI-FshTLGq8g4-s1ep4m-DHaykgM0A5v6iz02jWE"}"#;

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

fn wire(value: impl serde::Serialize) -> Vec<u8> {
    canonical_json(&serde_json::to_value(value).unwrap())
}

fn key() -> SigningKey {
    SigningKey::from_bytes(&[42; 32])
}

fn signed(unsigned: &[u8], key: &SigningKey) -> Release {
    let mut object = parse(unsigned);
    let envelope = serde_json::from_value(object["envelope"].clone()).unwrap();
    object["signatures"] = json!(sign_envelope(&envelope, KID, key).unwrap());
    serde_json::from_value(object).unwrap()
}

fn models_with_rename() -> Vec<u8> {
    let mut models = parse(MODELS);
    models["zeta"]["models"]["mini"]["name"] = json!("Mini 2");
    wire(models)
}

fn summary(models: &[u8], previous: &FactIndex, observed_at: i64) -> (Vec<u8>, FactIndex) {
    let step = publish_step(
        models,
        previous,
        &ChargeUnitEvidence::default(),
        observed_at,
    )
    .unwrap();
    assert!(step.conflicts.is_empty());
    (
        summary_bytes(previous, &step.index, &step.ops, step.next_date_boundary_ms),
        step.index,
    )
}

fn summary_bytes(
    previous: &FactIndex,
    index: &FactIndex,
    ops: &[ChangeOp],
    next_date_boundary_ms: Option<i64>,
) -> Vec<u8> {
    wire(json!({
        "base_upstream_hash": previous.upstream_hash(),
        "conflicts": [],
        "model_count": index.model_count(),
        "next_date_boundary_ms": next_date_boundary_ms,
        "ops": ops,
        "upstream_hash": index.upstream_hash(),
    }))
}

fn two_releases() -> (Release, Release, FactIndex) {
    let index = FactIndex::from_snapshot_upstream(&BTreeMap::new()).unwrap();
    let (genesis, index) = summary(MODELS, &index, START_MS - 2);
    let r1 = build_release(
        &genesis,
        CURATION.as_bytes(),
        b"",
        1,
        START_MS - 1,
        START_MS - 2,
    )
    .unwrap();
    let r1 = signed(&r1, &key());
    let (changed, index) = summary(&models_with_rename(), &index, START_MS - 1);
    let r2 = build_release(
        &changed,
        CURATION.as_bytes(),
        &wire(&r1.envelope),
        2,
        START_MS,
        START_MS - 1,
    )
    .unwrap();
    (r1, signed(&r2, &key()), index)
}

fn third_release(r2: &Release, index: &FactIndex) -> Release {
    let (unchanged, _) = summary(&models_with_rename(), index, START_MS + 86_399_998);
    let curation = CURATION.replace("\"version\":1", "\"version\":2");
    let r3 = build_release(
        &unchanged,
        curation.as_bytes(),
        &wire(&r2.envelope),
        3,
        START_MS + 86_399_999,
        START_MS + 86_399_998,
    )
    .unwrap();
    signed(&r3, &key())
}

fn jsonl(releases: &[&Release]) -> Vec<u8> {
    let mut lines = Vec::new();
    for release in releases {
        lines.extend(wire(release));
        lines.push(b'\n');
    }
    lines
}

struct SnapshotDocs {
    envelope: Vec<u8>,
    signatures: Vec<u8>,
    history: Vec<u8>,
    history_signatures: Vec<u8>,
}

impl SnapshotDocs {
    fn new(release: &Release, history: Vec<HistoryFile>) -> Self {
        let body = HistoryAttestationBody {
            release: release.envelope.release.try_into().unwrap(),
            state_hash: release.envelope.state_hash,
            history,
        };
        Self {
            envelope: wire(&release.envelope),
            signatures: wire(&release.signatures),
            history: wire(&body.history),
            history_signatures: wire(sign_history_attestation(&body, KID, &key())),
        }
    }

    fn inputs<'a>(
        &'a self,
        models_dev: &'a [u8],
        evidence: &'a ChargeUnitEvidence,
    ) -> SnapshotInputs<'a> {
        SnapshotInputs {
            models_dev,
            evidence,
            envelope: &self.envelope,
            curation_set_raw: CURATION.as_bytes(),
            signatures: &self.signatures,
            history_files: &self.history,
            history_signatures: &self.history_signatures,
            release_public_key: PUBLIC_KEY,
        }
    }
}

pub fn fixtures() -> BTreeMap<&'static str, Vec<u8>> {
    // The release-format vectors describe a historical catalog that predates
    // the mandatory v2 model type. Start from those facts so a parser change
    // cannot rewrite the release format's compatibility record.
    let empty = FactIndex::from_snapshot_upstream(&Upstream::new()).unwrap();
    let sparse = |context: Value| -> fusiform_release::Facts {
        wire_facts(json!({
            "capability.attachment": null, "capability.input_modalities": null,
            "capability.output_modalities": null, "capability.reasoning": null,
            "capability.reasoning_options": null, "capability.tool_call": null,
            "existence": "present", "limit.context": context, "limit.output": null,
            "model.canonical_id": null, "model.family": null, "model.open_weights": null,
            "model.status": null,
        }))
    };
    let mut alpha = sparse(Value::Null);
    alpha.extend(wire_facts(json!({
        "provider.name": "Fixture Labs",
        "rate.input": {"currency":"USD", "exponent":9, "state":"priced", "units":310000000},
    })));
    let mut upstream: Upstream = serde_json::from_value(json!({
        "alpha": {"models": {"-": alpha}, "provider": {}},
        "zeta": {"models": {"mini": sparse(json!(8192))}, "provider": {}},
    }))
    .unwrap();
    let genesis_ops: Vec<_> = upstream
        .iter()
        .flat_map(|(provider, facts)| {
            facts.models.iter().flat_map(move |(model, facts)| {
                facts.iter().map(move |(fact, value)| ChangeOp::Set {
                    provider: provider.clone(),
                    model: model.clone(),
                    fact: fact.clone(),
                    value: value.clone(),
                })
            })
        })
        .collect();
    let first = FactIndex::from_snapshot_upstream(&upstream).unwrap();
    let genesis = summary_bytes(&empty, &first, &genesis_ops, None);
    let r1 = signed(
        &build_release(
            &genesis,
            CURATION.as_bytes(),
            b"",
            1,
            START_MS - 1,
            START_MS - 2,
        )
        .unwrap(),
        &key(),
    );
    upstream
        .get_mut("zeta")
        .unwrap()
        .models
        .get_mut("mini")
        .unwrap()
        .insert("model.name".into(), json!("Mini 2"));
    let second = FactIndex::from_snapshot_upstream(&upstream).unwrap();
    let renamed = summary_bytes(&first, &second, &[rename_op()], None);
    let r2 = signed(
        &build_release(
            &renamed,
            CURATION.as_bytes(),
            &wire(&r1.envelope),
            2,
            START_MS,
            START_MS - 1,
        )
        .unwrap(),
        &key(),
    );
    let state = State {
        upstream,
        curation_set: r1.change.curation_set.clone().unwrap(),
    };
    chain_artifacts(&r1, &r2, |docs| {
        snapshot_bytes_from_state(
            &wire(state),
            &docs.envelope,
            &docs.signatures,
            &docs.history,
            &docs.history_signatures,
            PUBLIC_KEY,
        )
        .unwrap()
    })
}

fn wire_facts(value: Value) -> fusiform_release::Facts {
    serde_json::from_value(value).unwrap()
}

fn rename_op() -> ChangeOp {
    ChangeOp::Set {
        provider: "zeta".into(),
        model: "mini".into(),
        fact: "model.name".into(),
        value: json!("Mini 2"),
    }
}

fn chain_artifacts(
    r1: &Release,
    r2: &Release,
    snapshot: impl FnOnce(SnapshotDocs) -> Vec<u8>,
) -> BTreeMap<&'static str, Vec<u8>> {
    let base = history_base(r1);
    let day = history_day_bytes(DAY, &jsonl(&[r2])).unwrap();
    let docs = SnapshotDocs::new(
        r2,
        vec![
            HistoryFile {
                file: "base.json".into(),
                sha256: sha256(&base),
            },
            HistoryFile {
                file: format!("days/{DAY}.json"),
                sha256: sha256(&day),
            },
        ],
    );
    BTreeMap::from([
        ("chain/base.json", base),
        ("chain/release-1.json", wire(r1)),
        ("chain/release-2.json", wire(r2)),
        ("chain/snapshot.json", snapshot(docs)),
        ("chain/days/2026-11-19.json", day),
    ])
}

fn live_fixtures(r1: &Release, r2: &Release) -> BTreeMap<&'static str, Vec<u8>> {
    chain_artifacts(r1, r2, |docs| {
        snapshot_bytes(docs.inputs(&models_with_rename(), &ChargeUnitEvidence::default())).unwrap()
    })
}

fn history_base(import: &Release) -> Vec<u8> {
    let at = import.envelope.upstream_observed_at_ms;
    let era: Vec<_> = import
        .change
        .upstream
        .iter()
        .enumerate()
        .map(|(i, op)| {
            let ChangeOp::Set {
                provider,
                model,
                fact,
                value,
            } = op
            else {
                panic!("the fixture starts from an empty upstream with only model-scoped facts");
            };
            json!({"id": i + 1, "source": "models.dev", "provider_id": provider, "model_id": model,
            "fact_key": fact, "value_json": str::from_utf8(&wire(value)).unwrap(),
            "boundary_at_ms": at, "boundary_kind": "seed", "prior_observation_at_ms": null,
            "observation_id": null, "corrected_fields_json": null, "affected_from_ms": null,
            "affected_until_ms": null, "correction_reason": null})
        })
        .collect();
    let grade = &import.change.curation_set.as_ref().unwrap().grades["mini"];
    wire(
        json!({"format": "ck-models-history-base/v1", "release": 1, "tables": {
            "cache_lifetime_era": [], "curated_model_era": [], "era": era, "fact_override_era": [],
            "model_mark_era": [], "observation": [{"id": 1, "source": "models.dev", "observed_at_ms": at,
                "outcome": "seeded", "failure_class": null, "detail": null, "normalized_hash": null,
                "raw_hash": null, "etag": null, "duration_ms": null}],
            "observation_artifact_event": [], "plan_price_era": [],
            "quality_grade_era": [{"id": 1, "model": "mini", "elo": grade.elo.to_string(),
                "eq": grade.eq.to_string(), "withdrawn": 0, "graded_by": "operator",
                "reason": "Fixture grade", "recorded_at_ms": at, "grade": null, "kind": null}]
        }}),
    )
}

#[test]
fn signed_chain_refuses_curation_rollback() {
    let (r1, mut r2, _) = two_releases();
    let mut curation = r1.change.curation_set.clone().unwrap();
    curation.version -= 1;
    r2.envelope.curation_version = curation.version;
    r2.envelope.curation_hash = curation.curation_hash();
    r2.envelope.state_hash =
        fusiform_release::state_hash(r2.envelope.curation_hash, r2.envelope.upstream_hash);
    r2.change.curation_set = Some(curation);
    r2.envelope.change_hash = r2.change.change_hash();
    r2.signatures = sign_envelope(&r2.envelope, KID, &key()).unwrap();
    let keys = BTreeMap::from([(KID.to_string(), key().verifying_key())]);
    let failure = consumer::check_chain(&[r1, r2], &keys)
        .expect_err("publisher chain checker accepted a signed curation rollback");
    assert!(
        failure.contains("curation version does not advance"),
        "{failure}"
    );
}

#[test]
fn signed_chain_replays_and_snapshot_reproduces_state() {
    let (r1, r2, index) = two_releases();
    let r3 = third_release(&r2, &index);
    assert!(r1.change.curation_set.is_some());
    assert!(r2.change.curation_set.is_none());
    assert_eq!(r2.change.upstream, [rename_op()]);
    assert!(r3.change.curation_set.is_some());
    assert!(r3.change.upstream.is_empty());
    let keys = BTreeMap::from([(KID.to_string(), key().verifying_key())]);
    let chain = vec![r1, r2, r3];
    assert_eq!(consumer::check_chain(&chain, &keys).unwrap(), index);
    let mut tampered = chain.clone();
    tampered[2]
        .change
        .curation_set
        .as_mut()
        .unwrap()
        .grades
        .get_mut("mini")
        .unwrap()
        .elo = "1.0".parse().unwrap();
    assert_eq!(
        consumer::check_chain(&tampered, &keys).unwrap_err(),
        "verify release 3: change_hash does not match the release's change"
    );
    tampered[2].envelope.change_hash = tampered[2].change.change_hash();
    tampered[2].signatures = sign_envelope(&tampered[2].envelope, KID, &key()).unwrap();
    assert_eq!(
        consumer::check_chain(&tampered, &keys).unwrap_err(),
        "verify release 3: curation_hash does not match verified state"
    );

    let fixtures = live_fixtures(&chain[0], &chain[1]);
    let bytes = &fixtures["chain/snapshot.json"];
    let snapshot: Snapshot = serde_json::from_slice(bytes).unwrap();
    assert_eq!(*bytes, wire(&snapshot));
    assert_eq!(snapshot.envelope, chain[1].envelope);
    assert_eq!(snapshot.signatures, chain[1].signatures);
    assert_eq!(
        FactIndex::from_snapshot_upstream(&snapshot.state.upstream).unwrap(),
        index
    );
    let attestation = snapshot.history_attestation.unwrap();
    verify_envelope(&snapshot.envelope, &snapshot.signatures, &keys).unwrap();
    verify_history_attestation(&attestation, &snapshot.envelope, &keys).unwrap();
    for file in &attestation.body.history {
        file.verify_bytes(&fixtures[format!("chain/{}", file.file).as_str()])
            .unwrap();
    }
    let day = history_day_bytes(DAY, &jsonl(&[&chain[1], &chain[2]])).unwrap();
    assert_eq!(day, wire(json!({"day": DAY, "releases": &chain[1..]})));
}

#[test]
fn signed_release_replay_uses_the_envelope_time_on_both_sides_of_a_price_boundary() {
    let mut models = parse(MODELS);
    models["alpha"]["models"]["-"]["cost"]["multipliers"] = json!([
        {"when": {"date": {"gte": DAY}}, "factor": {"text": {"input": {"price": 2}}}}
    ]);
    let models = wire(models);
    let mut previous = FactIndex::from_snapshot_upstream(&Upstream::new()).unwrap();
    let mut parent = Vec::new();
    let mut releases = Vec::new();
    let mut history = Vec::new();
    for (number, observed_at, units, next_boundary) in [
        (1, START_MS - 1, 310_000_000, Some(START_MS)),
        (2, START_MS, 620_000_000, None),
    ] {
        let published_at = observed_at + 1;
        let (first_summary, next) = summary(&models, &previous, observed_at);
        let release = signed(
            &build_release(
                &first_summary,
                CURATION.as_bytes(),
                &parent,
                number,
                published_at,
                observed_at,
            )
            .unwrap(),
            &key(),
        );
        assert_eq!(release.envelope.upstream_observed_at_ms, observed_at);
        assert_eq!(
            parse(&first_summary)["next_date_boundary_ms"],
            json!(next_boundary)
        );
        let price = json!({"currency":"USD", "exponent":9, "state":"priced", "units":units});
        let price_op = ChangeOp::Set {
            provider: "alpha".into(),
            model: "-".into(),
            fact: "rate.input".into(),
            value: price,
        };
        assert!(release.change.upstream.contains(&price_op));
        if number == 2 {
            assert_eq!(release.change.upstream, [price_op]);
        }

        let original = wire(&release);
        let decoded: Release = serde_json::from_slice(&original).unwrap();
        let envelope = decoded.envelope;
        let (replay_summary, replay_index) =
            summary(&models, &previous, envelope.upstream_observed_at_ms);
        let replayed = signed(
            &build_release(
                &replay_summary,
                CURATION.as_bytes(),
                &parent,
                envelope.release,
                envelope.published_at_ms,
                envelope.upstream_observed_at_ms,
            )
            .unwrap(),
            &key(),
        );
        assert_eq!(replay_summary, first_summary);
        assert_eq!(replay_index.to_bytes(), next.to_bytes());
        assert_eq!(wire(&replayed), original);

        let (file, bytes) = if number == 1 {
            ("base.json".to_owned(), history_base(&release))
        } else {
            (
                format!("days/{DAY}.json"),
                history_day_bytes(DAY, &jsonl(&[&release])).unwrap(),
            )
        };
        history.push(HistoryFile {
            file,
            sha256: sha256(&bytes),
        });
        let docs = SnapshotDocs::new(&release, history.clone());
        let evidence = ChargeUnitEvidence::default();
        let snapshot: Snapshot =
            serde_json::from_slice(&snapshot_bytes(docs.inputs(&models, &evidence)).unwrap())
                .unwrap();
        assert_eq!(
            snapshot.state.upstream["alpha"].models["-"]["rate.input"]["units"],
            units
        );
        assert_eq!(
            FactIndex::from_snapshot_upstream(&snapshot.state.upstream).unwrap(),
            next
        );
        parent = wire(&release.envelope);
        previous = next;
        releases.push(release);
    }
    let keys = BTreeMap::from([(KID.to_owned(), key().verifying_key())]);
    assert_eq!(consumer::check_chain(&releases, &keys).unwrap(), previous);
}

#[test]
fn release_builder_refuses_invalid_parent_and_noop() {
    let (r1, r2, index) = two_releases();
    let (summary, _) = summary(&models_with_rename(), &index, START_MS);
    let build = |summary: &[u8], curation: &[u8], parent: &[u8], number| {
        build_release(summary, curation, parent, number, START_MS, START_MS)
            .unwrap_err()
            .class
    };
    assert_eq!(
        build(&summary, CURATION.as_bytes(), &wire(&r2.envelope), 3),
        "nothing_to_publish"
    );
    assert_eq!(
        build(&summary, CURATION.as_bytes(), &wire(&r1.envelope), 3),
        "invalid_parent"
    );
    assert_eq!(
        build(&summary, CURATION.as_bytes(), b"", 3),
        "invalid_parent"
    );
    assert_eq!(
        build(&summary, CURATION.as_bytes(), &wire(&r1.envelope), 1),
        "invalid_parent"
    );
    assert_eq!(
        build(&summary, CURATION.as_bytes(), b"null", 3),
        "invalid_parent"
    );
    assert_eq!(
        build(b"{\"refusal\":{}}", CURATION.as_bytes(), b"", 1),
        "invalid_summary"
    );
    assert_eq!(build(&summary, b"{}", b"", 1), "invalid_curation");
}

#[test]
fn curation_number_spelling_is_hashed_and_version_must_change() {
    let (r1, _, _) = two_releases();
    let prior = FactIndex::from_snapshot_upstream(&BTreeMap::new()).unwrap();
    let (genesis, index) = summary(MODELS, &prior, START_MS);
    let (changed, _) = summary(&models_with_rename(), &index, START_MS);
    let tampered = CURATION.replace("1.00", "1.0");
    let build = |raw: &str| {
        parse(&build_release(&genesis, raw.as_bytes(), b"", 1, START_MS, START_MS).unwrap())
    };
    let original = build(CURATION);
    let changed_spelling = build(&tampered);
    assert_ne!(
        original["envelope"]["curation_hash"],
        changed_spelling["envelope"]["curation_hash"]
    );
    assert_eq!(
        original["envelope"]["curation_hash"],
        json!(sha256(&wire(parse(CURATION.as_bytes()))))
    );
    let err = build_release(
        &changed,
        tampered.as_bytes(),
        &wire(&r1.envelope),
        2,
        START_MS,
        START_MS,
    )
    .unwrap_err();
    assert_eq!(err.class, "curation_version_reused");
    let mut unsorted = parse(&genesis);
    unsorted["ops"].as_array_mut().unwrap().reverse();
    let err = build_release(
        &wire(unsorted),
        CURATION.as_bytes(),
        b"",
        1,
        START_MS,
        START_MS,
    )
    .unwrap_err();
    assert_eq!(err.class, "invalid_summary");
}

#[test]
fn snapshot_builder_refuses_signed_invalid_history_lists() {
    let (_, r2, _) = two_releases();
    let models = models_with_rename();
    let evidence = ChargeUnitEvidence::default();
    for names in [
        &[][..],
        &["days/2026-11-19.json"][..],
        &["base.json", "days/2026-11-19.txt"][..],
        &["base.json", "days/2026-02-30.json"][..],
        &["base.json", "days/2026-11-20.json", "days/2026-11-19.json"][..],
        &["base.json", "days/2026-11-19.json", "days/2026-11-19.json"][..],
    ] {
        let history = names
            .iter()
            .map(|&file| HistoryFile {
                file: file.into(),
                sha256: sha256(file.as_bytes()),
            })
            .collect();
        let docs = SnapshotDocs::new(&r2, history);
        let result = snapshot_bytes(docs.inputs(&models, &evidence));
        assert_eq!(
            result.err().map(|err| err.class),
            Some("invalid_history_files"),
            "accepted signed history list {names:?}"
        );
    }
}

#[test]
fn history_day_builder_keeps_unknown_fields_in_the_canonical_bytes() {
    let (_, release, _) = two_releases();
    let mut stored = serde_json::to_value(release).unwrap();
    let extension: Value =
        serde_json::from_str(r#"{"z":[9007199254740993,1.00],"a":true}"#).unwrap();
    stored["extra"] = extension.clone();
    stored["envelope"]["extra"] = extension.clone();
    stored["change"]["extra"] = extension.clone();
    stored["change"]["upstream"][0]["extra"] = extension;
    let actual = history_day_bytes(DAY, &serde_json::to_vec(&stored).unwrap()).unwrap();
    assert_eq!(actual, wire(json!({"day": DAY, "releases": [stored]})));
}

#[test]
fn history_day_builder_refuses_a_change_that_does_not_match_its_envelope() {
    let (_, mut release, _) = two_releases();
    let fusiform_release::ChangeOp::Set { value, .. } = &mut release.change.upstream[0] else {
        panic!("the release must set the new model name");
    };
    *value = json!("Different name");
    let result = history_day_bytes(DAY, &jsonl(&[&release]));
    assert_eq!(
        result.err().map(|err| err.class),
        Some("history_change_mismatch"),
        "accepted a release whose change did not match its envelope"
    );
}

#[test]
fn snapshot_builders_refuse_semantically_invalid_curation() {
    let (r1, r2, _) = two_releases();
    let fixtures = live_fixtures(&r1, &r2);
    let mut snapshot: Snapshot = serde_json::from_slice(&fixtures["chain/snapshot.json"]).unwrap();
    snapshot
        .state
        .curation_set
        .vendor_facts
        .insert("plan-prices".into(), json!({"cells": [{}]}));
    let envelope = &mut snapshot.envelope;
    envelope.curation_hash = snapshot.state.curation_set.curation_hash();
    envelope.state_hash =
        fusiform_release::state_hash(envelope.curation_hash, envelope.upstream_hash);
    let attestation = snapshot.history_attestation.unwrap();
    let mut release: Release = serde_json::from_slice(&fixtures["chain/release-2.json"]).unwrap();
    release.envelope = envelope.clone();
    release.signatures = sign_envelope(envelope, KID, &key()).unwrap();
    let docs = SnapshotDocs::new(&release, attestation.body.history);
    let curation = wire(&snapshot.state.curation_set);
    let evidence = ChargeUnitEvidence::default();
    let models = models_with_rename();
    let refused = [
        snapshot_bytes(SnapshotInputs {
            curation_set_raw: &curation,
            ..docs.inputs(&models, &evidence)
        }),
        fusiform_publish::snapshot_bytes_from_state(
            &wire(snapshot.state),
            &docs.envelope,
            &docs.signatures,
            &docs.history,
            &docs.history_signatures,
            PUBLIC_KEY,
        ),
    ]
    .map(|result| result.err().map(|failure| failure.class));
    assert_eq!(
        refused,
        [Some("invalid_curation"); 2],
        "snapshot builders accepted a rejected plan-prices row"
    );
}

#[test]
fn snapshot_builder_refuses_mismatching_inputs() {
    let (_, r2, _) = two_releases();
    let models = models_with_rename();
    let evidence = ChargeUnitEvidence::default();
    let history_base = vec![HistoryFile {
        file: "base.json".into(),
        sha256: sha256(b"base"),
    }];
    let docs = SnapshotDocs::new(&r2, history_base.clone());
    let inputs = || docs.inputs(&models, &evidence);
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            models_dev: MODELS,
            ..inputs()
        })
        .unwrap_err()
        .class,
        "snapshot_hash_mismatch"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            models_dev: b"{",
            ..inputs()
        })
        .unwrap_err()
        .class,
        "parse"
    );
    let modified = CURATION.replace("1.00", "1.0");
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            curation_set_raw: modified.as_bytes(),
            ..inputs()
        })
        .unwrap_err()
        .class,
        "snapshot_curation_mismatch"
    );
    let modified = CURATION.replace("\"version\":1", "\"version\":2");
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            curation_set_raw: modified.as_bytes(),
            ..inputs()
        })
        .unwrap_err()
        .class,
        "snapshot_curation_mismatch"
    );
    let history = wire(vec![HistoryFile {
        file: "base.json".into(),
        sha256: sha256(b"different base"),
    }]);
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            history_files: &history,
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_history_signature"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            history_files: b"{}",
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_history_files"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            signatures: &docs.history_signatures,
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_release_signature"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            release_public_key: br#"{"kid":"a","public_key":"short"}"#,
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_release_public_key"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            envelope: b"{}",
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_envelope"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            curation_set_raw: b"{}",
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_curation"
    );
    let mut inconsistent = r2.clone();
    inconsistent.envelope.state_hash = sha256(b"not this state");
    inconsistent.signatures = sign_envelope(&inconsistent.envelope, KID, &key()).unwrap();
    let inconsistent = SnapshotDocs::new(&inconsistent, history_base);
    assert_eq!(
        snapshot_bytes(inconsistent.inputs(&models, &evidence))
            .unwrap_err()
            .class,
        "snapshot_state_mismatch"
    );
}

#[test]
fn history_attestation_refuses_tampering_and_cross_release_reuse() {
    let (r1, r2, _) = two_releases();
    let fixtures = live_fixtures(&r1, &r2);
    let snapshot: Snapshot = serde_json::from_slice(&fixtures["chain/snapshot.json"]).unwrap();
    let attestation = snapshot.history_attestation.unwrap();
    let keys = BTreeMap::from([(KID.to_string(), key().verifying_key())]);
    let mut tampered = attestation.clone();
    tampered.body.history.reverse();
    assert_eq!(
        verify_history_attestation(&tampered, &r2.envelope, &keys),
        Err(SignatureError::BadSignature)
    );
    assert_eq!(
        attestation.body.history[0]
            .verify_bytes(b"tampered base")
            .unwrap_err()
            .class,
        "history_file_hash_mismatch"
    );
    assert_eq!(
        verify_history_attestation(&attestation, &r1.envelope, &keys),
        Err(SignatureError::WrongHistoryAttestation)
    );
    let mut wrong_state = r2.envelope.clone();
    wrong_state.state_hash = sha256(b"a different state");
    assert_eq!(
        verify_history_attestation(&attestation, &wrong_state, &keys),
        Err(SignatureError::WrongHistoryAttestation)
    );
}

#[test]
fn day_builder_refuses_bad_batches_and_checks_utc_boundaries() {
    let (r1, r2, index) = two_releases();
    let r3 = third_release(&r2, &index);
    let mut gap = r3.clone();
    gap.envelope.release = 4;
    gap.envelope.parent = Some(3);
    let mut tomorrow = r3.clone();
    tomorrow.envelope.published_at_ms += 1;
    let mut yesterday = r2.clone();
    yesterday.envelope.published_at_ms -= 1;
    let class = |day, lines: &[u8]| history_day_bytes(day, lines).unwrap_err().class;
    assert_eq!(class(DAY, &jsonl(&[&r2, &gap])), "nonconsecutive_history");
    assert_eq!(class(DAY, &jsonl(&[&r3, &r2])), "nonconsecutive_history");
    assert_eq!(class(DAY, &jsonl(&[&r2, &r2])), "nonconsecutive_history");
    assert_eq!(
        class(DAY, &jsonl(&[&r2, &tomorrow])),
        "history_day_mismatch"
    );
    assert_eq!(class(DAY, &jsonl(&[&yesterday])), "history_day_mismatch");
    assert_eq!(
        class("2026-11-18", &jsonl(&[&r1])),
        "history_import_release"
    );
    assert_eq!(class("2026-02-29", &jsonl(&[&r2])), "invalid_day");
    assert_eq!(class("2026-1-19", &jsonl(&[&r2])), "invalid_day");
    assert_eq!(class(DAY, b""), "empty_history");
    assert_eq!(class(DAY, b"{}"), "invalid_history_release");
}
