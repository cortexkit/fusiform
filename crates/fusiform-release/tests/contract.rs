use std::{collections::BTreeMap, num::NonZeroU64};

use fusiform_release::{
    Change, ChangeOp, Hash, Head, HeadBody, Release, ReleaseEnvelope, SignatureError, SigningKey,
    Snapshot, canonical_json, fact_hash, sha256, sign_envelope, sign_head, upstream_hash,
    verify_envelope, verify_head,
};
use serde_json::{Value, json};

#[test]
fn canonical_json_orders_and_escapes_at_every_depth() {
    let input = br#" {"z":{"z":[],"a":{}},"\u00e9":"\u96ea\/","a":[{"x":1,"b":"\u0001\u0000\u000b\u0008\u000c\u000a\u000d\u0009\u0022\u005c"},null,true]} "#;
    assert_eq!(
        canonical_json(&serde_json::from_slice(input).unwrap()),
        r#"{"a":[{"b":"\u0001\u0000\u000b\b\f\n\r\t\"\\","x":1},null,true],"z":{"a":{},"z":[]},"é":"雪/"}"#.as_bytes()
    );
}

#[test]
fn canonical_numbers_follow_the_v1_spelling() {
    for (input, expected) in [
        ("0.30000000000000004", "0.30000000000000004"),
        ("-1.23000", "-1.23000"),
        ("-0.0", "-0.0"),
        ("1e-7", "1e-7"),
        ("1E-7", "1e-7"),
        ("1e7", "1e+7"),
        ("1E+007", "1e+007"),
        ("-0", "0"),
        ("18446744073709551616", "18446744073709551616"),
    ] {
        let value = serde_json::from_str(input).unwrap();
        assert_eq!(canonical_json(&value), expected.as_bytes(), "{input}");
    }
    assert_eq!(canonical_json(&json!(1e30)), b"1e+30");
}

#[test]
fn canonical_output_is_idempotent() {
    let input = br#" { "prices": [ 0.30000000000000004, 3E0, -0, -0.0 ], "0": "\t\/\u00e9" } "#;
    let canonical = canonical_json(&serde_json::from_slice(input).unwrap());
    assert_eq!(
        canonical_json(&serde_json::from_slice(&canonical).unwrap()),
        canonical
    );
}

#[test]
fn fact_hash_is_sha256_of_the_canonical_value() {
    assert_eq!(
        fact_hash(&json!(null)).to_string(),
        "74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b"
    );
}

#[test]
fn change_hash_matches_canonical_whole_document() {
    let change: Change = serde_json::from_str(
        r#"{
        "upstream": [
            {"op":"set_provider","provider":"é","fact":"provider.name","value":"A\nB"},
            {"op":"set","provider":"é","model":"m","fact":"data","value":{"z":3E0,"a":[-0.0]}},
            {"op":"remove","provider":"é","model":"m","fact":"old"}
        ],
        "curation_set": null
    }"#,
    )
    .unwrap();
    assert_eq!(
        change.change_hash(),
        fact_hash(&serde_json::to_value(&change).unwrap())
    );
}

#[test]
fn upstream_hash_frames_tabs_newlines_and_cr_without_collisions() {
    let digest = fact_hash(&json!(1));
    assert_ne!(
        upstream_hash([(("p\tm", Some("n"), "f"), digest)]),
        upstream_hash([(("p", Some("m\tn"), "f"), digest)]),
    );
    assert_ne!(
        upstream_hash([(("p\n", Some("m"), "f"), digest)]),
        upstream_hash([(("p\\n", Some("m"), "f"), digest)]),
    );
    assert_eq!(
        upstream_hash([(("p\n", Some("m\r"), "f\t"), digest)]),
        sha256(format!("[\"p\\n\",\"m\\r\",\"f\\t\",\"{digest}\"]\n").as_bytes())
    );
}

#[test]
fn a_dash_model_is_not_a_provider_level_fact() {
    let digest = fact_hash(&json!(1));
    assert_ne!(
        upstream_hash([(("p", Some("-"), "f"), digest)]),
        upstream_hash([(("p", None, "f"), digest)]),
    );
}

#[test]
fn upstream_hash_uses_tuple_order_not_encoded_line_order() {
    let digest = fact_hash(&json!(1));
    let facts = [
        (("p", Some("a"), "a"), digest),
        (("p", None, "z"), digest),
        (("p", Some("-"), "z"), digest),
        (("p", Some("a"), "\t"), digest),
        (("p\t", None, "c"), digest),
        (("pa", None, "c"), digest),
    ];
    let lines = format!(
        concat!(
            "[\"p\",null,\"z\",\"{0}\"]\n",
            "[\"p\",\"-\",\"z\",\"{0}\"]\n",
            "[\"p\",\"a\",\"\\t\",\"{0}\"]\n",
            "[\"p\",\"a\",\"a\",\"{0}\"]\n",
            "[\"p\\t\",null,\"c\",\"{0}\"]\n",
            "[\"pa\",null,\"c\",\"{0}\"]\n",
        ),
        digest
    );
    assert_eq!(upstream_hash(facts), sha256(lines.as_bytes()));
}

fn envelope() -> Value {
    let h = Hash::from_bytes([12; 32]);
    json!({
        "format": "ck-models-release/v1", "release": 2, "parent": 1,
        "published_at_ms": 910, "upstream_observed_at_ms": 905,
        "upstream_hash": h, "curation_version": 4, "curation_hash": h,
        "state_hash": h, "change_hash": h
    })
}

fn round_trip<T: serde::de::DeserializeOwned + serde::Serialize>(wire: Value) -> T {
    let object: T = serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(&object).unwrap(), wire);
    object
}

#[test]
fn release_head_and_snapshot_round_trip() {
    let ops = json!([
        {"op":"set_provider","provider":"p","fact":"a","value":42},
        {"op":"remove_provider","provider":"p","fact":"b"},
        {"op":"remove","provider":"p","model":"m","fact":"a"},
        {"op":"set","provider":"p","model":"m","fact":"b","value":null}
    ]);
    let signature = json!({"kid": "example-key", "sig": "placeholder"});
    let release: Release = round_trip(json!({
        "envelope":envelope(), "change":{"upstream":ops,"curation_set":null},
        "signatures":signature
    }));
    assert!(matches!(
        release.change.upstream[0],
        ChangeOp::SetProvider { .. }
    ));
    assert!(matches!(
        release.change.upstream[1],
        ChangeOp::RemoveProvider { .. }
    ));
    assert!(matches!(
        release.change.upstream[2],
        ChangeOp::Remove { .. }
    ));
    assert!(matches!(
        release.change.upstream[3],
        ChangeOp::Set {
            value: Value::Null,
            ..
        }
    ));

    round_trip::<Head>(json!({
        "head":{"release":2,"latest_snapshot":1,"state_hash":release.envelope.state_hash,"published_at_ms":910},
        "signatures":signature
    }));
    round_trip::<Snapshot>(json!({
        "envelope":envelope(), "signatures":signature,
        "state":{
            "upstream":{"p":{"provider":{"provider.name":"P"},"models":{"m":{"existence":"present"}}}},
            "curation_set":{
                "version":4, "approved_at_ms":null, "approved_drafts":["draft-42"],
                "grades":{"m":{"elo":13,"eq":14,"graded_effort":"low"},"legacy":{"elo":12,"eq":13}},
                "effort_grades":{"m":{"high":{"elo":15,"eq":16}}},
                "kind_grades":{"m":{"chat":12}},
                "workload_efforts":{"m":{"reason":{"default":"high","minimum":"low"}}},
                "decommissioned":{"retired":{"reason":"withdrawn"}},
                "vendor_facts":{"input-audio-per-token":{"schema":"input-audio-per-token/v1"}}
            }
        }
    }));
}

#[test]
fn unrecorded_graded_effort_is_omitted_and_empty_effort_is_refused() {
    use fusiform_release::Grade;

    round_trip::<Grade>(json!({"elo":13,"eq":14}));
    round_trip::<Grade>(json!({"elo":13,"eq":14,"graded_effort":"low"}));
    for invalid in [json!(""), Value::Null] {
        assert!(
            serde_json::from_value::<Grade>(json!({
                "elo":13,"eq":14,"graded_effort":invalid
            }))
            .is_err()
        );
    }
}

#[test]
fn workload_without_minimum_omits_it_and_empty_minimum_is_refused() {
    use fusiform_release::WorkloadEffort;

    round_trip::<WorkloadEffort>(json!({"default":"high"}));
    round_trip::<WorkloadEffort>(json!({"default":"high","minimum":"low"}));
    for invalid in [json!(""), Value::Null] {
        assert!(
            serde_json::from_value::<WorkloadEffort>(json!({
                "default":"high","minimum":invalid
            }))
            .is_err()
        );
    }
}

#[test]
fn workload_exclusion_requires_the_effort_fields_to_be_absent() {
    use fusiform_release::WorkloadEffort;

    round_trip::<WorkloadEffort>(json!({"excluded":true}));
    let not_excluded: WorkloadEffort =
        serde_json::from_value(json!({"default":"high","excluded":false})).unwrap();
    assert_eq!(
        serde_json::to_value(not_excluded).unwrap(),
        json!({"default":"high"})
    );
    for invalid in [
        json!({"excluded":true,"default":"high"}),
        json!({"excluded":true,"default":null}),
        json!({"excluded":true,"minimum":"low"}),
        json!({"excluded":true,"minimum":null}),
        json!({"excluded":false}),
        json!({"minimum":"low"}),
        json!({}),
    ] {
        assert!(
            serde_json::from_value::<WorkloadEffort>(invalid.clone()).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn head_only_accepts_an_existing_snapshot_number() {
    use fusiform_release::{HeadBody, head_signing_bytes};

    let valid = json!({"release":2,"latest_snapshot":1,"state_hash":sha256(b"state"),"published_at_ms":910});
    let body = round_trip::<HeadBody>(valid.clone());
    assert_eq!(head_signing_bytes(&body).unwrap(), canonical_json(&valid));
    for latest_snapshot in [0, 3] {
        let mut invalid = valid.clone();
        invalid["latest_snapshot"] = json!(latest_snapshot);
        assert!(serde_json::from_value::<HeadBody>(invalid).is_err());
    }
    let mut missing = valid;
    missing.as_object_mut().unwrap().remove("latest_snapshot");
    assert!(serde_json::from_value::<HeadBody>(missing).is_err());
}

#[test]
fn only_genesis_may_have_a_null_parent() {
    let mut wire = envelope();
    wire["release"] = json!(1);
    wire["parent"] = Value::Null;
    round_trip::<ReleaseEnvelope>(wire);
    round_trip::<ReleaseEnvelope>(envelope());
}

#[test]
fn invalid_release_parents_are_refused() {
    for (release, parent) in [
        (0, json!(null)),
        (0, json!(0)),
        (1, json!(0)),
        (1, json!(1)),
        (2, json!(null)),
        (2, json!(0)),
        (2, json!(2)),
        (3, json!(1)),
    ] {
        let mut wire = envelope();
        wire["release"] = json!(release);
        wire["parent"] = parent.clone();
        assert!(
            serde_json::from_value::<ReleaseEnvelope>(wire).is_err(),
            "{release}: {parent}"
        );
    }
    let mut wire = envelope();
    wire["release"] = json!(1);
    wire.as_object_mut().unwrap().remove("parent");
    assert!(serde_json::from_value::<ReleaseEnvelope>(wire).is_err());

    let mut fields: ReleaseEnvelope = serde_json::from_value(envelope()).unwrap();
    fields.parent = None;
    assert!(serde_json::to_value(fields).is_err());
}

const TEST_SIGNING_KEY_BYTES: [u8; 32] = [42; 32];

#[test]
fn envelope_verification_refuses_tampering_and_an_unknown_kid() {
    let key = SigningKey::from_bytes(&TEST_SIGNING_KEY_BYTES);
    let pinned_keys = BTreeMap::from([("test-key".into(), key.verifying_key())]);
    let envelope: ReleaseEnvelope = serde_json::from_value(envelope()).unwrap();
    let signatures = sign_envelope(&envelope, "test-key", &key).unwrap();
    assert_eq!(
        verify_envelope(&envelope, &signatures, &pinned_keys),
        Ok(())
    );

    let mut tampered = envelope.clone();
    tampered.change_hash = Hash::from_bytes([1; 32]);
    assert_eq!(
        verify_envelope(&tampered, &signatures, &pinned_keys),
        Err(SignatureError::BadSignature)
    );

    let mut unknown = signatures;
    unknown.kid = "unknown-key".into();
    assert_eq!(
        verify_envelope(&envelope, &unknown, &pinned_keys),
        Err(SignatureError::UnknownKey)
    );
}

#[test]
fn head_verification_accepts_only_unpadded_base64url() {
    let key = SigningKey::from_bytes(&TEST_SIGNING_KEY_BYTES);
    let pinned_keys = BTreeMap::from([("test-key".into(), key.verifying_key())]);
    let head = HeadBody {
        release: NonZeroU64::new(2).unwrap(),
        latest_snapshot: 1,
        state_hash: Hash::from_bytes([12; 32]),
        published_at_ms: 910,
    };
    let signatures = sign_head(&head, "test-key", &key).unwrap();
    assert_eq!(signatures.sig.len(), 86);
    assert_eq!(verify_head(&head, &signatures, &pinned_keys), Ok(()));

    let tampered = HeadBody {
        latest_snapshot: 2,
        ..head.clone()
    };
    assert_eq!(
        verify_head(&tampered, &signatures, &pinned_keys),
        Err(SignatureError::BadSignature)
    );

    let mut padded = signatures.clone();
    padded.sig.push_str("==");
    assert_eq!(
        verify_head(&head, &padded, &pinned_keys),
        Err(SignatureError::InvalidEncoding)
    );

    let mut standard = signatures.clone();
    standard.sig = standard.sig.replace('-', "+").replace('_', "/");
    assert_ne!(
        standard.sig, signatures.sig,
        "fixture must exercise the alphabets' differences"
    );
    assert_eq!(
        verify_head(&head, &standard, &pinned_keys),
        Err(SignatureError::InvalidEncoding)
    );
}
