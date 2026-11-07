//! `ck-fusiform --manifest` prints the registration manifest on a bare binary,
//! and `--version` answers there too.
//!
//! Before installing a new build, the fleet's deployment tooling runs
//! `--manifest` on the staged binary, with no daemon running, and compares the
//! manifest's `provenance` (the commit the build was made from) with the commit
//! the deployment request names. A binary that doesn't answer `--manifest`
//! leaves that check unable to run.
//!
//! The probe runs the binary cargo built for this package
//! (`CARGO_BIN_EXE_ck-fusiform`), through a `ckdev-` link: on this machine a
//! `ck-` process name means a placed production binary.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use subc_protocol::manifest::ModuleManifest;
use subc_protocol::PROTOCOL_VERSION;

/// Links the built `ck-fusiform` to a `ckdev-` name in `scratch`.
///
/// One link serves every probe in this file, made by the one test that runs
/// them in sequence. When the hard link falls back to a copy, the copy is a
/// file open for writing; on Linux, a process another test thread forks in that
/// moment inherits the open file, and executing it then fails with "Text file
/// busy". Two tests each copying and executing their own link hit exactly that.
fn ckdev_link(scratch: &std::path::Path) -> std::path::PathBuf {
    let built = std::path::Path::new(env!("CARGO_BIN_EXE_ck-fusiform"));
    let link = scratch.join(format!("ckdev-fusiform{}", std::env::consts::EXE_SUFFIX));
    if std::fs::hard_link(built, &link).is_err() {
        std::fs::copy(built, &link).expect("copy ck-fusiform to the ckdev name");
    }
    link
}

/// Runs `link` with `arg`, as deployment tooling would on a bare binary,
/// and returns its output.
fn probe(link: &std::path::Path, arg: &str) -> std::process::Output {
    let mut child = Command::new(link)
        .arg(arg)
        // The bare-binary case: no connection argument and no supervisor
        // environment, which is how the gate runs it.
        .env_remove("SUBC_MODULE_ID")
        .env_remove("SUBC_LAUNCH_NONCE")
        .env_remove("SUBC_LAUNCH_NONCE_FD")
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("ckdev-fusiform {arg} must start: {e}"));
    // A deadline so a probe that waits for a daemon fails here by name rather
    // than hanging the suite. The manifest is a few kilobytes, far below a pipe
    // buffer, so the child never blocks on output while this loop polls.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the probe") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill the probe after its deadline");
            child.wait().expect("reap the probe after kill");
            panic!("ckdev-fusiform {arg} exceeded its 10 s deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = child
        .wait_with_output()
        .expect("collect the probe's output");
    assert_eq!(output.status, status, "the reaped status is the one polled");
    output
}

#[test]
fn the_daemon_binary_answers_manifest_and_version_with_no_daemon() {
    // Not `tempfile::tempdir()`: on a Mac that is under /var/folders, where a
    // fresh executable can be held long enough to trip the probe's deadline.
    let scratch = tempfile::Builder::new()
        .prefix("fusiform-exec-")
        .tempdir_in(fusiform_testkit::exec_scratch_base())
        .expect("scratch dir for the ckdev link");
    let link = ckdev_link(scratch.path());
    manifest_is_printed(&link);
    version_is_answered(&link);
}

fn manifest_is_printed(link: &std::path::Path) {
    let output = probe(link, "--manifest");
    assert!(
        output.status.success(),
        "--manifest exited {:?}: the gate runs it with no daemon, so it must not \
         need one. stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let printed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--manifest prints one JSON value");

    // The whole printed value equals the library's, so the binary prints the
    // manifest the module registers with, provenance included, rather than a
    // separate copy that could drift from it.
    assert_eq!(printed, fusiform_module::manifest_json());
    assert_eq!(printed["runtime_computed"], serde_json::json!([]));

    // Decoded through the protocol's own type, which is what the gate does and
    // which refuses a malformed `runtime_computed`.
    let manifest: ModuleManifest =
        serde_json::from_value(printed).expect("the printed manifest decodes as a ModuleManifest");
    assert_eq!(manifest.module_id, fusiform_module::MODULE_ID);
    assert_eq!(manifest.protocol_ver, PROTOCOL_VERSION);
}

/// `--version` and `-V` answer on the bare binary with no daemon context, the
/// way deployment tooling or an incident responder runs them. The CLI crate's
/// version test covers `ck-models`; this binary belongs to this package, so
/// its probe lives here, where cargo names its path (`CARGO_BIN_EXE_*`)
/// instead of a test guessing it from its own location.
fn version_is_answered(link: &std::path::Path) {
    for flag in ["--version", "-V"] {
        let output = probe(link, flag);
        assert!(
            output.status.success(),
            "ck-fusiform {flag} exited {:?}: a version probe runs where no daemon \
             context exists. stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        let line = String::from_utf8_lossy(&output.stdout);
        assert!(
            line.starts_with("ck-fusiform"),
            "ck-fusiform {flag} must name itself first: {line:?}"
        );
        assert!(
            line.contains("schema "),
            "ck-fusiform {flag} must state the wire schema version: {line:?}"
        );
    }
}
