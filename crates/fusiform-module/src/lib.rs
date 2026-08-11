#![forbid(unsafe_code)]

//! The ck-fusiform daemon's logic, as a library.
//!
//! The binary is a thin `main` over this crate rather than the other way
//! around, so integration tests link the same code the daemon runs. The
//! alternative — a binary-only crate with tests pulling modules in by path —
//! compiles every module twice, once per target, and each copy sees the other's
//! exports as unused.

pub mod fetch;
pub mod health;
pub mod loop_;
pub mod route;
pub mod seed;
pub mod signals;

use subc_protocol::manifest::{
    Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ModuleManifest,
    ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
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
    ModuleManifest {
        module_id: MODULE_ID.to_string(),
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
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
            ],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency: Concurrency::ModuleManaged,
            emits_push: true,
            sub_supervises: true,
        }],
        consumes: Vec::new(),
        scheduled_tasks: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                // `Project` is the only variant this protocol version defines,
                // and it does not decide anything here: the daemon resolves
                // every module to one database (`isolation: module`) at
                // <data_home>/cortexkit/<module_id>/store.db regardless of what
                // this field says.
                //
                // Worth stating because the field READS like it partitions
                // storage per project, which for fusiform would be wrong — the
                // catalog describes the world, not a project, so two projects
                // asking what models exist must get the same answer. They do,
                // but because of the daemon's resolution rather than because of
                // this value.
                scope: StorageScope::Project,
                owns_schema: true,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: Vec::new(),
                optional: vec![IdentityScope::Project, IdentityScope::Session],
            },
        },
    }
}
