// Every integration test of this crate compiles into this one test binary, so
// the crate links and code-signs one executable rather than one per file.
// Each file below stays its own module. A test that needs a process of its own
// (one that sets environment variables, changes the working directory or
// supplies its own main) belongs beside this directory as tests/<name>.rs.
// Scope a run to one file with `cargo test -p fusiform-module --test it <file>::`.

mod bootstrap;
mod conditional_get;
mod correct_route;
mod creator_table;
mod failure_path;
mod golden_payload;
mod health_detail;
mod live;
mod mode_rates;
mod reasoning_options;
mod restart_adoption;
mod restart_health;
mod route;
mod scope_and_preset;
mod served_corrections;
mod shrink_guard;
mod tick;
mod window_overlay;
mod wire_fact_table;
mod wire_types_decode_the_wire;
