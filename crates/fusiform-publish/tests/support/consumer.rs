use std::collections::BTreeMap;

use fusiform_module::store_poll::verify_chain_for_tests;
use fusiform_publish::FactIndex;
use fusiform_release::{Release, VerifyingKey};

/// Rebuild the index from the state the receiving daemon verified.
pub fn check_chain(
    releases: &[Release],
    keys: &BTreeMap<String, VerifyingKey>,
) -> Result<FactIndex, String> {
    let state = verify_chain_for_tests(releases, keys)?;
    FactIndex::from_snapshot_upstream(&state.upstream).map_err(|err| err.to_string())
}
