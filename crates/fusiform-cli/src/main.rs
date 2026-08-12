#![forbid(unsafe_code)]

//! `ck-models` — the operator CLI, dispatched as `ck models <verb>`.
//!
//! `ck` resolves an unknown domain by running `ck-<domain>` from PATH with the
//! tail passed verbatim and the exit code propagated, so this binary needs no
//! registration and `ck` needs no change. Confirmed with SUBC rather than
//! inferred from reading their dispatcher.
//!
//! # Why this talks to the daemon instead of opening the database
//!
//! The obvious implementation reads the SQLite file directly. It is wrong here,
//! for two reasons that were verified rather than assumed:
//!
//! **The lease.** `cortexkit-store`'s only production open path acquires the
//! single-writer lease, so a CLI using the supported API would be refused while
//! the daemon runs — or, worse, would succeed while the daemon is down and hold
//! the lease against its restart.
//!
//! **The path.** A reader that derives the store path by convention can land on
//! a different file than the module opened. subc's own storage resolver
//! documents the case: a module that keys its own storage before connecting can
//! end up with a nested database while an empty file sits at the conventional
//! path, and "a reader inspecting that directory would reasonably conclude the
//! module has an empty store". The daemon knows which file is real because it
//! handed the module the descriptor; the CLI does not.
//!
//! So the CLI is a subc consumer. It asks the module, and the module answers
//! from the store it actually opened.
//!
//! # Usage
//!
//! ```text
//! ck models status [--polls N]
//! ck models get [--provider P] [--model M] [--at MS] [--rates|--capabilities]
//!               [--include-retired]
//! ck models history --provider P --model M --fact rate.input
//! ck models correct --provider P --model M --field rate.input
//!                   --from MS --until MS --reason docs/findings/... [--commit]
//! ```

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process,
    time::Duration,
};

use fusiform_store::prefix;
use subc_client_rs::consumer::{CallOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::{BindIdentity, RouteTarget};

/// How long to wait for the module to answer.
///
/// Longer than a typical interactive call because a full catalog read renders
/// megabytes: measured, the whole fact set is 3.0 MB of JSON. A timeout that
/// fires on the largest legitimate response teaches an operator to distrust the
/// tool.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

const USAGE: &str = "\
usage: ck models <command> [options]

commands:
  status                    what fusiform has been doing: recent polls,
                            catalog version, row counts
  get                       read the catalog
  history                   every recorded era for one fact
  correct                   record that fusiform's own record was wrong over a
                            past window (previews by default; --commit writes)

options:
  --provider <id>           narrow to one provider
  --model <id>              narrow to one model (requires --provider)
  --fact <key>              which fact, for history
                            (rate.input, limit.context, existence, ...)
  --at <epoch-ms>           resolve the catalog at a past instant
  --rates                   only pricing facts
  --capabilities            only capability and limit facts
  --include-retired         include models the upstream stopped publishing
  --polls <n>               how many recent polls status should show

correct options:
  --field <key>             a fact this correction names; repeatable
                            (rate.input, limit.context, capability.reasoning,
                            existence)
  --from <epoch-ms>         start of the bad window; a LOWER bound, so when the
                            true start is unknown use the earliest plausible
                            instant rather than a best guess
  --until <epoch-ms>        when the fix landed and fusiform stopped recording
                            the bad value
  --reason <path>           the finding document explaining the defect
  --commit                  actually write it (without this, nothing changes)
  --subc <path>             connection file (default: the per-user path)
  --json                    print the raw response instead of a summary
";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    disown_inherited_module_identity();

    if let Err(e) = run(env::args_os()).await {
        eprintln!("ck models: {e}");
        process::exit(1);
    }
}

/// Drop any module identity inherited from the environment.
///
/// `subc-client-rs` falls back to `SUBC_MODULE_ID` and `SUBC_LAUNCH_NONCE` from
/// the process environment when a call carries no explicit consumer identity,
/// and there is no way to say "explicitly none" — an absent identity in
/// `CallOptions` *means* "read the environment".
///
/// Those variables are injected by the supervisor into a supervised module's
/// process. A CLI is not a module. But an operator running this from a shell
/// inside a module's environment — which is how an agent-driven invocation
/// happens — inherits that module's id and its live nonce, and every route this
/// CLI opens is then attributed to that module.
///
/// Found by running the CLI against a lab daemon, where it failed with
/// `bad_consumer_identity (consumer_identity for module_id 'aft' did not match
/// a supervised launch nonce)`. The failure was luck: the lab daemon has no
/// `aft` module, so the nonce did not match. Against the real daemon the nonce
/// WOULD have matched, the call would have succeeded, and the impersonation
/// would have been invisible.
///
/// Clearing them here rather than passing an explicit identity, because the
/// honest statement is that this process has no module identity at all — not
/// that it has a different one.
fn disown_inherited_module_identity() {
    // Before any task is spawned, so no other thread can be reading the
    // environment concurrently.
    env::remove_var("SUBC_MODULE_ID");
    env::remove_var("SUBC_LAUNCH_NONCE");
}

struct Args {
    command: String,
    provider: Option<String>,
    model: Option<String>,
    fact: Option<String>,
    at_ms: Option<i64>,
    prefixes: Option<Vec<String>>,
    include_retired: bool,
    polls: Option<u32>,
    fields: Vec<String>,
    from_ms: Option<i64>,
    until_ms: Option<i64>,
    reason: Option<String>,
    commit: bool,
    connection_file: PathBuf,
    json: bool,
}

async fn run(argv: impl IntoIterator<Item = OsString>) -> Result<(), String> {
    let args = parse_args(argv)?;

    let (tool, arguments) = match args.command.as_str() {
        "status" => {
            let mut a = serde_json::Map::new();
            if let Some(n) = args.polls {
                a.insert("polls".into(), serde_json::json!(n));
            }
            (
                fusiform_module::route::TOOL_STATUS,
                serde_json::Value::Object(a),
            )
        }
        "get" => {
            let mut a = serde_json::Map::new();
            if let Some(p) = &args.provider {
                a.insert("provider_id".into(), serde_json::json!(p));
            }
            if let Some(m) = &args.model {
                a.insert("model_id".into(), serde_json::json!(m));
            }
            if let Some(at) = args.at_ms {
                a.insert("at_ms".into(), serde_json::json!(at));
            }
            if let Some(p) = &args.prefixes {
                a.insert("fact_prefixes".into(), serde_json::json!(p));
            }
            if args.include_retired {
                a.insert("include_retired".into(), serde_json::json!(true));
            }
            (
                fusiform_module::route::TOOL_GET,
                serde_json::Value::Object(a),
            )
        }
        "history" => {
            let provider = args.provider.as_ref().ok_or("history needs --provider")?;
            let model = args.model.as_ref().ok_or("history needs --model")?;
            let fact = args.fact.as_ref().ok_or(
                "history needs --fact (rate.input, limit.context, capability.reasoning, existence)",
            )?;
            (
                fusiform_module::route::TOOL_HISTORY,
                serde_json::json!({
                    "provider_id": provider,
                    "model_id": model,
                    "fact_key": fact,
                }),
            )
        }
        "correct" => {
            let provider = args.provider.as_ref().ok_or("correct needs --provider")?;
            let model = args.model.as_ref().ok_or("correct needs --model")?;
            let reason = args
                .reason
                .as_ref()
                .ok_or("correct needs --reason naming the finding document")?;
            let from = args.from_ms.ok_or("correct needs --from")?;
            let until = args.until_ms.ok_or("correct needs --until")?;
            if args.fields.is_empty() {
                return Err("correct needs at least one --field".to_string());
            }

            // Fact keys are translated to the served FieldId spelling here so an
            // operator types what they already read in `history` and `get`
            // output, rather than a JSON object.
            let fields: Vec<serde_json::Value> = args
                .fields
                .iter()
                .map(|k| field_id_for_fact_key(k))
                .collect::<Result<_, _>>()?;

            (
                fusiform_module::route::TOOL_CORRECT,
                serde_json::json!({
                    "provider_id": provider,
                    "model_id": model,
                    "fields": fields,
                    "affected_from_ms": from,
                    "affected_until_ms": until,
                    "reason": reason,
                    "dry_run": !args.commit,
                }),
            )
        }
        other => return Err(format!("unknown command {other:?}\n\n{USAGE}")),
    };

    let response = call(&args.connection_file, tool, arguments).await?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&response).map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    match args.command.as_str() {
        "status" => print_status(&response),
        "get" => print_catalog(&response),
        "history" => print_history(&response),
        "correct" => print_correction(&response),
        _ => unreachable!("the command was validated above"),
    }
    Ok(())
}

/// Open a route to fusiform, make one call, and close.
async fn call(
    connection_file: &Path,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let consumer = SubcConsumer::connect(connection_file, ConsumerOptions::default())
        .await
        .map_err(|e| {
            format!(
                "could not reach subc at {}: {e}\n\
                 is the daemon running? override the path with --subc",
                connection_file.display()
            )
        })?;

    let identity = BindIdentity {
        project_root: env::current_dir().map_err(|e| e.to_string())?,
        harness: "fusiform-cli".to_string(),
        // A per-invocation session: the CLI is not a durable participant, and
        // reusing one id across invocations would make two concurrent operator
        // commands look like one session to anything counting them.
        session: format!("cli-{}", process::id()),
    };

    let body = serde_json::to_vec(&serde_json::json!({
        "name": tool,
        "arguments": arguments,
    }))
    .map_err(|e| e.to_string())?;

    let opts = CallOptions {
        timeout: CALL_TIMEOUT,
        ..CallOptions::default()
    };

    let raw = consumer
        .call(
            RouteTarget::ToolProvider {
                module_id: fusiform_module::MODULE_ID.to_string(),
            },
            identity,
            body,
            opts,
        )
        .await
        .map_err(|e| format!("{tool} failed: {e}"))?;

    consumer.close().await;

    serde_json::from_slice(&raw)
        .map_err(|e| format!("{tool} returned a body that did not parse: {e}"))
}

fn print_status(response: &serde_json::Value) {
    let get = |k: &str| response.get(k);
    println!(
        "source           {}",
        get("source").and_then(|v| v.as_str()).unwrap_or("?")
    );
    println!(
        "catalog version  {}",
        get("catalog_version")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1)
    );
    println!(
        "models           {}",
        get("model_count").and_then(|v| v.as_i64()).unwrap_or(-1)
    );
    println!(
        "eras             {}",
        get("era_count").and_then(|v| v.as_i64()).unwrap_or(-1)
    );

    let Some(polls) = get("recent_polls").and_then(|v| v.as_array()) else {
        return;
    };
    if polls.is_empty() {
        // Distinct from "no changes": a store with no observations at all has
        // never polled, which is a different situation from polling quietly.
        println!("\nno polls recorded yet");
        return;
    }

    println!("\nrecent polls (newest first)");
    for poll in polls {
        let at = poll
            .get("observed_at_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let outcome = poll.get("outcome").and_then(|v| v.as_str()).unwrap_or("?");
        let class = poll
            .get("failure_class")
            .and_then(|v| v.as_str())
            .map(|c| format!(" ({c})"))
            .unwrap_or_default();
        let took = poll
            .get("duration_ms")
            .and_then(|v| v.as_i64())
            .map(|d| format!("  {d}ms"))
            .unwrap_or_default();
        println!("  {}  {outcome}{class}{took}", format_instant(at));
    }
}

fn print_catalog(response: &serde_json::Value) {
    let resolved = response
        .get("resolved_at_ms")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let empty = serde_json::Map::new();
    let models = response
        .get("models")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);

    println!(
        "{} models at {}  (catalog version {})",
        models.len(),
        format_instant(resolved),
        response
            .get("catalog_version")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1)
    );

    // A single model prints its facts; a whole catalog prints one line each,
    // because 6,253 models times eleven facts is not something to read.
    if models.len() == 1 {
        for (identity, facts) in models {
            println!("\n{identity}");
            let facts: BTreeMap<_, _> = facts
                .as_object()
                .map(|o| o.iter().collect())
                .unwrap_or_default();
            for (key, value) in facts {
                println!("  {key:<34}  {value}");
            }
        }
        print_withheld(response);
        return;
    }

    println!();
    for identity in models.keys() {
        println!("  {identity}");
    }

    print_withheld(response);
}

/// Report facts the read refused to answer.
///
/// Printed after the models rather than folded into them, because a withheld
/// fact is absent from that list. Without this an operator sees a model with no
/// input rate and concludes the upstream publishes none.
fn print_withheld(response: &serde_json::Value) {
    let Some(withheld) = response.get("withheld").and_then(|v| v.as_array()) else {
        return;
    };
    if withheld.is_empty() {
        return;
    }

    println!(
        "\n{} fact(s) withheld — the record fusiform holds for these is known bad:",
        withheld.len()
    );
    for item in withheld {
        let model = item.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        let fact = item.get("fact_key").and_then(|v| v.as_str()).unwrap_or("?");
        println!("  {model}  {fact}");
        if let Some(corrections) = item.get("corrections").and_then(|v| v.as_array()) {
            for c in corrections {
                let from = c
                    .get("affected_from_ms")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let until = c
                    .get("affected_until_ms")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let reason = c.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                println!(
                    "      {} to {}: {reason}",
                    format_instant(from),
                    format_instant(until)
                );
            }
        }
    }
}

fn print_history(response: &serde_json::Value) {
    println!(
        "{}/{}  {}",
        response
            .get("provider_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        response
            .get("model_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        response
            .get("fact_key")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
    );

    let Some(eras) = response.get("eras").and_then(|v| v.as_array()) else {
        return;
    };
    if eras.is_empty() {
        // Not the same as a fact that never changed: no eras means the store
        // has never recorded this fact at all, usually a typo in the key.
        println!("\nno eras recorded for this fact");
        return;
    }

    println!();
    for era in eras {
        let at = era
            .get("boundary_at_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let kind = era
            .get("boundary_kind")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let value = era.get("value").cloned().unwrap_or(serde_json::Value::Null);

        // The window is the honest part: for an observed boundary the change
        // happened somewhere inside it, and printing only the boundary would
        // imply fusiform saw the moment it happened.
        let window = match era.get("window_from_ms").and_then(|v| v.as_i64()) {
            Some(from) => format!(
                "  (changed between {} and {})",
                format_instant(from),
                format_instant(at)
            ),
            None => String::new(),
        };

        println!("  {}  {kind:<10} {value}{window}", format_instant(at));

        // A `corrected` boundary without its extent tells an operator that
        // something was wrong and not what. The reason is recorded in the
        // store; printing the kind alone leaves it there.
        if let Some(c) = era.get("correction") {
            let from = c
                .get("affected_from_ms")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let until = c
                .get("affected_until_ms")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let reason = c.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            println!(
                "                        corrects {} to {}: {reason}",
                format_instant(from),
                format_instant(until)
            );
        }
    }
}

/// Render an epoch-millisecond instant.
///
/// Printed as the raw epoch value plus a UTC rendering, because an operator
/// comparing against a log line needs the number and an operator reading a
/// history needs the date. Formatting by hand rather than adding a date
/// dependency to a CLI that needs exactly this one thing.
fn format_instant(ms: i64) -> String {
    if ms <= 0 {
        return "-".to_string();
    }
    let secs = ms / 1000;
    let days = secs / 86_400;
    let time_of_day = secs % 86_400;
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02}Z",
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    )
}

/// Days since the Unix epoch to a civil date.
///
/// Howard Hinnant's `civil_from_days`, the same algorithm the standard date
/// libraries use. Correct across leap years and centuries, which a naive
/// division is not.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn parse_args(argv: impl IntoIterator<Item = OsString>) -> Result<Args, String> {
    let mut it = argv.into_iter().skip(1);
    let command = it
        .next()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| format!("a command is required\n\n{USAGE}"))?;

    if command == "--help" || command == "-h" || command == "help" {
        println!("{USAGE}");
        process::exit(0);
    }

    let mut args = Args {
        command,
        provider: None,
        model: None,
        fact: None,
        at_ms: None,
        prefixes: None,
        include_retired: false,
        polls: None,
        fields: Vec::new(),
        from_ms: None,
        until_ms: None,
        reason: None,
        commit: false,
        connection_file: subc_core::bootstrap::connection_file_path(),
        json: false,
    };

    while let Some(flag) = it.next() {
        let flag = flag.to_string_lossy().into_owned();
        let mut value = || {
            it.next()
                .map(|v| v.to_string_lossy().into_owned())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--provider" => args.provider = Some(value()?),
            "--model" => args.model = Some(value()?),
            "--fact" => args.fact = Some(value()?),
            "--at" => {
                let raw = value()?;
                args.at_ms = Some(
                    raw.parse()
                        .map_err(|_| format!("--at needs epoch milliseconds, got {raw:?}"))?,
                );
            }
            "--polls" => {
                let raw = value()?;
                args.polls = Some(
                    raw.parse()
                        .map_err(|_| format!("--polls needs a number, got {raw:?}"))?,
                );
            }
            // Repeatable: one defect commonly touches several facts, and each
            // --field adds to the extent rather than replacing it.
            "--field" => args.fields.push(value()?),
            "--from" => {
                let raw = value()?;
                args.from_ms = Some(
                    raw.parse()
                        .map_err(|_| format!("--from needs epoch milliseconds, got {raw:?}"))?,
                );
            }
            "--until" => {
                let raw = value()?;
                args.until_ms = Some(
                    raw.parse()
                        .map_err(|_| format!("--until needs epoch milliseconds, got {raw:?}"))?,
                );
            }
            "--reason" => args.reason = Some(value()?),
            "--commit" => args.commit = true,
            "--subc" => args.connection_file = PathBuf::from(value()?),
            // Prefixes come from the store's own constants, not string
            // literals here. A prefix that matches nothing returns an empty
            // catalog rather than an error, so a drifted literal would make
            // `--rates` quietly print nothing at all.
            "--rates" => args.prefixes = Some(vec![prefix::RATE.to_string()]),
            "--capabilities" => {
                args.prefixes = Some(vec![
                    prefix::CAPABILITY.to_string(),
                    prefix::LIMIT.to_string(),
                ])
            }
            "--include-retired" => args.include_retired = true,
            "--json" => args.json = true,
            other => return Err(format!("unknown option {other:?}\n\n{USAGE}")),
        }
    }

    Ok(args)
}

/// Translate a fact key into the `FieldId` the wire expects.
///
/// An operator types the key they read in `history` and `get` output. Building
/// the JSON object here rather than making them write it keeps one spelling of
/// a fact across every verb — and a key this cannot translate is refused by
/// name, listing what it can, rather than being passed through to fail with a
/// serde message about an unknown variant.
fn field_id_for_fact_key(key: &str) -> Result<serde_json::Value, String> {
    // Rates carry their token class.
    if let Some(class) = key.strip_prefix("rate.") {
        if class.contains(".above_context.") {
            return Err(format!(
                "{key} is a tiered rate; tier corrections carry a threshold this \
                 command cannot address yet"
            ));
        }
        return Ok(serde_json::json!({"field": "rate", "class": class}));
    }
    if let Some(limit) = key.strip_prefix("limit.") {
        return Ok(serde_json::json!({"field": "limit", "limit": limit}));
    }
    if let Some(capability) = key.strip_prefix("capability.") {
        return Ok(serde_json::json!({"field": "capability", "capability": capability}));
    }
    if key == "existence" {
        return Ok(serde_json::json!({"field": "existence"}));
    }
    Err(format!(
        "{key:?} is not a fact this catalog can correct; expected one of: {}",
        fusiform_store::ingest::SERVED_FACT_NAMESPACE.join(", ")
    ))
}

/// Print what a correction did, or would do.
fn print_correction(response: &serde_json::Value) {
    let written = response
        .get("written")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let facts = response
        .get("facts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if written {
        println!("recorded a correction over {} fact(s)", facts.len());
    } else {
        println!(
            "PREVIEW — nothing written. {} fact(s) would be corrected.",
            facts.len()
        );
    }
    println!();
    println!(
        "  window   {} to {}",
        format_instant(
            response
                .get("affected_from_ms")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
        ),
        format_instant(
            response
                .get("affected_until_ms")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
        ),
    );
    if let Some(reason) = response.get("reason").and_then(|v| v.as_str()) {
        println!("  reason   {reason}");
    }
    println!();
    println!("  reads inside that window will refuse and name this correction.");
    println!("  the value below stays in force and is NOT changed:");
    println!();

    for fact in &facts {
        let key = fact.get("fact_key").and_then(|v| v.as_str()).unwrap_or("?");
        let value = fact
            .get("value")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into());
        let since = fact
            .get("current_since_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        println!("    {key:<32}  {value}");
        println!("    {:<32}  in force since {}", "", format_instant(since));
    }

    if !written {
        println!();
        println!("  add --commit to record it.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Date arithmetic, against instants whose rendering is known independently.
    ///
    /// Hand-written calendar code fails in the way that matters least often and
    /// costs most: it is right for today's date and wrong across a leap day or
    /// a century boundary, so it looks correct for months. The expected strings
    /// here were produced by `date -u -r <seconds>` rather than by this code.
    ///
    /// One of them was not, on the first attempt: an expected time was written
    /// from memory under a comment claiming it came from `date`, and the run
    /// showed the code right and the expectation wrong by eight hours. A cited
    /// source that was not consulted is worse than an uncited guess, because it
    /// stops the next reader from checking.
    #[test]
    fn instants_render_as_the_dates_they_are() {
        // One millisecond after the epoch, which is still second zero.
        assert_eq!(format_instant(1), "1970-01-01 00:00:00Z");
        // date -u -r 1 => Thu Jan  1 00:00:01 UTC 1970
        assert_eq!(format_instant(1_000), "1970-01-01 00:00:01Z");

        // A leap day, which a naive 365-day division gets wrong.
        // date -u -r 1709208000 => Thu Feb 29 12:00:00 UTC 2024
        assert_eq!(format_instant(1_709_208_000_000), "2024-02-29 12:00:00Z");

        // 2000 is a leap year (divisible by 400) while 1900 is not: the two
        // cases a "divisible by 4" rule gets backwards.
        // date -u -r 951825600 => Tue Feb 29 12:00:00 UTC 2000
        assert_eq!(format_instant(951_825_600_000), "2000-02-29 12:00:00Z");

        // A timestamp taken from a real upstream observation, kept as a fixed
        // value so the rendering can be checked against an independent source.
        // date -u -r 1784494281 => Sun Jul 19 20:51:21 UTC 2026
        assert_eq!(format_instant(1_784_494_281_391), "2026-07-19 20:51:21Z");

        // Year boundaries in both directions.
        // date -u -r 1767225599 => Wed Dec 31 23:59:59 UTC 2025
        assert_eq!(format_instant(1_767_225_599_000), "2025-12-31 23:59:59Z");
        // date -u -r 1767225600 => Thu Jan  1 00:00:00 UTC 2026
        assert_eq!(format_instant(1_767_225_600_000), "2026-01-01 00:00:00Z");
    }

    /// A module identity inherited from the environment is dropped.
    ///
    /// `subc-client-rs` reads `SUBC_MODULE_ID` and `SUBC_LAUNCH_NONCE` from the
    /// process environment when a call carries no explicit consumer identity,
    /// and an absent identity in `CallOptions` MEANS "read the environment" —
    /// there is no way to say "explicitly none". So the only way for this CLI
    /// to have no module identity is for those variables not to be set.
    ///
    /// Found against a live daemon: the CLI inherited `SUBC_MODULE_ID=aft` from
    /// the shell it was run in and every route it opened was attributed to that
    /// module. It failed only because the lab daemon has no `aft` module, so
    /// the nonce did not match. Against the daemon that launched the shell it
    /// would have matched, and the impersonation would have been silent.
    #[test]
    fn an_inherited_module_identity_is_dropped() {
        // Deliberately set both, as a supervised module's environment has them.
        env::set_var("SUBC_MODULE_ID", "aft");
        env::set_var("SUBC_LAUNCH_NONCE", "a-live-nonce");
        assert!(
            env::var("SUBC_MODULE_ID").is_ok(),
            "the test must actually set the variable, or it proves nothing"
        );

        disown_inherited_module_identity();

        assert!(
            env::var("SUBC_MODULE_ID").is_err(),
            "an inherited module id must not survive: this CLI is not a module"
        );
        assert!(
            env::var("SUBC_LAUNCH_NONCE").is_err(),
            "an inherited launch nonce must not survive"
        );
    }

    /// A zero or negative instant renders as absent rather than as 1970.
    ///
    /// A missing timestamp printed as `1970-01-01` reads like a real date and
    /// sends an operator looking for what happened in 1970.
    #[test]
    fn an_absent_instant_is_not_printed_as_1970() {
        assert_eq!(format_instant(0), "-");
        assert_eq!(format_instant(-1), "-");
    }

    /// Every month renders with the right number of days around it.
    ///
    /// Walks a full non-leap year day by day and checks the sequence never
    /// skips or repeats, which catches an off-by-one in the month arithmetic
    /// that a handful of spot checks would miss.
    #[test]
    fn a_full_year_of_days_is_continuous() {
        let start = 1_735_689_600i64; // 2025-01-01T00:00:00Z
        let mut expected_day = 1u32;
        let mut expected_month = 1u32;
        const DAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

        for offset in 0..365 {
            let rendered = format_instant((start + offset * 86_400) * 1000);
            let want = format!("2025-{expected_month:02}-{expected_day:02} 00:00:00Z");
            assert_eq!(rendered, want, "day {offset} of 2025");

            expected_day += 1;
            if expected_day > DAYS[(expected_month - 1) as usize] {
                expected_day = 1;
                expected_month += 1;
            }
        }
        assert_eq!(expected_month, 13, "the walk must cover all twelve months");
    }
}
