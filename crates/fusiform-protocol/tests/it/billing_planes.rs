//! Pin plane-only wire additions without changing the ordinary catalog shape.

use fusiform_protocol::{
    BillingRuleWire, CatalogGetRequest, CatalogGetResponse, PlaneWire, RateValue, UnpricedReason,
};
use serde_json::json;

#[test]
fn catalog_get_accepts_auth_method_but_still_rejects_unknown_fields() {
    let empty: CatalogGetRequest = serde_json::from_str("{}").unwrap();
    assert_eq!(empty.auth_method, None);
    for auth_method in ["apikey", "chatgpt", "oauth", "antigravity"] {
        let request: CatalogGetRequest = serde_json::from_value(json!({
            "provider_id": "openai", "auth_method": auth_method,
        }))
        .unwrap();
        assert_eq!(request.auth_method.as_deref(), Some(auth_method));
    }
    let error = serde_json::from_value::<CatalogGetRequest>(json!({
        "provider_id": "openai", "auth_method": "chatgpt", "auth_methd": "chatgpt",
    }))
    .unwrap_err();
    assert!(error.to_string().contains("unknown field `auth_methd`"));
}

#[test]
fn billed_as_round_trips_with_the_full_target_key() {
    for key in ["rate.input", "rate.input.above_context.272000"] {
        let literal = json!({"state": "billed_as", "key": key});
        let rate = RateValue::BilledAs { key: key.into() };
        assert_eq!(serde_json::to_value(&rate).unwrap(), literal);
        assert_eq!(serde_json::from_value::<RateValue>(literal).unwrap(), rate);
    }
}

#[test]
fn not_established_round_trips_as_an_unpriced_reason() {
    let reason = UnpricedReason::NotEstablished;
    assert_eq!(
        serde_json::to_string(&reason).unwrap(),
        "\"not_established\""
    );
    assert_eq!(
        serde_json::from_str::<UnpricedReason>("\"not_established\"").unwrap(),
        reason
    );
    let literal = json!({"state": "unpriced", "reason": "not_established"});
    let rate = RateValue::Unpriced {
        reason,
        inherited_from: None,
    };
    assert_eq!(serde_json::to_value(&rate).unwrap(), literal);
    assert_eq!(serde_json::from_value::<RateValue>(literal).unwrap(), rate);
}

// The pre-plane response shape, stated independently of the new serializer so
// a missing serde default cannot be hidden by a round trip through new code.
const NON_PLANE: &str = r#"{
    "source":"models.dev",
    "resolved_at_ms":1786488349024,
    "catalog_version":1786488349024,
    "models":{"openai/gpt-5.6-luna":{"rate.input":{
        "state":"priced","units":2500000000,"exponent":9,"currency":"USD"
    }}}
}"#;

#[test]
fn a_pre_plane_response_decodes_and_omits_the_new_fields_on_encode() {
    let response: CatalogGetResponse = serde_json::from_str(NON_PLANE).unwrap();
    assert_eq!(response.plane, None);
    assert!(response.billing_rules.is_empty());
    assert_eq!(
        serde_json::to_value(response).unwrap(),
        serde_json::from_str::<serde_json::Value>(NON_PLANE).unwrap(),
        "ordinary reads must not acquire plane or billing_rules fields"
    );
}

#[test]
fn plane_and_billing_rule_fields_round_trip_with_their_provenance() {
    let plane = PlaneWire {
        provider_id: "openai".into(),
        auth_method: "chatgpt".into(),
        kind: "quota_proxy".into(),
        source_ref: Some("https://help.openai.com/en/articles/20001106".into()),
        review_by_ms: Some(1_796_601_600_000),
    };
    let plane_literal = json!({
        "provider_id":"openai", "auth_method":"chatgpt", "kind":"quota_proxy",
        "source_ref":"https://help.openai.com/en/articles/20001106",
        "review_by_ms":1796601600000i64,
    });
    assert_eq!(serde_json::to_value(&plane).unwrap(), plane_literal);
    assert_eq!(
        serde_json::from_value::<PlaneWire>(plane_literal.clone()).unwrap(),
        plane
    );

    let mut rules = Vec::new();
    let mut literals = Vec::new();
    for (state, as_class) in [
        ("billed_as", Some("input")),
        ("stated_zero", None),
        ("not_established", Some("input")),
    ] {
        let rule = BillingRuleWire {
            model: "openai/gpt-5.6-luna".into(),
            fact_key: "rate.cache_write.above_context.272000".into(),
            state: state.into(),
            as_class: as_class.map(str::to_string),
            source_ref: "https://help.openai.com/en/articles/20001106".into(),
            established_by: "fusiform".into(),
            established_at_ms: 1_791_417_600_000,
            review_by_ms: 1_796_601_600_000,
        };
        let mut literal = json!({
            "model":"openai/gpt-5.6-luna", "fact_key":"rate.cache_write.above_context.272000",
            "state":state, "source_ref":"https://help.openai.com/en/articles/20001106",
            "established_by":"fusiform", "established_at_ms":1791417600000i64,
            "review_by_ms":1796601600000i64,
        });
        if let Some(class) = as_class {
            literal["as_class"] = json!(class);
        }
        assert_eq!(serde_json::to_value(&rule).unwrap(), literal);
        assert_eq!(
            serde_json::from_value::<BillingRuleWire>(literal.clone()).unwrap(),
            rule
        );
        rules.push(rule);
        literals.push(literal);
    }

    let mut literal: serde_json::Value = serde_json::from_str(NON_PLANE).unwrap();
    literal["plane"] = plane_literal;
    literal["billing_rules"] = json!(literals);
    let response: CatalogGetResponse = serde_json::from_value(literal.clone()).unwrap();
    assert_eq!(response.plane, Some(plane));
    assert_eq!(response.billing_rules, rules);
    assert_eq!(serde_json::to_value(&response).unwrap(), literal);
}

#[test]
fn an_undeclared_api_plane_omits_provenance_and_needs_no_billing_rules() {
    let literal = json!({"provider_id":"openai", "auth_method":"apikey", "kind":"upstream"});
    let plane: PlaneWire = serde_json::from_value(literal.clone()).unwrap();
    assert_eq!(plane.source_ref, None);
    assert_eq!(plane.review_by_ms, None);
    assert_eq!(serde_json::to_value(&plane).unwrap(), literal);

    let mut literal: serde_json::Value = serde_json::from_str(NON_PLANE).unwrap();
    literal["plane"] = serde_json::to_value(plane).unwrap();
    let response: CatalogGetResponse = serde_json::from_value(literal.clone()).unwrap();
    assert!(response.billing_rules.is_empty());
    assert_eq!(serde_json::to_value(response).unwrap(), literal);
}

#[test]
fn billing_rules_can_decode_without_a_plane_field() {
    let mut literal: serde_json::Value = serde_json::from_str(NON_PLANE).unwrap();
    literal["billing_rules"] = json!([{
        "model":"openai/gpt-image-1", "fact_key":"rate.cache_write",
        "state":"not_established", "as_class":"input",
        "source_ref":"https://help.openai.com/en/articles/20001106",
        "established_by":"fusiform", "established_at_ms":1791417600000i64,
        "review_by_ms":1796601600000i64,
    }]);
    let response: CatalogGetResponse = serde_json::from_value(literal.clone()).unwrap();
    assert_eq!(response.plane, None);
    assert_eq!(response.billing_rules.len(), 1);
    assert_eq!(serde_json::to_value(response).unwrap(), literal);
}
