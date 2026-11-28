//! Best-effort wakeups for readers; the catalog remains the source of truth.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use async_trait::async_trait;
use cortexkit_bus_client::{BusClient, BusClientOptions, Error as BusError};
use cortexkit_bus_nats::NatsConnectionState;
use fusiform_store::CatalogStore;
use subc_client_rs::{ConsumerOptions, SubcConsumer};
use subc_protocol::error_codes::TARGET_UNAVAILABLE;
use tokio::sync::watch;

use crate::signals::Signals;

const RECHECK: Duration = Duration::from_secs(60);
const TRANSIENT_RETRY: Duration = Duration::from_secs(1);
const TRANSIENT_CAP: Duration = Duration::from_secs(60);
const ABSENT_RETRY: Duration = Duration::from_secs(30);
const ABSENT_CAP: Duration = Duration::from_secs(15 * 60);
type PublishError = Box<dyn std::error::Error + Send + Sync>;
type ConnectionMonitor = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The task owns the transport, so no writer or tool handler can wait on it.
#[async_trait]
pub trait EventPublisher: Send {
    /// The monitor stamps connection transitions and ends when the client must
    /// be replaced. For a publisher with no state watch, an ack proves reachability.
    async fn connect(
        &mut self,
        _signals: Arc<Signals>,
    ) -> Result<Option<ConnectionMonitor>, PublishError> {
        Ok(None)
    }

    async fn publish(&mut self, msg_id: &str, body: Vec<u8>) -> Result<(), PublishError>;

    fn disconnect(&mut self) {}
}

#[derive(Default)]
pub struct CatalogBus {
    client: Option<Arc<BusClient>>,
    consumer: Option<Arc<SubcConsumer>>,
}

#[async_trait]
impl EventPublisher for CatalogBus {
    async fn connect(
        &mut self,
        signals: Arc<Signals>,
    ) -> Result<Option<ConnectionMonitor>, PublishError> {
        let consumer = Arc::new(SubcConsumer::connect_default(ConsumerOptions::default()).await?);
        let client = Arc::new(
            BusClient::connect(Arc::clone(&consumer), BusClientOptions::new("fusiform")?).await?,
        );
        self.client = Some(Arc::clone(&client));
        self.consumer = Some(consumer);
        let replacements = client.connection_changes();
        Ok(Some(Box::pin(watch_connection(
            signals,
            replacements,
            move || client.connection().connection_state(),
        ))))
    }

    async fn publish(&mut self, msg_id: &str, body: Vec<u8>) -> Result<(), PublishError> {
        // A duplicate acknowledgement is also success: readers already have
        // the wake identified by this version.
        let _ = self
            .client
            .as_ref()
            .expect("connect succeeds before publishing")
            .publish_event("catalog_changed", 1, None, msg_id, body)
            .await?;
        Ok(())
    }

    fn disconnect(&mut self) {
        // A fresh connection also reissues credentials if ck-bus restarted.
        self.client = None;
        self.consumer = None;
    }
}

async fn watch_connection(
    signals: Arc<Signals>,
    mut replacements: watch::Receiver<u64>,
    mut snapshot: impl FnMut() -> watch::Receiver<NatsConnectionState>,
) {
    while replacements.has_changed().is_ok() {
        let mut states = snapshot();
        let observe = async {
            while states.has_changed().is_ok() {
                if *states.borrow_and_update() == NatsConnectionState::Connected {
                    signals.bus_connected();
                } else {
                    signals.bus_disconnected();
                }
                if states.changed().await.is_err() {
                    break;
                }
            }
            signals.bus_disconnected();
        };
        // Prefer a replacement that is already ready over the retired state.
        // If only the NATS watch closes, the bus client can still replace it.
        let replacement = tokio::select! {
            biased;
            result = replacements.changed() => result,
            () = observe => replacements.changed().await,
        };
        if replacement.is_err() {
            break;
        }
    }
}

/// Publish only a newly read high-water mark, with bounded retries and a poll
/// backstop. Notifications carry only the version number, not catalog data,
/// so a delayed wake cannot replay old facts.
///
/// A refused route open carries the daemon's `expected_back` verdict: whether
/// the bus module is expected to come back (restarting) or is not installed or
/// disabled (`false`). Only `false` moves the publisher to the long retry
/// schedule. Older daemons that omit the verdict stay on the short schedule.
pub async fn run_publisher(
    store: Arc<CatalogStore>,
    signals: Arc<Signals>,
    mut publisher: impl EventPublisher,
) {
    let mut published = None;
    let mut backoff = Backoff::INITIAL;
    loop {
        let error = match publisher.connect(Arc::clone(&signals)).await {
            Err(error) => Some(error),
            Ok(monitor) => {
                let reports_state = monitor.is_some();
                let monitor = monitor.unwrap_or_else(|| Box::pin(std::future::pending()));
                tokio::select! {
                    biased;
                    () = monitor => None,
                    error = publish_updates(&store, &signals, &mut publisher, reports_state, &mut published, &mut backoff) => Some(error),
                }
            }
        };
        signals.bus_disconnected();
        publisher.disconnect();
        backoff.wait(&signals, error.as_deref()).await;
    }
}

async fn publish_updates(
    store: &CatalogStore,
    signals: &Signals,
    publisher: &mut impl EventPublisher,
    reports_state: bool,
    published: &mut Option<i64>,
    backoff: &mut Backoff,
) -> PublishError {
    loop {
        let version = match store.catalog_version() {
            Ok(version) => version,
            Err(error) => {
                backoff.wait(signals, Some(&error)).await;
                continue;
            }
        };
        if published.is_none_or(|last| version > last) {
            let body = serde_json::json!({"catalog_version": version})
                .to_string()
                .into_bytes();
            if let Err(error) = publisher.publish(&version.to_string(), body).await {
                return error;
            }
            *published = Some(version);
            signals.bus_published(version);
            if !reports_state {
                signals.bus_connected();
            }
        }
        *backoff = Backoff::INITIAL;
        tokio::select! {
            () = store.catalog_changed() => {},
            () = tokio::time::sleep(RECHECK) => {},
        }
    }
}

struct Backoff {
    transient: Duration,
    absent: Duration,
}

impl Backoff {
    const INITIAL: Self = Self {
        transient: TRANSIENT_RETRY,
        absent: ABSENT_RETRY,
    };

    async fn wait(
        &mut self,
        signals: &Signals,
        error: Option<&(dyn std::error::Error + Send + Sync + 'static)>,
    ) {
        if let Some(error) = error {
            signals.bus_publish_failed(&error);
        }
        let (delay, cap) = match error.and_then(|error| error.downcast_ref::<BusError>()) {
            Some(BusError::RouteRefused {
                code,
                expected_back: Some(false),
            }) if code == TARGET_UNAVAILABLE => (&mut self.absent, ABSENT_CAP),
            _ => (&mut self.transient, TRANSIENT_CAP),
        };
        // Write notifications must not bypass backoff during an outage.
        tokio::time::sleep(*delay).await;
        *delay = (*delay * 2).min(cap);
    }
}

#[cfg(test)]
mod tests;
