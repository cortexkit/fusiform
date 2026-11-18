//! Compare a signed store bootstrap with the backup used to publish it.

use std::{collections::BTreeSet, env, fs, path::Path, process::ExitCode, time::Instant};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_module::{route::serve_tool_call, settings, store_poll::StorePoller};
use fusiform_protocol::{
    TOOL_GET, TOOL_HISTORY, TOOL_PLAN_PRICES, TOOL_QUALITY_GRADES, TOOL_STATUS,
};
use fusiform_store::CatalogStore;
use rusqlite::{Connection, DatabaseName, OpenFlags};
use serde_json::{json, Value};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const DATED_INSTANTS_MS: [i64; 3] = [1786529249396, 1786865960643, 1793491200000];
const HISTORY_PROBES: [(&str, &str, &str); 10] = [
    ("anthropic", "claude-fable-5", "rate.cache_write"),
    ("anthropic", "claude-sonnet-4-5", "model.status"),
    ("openai", "gpt-5.6", "rate.input"),
    ("openai", "gpt-5.6", "rate.output"),
    (
        "google",
        "gemini-2.5-computer-use-preview-10-2025",
        "rate.input.above_context.200000",
    ),
    (
        "google",
        "deep-research-max-preview-04-2026",
        "limit.context",
    ),
    ("deepseek", "deepseek-v4-flash", "rate.reasoning"),
    ("deepseek", "deepseek-v4-flash", "limit.output"),
    ("xai", "grok-4.20-0309-reasoning", "rate.cache_read"),
    ("xai", "grok-imagine-image-2.0", "existence"),
];

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let [base, kid, key, source_path, work_dir] = env::args_os()
        .skip(1)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| {
            "usage: store-rehearsal <base-url> <kid> <public-key-b64url> <source-store.db> <work-dir>"
        })?;
    let base = base.to_str().ok_or("base URL must be UTF-8")?;
    let kid = kid.to_str().ok_or("key ID must be UTF-8")?;
    let key = key.to_str().ok_or("public key must be UTF-8")?;
    let keys: Vec<settings::PinnedKey> =
        serde_json::from_value(json!([{"kid": kid, "public_key": key}]))?;
    let (url, keys) = settings::validate_store(base, &keys)?;
    let mut poller = StorePoller::new(url, keys)?;
    fs::create_dir_all(&work_dir)?;
    let run_dir = tempfile::tempdir_in(work_dir)?;

    println!("Store rehearsal: base={base}, pinned kid={kid}");
    let started = Instant::now();
    let rebuilt = CatalogStore::open_for_store(&descriptor(&run_dir.path().join("rebuilt.db"))?)?;
    let bootstrap = poller.poll(&rebuilt).await;
    let elapsed = started.elapsed();
    let peak = peak_rss_kib().map_or_else(|| "unavailable".into(), |n| format!("{n} KiB"));
    println!(
        "Bootstrap: {:.3} s, process peak RSS {peak}",
        elapsed.as_secs_f64()
    );
    bootstrap?;
    let release = rebuilt
        .latest_store_release()?
        .ok_or("bootstrap installed no release")?;
    println!(
        "Applied release {} (state hash {})",
        release.release, release.state_hash
    );

    let source_copy = run_dir.path().join("source-copy.db");
    let original =
        Connection::open_with_flags(Path::new(&source_path), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let newest: i64 =
        original.query_row("SELECT MAX(observed_at_ms) FROM observation", [], |row| {
            row.get(0)
        })?;
    original.backup(DatabaseName::Main, &source_copy, None)?;
    drop(original);
    // Only the disposable copy is opened by the migrator; the comparison then
    // calls only read routes on both stores.
    let source = CatalogStore::open(&descriptor(&source_copy)?)?;

    println!("Masks (top-level fields only):");
    println!("  /catalog_version (catalog.get, catalog.status): local database revision");
    println!("  /source (catalog.get, catalog.history, catalog.status): data-source label");
    println!(
        "Bytes are measured before masking. An error's byte count is its JSON code/message pair."
    );
    let mut findings = compare_catalog(&rebuilt, &source, None)?;
    for at in DATED_INSTANTS_MS.into_iter().chain([newest]) {
        findings += compare_catalog(&rebuilt, &source, Some(at))?;
    }
    for (provider, model, fact) in HISTORY_PROBES {
        let args =
            json!({"provider_id": provider, "model_id": model, "fact_key": fact, "served": true});
        findings += compare_single(
            &rebuilt,
            &source,
            TOOL_HISTORY,
            &format!("{TOOL_HISTORY} {provider}/{model} {fact}"),
            &args,
        )?;
    }
    for (tool, args) in [
        (TOOL_PLAN_PRICES, json!({})),
        (TOOL_QUALITY_GRADES, json!({"prefix": "local:"})),
        (TOOL_STATUS, json!({})),
    ] {
        findings += compare_single(&rebuilt, &source, tool, tool, &args)?;
    }
    println!("Rehearsal complete: {findings} differing replies");
    Ok(if findings == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn descriptor(path: &Path) -> Result<StorageDescriptor> {
    Ok(StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_str().ok_or("store path must be UTF-8")?.into(),
        },
    })
}

struct Reply {
    body: Value,
    bytes: usize,
    is_error: bool,
}

fn read(store: &CatalogStore, tool: &str, args: &Value) -> Result<Reply> {
    let call = serde_json::to_vec(&json!({"name": tool, "arguments": args}))?;
    let (bytes, is_error) = match serve_tool_call(store, &call) {
        Ok(response) => (serde_json::to_vec(&response)?, false),
        Err(error) => (
            serde_json::to_vec(&json!({"code": error.code, "message": error.message}))?,
            true,
        ),
    };
    let mut body: Value = serde_json::from_slice(&bytes)?;
    if let Some(fields) = body.as_object_mut().filter(|_| !is_error) {
        if matches!(tool, TOOL_GET | TOOL_STATUS) {
            fields.remove("catalog_version");
        }
        if matches!(tool, TOOL_GET | TOOL_HISTORY | TOOL_STATUS) {
            fields.remove("source");
        }
    }
    Ok(Reply {
        body,
        bytes: bytes.len(),
        is_error,
    })
}

struct PageScan {
    next: Option<Value>,
    seen_tokens: BTreeSet<String>,
}

impl PageScan {
    fn new(at: Option<i64>) -> Self {
        let mut args = json!({"paged": true});
        if let Some(at) = at {
            args["at_ms"] = at.into();
        }
        Self {
            next: Some(args),
            seen_tokens: BTreeSet::new(),
        }
    }

    fn read(&mut self, store: &CatalogStore) -> Result<Option<Reply>> {
        let Some(mut args) = self.next.take() else {
            return Ok(None);
        };
        let reply = read(store, TOOL_GET, &args)?;
        if let Some(token) = reply.body.get("next_page_token") {
            let token = token.as_str().ok_or("page token is not a string")?;
            if !self.seen_tokens.insert(token.to_owned()) {
                return Err("catalog.get repeated a page token".into());
            }
            args["page_token"] = token.into();
            self.next = Some(args);
        }
        Ok(Some(reply))
    }
}

fn compare_catalog(
    rebuilt: &CatalogStore,
    source: &CatalogStore,
    at: Option<i64>,
) -> Result<usize> {
    let label = at.map_or_else(
        || format!("{TOOL_GET} current"),
        |at| format!("{TOOL_GET} at_ms={at}"),
    );
    let (mut rebuilt_scan, mut source_scan) = (PageScan::new(at), PageScan::new(at));
    let (mut rebuilt_pages, mut source_pages, mut rebuilt_bytes, mut source_bytes) = (0, 0, 0, 0);
    let mut findings = 0;
    loop {
        let (left, right) = (rebuilt_scan.read(rebuilt)?, source_scan.read(source)?);
        if left.is_none() && right.is_none() {
            break;
        }
        rebuilt_pages += usize::from(left.is_some());
        source_pages += usize::from(right.is_some());
        rebuilt_bytes += left.as_ref().map_or(0, |page| page.bytes);
        source_bytes += right.as_ref().map_or(0, |page| page.bytes);
        let page = rebuilt_pages.max(source_pages);
        findings += report(
            &format!("{label} page {page}"),
            left.as_ref(),
            right.as_ref(),
        );
    }
    println!(
        "TOTAL {label}: rebuilt {rebuilt_bytes} B/{rebuilt_pages} pages; source {source_bytes} B/{source_pages} pages; {findings} differing replies"
    );
    Ok(findings)
}

fn compare_single(
    rebuilt: &CatalogStore,
    source: &CatalogStore,
    tool: &str,
    label: &str,
    args: &Value,
) -> Result<usize> {
    let left = read(rebuilt, tool, args)?;
    let right = read(source, tool, args)?;
    Ok(report(label, Some(&left), Some(&right)))
}

fn report(label: &str, rebuilt: Option<&Reply>, source: Option<&Reply>) -> usize {
    fn size(reply: Option<&Reply>) -> String {
        match reply {
            Some(reply) if reply.is_error => format!("{} B (error)", reply.bytes),
            Some(reply) => format!("{} B", reply.bytes),
            None => "absent".into(),
        }
    }

    let difference = match (rebuilt, source) {
        (Some(rebuilt), Some(source)) if rebuilt.is_error == source.is_error => {
            first_difference(&rebuilt.body, &source.body, "")
        }
        _ => Some(Difference {
            pointer: String::new(),
            rebuilt: display(rebuilt.map(|r| &r.body)),
            source: display(source.map(|r| &r.body)),
        }),
    };
    let comparison = if difference.is_some() {
        "DIFF"
    } else {
        "MATCH"
    };
    println!(
        "{comparison} {label}: rebuilt {}; source {}",
        size(rebuilt),
        size(source)
    );
    if let Some(diff) = difference {
        let path = if diff.pointer.is_empty() {
            "(root)"
        } else {
            &diff.pointer
        };
        println!(
            "  {path}\n  rebuilt: {}\n  source:  {}",
            diff.rebuilt, diff.source
        );
        1
    } else {
        if let Some(error) = rebuilt.filter(|reply| reply.is_error) {
            println!("  both returned {}", error.body);
        }
        0
    }
}

fn display(value: Option<&Value>) -> String {
    value.map_or_else(|| "<absent>".into(), Value::to_string)
}

struct Difference {
    pointer: String,
    rebuilt: String,
    source: String,
}

fn first_difference(rebuilt: &Value, source: &Value, pointer: &str) -> Option<Difference> {
    fn at(rebuilt: Option<&Value>, source: Option<&Value>, pointer: String) -> Option<Difference> {
        match (rebuilt, source) {
            (Some(rebuilt), Some(source)) => first_difference(rebuilt, source, &pointer),
            _ => Some(Difference {
                pointer,
                rebuilt: display(rebuilt),
                source: display(source),
            }),
        }
    }

    match (rebuilt, source) {
        (Value::Object(rebuilt), Value::Object(source)) => rebuilt
            .keys()
            .chain(source.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .find_map(|key| {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                at(
                    rebuilt.get(key),
                    source.get(key),
                    format!("{pointer}/{escaped}"),
                )
            }),
        (Value::Array(rebuilt), Value::Array(source)) => (0..rebuilt.len().max(source.len()))
            .find_map(|index| {
                at(
                    rebuilt.get(index),
                    source.get(index),
                    format!("{pointer}/{index}"),
                )
            }),
        _ if rebuilt == source => None,
        _ => Some(Difference {
            pointer: pointer.into(),
            rebuilt: rebuilt.to_string(),
            source: source.to_string(),
        }),
    }
}

fn peak_rss_kib() -> Option<u64> {
    // Linux records the process's resident high-water mark, not just its current RSS.
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("VmHWM:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}
