use std::collections::BTreeMap;

use fusiform_publish::FactIndex;
use fusiform_release::{Hash, Release, VerifyingKey, state_hash, verify_envelope};

/// Check the signed change chain, starting from an empty pre-import state.
/// This does not validate approval or the contents of vendor fact documents.
pub fn check_chain(
    releases: &[Release],
    keys: &BTreeMap<String, VerifyingKey>,
) -> Result<FactIndex, String> {
    let mut index = FactIndex::from_snapshot_upstream(&BTreeMap::new()).unwrap();
    let mut installed = None::<(u64, u64, Hash)>;
    for release in releases {
        let envelope = &release.envelope;
        verify_envelope(envelope, &release.signatures, keys).map_err(|err| err.to_string())?;
        if envelope.parent != installed.map(|(number, _, _)| number) {
            return Err("release does not follow the installed parent".into());
        }
        if release.change.change_hash() != envelope.change_hash {
            return Err("change hash mismatch".into());
        }
        let (version, curation_hash) = match &release.change.curation_set {
            Some(curation)
                if installed.is_none_or(|(_, version, _)| version != curation.version) =>
            {
                (curation.version, curation.curation_hash())
            }
            None => installed
                .map(|(_, version, hash)| (version, hash))
                .ok_or("import must include its curation set")?,
            Some(_) => return Err("changed curation must have a different version".into()),
        };
        index
            .apply_change(&release.change)
            .map_err(|err| err.to_string())?;
        let upstream_hash = index.upstream_hash();
        if upstream_hash != envelope.upstream_hash
            || version != envelope.curation_version
            || curation_hash != envelope.curation_hash
            || state_hash(curation_hash, upstream_hash) != envelope.state_hash
        {
            return Err("replayed state does not match the envelope".into());
        }
        installed = Some((envelope.release, version, curation_hash));
    }
    Ok(index)
}
