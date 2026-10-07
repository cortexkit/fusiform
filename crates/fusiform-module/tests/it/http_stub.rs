//! A local server must stop with its test, even if a client never finishes a request.

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) async fn within_deadline<F: std::future::Future>(operation: F) -> F::Output {
    // The production client's budget is intentionally generous. A local stub
    // should finish promptly, and must still fail if that client budget drifts.
    tokio::time::timeout(Duration::from_secs(60), operation)
        .await
        .expect("local HTTP operation exceeded its test deadline")
}

pub(super) struct StubServer {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl StubServer {
    pub(super) fn start(
        listener: TcpListener,
        mut serve: impl FnMut(TcpStream) + Send + 'static,
    ) -> Self {
        listener.set_nonblocking(true).expect("nonblocking accept");
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Accepted sockets can inherit the listener's mode on
                        // some platforms. Only accept is nonblocking; request
                        // reads and response writes use the budgets below.
                        stream.set_nonblocking(false).expect("blocking stub socket");
                        // Parallel seed/store tests can delay a client between
                        // connecting and writing; this is a hang budget, not a
                        // latency expectation for the local exchange.
                        let budget = Some(Duration::from_secs(30));
                        stream
                            .set_read_timeout(budget)
                            .expect("bounded request read");
                        stream
                            .set_write_timeout(budget)
                            .expect("bounded response write");
                        serve(stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("stub accept: {error}"),
                }
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let worker = self.worker.take().expect("one server worker");
        // Never join a running worker: a socket or handler regression must
        // report against the owning test instead of holding the test process.
        let deadline = Instant::now() + Duration::from_secs(120);
        while !worker.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if std::thread::panicking() {
            // Do not turn an existing assertion failure into a double-panic abort.
            return;
        }
        assert!(
            worker.is_finished(),
            "stub worker did not stop before its deadline"
        );
        worker.join().expect("stub worker failed");
    }
}
