// Every integration test of this crate compiles into this one test binary, so
// the crate links and code-signs one executable rather than one per file.
// Each file below stays its own module. A test that needs a process of its own
// (one that sets environment variables, changes the working directory or
// supplies its own main) belongs beside this directory as tests/<name>.rs.
// Scope a run to one file with `cargo test -p fusiform-store --test it <file>::`.

mod boundary_vocabulary;
mod correct;
mod correction_extent;
mod correction_never_denies_now;
mod corrections;
mod every_write_is_fenced;
mod failure_class_roundtrip;
mod full_ingest;
mod history;
mod ingest;
mod journal_mode;
mod mode_rates;
mod schema_doc;
mod seed_boundary;
mod serve;
mod served_vocabulary;
mod steady_state;
mod store_durability;
mod uncertain_reads;
