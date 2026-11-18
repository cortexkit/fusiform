use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
};

use fusiform_release::{decode_release_public_key, VerifyingKey};
use reqwest::Url;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub source: Source,
}

#[derive(Debug, Default, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    #[default]
    ModelsDev,
    Store {
        url: String,
        keys: Vec<PinnedKey>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedKey {
    kid: String,
    public_key: String,
}

pub type ReleaseKeys = BTreeMap<String, VerifyingKey>;

pub fn validate_store(url: &str, keys: &[PinnedKey]) -> Result<(Url, ReleaseKeys), String> {
    let mut url = Url::parse(url).map_err(|err| format!("invalid store URL: {err}"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "store URL must be an HTTP(S) base URL without credentials, query, or fragment".into(),
        );
    }
    if !url.path().ends_with('/') {
        url.path_segments_mut()
            .map_err(|_| "store URL cannot be a base")?
            .push("");
    }
    let mut pinned = BTreeMap::new();
    for key in keys {
        if key.kid.trim().is_empty() || pinned.contains_key(&key.kid) {
            return Err("store key ids must be nonempty and unique".into());
        }
        let value = decode_release_public_key(&key.public_key)
            .map_err(|err| format!("store key {:?}: {err}", key.kid))?;
        pinned.insert(key.kid.clone(), value);
    }
    if pinned.is_empty() {
        return Err("store mode requires at least one pinned key".into());
    }
    Ok((url, pinned))
}

impl Settings {
    pub fn load() -> Result<Self, String> {
        fn absolute_env(name: &str) -> Option<PathBuf> {
            env::var_os(name)
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute())
        }
        let folder = if cfg!(windows) {
            absolute_env("APPDATA")
        } else {
            (cfg!(target_os = "linux"))
                .then(|| absolute_env("XDG_CONFIG_HOME"))
                .flatten()
                .or_else(|| absolute_env("HOME").map(|home| home.join(".config")))
        }
        .ok_or("cannot resolve the platform configuration folder")?;
        Self::load_from(&folder.join("cortexkit/fusiform.jsonc"))
    }

    fn load_from(path: &Path) -> Result<Self, String> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(format!("{}: {err}", path.display())),
        };
        let contextual = |err: String| format!("{}: {err}", path.display());
        let parsed: Self = serde_json::from_slice(
            &json_without_comments(contents.as_bytes()).map_err(contextual)?,
        )
        .map_err(|err| contextual(err.to_string()))?;
        if let Source::Store { url, keys } = &parsed.source {
            validate_store(url, keys).map_err(contextual)?;
        }
        Ok(parsed)
    }
}

fn json_without_comments(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = input.to_vec();
    let mut at = 0;
    let mut string = false;
    while at < input.len() {
        match input[at] {
            b'"' => string = !string,
            b'\\' if string => at += 1,
            b'/' if !string && input.get(at + 1) == Some(&b'/') => {
                while at < input.len() && !matches!(input[at], b'\n' | b'\r') {
                    out[at] = b' ';
                    at += 1;
                }
                continue;
            }
            b'/' if !string && input.get(at + 1) == Some(&b'*') => {
                let start = at;
                let end = input[at + 2..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .ok_or("unterminated JSONC block comment")?;
                at += end + 4;
                for byte in &mut out[start..at] {
                    if !matches!(*byte, b'\n' | b'\r') {
                        *byte = b' ';
                    }
                }
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_settings_only_missing_file_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fusiform.jsonc");
        assert!(matches!(
            Settings::load_from(&path).unwrap().source,
            Source::ModelsDev
        ));
        fs::write(
            &path,
            r#"{/* explicit */ "source": {"mode":"models_dev"}} // done"#,
        )
        .unwrap();
        assert!(matches!(
            Settings::load_from(&path).unwrap().source,
            Source::ModelsDev
        ));
        fs::write(
            &path,
            r#"{"source":{"mode":"store","url":"https://example.test/store","keys":[]}}"#,
        )
        .unwrap();
        assert!(Settings::load_from(&path)
            .unwrap_err()
            .contains("at least one pinned key"));
        fs::write(&path, "{ /* unfinished").unwrap();
        assert!(Settings::load_from(&path)
            .unwrap_err()
            .contains("unterminated"));
    }
}
