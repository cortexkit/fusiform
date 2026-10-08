//! `--version` must answer on a bare binary, before any config gate.
//!
//! SUBC's placement ladder invoked `ck-fusiform --version` and got
//! `MissingSubcArg` with exit 1, because the argument parser required the subc
//! connection argument first. It fell back to timing an argument refusal.
//!
//! That is the failure the probe exists to prevent: a version probe runs on a
//! bare binary with no daemon context, by a deploy ladder or an incident
//! responder, which is exactly when no config is available. Gating it behind
//! config makes it fail in the only situation it is for.
//!
//! Fleet convention, from CKCRED via SUBC: name, version and build rev before
//! any config gate.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

fn read_pipe(pipe: impl Read + Send + 'static) -> Receiver<std::io::Result<Vec<u8>>> {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = {
            let mut pipe = pipe;
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        };
        let _ = send.send(result);
    });
    receive
}

fn bounded_output(command: &mut Command, budget: Duration, label: &str) -> Output {
    let deadline = Instant::now() + budget;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("{label} must run: {error}"));
    // Drain both pipes concurrently so a full stderr cannot block stdout (or
    // vice versa). Receiving the captured bytes has the same overall deadline.
    let stdout = read_pipe(child.stdout.take().unwrap());
    let stderr = read_pipe(child.stderr.take().unwrap());
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .unwrap_or_else(|error| panic!("{label}: {error}"))
        {
            break status;
        }
        if Instant::now() >= deadline {
            child
                .kill()
                .unwrap_or_else(|error| panic!("{label}: kill after deadline: {error}"));
            child
                .wait()
                .unwrap_or_else(|error| panic!("{label}: reap after kill: {error}"));
            panic!("{label} exceeded its subprocess deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let capture = |receiver: Receiver<std::io::Result<Vec<u8>>>| {
        receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| panic!("{label}: output deadline: {error}"))
            .unwrap_or_else(|error| panic!("{label}: capture output: {error}"))
    };
    Output {
        status,
        stdout: capture(stdout),
        stderr: capture(stderr),
    }
}

/// Where cargo put the binary `name` for this test profile, with the platform's
/// executable suffix (`.exe` on Windows, where a bare name finds nothing and
/// the probe below would skip both binaries).
fn binary(name: &str) -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop(); // deps/
    path.pop(); // profile dir
    path.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    path
}

/// A link to `built` named `ckdev-<suffix>`, in a scratch directory this test owns.
///
/// On this machine a `ck-` process name means a production binary placed in the
/// fleet's bin directory, so the live fleet can be told apart from test runs in
/// a process list. Running cargo's `target/*/ck-fusiform` directly would show a
/// second `ck-fusiform` beside the live module. A hard link keeps the same
/// inode and bytes, so the probe still runs exactly the binary cargo built;
/// copying is the fallback when the scratch directory is on another volume.
fn ckdev_link(built: &std::path::Path, scratch: &std::path::Path) -> std::path::PathBuf {
    let name = built
        .file_name()
        .and_then(|n| n.to_str())
        .expect("binary file name");
    let suffix = name.strip_prefix("ck-").expect("a ck- binary");
    let link = scratch.join(format!("ckdev-{suffix}"));
    if std::fs::hard_link(built, &link).is_err() {
        std::fs::copy(built, &link)
            .unwrap_or_else(|error| panic!("{} -> {}: {error}", built.display(), link.display()));
    }
    link
}

#[cfg(unix)]
#[test]
#[should_panic(expected = "sleep version probe exceeded its subprocess deadline")]
fn a_hung_version_probe_is_killed_at_its_deadline() {
    // exec leaves no grandchild behind when the deadline kills the command.
    bounded_output(
        Command::new("sh").args(["-c", "exec sleep 60"]),
        Duration::from_millis(100),
        "sleep version probe",
    );
}

#[test]
fn both_binaries_answer_version_with_no_arguments_and_no_daemon() {
    let mut checked = 0;
    let scratch = tempfile::tempdir().expect("scratch dir for ckdev links");
    for name in ["ck-fusiform", "ck-models"] {
        let built = binary(name);
        if !built.exists() {
            eprintln!("skipping {name}: not built in this profile");
            continue;
        }
        checked += 1;
        let path = ckdev_link(&built, scratch.path());

        for flag in ["--version", "-V"] {
            let out = bounded_output(
                Command::new(&path)
                    .arg(flag)
                    // Deliberately no SUBC_* environment and no connection
                    // argument: the bare-binary case.
                    .env_remove("SUBC_MODULE_ID")
                    .env_remove("SUBC_LAUNCH_NONCE")
                    .env_remove("XDG_RUNTIME_DIR"),
                Duration::from_secs(10),
                &format!("{name} {flag}"),
            );

            assert!(
                out.status.success(),
                "{name} {flag} exited {:?} — a version probe runs when no daemon \
                 context exists, so a config gate makes it fail exactly when it \
                 is needed. stderr: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            );

            let line = String::from_utf8_lossy(&out.stdout);
            assert!(
                line.starts_with(name),
                "{name} {flag} must name itself first, so a forensic reading a \
                 log knows which binary answered: {line:?}"
            );
            assert!(
                line.contains("schema "),
                "{name} {flag} must state the wire schema version, which is what \
                 a consumer's compatibility question is about: {line:?}"
            );
        }
    }

    assert!(
        checked > 0,
        "no binary was found to check — this test would pass while proving \
         nothing, which is the shape it exists to prevent elsewhere"
    );
}
