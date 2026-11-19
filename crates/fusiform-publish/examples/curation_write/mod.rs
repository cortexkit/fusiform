use std::{collections::BTreeMap, fs, io::Write as _, path::PathBuf};

use fusiform_module::{
    settings::{PinnedKey, validate_store},
    store_poll::StorePoller,
};
use reqwest::Url;

mod drafts;
#[cfg(test)]
mod tests;
mod vendor;

const USAGE: &str = "\
usage: curation-write --url <https://store> --key-id <kid> --public-key <base64url> <command>
  vendor <document> --file <path> --note <text> --source <text> --review-by <date> --out <dir>
  draft <family> <model-key> (--value <json> | --remove) --note <text> --out <dir>
  from-local --model <key> --local-json <path> --out <dir>";

type Result<T> = std::result::Result<T, String>;

struct Request {
    path: String,
    body: String,
    summary: String,
    subdirectory: Option<&'static str>,
}

struct Args {
    positionals: Vec<String>,
    flags: BTreeMap<String, String>,
}

impl Args {
    fn parse(mut input: impl Iterator<Item = String>) -> Result<Self> {
        let mut args = Self {
            positionals: Vec::new(),
            flags: BTreeMap::new(),
        };
        while let Some(arg) = input.next() {
            if arg.starts_with("--") {
                let value = if arg == "--remove" {
                    String::new()
                } else {
                    input
                        .next()
                        .ok_or_else(|| format!("missing value for {arg}"))?
                };
                if args.flags.insert(arg.clone(), value).is_some() {
                    return Err(format!("duplicate option {arg}"));
                }
            } else {
                args.positionals.push(arg);
            }
        }
        Ok(args)
    }

    fn take(&mut self, flag: &str) -> Result<String> {
        self.flags
            .remove(flag)
            .ok_or_else(|| format!("missing {flag}"))
    }
}

enum Command {
    Vendor(vendor::Input),
    Draft(drafts::Input),
    FromLocal(drafts::LocalInput),
}

pub(super) async fn run(input: impl Iterator<Item = String>) -> Result<()> {
    let input: Vec<_> = input.collect();
    if input.as_slice() == ["--help"] || input.is_empty() {
        println!("{USAGE}");
        return Ok(());
    }
    let mut args = Args::parse(input.into_iter())?;
    let command = match std::mem::take(&mut args.positionals).as_slice() {
        [command, document] if command == "vendor" => Command::Vendor(vendor::Input::new(
            document,
            &read(&args.take("--file")?)?,
            args.take("--note")?,
            args.take("--source")?,
            args.take("--review-by")?,
        )?),
        [command, family, key] if command == "draft" => {
            let value = match (args.flags.remove("--value"), args.flags.remove("--remove")) {
                (Some(value), None) if value.trim() != "null" => value,
                (Some(_), None) => return Err("use --remove to propose a removal".into()),
                (None, Some(_)) => "null".into(),
                _ => return Err("draft requires exactly one of --value or --remove".into()),
            };
            Command::Draft(drafts::Input::new(
                family,
                key,
                &value,
                args.take("--note")?,
            )?)
        }
        [command] if command == "from-local" => Command::FromLocal(drafts::LocalInput::new(
            args.take("--model")?,
            &read(&args.take("--local-json")?)?,
        )?),
        _ => return Err(USAGE.into()),
    };
    let out = PathBuf::from(args.take("--out")?);
    let url = args.take("--url")?;
    let key = PinnedKey::new(args.take("--key-id")?, args.take("--public-key")?);
    if let Some(flag) = args.flags.keys().next() {
        return Err(format!("unrecognized option {flag}"));
    }
    let (url, keys) = validate_store(&url, &[key])?;
    require_https(&url)?;
    let head = StorePoller::new(url, keys)
        .map_err(|error| error.to_string())?
        .read_verified_head()
        .await?;
    println!(
        "Verified release {}, curation version {}",
        head.release, head.state.curation_set.version
    );
    let requests = match command {
        Command::Vendor(input) => vec![input.prepare(&head)?],
        Command::Draft(input) => vec![input.prepare(&head.state)?],
        Command::FromLocal(input) => input.prepare(&head.state)?,
    };
    if requests.is_empty() {
        println!("No changed drafts");
    }
    for request in requests {
        let dir = request
            .subdirectory
            .map_or_else(|| out.clone(), |family| out.join(family));
        fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let path = dir.join("request.json");
        let mut body = tempfile::NamedTempFile::new_in(&dir).map_err(|error| error.to_string())?;
        body.write_all(request.body.as_bytes())
            .map_err(|error| error.to_string())?;
        body.persist_noclobber(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        println!(
            "{}\nPUT {} ({})",
            request.summary,
            request.path,
            path.display()
        );
    }
    Ok(())
}

fn require_https(url: &Url) -> Result<()> {
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() == "https" || loopback {
        Ok(())
    } else {
        Err("the store URL must use HTTPS (HTTP is only allowed for loopback)".into())
    }
}

fn read(path: &str) -> Result<String> {
    fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))
}

fn segment(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

fn metadata(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 4096 {
        return Err(format!("{field} must be nonempty and at most 4 KiB"));
    }
    Ok(())
}

fn json<T: serde::Serialize>(value: T) -> String {
    serde_json::to_string(&value).expect("request fields are JSON")
}
