// Every integration test of this crate compiles into this one test binary, so
// the crate links and code-signs one executable rather than one per file.
// Each file below stays its own module. A test that needs a process of its own
// (one that sets environment variables, changes the working directory or
// supplies its own main) belongs beside this directory as tests/<name>.rs.
// Scope a run to one file with `cargo test -p fusiform-core --test it <file>::`.

mod full_payload;
mod measured_fields_are_read_or_declared;
mod models_dev_normalization;
