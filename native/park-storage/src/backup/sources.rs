use super::{object, read_json, Error, Object, Result};
use crate::cookies::{self, Files};
use crate::recovery::valid_key;
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs, io::ErrorKind, path::Path};

pub(crate) fn cookies_on_disk(root: &Path, key: &str) -> Result<Files> {
    let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    let mut dir = root.to_path_buf();
    for name in ["cookie_jars", &hex, "ie0_ps1"] {
        dir.push(name);
        match fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => (),
            Ok(_) => return Err(Error::Invalid),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Files::new()),
            Err(e) => return Err(e.into()),
        }
        if name == hex {
            if let Some(pending) = read_json(&dir.join("pending_cookies.json"))? {
                let files = serde_json::from_value(pending)?;
                cookies::parse(&files, true).map_err(|_| Error::Invalid)?;
                return Ok(files);
            }
        }
    }
    let mut files = Files::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(Error::Invalid);
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::Invalid)?;
        files.insert(format!("ie0_ps1/{name}"), fs::read_to_string(entry.path())?);
    }
    cookies::parse(&files, true).map_err(|_| Error::Invalid)?;
    Ok(files)
}

pub(super) fn portable_metadata(row: &Value) -> Value {
    ["key", "origin", "url", "enabled", "order"]
        .iter()
        .map(|&key| (key.to_owned(), row[key].clone()))
        .collect::<Object>()
        .into()
}

pub(super) fn snapshot(root: &Path, credentials: bool) -> Result<Value> {
    let mut registry = object(read_json(&root.join("sources.json"))?)?;
    let removed = registry
        .remove("removedOfficial")
        .unwrap_or_else(|| json!([]));
    let mut records = Vec::new();
    let mut keys = BTreeSet::new();
    let installed = registry.remove("sources").unwrap_or_else(|| json!([]));
    for row in installed.as_array().ok_or(Error::Invalid)? {
        let key = row["key"]
            .as_str()
            .filter(|k| valid_key(k))
            .ok_or(Error::Invalid)?;
        if !keys.insert(key.to_owned()) {
            return Err(Error::Invalid);
        }
        records.push(portable_metadata(row));
    }
    records.sort_by_key(|row| row["order"].as_i64().unwrap_or_default());
    let registered = keys.clone();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(key) = name
            .to_str()
            .and_then(|s| s.strip_prefix("source_state_"))
            .and_then(|s| s.strip_suffix(".json"))
            .filter(|s| valid_key(s))
        {
            keys.insert(key.to_owned());
        }
    }
    let mut settings = Object::new();
    let mut data = Object::new();
    let mut cookies = Object::new();
    for key in keys {
        let mut state = object(read_json(&root.join(format!("source_state_{key}.json")))?)?;
        settings.insert(key.clone(), object(state.remove("settings"))?.into());
        if !registered.contains(&key) {
            if let Some(meta) = state.remove("restoreMetadata") {
                if meta["key"] != key {
                    return Err(Error::Invalid);
                }
                records.push(portable_metadata(&meta));
            }
        }
        if credentials {
            data.insert(key.clone(), object(state.remove("data"))?.into());
            let jar = match state.remove("restoreCookies") {
                None => cookies_on_disk(root, &key)?,
                Some(value) => {
                    let jar = serde_json::from_value(value)?;
                    cookies::parse(&jar, true).map_err(|_| Error::Invalid)?;
                    jar
                }
            };
            cookies.insert(key, serde_json::to_value(jar)?);
        }
    }
    let mut out = json!({"removedOfficial":removed,"sources":records,"settings":settings});
    if credentials {
        out["cookies"] = cookies.into();
        out["data"] = data.into();
    }
    Ok(out)
}
