// Every integration test of this crate compiles into this one test binary, so
// the crate links and code-signs one executable rather than one per file.
// Each file below stays its own module. A test that needs a process of its own
// (one that sets environment variables, changes the working directory or
// supplies its own main) belongs beside this directory as tests/<name>.rs.
// Scope a run to one file with `cargo test -p fusiform-protocol --test it <file>::`.

mod billing_planes;
mod deps;
mod refusal_kinds;
