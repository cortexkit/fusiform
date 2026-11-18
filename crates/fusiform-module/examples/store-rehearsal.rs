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
    let run_dir = tempfile::Builder::new()
        .prefix("rehearsal-")
        .tempdir_in(work_dir)?;
    let run_name = run_dir
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("temporary store directory name must be UTF-8")?;

    println!("Store rehearsal: base={base}, pinned kid={kid}");
    let started = Instant::now();
    let rebuilt = CatalogStore::open_for_store(&descriptor(
        &run_dir.path().join("rebuilt.db"),
        format!("{run_name}-rebuilt"),
    )?)?;
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
    // The store's writer lease is keyed by namespace, not by database path.
    let source = CatalogStore::open(&descriptor(&source_copy, format!("{run_name}-source"))?)?;

    println!("Masks:");
    println!("  /catalog_version (catalog.get, catalog.status): local database revision");
    println!(
        "  /rates_version (catalog.status): local revision issued for the last relevant change"
    );
    println!("  /source (catalog.get, catalog.history, catalog.status): data-source label");
    println!("  /next_page_token catalog_version component: repeats the local database revision");
    println!("  /resolved_at_ms (current catalog.get and plan.prices) and /next_page_token resolved_at_ms component (current catalog.get): each side is read at a different instant");
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

fn descriptor(path: &Path, storage_namespace: String) -> Result<StorageDescriptor> {
    Ok(StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace,
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
    Ok(Reply {
        body: serde_json::from_slice(&bytes)?,
        bytes: bytes.len(),
        is_error,
    })
}

fn mask(reply: &mut Reply, tool: &str, current: bool) -> Result<()> {
    if let Some(fields) = reply.body.as_object_mut().filter(|_| !reply.is_error) {
        if matches!(tool, TOOL_GET | TOOL_STATUS) {
            fields.remove("catalog_version");
        }
        if tool == TOOL_STATUS {
            fields.remove("rates_version");
        }
        if matches!(tool, TOOL_GET | TOOL_HISTORY | TOOL_STATUS) {
            fields.remove("source");
        }
        if current {
            fields.remove("resolved_at_ms");
        }
        if tool == TOOL_GET {
            if let Some(token) = fields.get_mut("next_page_token") {
                let raw = token.as_str().ok_or("page token is not a string")?;
                let parts: Vec<_> = raw.splitn(4, ':').collect();
                let ["v1", _version, at, provider] = parts.as_slice() else {
                    return Err(format!("unexpected page token format: {raw:?}").into());
                };
                let at = if current { "<instant>" } else { *at };
                *token = format!("v1:<version>:{at}:{provider}").into();
            }
        }
    }
    Ok(())
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
        let (mut left, mut right) = (rebuilt_scan.read(rebuilt)?, source_scan.read(source)?);
        if left.is_none() && right.is_none() {
            break;
        }
        rebuilt_pages += usize::from(left.is_some());
        source_pages += usize::from(right.is_some());
        rebuilt_bytes += left.as_ref().map_or(0, |page| page.bytes);
        source_bytes += right.as_ref().map_or(0, |page| page.bytes);
        let page = rebuilt_pages.max(source_pages);
        // Follow the unmodified continuation token before masking its copy.
        for reply in left.iter_mut().chain(right.iter_mut()) {
            mask(reply, TOOL_GET, at.is_none())?;
        }
        findings += report(&label, Some(page), left.as_ref(), right.as_ref());
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
    let mut left = read(rebuilt, tool, args)?;
    let mut right = read(source, tool, args)?;
    let current = tool == TOOL_PLAN_PRICES && args.get("at_ms").is_none();
    mask(&mut left, tool, current)?;
    mask(&mut right, tool, current)?;
    Ok(report(label, None, Some(&left), Some(&right)))
}

fn report(
    label: &str,
    page: Option<usize>,
    rebuilt: Option<&Reply>,
    source: Option<&Reply>,
) -> usize {
    fn size(reply: Option<&Reply>) -> String {
        match reply {
            Some(reply) if reply.is_error => format!("{} B (error)", reply.bytes),
            Some(reply) => format!("{} B", reply.bytes),
            None => "absent".into(),
        }
    }

    let left_body = rebuilt.map(|reply| &reply.body);
    let right_body = source.map(|reply| &reply.body);
    if label == TOOL_PLAN_PRICES {
        let rows = |body: Option<&Value>| body?.get("prices")?.as_array().map(Vec::len);
        if let (Some(left), Some(right)) = (rows(left_body), rows(right_body)) {
            println!("ROWS {label}: rebuilt {left}; source {right}");
        }
    }
    let mut count = 0;
    let mut emit = |path: &str, left: Option<&Value>, right: Option<&Value>| {
        count += 1;
        let plan_row = |value: Option<&Value>| {
            let index = path.strip_prefix("/prices/")?.split('/').next()?;
            let row = value?.get("prices")?.get(index.parse::<usize>().ok()?)?;
            Some(json!([row.get("provider_id")?, row.get("tier")?]))
        };
        // An absent value and an explicit JSON null are different findings.
        let mut finding = json!({"read": label, "page": page, "path": path});
        if let Some(value) = left {
            finding["rebuilt"] = value.clone();
        }
        if let Some(value) = right {
            finding["source"] = value.clone();
        }
        if label == TOOL_PLAN_PRICES {
            finding["rebuilt_row"] = json!(plan_row(left_body));
            finding["source_row"] = json!(plan_row(right_body));
        }
        println!("DIFFERENCE {finding}");
    };
    if rebuilt.map(|r| r.is_error) != source.map(|r| r.is_error) {
        emit("", left_body, right_body);
    } else {
        differences(left_body, right_body, "", &mut emit);
    }
    let page = page.map_or_else(String::new, |p| format!(" page {p}"));
    let comparison = if count == 0 { "MATCH" } else { "DIFF" };
    println!(
        "{comparison} {label}{page}: rebuilt {}; source {}; {count} differing paths",
        size(rebuilt),
        size(source),
    );
    if count == 0 {
        if let Some(error) = rebuilt.filter(|reply| reply.is_error) {
            println!("  both returned {}", error.body);
        }
    }
    usize::from(count != 0)
}

fn differences(
    rebuilt: Option<&Value>,
    source: Option<&Value>,
    path: &str,
    emit: &mut impl FnMut(&str, Option<&Value>, Option<&Value>),
) {
    if rebuilt == source {
        return;
    }
    match (rebuilt, source) {
        (Some(Value::Object(rebuilt)), Some(Value::Object(source))) => {
            for key in rebuilt.keys().chain(source.keys()).collect::<BTreeSet<_>>() {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                differences(
                    rebuilt.get(key),
                    source.get(key),
                    &format!("{path}/{escaped}"),
                    emit,
                );
            }
        }
        (Some(Value::Array(rebuilt)), Some(Value::Array(source))) => {
            for index in 0..rebuilt.len().max(source.len()) {
                differences(
                    rebuilt.get(index),
                    source.get(index),
                    &format!("{path}/{index}"),
                    emit,
                );
            }
        }
        // If a whole subtree is missing, report its path and full value once.
        _ => emit(path, rebuilt, source),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_mask_keeps_provider_and_dated_instant() {
        let body = json!({
            "catalog_version": 91,
            "source": "models.dev",
            "resolved_at_ms": 123,
            "next_page_token": "v1:91:123:provider:suffix",
            "models": {},
        });
        for (current, expected_token) in [
            (false, "v1:<version>:123:provider:suffix"),
            (true, "v1:<version>:<instant>:provider:suffix"),
        ] {
            let mut reply = Reply {
                body: body.clone(),
                bytes: 1,
                is_error: false,
            };
            mask(&mut reply, TOOL_GET, current).unwrap();
            assert_eq!(reply.body["next_page_token"], expected_token);
            assert_eq!(
                reply.body.get("resolved_at_ms"),
                (!current).then_some(&body["resolved_at_ms"])
            );
            assert!(
                reply.body.get("catalog_version").is_none() && reply.body.get("source").is_none()
            );
            assert_eq!(reply.body["models"], body["models"]);
        }
    }

    #[test]
    fn full_diff_keeps_later_paths_and_distinguishes_absent_from_null() {
        let rebuilt =
            json!({"models": {"slash/~": {"first": null, "later": true}}, "tail": [1, 2]});
        let source = json!({"models": {"slash/~": {"later": false}}, "tail": [1]});
        let mut changes = Vec::new();
        differences(
            Some(&rebuilt),
            Some(&source),
            "",
            &mut |path, left, right| {
                changes.push((path.to_string(), left.cloned(), right.cloned()));
            },
        );
        assert_eq!(
            changes,
            [
                ("/models/slash~1~0/first".into(), Some(Value::Null), None),
                (
                    "/models/slash~1~0/later".into(),
                    Some(json!(true)),
                    Some(json!(false))
                ),
                ("/tail/1".into(), Some(json!(2)), None),
            ]
        );
    }
}
