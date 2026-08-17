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
use std::fmt::Write as _;
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
  --at <epoch-ms>           resolve the catalog at a past instant (get only:
                            history returns the whole timeline)
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
    // Before argument parsing and before the environment is touched: a version
    // probe must work on a bare binary, because that is the situation a deploy
    // ladder and an incident responder invoke it in. Fleet convention, from
    // CKCRED via SUBC.
    if env::args().skip(1).any(|a| a == "--version" || a == "-V") {
        println!(
            "{}",
            fusiform_protocol::version_line("ck-models", env!("CARGO_PKG_VERSION"))
        );
        return;
    }

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
    /// An explicit `--subc` path, or `None` to discover.
    ///
    /// An override is EXCLUSIVE rather than first-in-a-list: a path the operator
    /// named and got wrong must fail loudly, not fall through to whichever
    /// daemon discovery happens to find. Otherwise the reply is true and about
    /// the wrong machine, and every later conclusion inherits that while the
    /// operator believes they are reading the one they named.
    connection_file: Option<PathBuf>,
    json: bool,
}

/// The tool name and request body a command produces, with no I/O.
///
/// # Why this is a function and not the body of `run`
///
/// It was inline, so the only way to see a request was to send one — which
/// meant the REQUEST BODY had never been asserted anywhere. Mutation proved
/// the cost: flipping `!args.commit` to `args.commit`, turning every preview
/// into a write and every commit into a no-op, survived the whole suite.
///
/// A test of the parsed FLAG cannot catch that, and I wrote one first: it
/// proved `--commit` sets a bool and said nothing about what crosses the
/// wire. Same defect as testing a rendered field instead of rendered output,
/// which this repo has now produced three times in a day — the artifact under
/// test has to be the thing that ships.
fn request_for(args: &Args) -> Result<(&'static str, serde_json::Value), String> {
    // A flag that does nothing must SAY so rather than be dropped.
    //
    // `--at` is a global option but only `get` resolves an instant: `history`
    // returns the whole timeline by construction (its request type has no
    // instant field), `status` describes the module, and `correct` takes a
    // window through --from/--until. Accepting it elsewhere and ignoring it
    // means an operator asking for the past gets a confident answer about
    // something else, with nothing in the output to say the flag was dropped.
    //
    // Found by asserting the request body: the test expected history to carry
    // --at, and the honest resolution is not to add the field but to refuse
    // the flag.
    if args.at_ms.is_some() && args.command != "get" {
        return Err(format!(
            "--at applies to `get` only; `{}` does not resolve an instant.\n\
             history returns the whole timeline for a fact, and correct takes \
             a window through --from and --until.",
            args.command
        ));
    }

    match args.command.as_str() {
        "status" => {
            let mut a = serde_json::Map::new();
            if let Some(n) = args.polls {
                a.insert("polls".into(), serde_json::json!(n));
            }
            Ok((
                fusiform_module::route::TOOL_STATUS,
                serde_json::Value::Object(a),
            ))
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
            Ok((
                fusiform_module::route::TOOL_GET,
                serde_json::Value::Object(a),
            ))
        }
        "history" => {
            let provider = args.provider.as_ref().ok_or("history needs --provider")?;
            let model = args.model.as_ref().ok_or("history needs --model")?;
            let fact = args.fact.as_ref().ok_or(
                "history needs --fact (rate.input, limit.context, capability.reasoning, existence)",
            )?;
            Ok((
                fusiform_module::route::TOOL_HISTORY,
                serde_json::json!({
                    "provider_id": provider,
                    "model_id": model,
                    "fact_key": fact,
                }),
            ))
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

            Ok((
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
            ))
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    }
}

async fn run(argv: impl IntoIterator<Item = OsString>) -> Result<(), String> {
    let args = parse_args(argv)?;

    let (tool, arguments) = request_for(&args)?;

    let response = call(args.connection_file.as_deref(), tool, arguments).await?;

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

/// Where a CLIENT looks for the daemon's connection file, in order.
///
/// # Why this is not `bootstrap::connection_file_path()`
///
/// That function is the DAEMON'S WRITER: it answers "where do I put the file",
/// and it answers correctly — `XDG_RUNTIME_DIR` if set, otherwise a temp path.
/// Using it here asked a writer's question to answer a reader's one.
///
/// The two differ because **the daemon writes one path and a client must search
/// several.** A daemon started under a launch agent has an environment the
/// operator's shell does not, so the client cannot derive the path the daemon
/// chose — it has to look where daemons put them.
///
/// Reported from Ufuk's terminal 2026-08-13: `ck models status` failed naming a
/// temp path while `ck health` in the same shell served fine. Not a daemon
/// problem — fusiform's CLI had jumped straight to the LAST rung, because
/// without `XDG_RUNTIME_DIR` the writer's answer is the temp fallback and the
/// client took that as its only candidate.
///
/// The order matches `ck`'s own, read from `subc-core/src/bin/ck.rs`:
///
/// 1. `--subc`, exclusive
/// 2. `SUBC_CONNECTION_FILE`, exclusive
/// 3. `$XDG_RUNTIME_DIR/subc-connection.json`
/// 4. `~/.local/share/cortexkit/run/subc-connection.json`
/// 5. `$TMPDIR/subc-<token>.connection.json`
///
/// The first two are EXCLUSIVE rather than first-in-a-list, which is the part
/// worth preserving deliberately: a path the operator named and got wrong must
/// fail loudly. Falling through to discovery would answer from whichever daemon
/// is found — in practice production — and the reply would be true and about
/// the wrong machine.
fn connection_file_candidates(override_path: Option<&Path>) -> Vec<PathBuf> {
    candidates_with(
        override_path,
        non_empty("SUBC_CONNECTION_FILE"),
        non_empty("XDG_RUNTIME_DIR"),
        non_empty("HOME"),
        subc_core::bootstrap::connection_file_path(),
    )
}

fn non_empty(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The ladder, with the environment passed in rather than read.
///
/// Taking the values as parameters is what makes the exclusivity rule testable.
/// Reading them here would force a test to mutate the process environment,
/// which races under threaded test execution — the same reason `ck` structures
/// its equivalent this way.
fn candidates_with(
    override_path: Option<&Path>,
    env_named: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    temp_fallback: PathBuf,
) -> Vec<PathBuf> {
    if let Some(path) = override_path {
        return vec![path.to_path_buf()];
    }
    if let Some(named) = env_named {
        return vec![named];
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    let push = |p: PathBuf, out: &mut Vec<PathBuf>| {
        if !out.contains(&p) {
            out.push(p);
        }
    };

    if let Some(runtime) = runtime_dir {
        push(runtime.join(CONNECTION_FILE_NAME), &mut candidates);
    }
    if let Some(home) = home {
        let mut path = home;
        for part in [".local", "share", "cortexkit", "run", CONNECTION_FILE_NAME] {
            path.push(part);
        }
        push(path, &mut candidates);
    }
    push(temp_fallback, &mut candidates);
    candidates
}

/// The connection file's name, matching `ck` and the daemon.
const CONNECTION_FILE_NAME: &str = "subc-connection.json";

/// Open a route to fusiform, make one call, and close.
async fn call(
    override_path: Option<&Path>,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let candidates = connection_file_candidates(override_path);

    let mut tried: Vec<String> = Vec::new();
    let mut consumer = None;
    for path in &candidates {
        match SubcConsumer::connect(path, ConsumerOptions::default()).await {
            Ok(c) => {
                consumer = Some(c);
                break;
            }
            Err(e) => tried.push(format!("  {}: {e}", path.display())),
        }
    }

    let Some(consumer) = consumer else {
        // Every rung, named. The previous message reported ONE path as though
        // it were THE path, which sent a reader toward "the daemon is down"
        // when the daemon was healthy and the file was two rungs up the ladder.
        return Err(format!(
            "could not reach subc. Tried {} path(s):\n{}\n\
             If the daemon is running, name its connection file with --subc.",
            candidates.len(),
            tried.join("\n")
        ));
    };

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
    let models = get("model_count").and_then(|v| v.as_i64()).unwrap_or(-1);
    // Pricing coverage beside the model total, because the total cannot express
    // it: measured on the live catalog, 420 of 6,293 models carry no rate at
    // all. Shown only when some model is unpriced, and omitted entirely by a
    // module too old to report it — an absent field is not zero coverage.
    let unpriced = get("models_priced")
        .and_then(|v| v.as_i64())
        .filter(|priced| *priced < models)
        .map(|priced| format!("  ({} with no rate)", models - priced))
        .unwrap_or_default();
    println!("models           {models}{unpriced}");
    // Overrides on the status surface, because "what is fusiform doing"
    // includes "deliberately disagreeing with the upstream about these facts".
    // An operator would otherwise learn it only by happening to read one of the
    // affected models.
    print!("{}", render_overridden(response));
    println!(
        "eras             {}",
        get("era_count").and_then(|v| v.as_i64()).unwrap_or(-1)
    );

    // Failed polls across the WHOLE history, printed before the poll window
    // rather than inside it.
    //
    // The window is ten by default and the live store's only failure sits about
    // forty polls back, so an operator sees ten clean rows and nothing telling
    // them to look further. This line is what tells them, and it names the flag
    // that would show it — a count with no way to reach the detail is a
    // diagnostic an operator has to guess at.
    print!("{}", render_failures(response));

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
        println!(
            "  {}  {outcome}{class}{took}{}",
            format_instant(at),
            render_changes(poll.get("changes"))
        );
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
        "{} at {}  (catalog version {})",
        count(models.len(), "model"),
        format_instant(resolved),
        response
            .get("catalog_version")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1)
    );

    // A single model prints its facts; a whole catalog prints one line each,
    // because 6,253 models times eleven facts is not something to read.
    if models.len() == 1 {
        print!("{}", render_single_model(response, models));
        return;
    }

    println!();
    for identity in models.keys() {
        println!("  {identity}");
    }

    print_withheld(response);
    print_uncertain(response);
    print_overridden(response);
}

/// Report facts whose served value differs from what the upstream published.
///
/// # Why a silent correction is the failure this exists to prevent
///
/// The value IS in the model list above, and it is the right one — which is
/// exactly the problem. An operator reading `limit.context  200000` for
/// claude-sonnet-4-5 has no way to know models.dev publishes 1,000,000 there
/// and fusiform overrode it. If the override is ever wrong, the wrong direction
/// is silent: a window under-used forever, nothing failing, nobody looking.
///
/// This surface was built on the wire, tested on the wire, pinned in the golden
/// fixture, and rendered nowhere — found by testing a peer's prediction that
/// recent attention on an artifact makes its UNEXAMINED dimensions less visible,
/// because the artifact as a whole feels checked. It is the second instance of
/// exactly this defect in this file today; the first was `uncertain`.
fn print_overridden(response: &serde_json::Value) {
    print!("{}", render_overridden(response));
}

/// The rendered block, returned rather than printed so it can be asserted.
///
/// The defect this file has already shipped once was in a COMPOSED sentence
/// ("1 models arrived"), where every unit test of the part passed because the
/// part was correct. So the artifact under test is the text an operator reads.
/// The failed-poll history line, or nothing when no poll has ever failed.
///
/// Returned rather than printed so the COMPOSED SENTENCE is what gets tested.
/// This file has already shipped `1 models arrived` to production: every unit
/// test of the pluralisation passed, because the pluralisation was correct and
/// the defect was in the caller. Nothing exercised the line an operator reads.
fn render_failures(response: &serde_json::Value) -> String {
    let Some(f) = response.get("failures") else {
        return String::new();
    };
    let (Some(ever), Some(at)) = (
        f.get("ever").and_then(|v| v.as_i64()),
        f.get("last_at_ms").and_then(|v| v.as_i64()),
    ) else {
        return String::new();
    };

    // The CLASS is what routes an operator: `network` sends them to the
    // upstream, `parse` to the payload, `implausible` to the shrink guard. A
    // count and an instant say THAT and WHEN; without the class the line says
    // something went wrong and not what.
    //
    // Omitted rather than filled when the module is too old to send it, because
    // an invented class sends an operator somewhere specific and wrong.
    let class = f
        .get("last_class")
        .and_then(|v| v.as_str())
        .map(|c| format!(" ({c})"))
        .unwrap_or_default();

    // The window that would actually reach the failure, from the module rather
    // than from a constant.
    //
    // This was `(--polls 60 to see it)`, hardcoded. True when written and
    // false by 2026-08-17: the recorded failure was 196 polls back, so the
    // hint sent an operator to run a costly query that would not show them the
    // failure it pointed at. A number describing a moving relationship has to
    // be computed from it.
    //
    // Absent when the module predates the field, because a wrong number is
    // worse than no number here — the operator acts on it either way.
    let hint = f
        .get("polls_back")
        .and_then(|v| v.as_i64())
        .map(|n| format!("  (--polls {n} to see it)"))
        .unwrap_or_default();

    format!(
        "failed polls     {ever} ever, last {}{class}{hint}\n",
        format_instant(at)
    )
}

fn render_overridden(response: &serde_json::Value) -> String {
    let Some(overridden) = response.get("overridden").and_then(|v| v.as_array()) else {
        return String::new();
    };
    if overridden.is_empty() {
        return String::new();
    }

    let mut out = format!(
        "\n{} overridden — fusiform serves a different value than the \
         upstream published:\n",
        count(overridden.len(), "fact")
    );
    for o in overridden {
        let field = |k: &str| o.get(k).and_then(|v| v.as_str()).unwrap_or("?");
        out.push_str(&format!(
            "  {}/{}  {}\n",
            field("provider_id"),
            field("model_id"),
            field("fact_key")
        ));
        out.push_str(&format!(
            "      upstream {} -> served {}\n",
            field("upstream_value"),
            field("served_value")
        ));
        // The authority on its own line and in full: an operator asking "says
        // who" must be able to answer from what is on screen. A line they
        // cannot re-check is one they learn to skip, and a skipped line is the
        // same as no line.
        out.push_str(&format!("      {}\n", field("authority")));
    }
    out
}

/// One model's facts, with the qualification blocks that belong beside them.
///
/// # The co-location is load-bearing, not layout
///
/// An override is reported by its ABSENCE returning to normal: when the
/// upstream adopts fusiform's value the block disappears, and the only thing
/// telling an operator the override ended is the fact value printed above it
/// having reverted. Those two must be in ONE output or the transition is
/// invisible — a block that goes quiet is indistinguishable from an override
/// still holding.
///
/// BROCA's point, and they are right that it was accidental: the property lived
/// in the layout rather than in a decision, so moving the block to another
/// command would silently lose it and the change would look like tidying.
/// Returned as text so a test can hold the two together.
fn render_single_model(
    response: &serde_json::Value,
    models: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut out = String::new();
    for (identity, facts) in models {
        out.push_str(&format!("\n{identity}\n"));
        let facts: BTreeMap<_, _> = facts
            .as_object()
            .map(|o| o.iter().collect())
            .unwrap_or_default();
        for (key, value) in facts {
            out.push_str(&format!("  {key:<34}  {value}\n"));
        }
    }
    // PRINTS NOTHING. The caller prints what this returns, and for a while
    // this function did both — so every single-model read rendered its facts
    // TWICE, under a header correctly saying "1 model".
    //
    // It survived because the duplicate is the second screenful: `--json`
    // shows one model, the header says one model, and every hand check of this
    // path went through `head`, which cuts before the repeat. Found by driving
    // a correction end to end in a lab and reading the whole output.
    //
    // The mixed shape came from making the override block testable: the block
    // was returned so a test could hold it beside the fact value, and the
    // printing was left in place beneath it. A function that both prints and
    // returns the same text has two callers by construction — itself and
    // whoever uses the return.
    out.push_str(&render_withheld(response));
    out.push_str(&render_uncertain(response));
    out.push_str(&render_overridden(response));
    out
}

/// Report facts the read refused to answer.
///
/// Printed after the models rather than folded into them, because a withheld
/// fact is absent from that list. Without this an operator sees a model with no
/// input rate and concludes the upstream publishes none.
fn print_withheld(response: &serde_json::Value) {
    print!("{}", render_withheld(response));
}

fn render_withheld(response: &serde_json::Value) -> String {
    let mut out = String::new();
    let Some(withheld) = response.get("withheld").and_then(|v| v.as_array()) else {
        return String::new();
    };
    if withheld.is_empty() {
        return String::new();
    }

    let _ = writeln!(
        out,
        "\n{} withheld — the record fusiform holds for these is known bad:",
        count(withheld.len(), "fact")
    );
    for item in withheld {
        let model = item.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        let fact = item.get("fact_key").and_then(|v| v.as_str()).unwrap_or("?");
        let _ = writeln!(out, "  {model}  {fact}");
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
                let _ = writeln!(
                    out,
                    "      {} to {}: {reason}",
                    format_instant(from),
                    format_instant(until)
                );
            }
        }
    }
    out
}

/// Facts whose value at the read instant may already have been superseded.
///
/// Printed after the models rather than folded into them, because unlike a
/// withheld fact the value IS returned — this qualifies an answer rather than
/// replacing one. An operator who sees the value alone reads it as what was in
/// force; what fusiform actually knows is that it was in force at the start of
/// an interval containing the read instant, and it did not look again until the
/// end.
///
/// Empty for a read of the current catalog, always: an era covering now has no
/// successor. So this only appears on a historical read, which is the question
/// a wrong answer costs money on.
///
/// Written after the wire field had been live for three commits with nothing
/// rendering it — the bracket reached the payload and stopped one layer short
/// of the only surface an operator uses.
fn print_uncertain(response: &serde_json::Value) {
    print!("{}", render_uncertain(response));
}

fn render_uncertain(response: &serde_json::Value) -> String {
    let mut out = String::new();
    let Some(uncertain) = response.get("uncertain").and_then(|v| v.as_array()) else {
        return String::new();
    };
    if uncertain.is_empty() {
        return String::new();
    }

    let _ = writeln!(
        out,
        "\n{} uncertain at this instant — the value is real, and \
         fusiform did not look during the window it may have changed in:",
        count(uncertain.len(), "fact")
    );
    for item in uncertain {
        let provider = item
            .get("provider_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let model = item.get("model_id").and_then(|v| v.as_str()).unwrap_or("?");
        let fact = item.get("fact_key").and_then(|v| v.as_str()).unwrap_or("?");
        let after = item
            .get("superseded_after_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let by = item
            .get("superseded_by_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let _ = writeln!(out, "  {provider}/{model}  {fact}");
        let _ = writeln!(
            out,
            "      last confirmed {}, changed by {}",
            format_instant(after),
            format_instant(by)
        );
    }
    out
}

/// The override in force on a fact whose history is being read.
///
/// Printed BEFORE the eras rather than after, which is the opposite of
/// `withheld` and deliberate: this line changes how every row below should be
/// read. A reader who reaches the eras first has already taken the newest one
/// as the current value, and a footnote afterwards is a correction to a
/// conclusion they have made rather than context for one they have not.
///
/// Returns the text so the artifact under test is the sentence an operator
/// reads. This file shipped "1 models arrived" to production once because the
/// pluralisation was correct and nothing exercised the composed line.
fn render_history_override(response: &serde_json::Value) -> String {
    let Some(o) = response.get("overridden").filter(|v| !v.is_null()) else {
        return String::new();
    };
    let get = |k: &str| o.get(k).and_then(|v| v.as_str()).unwrap_or("?");
    format!(
        "  NOTE: fusiform serves {} for this fact, not the {} recorded below.\n\
         \x20       The eras are what the upstream published and are not rewritten;\n\
         \x20       the override applies to the current catalog only.\n\
         \x20       {}\n\n",
        get("served_value"),
        get("upstream_value"),
        get("authority"),
    )
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

    // Before the eras: this note changes how every row below reads.
    print!("\n{}", render_history_override(response));

    let Some(eras) = response.get("eras").and_then(|v| v.as_array()) else {
        return;
    };
    if eras.is_empty() {
        // By the time this renders, the route has already refused an unknown
        // provider and an unknown model. So the remaining causes really are
        // about the fact: either the key is mistyped, or the model genuinely
        // has no era for it — a model with no reasoning rate, for instance.
        //
        // The earlier comment here named a typo in the key as the cause while
        // the route still answered all three cases identically, so an operator
        // who mistyped the MODEL was pointed at the key.
        println!("\nno eras recorded for this fact: the model exists, so check the fact key");
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

/// What a poll changed, as it appears at the end of a status line.
///
/// Extracted from the print loop so a test can read THE RENDERED LINE rather
/// than the pieces it is built from. The distinction is not academic: the
/// defect that shipped here was `1 models arrived`, which every unit test of
/// `plural` would have passed, because the defect was in the caller and the
/// tests were written in the dialect of tests rather than the dialect of the
/// output.
///
/// The parts are printed rather than the total, and only the non-zero ones, so
/// a line says what happened instead of how much happened. Measured over 11
/// hours of live polling: 72% of era churn was models arriving and leaving
/// rather than facts changing, because an arriving model writes one era per
/// fact it has. One real poll wrote 96 eras for 19 genuine changes.
///
/// Empty when the poll changed nothing, which is the ordinary case — a 304, an
/// unchanged document, or a failure.
fn render_changes(changes: Option<&serde_json::Value>) -> String {
    let Some(c) = changes else {
        return String::new();
    };
    let n = |k: &str| c.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let (changed, arrived, withdrawn) = (
        n("facts_changed"),
        n("models_arrived"),
        n("models_withdrawn"),
    );

    let mut parts = Vec::new();
    if changed > 0 {
        parts.push(format!("{} changed", count(changed as usize, "fact")));
    }
    if arrived > 0 {
        parts.push(format!("{} arrived", count(arrived as usize, "model")));
    }
    if withdrawn > 0 {
        parts.push(format!("{withdrawn} withdrawn"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("  — {}", parts.join(", "))
}

/// The singular or plural form of a noun, for a count.
///
/// Every count in this output is a real quantity that is genuinely 1 sometimes
/// — many polls change exactly one fact, and a single model arriving is the
/// most common non-zero arrival — so "1 models arrived" is not a rare edge
/// case, it is the ordinary reading. Operator output that looks like a debug
/// print invites being read like one.
/// A count and its noun as ONE string: `1 fact`, `2 facts`, `0 facts`.
///
/// # Why this exists rather than a bare pluraliser
///
/// The pluraliser below is correct and always was. What shipped
/// `1 models arrived` to production was the CALLER: a format string with two
/// holes, one fed the count and the other fed a pluralisation of something
/// else. Every unit test of the pluraliser passed, because the pluraliser was
/// never wrong.
///
/// Two holes can disagree. One cannot. This takes the count once and renders
/// both halves from it, so the defect is not a mistake to avoid — it is a
/// sentence that cannot be written.
fn count(n: usize, noun: &str) -> String {
    format!("{n} {}", plural(n as i64, noun))
}

/// The bare pluraliser. **Call [`count`] instead.**
///
/// Correct, and never the thing that was wrong. What shipped `1 models
/// arrived` was a CALLER holding a count in one hole and a pluralisation in
/// the other; the same shape then produced `1 fact(s) would be corrected` and
/// `1 models at ...`, three separate times in one binary.
///
/// It has exactly one production caller — `count` — and
/// `every_plural_goes_through_count` fails if a second appears.
fn plural(n: i64, noun: &str) -> String {
    if n == 1 {
        noun.to_string()
    } else {
        format!("{noun}s")
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
        connection_file: None,
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
            "--subc" => args.connection_file = Some(PathBuf::from(value()?)),
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
        println!("recorded a correction over {}", count(facts.len(), "fact"));
    } else {
        println!(
            "PREVIEW — nothing written. {} would be corrected.",
            count(facts.len(), "fact")
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

    /// The override block renders, and says everything an operator needs.
    ///
    /// Written because the block existed on the wire, in the golden fixture,
    /// and in three integration tests while rendering NOWHERE — the same defect
    /// as `uncertain` earlier the same day, in the same file. Found by testing
    /// BROCA's prediction that recent attention on an artifact makes its
    /// unexamined dimensions less visible, and the prediction paid out on the
    /// most recent thing I had built.
    #[test]
    fn an_override_names_the_values_and_the_authority() {
        // Real shape, taken from the live response rather than invented.
        let response = serde_json::json!({
            "overridden": [{
                "provider_id": "anthropic",
                "model_id": "claude-sonnet-4-5",
                "fact_key": "limit.context",
                "upstream_value": "1000000",
                "served_value": "200000",
                "authority": "https://docs.claude.com/... — '200k-token context window'"
            }]
        });

        let out = render_overridden(&response);

        // Singular, because one override is the common case rather than a
        // boundary. "1 facts overridden" shipped to production once already, in
        // this file, from a composed sentence whose parts were each correct.
        assert!(
            out.contains("1 fact overridden"),
            "the count must agree with its noun: {out}"
        );
        assert!(
            out.contains("anthropic/claude-sonnet-4-5"),
            "the row must be identified: {out}"
        );
        assert!(
            out.contains("upstream 1000000 -> served 200000"),
            "BOTH values must appear. Showing only the served one leaves an \
             operator unable to see that anything was changed: {out}"
        );
        assert!(
            out.contains("docs.claude.com"),
            "the authority must be present, or 'says who' cannot be answered \
             from the screen: {out}"
        );
    }

    /// The fact value and the override block appear in ONE output.
    ///
    /// The property BROCA identified as accidental. An override ending is
    /// reported by its block DISAPPEARING, so the only thing telling an
    /// operator it ended is the value above having reverted — and that only
    /// works if the two are in the same output. A block that goes quiet is
    /// otherwise indistinguishable from an override still holding.
    ///
    /// This test exists because nothing stopped someone moving the block to
    /// another command, where the change would look like tidying and the
    /// transition would be silently lost.
    #[test]
    fn the_fact_value_and_its_override_are_rendered_together() {
        let response = serde_json::json!({
            "resolved_at_ms": 1_786_600_000_000_i64,
            "catalog_version": 1_786_600_000_000_i64,
            "models": { "anthropic/claude-sonnet-4-5": { "limit.context": 200_000 } },
            "overridden": [{
                "provider_id": "anthropic", "model_id": "claude-sonnet-4-5",
                "fact_key": "limit.context",
                "upstream_value": "1000000", "served_value": "200000",
                "authority": "https://docs.claude.com/... — '200k-token context window'"
            }]
        });
        let models = response["models"].as_object().unwrap().clone();
        let out = render_single_model(&response, &models);

        let value_at = out
            .find("limit.context")
            .expect("the fact must be rendered");
        let block_at = out
            .find("overridden")
            .expect("the override block must be rendered in the SAME output");
        assert!(
            value_at < block_at,
            "the served value must appear before its override block, so a \
             reverted value and a vanished block read as one transition: {out}"
        );
    }

    /// Nothing is printed when nothing was overridden.
    ///
    /// The ordinary case, and the one that decides whether the block is read at
    /// all: a header printed on every response is noise an operator learns to
    /// skip, which is the same as not printing it.
    #[test]
    fn an_ordinary_response_prints_no_override_block() {
        assert!(render_overridden(&serde_json::json!({})).is_empty());
        assert!(render_overridden(&serde_json::json!({"overridden": []})).is_empty());
    }

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

    /// Counts read as English, including when they are 1.
    ///
    /// Not cosmetic. Every count in the status output is genuinely 1 sometimes
    /// — many polls change exactly one fact, and a single model arriving is
    /// the most common non-zero arrival — so "1 models arrived" is the
    /// ordinary reading rather than a rare edge case. It shipped to production
    /// and appeared in the first status read after placement.
    /// `plural` has exactly ONE production caller, and it is `count`.
    ///
    /// Three times in this binary a caller held the count in one hole and the
    /// pluralisation in another: `1 models arrived`, `1 fact(s) would be
    /// corrected`, `1 models at ...`. Each was fixed where it was found, and
    /// the shape came back, because a fixed instance leaves the pattern
    /// available to the next person writing a line.
    ///
    /// So the shape is fenced rather than the instances: a second production
    /// caller of `plural` fails here. Reading the source is the only way to
    /// ask this question — a value-based test cannot distinguish a correct
    /// two-hole caller from the one that will drift.
    #[test]
    fn every_plural_goes_through_count() {
        let src = include_str!("main.rs");
        let (production, _tests) = src
            .split_once("#[cfg(test)]")
            .expect("this file has a test module");

        let callers: Vec<&str> = production
            .lines()
            .filter(|l| l.contains("plural("))
            .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("///"))
            .filter(|l| !l.contains("fn plural("))
            .collect();

        assert_eq!(
            callers.len(),
            1,
            "plural must have exactly one production caller (count). Found: {callers:#?}\n\
             Use `count(n, noun)`, which renders the number and the noun from \
             one argument so they cannot disagree."
        );
        assert!(
            callers[0].contains("format!(\"{n} {}\""),
            "the one caller must be `count`, got: {}",
            callers[0]
        );
    }

    /// A count and its noun cannot disagree, because there is one of them.
    ///
    /// The production defect this closes was never in the pluraliser: it was a
    /// caller with two holes in a format string. `1` is the COMMON case in
    /// this catalog — most polls change one fact, most reads withhold one —
    /// so it is the case a hand-picked fixture treats as a boundary and real
    /// data treats as the norm.
    /// Flags whose absence from the request FAILS SILENTLY, asserted on the
    /// request body.
    ///
    /// `request_for` was extracted so a request could be examined without
    /// sending one, and this covers the arms whose flags go wrong quietly:
    ///
    /// - `--at` dropped means a point-in-time read answers with CURRENT values.
    ///   The response is well-formed, the numbers are real, and an operator
    ///   reading history gets today's catalog believing it is the past. That
    ///   confusion has a precedent: a shrunken current read and a historical
    ///   read are already hard to tell apart at the version field, and this
    ///   would make them identical at the values too.
    /// - a prefix filter dropped means `--rates` returns the WHOLE catalog
    ///   rather than the rate plane, which is loud enough to notice; but a
    ///   filter that reaches the wire WRONG returns an empty catalog, which
    ///   reads as "this catalog has no prices". The constants are held against
    ///   real fact keys in fusiform-store; what is checked here is that they
    ///   arrive at all.
    /// - `--include-retired` dropped silently hides models the upstream
    ///   stopped publishing, which is the difference between "retired" and
    ///   "never existed" — the distinction this whole store is built to keep.
    #[test]
    fn flags_that_fail_silently_reach_the_request() {
        let body = |argv: &[&str]| -> serde_json::Value {
            let full: Vec<std::ffi::OsString> = std::iter::once("ck-models")
                .chain(argv.iter().copied())
                .map(std::ffi::OsString::from)
                .collect();
            let args = parse_args(full).expect("these arguments must parse");
            request_for(&args).expect("this request must build").1
        };

        // --at must reach the wire, and its ABSENCE must leave the field out
        // rather than send a zero: at_ms 0 is a real instant (1970), and a
        // read resolved there returns nothing while looking like a valid
        // point-in-time answer.
        let past = body(&["get", "--at", "1786000000000"]);
        assert_eq!(
            past["at_ms"],
            serde_json::json!(1_786_000_000_000i64),
            "--at must reach the request, or a point-in-time read silently \
             answers with current values"
        );
        let now = body(&["get"]);
        assert!(
            now.get("at_ms").is_none(),
            "without --at the field must be ABSENT, not zero: {now}"
        );

        // The plane filters must arrive, and must differ from each other.
        let rates = body(&["get", "--rates"]);
        let caps = body(&["get", "--capabilities"]);
        assert_eq!(
            rates["fact_prefixes"],
            serde_json::json!([fusiform_store::prefix::RATE]),
            "--rates must send the rate prefix"
        );
        assert_eq!(
            caps["fact_prefixes"],
            serde_json::json!([
                fusiform_store::prefix::CAPABILITY,
                fusiform_store::prefix::LIMIT
            ]),
            "--capabilities must send both the capability and limit planes: \
             a limit is a capacity claim and an operator asking about \
             capabilities means both"
        );
        assert_ne!(
            rates["fact_prefixes"], caps["fact_prefixes"],
            "the two filters must differ, or one of them selects the wrong plane"
        );
        assert!(
            body(&["get"]).get("fact_prefixes").is_none(),
            "with no filter the field must be absent, or every read is filtered"
        );

        // --include-retired must arrive, and must default to absent.
        assert_eq!(
            body(&["get", "--include-retired"])["include_retired"],
            serde_json::json!(true),
            "--include-retired must reach the request, or a retired model is \
             indistinguishable from one that never existed"
        );
        assert!(
            body(&["get"]).get("include_retired").is_none(),
            "the default must not send the flag"
        );

        // A verb that cannot use --at must REFUSE it, not drop it.
        //
        // This assertion started as "history must carry --at" and failed,
        // which is how the flag's silent uselessness was found: history has no
        // instant field at all, so the flag had been accepted and discarded
        // since the verb existed.
        let refused = |argv: &[&str]| -> String {
            let full: Vec<std::ffi::OsString> = std::iter::once("ck-models")
                .chain(argv.iter().copied())
                .map(std::ffi::OsString::from)
                .collect();
            let args = parse_args(full).expect("these arguments must parse");
            request_for(&args).expect_err("this must be refused")
        };

        for argv in [
            vec![
                "history",
                "--provider",
                "anthropic",
                "--model",
                "claude-sonnet-4-5",
                "--fact",
                "limit.context",
                "--at",
                "1786000000000",
            ],
            vec!["status", "--at", "1786000000000"],
        ] {
            let msg = refused(&argv);
            assert!(
                msg.contains("--at applies to `get` only"),
                "{:?} must refuse --at rather than ignore it, got: {msg}",
                argv[0]
            );
        }

        // Control: the verb that DOES resolve an instant must still accept it,
        // or the refusal above is just a broken flag.
        assert_eq!(
            body(&["get", "--at", "1786000000000"])["at_ms"],
            serde_json::json!(1_786_000_000_000i64),
            "control: get must still carry --at"
        );
    }

    /// `--commit` and the wire's `dry_run` are OPPOSITE polarities, asserted
    /// on the REQUEST BODY rather than the parsed flag.
    ///
    /// The CLI sends `"dry_run": !args.commit`. One flipped `!` turns every
    /// preview into a write or every commit into a no-op — the first rewrites
    /// the past for an operator who asked only to look, the second reports a
    /// correction that never happened.
    ///
    /// MY FIRST VERSION OF THIS TEST ASSERTED ON `args.commit` AND THE
    /// MUTATION SURVIVED. Proving the flag parses says nothing about what
    /// crosses the wire, and the builder was inline in `run` beside the
    /// network call, so no test could reach it. That is why `request_for`
    /// exists: the artifact under test has to be the thing that ships.
    #[test]
    fn the_commit_flag_inverts_into_dry_run_on_the_wire() {
        let body = |extra: &[&str]| -> serde_json::Value {
            let mut argv: Vec<std::ffi::OsString> = vec![
                "ck-models".into(),
                "correct".into(),
                "--provider".into(),
                "anthropic".into(),
                "--model".into(),
                "claude-sonnet-4-5".into(),
                "--field".into(),
                "limit.context".into(),
                "--from".into(),
                "1000".into(),
                "--until".into(),
                "2000".into(),
                "--reason".into(),
                "docs/findings/x.md".into(),
            ];
            argv.extend(extra.iter().map(|s| std::ffi::OsString::from(*s)));
            let args = parse_args(argv).expect("these arguments must parse");
            request_for(&args).expect("this request must build").1
        };

        // Absent: the case an operator hits by default, and the one that must
        // never write.
        assert_eq!(
            body(&[])["dry_run"],
            serde_json::json!(true),
            "without --commit the wire must carry dry_run TRUE: a preview that \
             writes is the worst outcome this command has"
        );

        // Present: a write.
        assert_eq!(
            body(&["--commit"])["dry_run"],
            serde_json::json!(false),
            "with --commit the wire must carry dry_run FALSE, or an operator \
             is told a correction was recorded when nothing happened"
        );

        // And they must differ, which catches a builder that hardcodes either.
        assert_ne!(
            body(&[])["dry_run"],
            body(&["--commit"])["dry_run"],
            "the flag must change the request, or it does nothing at all"
        );
    }

    #[test]
    fn a_count_and_its_noun_are_rendered_together() {
        assert_eq!(count(1, "fact"), "1 fact");
        assert_eq!(count(0, "fact"), "0 facts");
        assert_eq!(count(2, "fact"), "2 facts");
        assert_eq!(count(1, "model"), "1 model");

        // The composed lines an operator actually reads, at the count that
        // occurs most.
        assert_eq!(
            format!(
                "PREVIEW — nothing written. {} would be corrected.",
                count(1, "fact")
            ),
            "PREVIEW — nothing written. 1 fact would be corrected."
        );
        assert_eq!(
            format!("recorded a correction over {}", count(1, "fact")),
            "recorded a correction over 1 fact"
        );
    }

    #[test]
    fn a_count_of_one_reads_as_english() {
        assert_eq!(plural(1, "fact"), "fact");
        assert_eq!(plural(1, "model"), "model");

        // Zero is plural in English, and the boundary is worth pinning because
        // the obvious implementation (`n > 1`) gets it wrong.
        assert_eq!(plural(0, "fact"), "facts");
        assert_eq!(plural(2, "model"), "models");
        assert_eq!(plural(24, "fact"), "facts");
    }

    /// The rendered line reads as English, for the counts a real poll produces.
    ///
    /// Asserts on the OUTPUT rather than on the pieces. `1 models arrived`
    /// shipped to production and would have passed any test of `plural` in
    /// isolation, because the defect was in the caller: the unit test was
    /// written in the dialect of tests, and the defect lived in the dialect of
    /// the output.
    ///
    /// The inputs are real poll compositions read off the live store, not
    /// hand-picked numbers. A single model arriving is the most common non-zero
    /// arrival, so the 1 case is the ordinary reading rather than a boundary.
    #[test]
    fn a_rendered_change_line_reads_as_english() {
        let line = |changed, arrived, withdrawn| {
            render_changes(Some(&serde_json::json!({
                "facts_changed": changed,
                "models_arrived": arrived,
                "models_withdrawn": withdrawn,
            })))
        };

        // Measured polls, from `ck models status` on the production store.
        assert_eq!(line(24, 0, 0), "  \u{2014} 24 facts changed");
        assert_eq!(line(7, 1, 0), "  \u{2014} 7 facts changed, 1 model arrived");
        assert_eq!(line(1, 0, 0), "  \u{2014} 1 fact changed");
        assert_eq!(
            line(19, 7, 0),
            "  \u{2014} 19 facts changed, 7 models arrived"
        );
        assert_eq!(line(0, 0, 1), "  \u{2014} 1 withdrawn");

        // A poll that changed nothing renders nothing, rather than a dash with
        // an empty list after it.
        assert_eq!(line(0, 0, 0), "");
        assert_eq!(render_changes(None), "");
    }

    /// History must reconcile itself with what the catalog serves.
    ///
    /// The defect this pins: `ck models history --fact limit.context` for
    /// claude-sonnet-4-5 printed the upstream's 1,000,000 and said nothing
    /// about the 200,000 that `catalog.get` returns. Both surfaces correct,
    /// neither mentioning the other, which reads as one of them being wrong —
    /// and the reader most likely to hit it is one checking a suspicious number
    /// against the record.
    #[test]
    fn history_reconciles_itself_with_what_is_served() {
        let with = serde_json::json!({
            "overridden": {
                "provider_id": "anthropic",
                "model_id": "claude-sonnet-4-5",
                "fact_key": "limit.context",
                "upstream_value": "1000000",
                "served_value": "200000",
                "authority": "docs.claude.com — 'a 200k-token context window'"
            }
        });
        let line = render_history_override(&with);
        // BOTH numbers, because showing only the served one leaves a reader
        // unable to see that anything was changed.
        assert!(
            line.contains("200000") && line.contains("1000000"),
            "the note must name both values or a reader cannot see a change \
             occurred: {line}"
        );
        assert!(
            line.contains("docs.claude.com"),
            "the note must carry the authority, or it asserts without a source: {line}"
        );
        // And it must not claim the eras were rewritten.
        assert!(
            line.contains("not rewritten"),
            "the note must say the record is intact, or an operator reads it as \
             history having been edited: {line}"
        );

        // The ordinary case prints nothing. A note on every response is noise a
        // reader learns to skip, which is the same as no note at all.
        assert_eq!(
            render_history_override(&serde_json::json!({"eras": []})),
            "",
            "a fact with no override must print nothing"
        );
        assert_eq!(
            render_history_override(&serde_json::json!({"overridden": null})),
            "",
            "an explicit null must print nothing rather than a header with \
             question marks in it"
        );
    }

    #[test]
    fn the_failure_line_routes_an_operator() {
        // With a class: the line must name where to look.
        let with_class = serde_json::json!({
            "failures": { "ever": 1, "last_at_ms": 1_786_609_178_000i64,
                          "last_class": "network", "polls_back": 196 }
        });
        let line = render_failures(&with_class);
        assert!(
            line.contains("(network)"),
            "the class routes the operator and must appear: {line}"
        );
        assert!(
            line.contains("--polls 196"),
            "the hint must carry the window the MODULE derived, not a constant. \
             It was hardcoded to 60, which was true when written and wrong by \
             2026-08-17 when the failure sat 196 polls back — the operator ran \
             a costly query and still did not see it: {line}"
        );

        // Without one — an older module. The line must still report, and must
        // NOT invent a class.
        let no_class = serde_json::json!({
            "failures": { "ever": 2, "last_at_ms": 1_786_609_178_000i64 }
        });
        let line = render_failures(&no_class);
        assert!(
            !line.contains("--polls"),
            "a module too old to derive the window must produce NO hint: a \
             wrong number is worse than none, because the operator acts on it \
             either way: {line}"
        );
        assert!(
            line.contains("2 ever"),
            "the count must survive a module too old to send the class: {line}"
        );
        // Named classes rather than any parenthesis: the line legitimately
        // carries "(--polls 60 to see it)", so a bare paren check fails on
        // correct output. My first version did exactly that — an assertion
        // testing a character rather than the property.
        for word in ["network", "http_status", "parse", "implausible"] {
            assert!(
                !line.contains(word),
                "no class must be invented when the producer sent none — a \
                 wrong cause sends an operator somewhere specific and wrong. \
                 Found {word:?} in: {line}"
            );
        }

        // Nothing ever failed: silence, not a zero.
        assert_eq!(
            render_failures(&serde_json::json!({})),
            "",
            "a clean history must print nothing rather than a zero line"
        );
    }
}

#[cfg(test)]
mod connection_discovery_tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// The run dir is tried before the temp fallback.
    ///
    /// The defect this pins, reported from a live terminal 2026-08-13: with no
    /// `XDG_RUNTIME_DIR`, `ck models status` failed naming a temp path while
    /// `ck health` in the same shell served fine. The CLI had used the DAEMON'S
    /// WRITER function — "where do I put the file" — to answer a client's
    /// question, "where do I look for it". Those differ because the daemon
    /// writes one path and a client must search several: a daemon under a
    /// launch agent has an environment the operator's shell does not.
    #[test]
    fn the_run_dir_is_tried_before_the_temp_fallback() {
        let got = candidates_with(
            None,
            None,
            None,
            Some(p("/home/u")),
            p("/tmp/subc-u.connection.json"),
        );
        assert_eq!(
            got,
            vec![
                p("/home/u/.local/share/cortexkit/run/subc-connection.json"),
                p("/tmp/subc-u.connection.json"),
            ],
            "the temp fallback is the LAST rung, not the only one"
        );
    }

    /// With a runtime dir set, it goes first — matching the daemon's own choice.
    #[test]
    fn the_runtime_dir_leads_when_it_is_set() {
        let got = candidates_with(
            None,
            None,
            Some(p("/run/user/501")),
            Some(p("/home/u")),
            p("/tmp/subc-u.connection.json"),
        );
        assert_eq!(got.first(), Some(&p("/run/user/501/subc-connection.json")));
        assert_eq!(got.len(), 3, "all three rungs, in order: {got:?}");
    }

    /// `--subc` is EXCLUSIVE, not first-in-a-list.
    ///
    /// A path the operator named and got wrong must fail loudly. Falling
    /// through to discovery would answer from whichever daemon is found — in
    /// practice production — and the reply would be true and about the wrong
    /// machine, with every later conclusion inheriting that while the operator
    /// believes they are reading the one they named.
    #[test]
    fn an_explicit_override_is_exclusive() {
        let got = candidates_with(
            Some(&p("/lab/subc.json")),
            Some(p("/env/subc.json")),
            Some(p("/run/user/501")),
            Some(p("/home/u")),
            p("/tmp/subc-u.connection.json"),
        );
        assert_eq!(
            got,
            vec![p("/lab/subc.json")],
            "a named path must not fall back to a healthy daemon elsewhere"
        );
    }

    /// So is the environment variable, for the same reason.
    #[test]
    fn the_environment_variable_is_exclusive_too() {
        let got = candidates_with(
            None,
            Some(p("/env/subc.json")),
            Some(p("/run/user/501")),
            Some(p("/home/u")),
            p("/tmp/subc-u.connection.json"),
        );
        assert_eq!(got, vec![p("/env/subc.json")]);
    }

    /// A duplicate path is tried once.
    ///
    /// Reachable in practice: `XDG_RUNTIME_DIR` pointing at the temp dir makes
    /// the first and last rungs identical, and reporting the same failure twice
    /// in an error an operator reads during an incident is noise that looks
    /// like two distinct problems.
    #[test]
    fn a_duplicate_rung_appears_once() {
        let got = candidates_with(
            None,
            None,
            Some(p("/tmp")),
            None,
            p("/tmp/subc-connection.json"),
        );
        assert_eq!(got, vec![p("/tmp/subc-connection.json")]);
    }
}

#[cfg(test)]
mod render_once_tests {
    use super::*;

    /// A single-model read renders its facts exactly once.
    ///
    /// # The defect
    ///
    /// `render_single_model` both PRINTED its output and RETURNED it, and the
    /// caller printed the return. So every single-model read showed the model
    /// and every fact twice, under a header correctly reading "1 model".
    ///
    /// It survived because the duplicate is the second screenful. `--json`
    /// carries one model, the header says one model, and every hand check of
    /// this path went through `head`, which cuts before the repeat. Found by
    /// driving a correction end to end against a live lab daemon and reading
    /// the whole output rather than the top of it.
    ///
    /// The mixed shape came from making the override block testable: the block
    /// was returned so a test could hold it beside the fact value, and the
    /// printing was left in place beneath it. A function that both prints and
    /// returns the same text has two callers by construction.
    #[test]
    fn a_single_model_renders_its_facts_exactly_once() {
        let response = serde_json::json!({
            "models": {
                "crossmodel/deepseek/deepseek-v4-flash": {
                    "rate.input": {"state": "priced", "units": 405000000},
                    "limit.context": 1000000
                }
            }
        });
        let models = response["models"].as_object().unwrap();
        let out = render_single_model(&response, models);

        assert_eq!(
            out.matches("crossmodel/deepseek/deepseek-v4-flash").count(),
            1,
            "the model identity must appear once; it appeared twice for as long \
             as this function both printed and returned: {out}"
        );
        assert_eq!(
            out.matches("rate.input").count(),
            1,
            "each fact must appear once: {out}"
        );
    }
}
