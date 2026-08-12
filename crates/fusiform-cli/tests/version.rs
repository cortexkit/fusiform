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

use std::process::Command;

fn binary(name: &str) -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop(); // deps/
    path.pop(); // profile dir
    path.push(name);
    path
}

#[test]
fn both_binaries_answer_version_with_no_arguments_and_no_daemon() {
    let mut checked = 0;
    for name in ["ck-fusiform", "ck-models"] {
        let path = binary(name);
        if !path.exists() {
            eprintln!("skipping {name}: not built in this profile");
            continue;
        }
        checked += 1;

        for flag in ["--version", "-V"] {
            let out = Command::new(&path)
                .arg(flag)
                // Deliberately no SUBC_* environment and no connection
                // argument: the bare-binary case.
                .env_remove("SUBC_MODULE_ID")
                .env_remove("SUBC_LAUNCH_NONCE")
                .env_remove("XDG_RUNTIME_DIR")
                .output()
                .unwrap_or_else(|e| panic!("{name} {flag} must run: {e}"));

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
