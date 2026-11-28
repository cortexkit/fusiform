//! Best-effort wakeups for readers; the catalog remains the source of truth.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use cortexkit_bus_client::{BusClient, BusClientOptions, Error as BusError};
use fusiform_store::CatalogStore;
use subc_client_rs::{ConsumerOptions, SubcConsumer};
use subc_protocol::error_codes::TARGET_UNAVAILABLE;

use crate::signals::Signals;

const RECHECK: Duration = Duration::from_secs(60);
const TRANSIENT_RETRY: Duration = Duration::from_secs(1);
const TRANSIENT_CAP: Duration = Duration::from_secs(60);
const ABSENT_RETRY: Duration = Duration::from_secs(30);
const ABSENT_CAP: Duration = Duration::from_secs(15 * 60);
type PublishError = Box<dyn std::error::Error + Send + Sync>;

/// The task owns the transport, so no writer or tool handler can wait on it.
#[async_trait]
pub trait EventPublisher: Send {
    async fn publish(&mut self, msg_id: &str, body: Vec<u8>) -> Result<(), PublishError>;
}

#[derive(Default)]
pub struct CatalogBus {
    client: Option<BusClient>,
    consumer: Option<Arc<SubcConsumer>>,
}

#[async_trait]
impl EventPublisher for CatalogBus {
    async fn publish(&mut self, msg_id: &str, body: Vec<u8>) -> Result<(), PublishError> {
        let result = async {
            if self.client.is_none() {
                let consumer =
                    Arc::new(SubcConsumer::connect_default(ConsumerOptions::default()).await?);
                self.client = Some(
                    BusClient::connect(Arc::clone(&consumer), BusClientOptions::new("fusiform")?)
                        .await?,
                );
                self.consumer = Some(consumer);
            }
            // A duplicate acknowledgement is also success: readers already have
            // the wake identified by this version.
            let _ = self
                .client
                .as_ref()
                .expect("connected above")
                .publish_event("catalog_changed", 1, None, msg_id, body)
                .await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            // A fresh connection also reissues credentials if ck-bus restarted.
            self.client = None;
            self.consumer = None;
        }
        result
    }
}

/// Publish only a newly read high-water mark, with bounded retries and a poll
/// backstop. Notifications carry only the version number, not catalog data,
/// so a delayed wake cannot replay old facts.
/// Older daemons that omit `expected_back` stay on the short retry schedule.
pub async fn run_publisher(
    store: Arc<CatalogStore>,
    signals: Arc<Signals>,
    mut publisher: impl EventPublisher,
) {
    let mut published = None;
    let mut transient_backoff = TRANSIENT_RETRY;
    let mut absent_backoff = ABSENT_RETRY;
    loop {
        let result = match store.catalog_version() {
            Ok(version) if published.is_none_or(|last| version > last) => {
                let body = serde_json::json!({"catalog_version": version})
                    .to_string()
                    .into_bytes();
                publisher
                    .publish(&version.to_string(), body)
                    .await
                    .map(|()| {
                        published = Some(version);
                        signals.bus_published(version);
                    })
            }
            Ok(_) => Ok(()),
            Err(error) => Err(Box::new(error) as PublishError),
        };
        if let Err(error) = result {
            signals.bus_publish_failed(&error);
            let (delay, cap) = match error.downcast_ref::<BusError>() {
                Some(BusError::RouteRefused {
                    code,
                    expected_back: Some(false),
                }) if code == TARGET_UNAVAILABLE => (&mut absent_backoff, ABSENT_CAP),
                _ => (&mut transient_backoff, TRANSIENT_CAP),
            };
            // Write notifications must not bypass backoff during an outage.
            tokio::time::sleep(*delay).await;
            *delay = (*delay * 2).min(cap);
            continue;
        }
        transient_backoff = TRANSIENT_RETRY;
        absent_backoff = ABSENT_RETRY;
        tokio::select! {
            () = store.catalog_changed() => {},
            () = tokio::time::sleep(RECHECK) => {},
        }
    }
}

#[cfg(test)]
mod tests;
