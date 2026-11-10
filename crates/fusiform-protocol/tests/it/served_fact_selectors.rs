use fusiform_protocol::{served_fact_key_matches, FactClass, SERVED_FACTS};

const FAMILY: &str = "limit.image_long_edge_px.above_images.<n>";

#[test]
fn image_count_selector_family_matches_concrete_thresholds() {
    let family = SERVED_FACTS.iter().find(|fact| fact.key == FAMILY).unwrap();
    assert_eq!(family.class, FactClass::Advisory);
    for key in [
        "limit.image_long_edge_px.above_images.14",
        "limit.image_long_edge_px.above_images.20",
        "limit.image_long_edge_px.above_images.1",
    ] {
        assert!(served_fact_key_matches(family.key, key), "{key}");
    }
    let base = SERVED_FACTS
        .iter()
        .find(|fact| fact.key == "limit.image_long_edge_px")
        .unwrap();
    assert!(served_fact_key_matches(base.key, base.key));
    assert!(!served_fact_key_matches(
        base.key,
        "limit.image_long_edge_px.above_images.20"
    ));
}

#[test]
fn image_count_selector_family_rejects_bad_suffixes() {
    let family = SERVED_FACTS.iter().find(|fact| fact.key == FAMILY).unwrap();
    for key in [
        "limit.image_long_edge_px.above_images.many",
        "limit.image_long_edge_px.above_images.0",
        "limit.image_long_edge_px.above_images.020",
        "limit.image_long_edge_px.above_images.+20",
        "limit.image_long_edge_px.above_images.-20",
        "limit.image_long_edge_px.above_images.20.1",
        "limit.image_long_edge_px.above_images.18446744073709551616",
        "limit.image_long_edge_px.above_images.",
        "limit.image_long_edge_px.above_images.<n>",
        "limit.image_long_edge_px",
        "limit.image_bytes.above_images.20",
    ] {
        assert!(
            !served_fact_key_matches(family.key, key),
            "accepted invalid selector: {key}"
        );
    }
}
