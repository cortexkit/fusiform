use std::sync::mpsc;
use std::time::Duration;

use fusiform_module::run_off_reader;
use subc_client_rs::HandlerOutcome;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[tokio::test(flavor = "current_thread")]
async fn blocked_request_keeps_reader_runtime_responsive() {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (progress_tx, progress_rx) = mpsc::channel();

    // An independent watchdog releases the work even if it runs inline and
    // stalls the only runtime thread: a Tokio timer alone could never fire in
    // that case. Success requires async progress before this wall-clock deadline.
    let watchdog = std::thread::spawn(move || {
        let progressed = progress_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        let _ = release_tx.send(());
        progressed
    });
    let request = tokio::spawn(run_off_reader(move || {
        let _ = entered_tx.send(());
        release_rx.recv().expect("watchdog releases the request");
        HandlerOutcome::Response(b"finished".to_vec())
    }));

    let entered = timeout(Duration::from_secs(1), entered_rx).await;
    // The request has started, and only this async probe can ask the watchdog
    // to release it early. This proves progress while request work is blocked,
    // rather than before it starts or after it finishes.
    let probe = tokio::spawn(async move {
        let _ = progress_tx.send(());
    });
    let progressed = timeout(Duration::from_secs(1), probe).await;
    let outcome = timeout(Duration::from_secs(3), request).await;
    let progressed_while_blocked = watchdog.join().expect("watchdog did not panic");

    assert!(
        progressed_while_blocked,
        "reader runtime stalled until the watchdog released the blocking request"
    );
    entered
        .expect("request starts within the deadline")
        .expect("request reports it has started");
    progressed
        .expect("async probe completes within the deadline")
        .expect("async probe did not panic");
    match outcome
        .expect("request finishes within the deadline after release")
        .expect("request task did not panic")
    {
        HandlerOutcome::Response(bytes) => assert_eq!(bytes, b"finished"),
        other => panic!("expected the work's response, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn panicking_request_returns_internal_error() {
    let outcome = timeout(
        Duration::from_secs(1),
        run_off_reader(|| panic!("request panic sentinel")),
    )
    .await
    .expect("a panicking request completes within the deadline");

    match outcome {
        HandlerOutcome::Error { code, message } => {
            assert_eq!(code, "internal");
            assert!(message.contains("request panicked"), "{message}");
        }
        other => panic!("expected an internal error, got {other:?}"),
    }
}
