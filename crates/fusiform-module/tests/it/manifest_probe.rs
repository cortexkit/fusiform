//! `ck-fusiform --manifest` prints the registration manifest on a bare binary.
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

#[test]
fn the_daemon_binary_prints_its_registration_manifest_with_no_daemon() {
    let scratch = tempfile::tempdir().expect("scratch dir for the ckdev link");
    let built = std::path::Path::new(env!("CARGO_BIN_EXE_ck-fusiform"));
    let link = scratch
        .path()
        .join(format!("ckdev-fusiform{}", std::env::consts::EXE_SUFFIX));
    if std::fs::hard_link(built, &link).is_err() {
        std::fs::copy(built, &link).expect("copy ck-fusiform to the ckdev name");
    }

    let mut child = Command::new(&link)
        .arg("--manifest")
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
        .expect("ckdev-fusiform --manifest must start");
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
            panic!("ckdev-fusiform --manifest exceeded its 10 s deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = child
        .wait_with_output()
        .expect("collect the probe's output");

    assert!(
        status.success(),
        "--manifest exited {:?}: the gate runs it with no daemon, so it must not \
         need one. stderr: {}",
        status.code(),
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
