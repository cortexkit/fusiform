//! What fusiform does with the two caller facts subc-protocol 0.29 added:
//! a session's tool preset and a scope's flow id.
//!
//! Neither is something fusiform acts on. Both still reach it, and each has a
//! failure that a module ignoring them would ship without noticing.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_module::route::serve_tool_call;
use fusiform_store::CatalogStore;
use subc_protocol::scope::ScopeStamp;

fn store() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap();
    (store, dir)
}

/// A named preset is refused by name, and a call without one is served.
///
/// The envelope type tolerates unknown keys, so before this rule existed a
/// call carrying `preset` was answered as though the preset were absent. A
/// caller whose session was meant to be restricted would get the full surface
/// and believe the restriction had applied.
#[test]
fn a_named_preset_is_refused_and_an_absent_one_is_served() {
    let (store, _d) = store();

    // Control: the same call without a preset is served, so the refusal below
    // is about the preset and not about the tool or its arguments.
    serve_tool_call(&store, br#"{"name":"catalog.status","arguments":{}}"#)
        .expect("a call without a preset gets fusiform's one surface");

    let err = serve_tool_call(
        &store,
        br#"{"name":"catalog.status","arguments":{},"preset":"reader"}"#,
    )
    .expect_err("fusiform serves no presets, so a named one must be refused");
    assert_eq!(err.code, "bad_request");
    assert!(
        err.message.contains("\"reader\""),
        "the refusal must name the preset it refused: {}",
        err.message
    );
}

/// A scope stamp carrying `flow_id` decodes.
///
/// fusiform never reads a scope, but its SDK decodes every `route.bind` the
/// daemon relays, and a scoped bind carries this stamp, whose attributes refuse
/// unknown fields. On subc-protocol 0.28 or older this stamp fails to decode,
/// so a scoped route to fusiform would be refused at bind the moment an
/// authority owner starts stamping flows. Written as wire JSON rather than a
/// struct literal, so a lock moving back to an older protocol fails here at
/// run time instead of at a compile error someone might "fix" by dropping the
/// field.
#[test]
fn a_scope_stamp_naming_a_flow_decodes() {
    let stamp = serde_json::json!({
        "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
        "ref": "session-1",
        "scope_epoch": 1,
        "kind": "worker",
        "attributes": {"flow_id": "flow-nightly-1"},
        "owner_authorized": true
    });

    let decoded: ScopeStamp = serde_json::from_value(stamp.clone())
        .unwrap_or_else(|e| panic!("a stamp naming a flow must decode: {e}\n{stamp}"));

    // Control: the flow id survived decoding, so this did not pass by parsing
    // a stamp that never carried one.
    assert_eq!(
        decoded.attributes.flow_id.as_deref(),
        Some("flow-nightly-1")
    );
}
