use std::collections::BTreeMap;

use fusiform_release::{Release, Snapshot, State};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::{check_head, step, StorePoller, Verified};

/// Verified upstream facts and curation at the signed head, without importing
/// historical rows into SQLite.
pub struct VerifiedStoreHead {
    pub release: u64,
    pub state: State,
    /// Original document JSON in the public snapshot or release carrying the set.
    pub vendor_documents: BTreeMap<String, Box<RawValue>>,
}

#[derive(Deserialize)]
struct Documents {
    vendor_facts: BTreeMap<String, Box<RawValue>>,
}

impl StorePoller {
    /// Read the signed head, anchored at its latest snapshot, without a local database.
    pub async fn read_verified_head(&self) -> Result<VerifiedStoreHead, String> {
        let head = self.head().await?;
        let path = format!("v1/snapshots/{}", head.latest_snapshot);
        let bytes = step(&path, self.response(&path).await?.bytes().await)?;
        let snapshot: Snapshot = step(&path, serde_json::from_slice(&bytes))?;
        if snapshot.envelope.release != head.latest_snapshot {
            return Err(format!("{path}: envelope names a different release"));
        }

        #[derive(Deserialize)]
        struct SnapshotDocuments {
            state: SnapshotCuration,
        }
        #[derive(Deserialize)]
        struct SnapshotCuration {
            curation_set: Documents,
        }
        let raw: SnapshotDocuments = step(&path, serde_json::from_slice(&bytes))?;
        let mut documents = raw.state.curation_set.vendor_facts;
        drop(bytes);
        let mut current = Verified::from_snapshot(snapshot, &self.keys)?;

        // Release 1 is the one anchor whose full change is known without replaying history.
        if head.latest_snapshot == 1 {
            let release: Release = self.json("v1/releases/1").await?;
            let first = Verified::first(release, &self.keys)?;
            if first.envelope != current.envelope {
                return Err("release 1 does not reproduce the snapshot envelope".into());
            }
            current = first;
        }

        #[derive(Deserialize)]
        struct ReleaseDocuments {
            change: ChangedCuration,
        }
        #[derive(Deserialize)]
        struct ChangedCuration {
            curation_set: Documents,
        }
        for number in head.latest_snapshot + 1..=head.release.get() {
            let path = format!("v1/releases/{number}");
            let bytes = step(&path, self.response(&path).await?.bytes().await)?;
            let release: Release = step(&path, serde_json::from_slice(&bytes))?;
            if release.envelope.release != number {
                return Err(format!("{path}: wrong release number"));
            }
            current.advance(&release, &self.keys)?;
            if release.change.curation_set.is_some() {
                let raw: ReleaseDocuments = step(&path, serde_json::from_slice(&bytes))?;
                documents = raw.change.curation_set.vendor_facts;
            }
        }
        check_head(&current.envelope, &head)?;
        Ok(VerifiedStoreHead {
            release: head.release.get(),
            state: current.state,
            vendor_documents: documents,
        })
    }
}
