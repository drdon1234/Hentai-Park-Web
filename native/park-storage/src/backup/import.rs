use super::{
    library::{self, required},
    object, portable_settings, read_json, sources, Error, Object, Result,
};
use crate::{
    cookies::{self, Files},
    recovery::{valid_key, Partition},
};
use flate2::read::MultiGzDecoder;
use rusqlite::{params, Transaction};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};
use url::Url;

pub struct Import {
    pub files: BTreeMap<String, Value>,
    pub cookies: BTreeMap<String, Partition>,
    pub(super) data: Value,
}
fn notes(root: &Path, incoming: &Value) -> Result<Value> {
    let old = read_json(&root.join("reading_notes.json"))?.unwrap_or_else(|| json!([]));
    let mut result = Object::new();
    for rows in [&old, incoming] {
        for row in rows.as_array().ok_or(Error::Invalid)? {
            if !["uid", "id", "chapterId", "quote", "note"]
                .iter()
                .all(|n| row[*n].is_string())
                || !row["offset"].as_i64().is_some_and(|s| s >= 0)
                || !row["time"]
                    .as_i64()
                    .is_some_and(|t| (-8640000000000000..=8640000000000000).contains(&t))
            {
                return Err(Error::Invalid);
            }
            let id = row["id"].as_str().ok_or(Error::Invalid)?;
            if result
                .get(id)
                .is_none_or(|v| v["time"].as_i64() < row["time"].as_i64())
            {
                result.insert(id.to_owned(), row.clone());
            }
        }
    }
    Ok(result.into_values().collect::<Vec<_>>().into())
}
pub(super) fn metadata(row: &Value) -> Result<&str> {
    let key = required(row, "key")?;
    if !valid_key(key)
        || !row["enabled"].is_null() && !row["enabled"].is_boolean()
        || !row["order"].is_null() && !row["order"].is_u64()
        || !row["url"].is_null() && !row["url"].is_string()
    {
        return Err(Error::Invalid);
    }
    if row["origin"] == "url" {
        let u = Url::parse(required(row, "url")?).map_err(|_| Error::Invalid)?;
        if !matches!(u.scheme(), "http" | "https")
            || !u.has_host()
            || !u.username().is_empty()
            || u.password().is_some()
        {
            return Err(Error::Invalid);
        }
    }
    Ok(key)
}
pub(crate) fn subscriptions(tx: &Transaction<'_>, v: &Value) -> Result<()> {
    let items = v
        .get("items")
        .unwrap_or(&v["targets"])
        .as_array()
        .filter(|a| a.len() <= 10000)
        .ok_or(Error::Invalid)?;
    for item in items {
        let source = required(item, "source")?;
        let target = required(item, "target")?.trim();
        let field = |k: &str, default: bool| -> Result<bool> {
            if item[k].is_null() {
                Ok(default)
            } else {
                item[k].as_bool().ok_or(Error::Invalid)
            }
        };
        let dim = item
            .get("dimension")
            .filter(|v| !v.is_null())
            .map_or(Some("posts"), Value::as_str)
            .ok_or(Error::Invalid)?;
        let label = item
            .get("label")
            .filter(|v| !v.is_null())
            .map_or(Some(target), Value::as_str)
            .ok_or(Error::Invalid)?;
        let identity = crate::subscriptions::target_identity(source, dim, target, label)?;
        tx.execute("INSERT OR IGNORE INTO subscriptions(identity,source,dimension,target,label,enabled,notify,created_at) VALUES(?,?,?,?,?,?,?,unixepoch('subsec')*1000)",
            params![identity,source,dim,target,label,field("enabled",true)?,field("notify",true)?])?;
        tx.execute(
            "INSERT OR IGNORE INTO subscription_checks(identity) VALUES(?)",
            [identity],
        )?;
    }
    Ok(())
}
impl Import {
    pub fn read(
        root: &Path,
        stream: impl Read,
        mut domains: impl FnMut(&str, &str) -> Result<Vec<String>>,
    ) -> Result<Self> {
        let data = Self::decode(stream)?;
        Self::portable(root, data, &mut domains)
    }
    pub(super) fn decode(stream: impl Read) -> Result<Value> {
        let data: Value =
            serde_json::from_reader(std::io::BufReader::new(MultiGzDecoder::new(stream)))?;
        if data["format"] != "parkbak" || data["version"].as_i64() != Some(1) {
            return Err(Error::Invalid);
        }
        Ok(data)
    }
    pub(super) fn portable(
        root: &Path,
        data: Value,
        mut domains: impl FnMut(&str, &str) -> Result<Vec<String>>,
    ) -> Result<Self> {
        let mut out = Self {
            files: BTreeMap::new(),
            cookies: BTreeMap::new(),
            data: Value::Null,
        };
        if !data["settings"].is_null() {
            let mut incoming = data["settings"].as_object().ok_or(Error::Invalid)?.clone();
            super::desktop::portable_theme(&mut incoming)?;
            if !incoming
                .iter()
                .all(|(k, v)| park_domain::settings::valid(k, v))
            {
                return Err(Error::Invalid);
            }
            let mut settings = object(read_json(&root.join("settings.json"))?)?;
            settings.extend(portable_settings(incoming.clone()));
            park_domain::settings::normalize(&mut settings);
            out.files.insert("settings.json".into(), settings.into());
        }
        if !data["readingNotes"].is_null() {
            out.files.insert(
                "reading_notes.json".into(),
                notes(root, &data["readingNotes"])?,
            );
        }
        if !data["sources"].is_null() {
            out.sources(root, &data["sources"], &mut domains)?;
        }
        out.data = data;
        Ok(out)
    }
    fn sources(
        &mut self,
        root: &Path,
        data: &Value,
        domains: &mut impl FnMut(&str, &str) -> Result<Vec<String>>,
    ) -> Result<()> {
        if !data.is_object() {
            return Err(Error::Invalid);
        }
        let mut registry = object(read_json(&root.join("sources.json"))?)?;
        let mut installed = Object::new();
        for row in registry
            .remove("sources")
            .unwrap_or_else(|| json!([]))
            .as_array()
            .ok_or(Error::Invalid)?
        {
            installed.insert(metadata(row)?.to_owned(), row.clone());
        }
        let mut removed = BTreeSet::new();
        for list in [registry.get("removedOfficial"), data.get("removedOfficial")]
            .into_iter()
            .flatten()
        {
            for k in list.as_array().ok_or(Error::Invalid)? {
                removed.insert(
                    k.as_str()
                        .filter(|s| valid_key(s))
                        .ok_or(Error::Invalid)?
                        .to_owned(),
                );
            }
        }
        let mut incoming = Object::new();
        for row in library::rows(data, "sources")? {
            let key = metadata(row)?;
            incoming.insert(key.to_owned(), row.clone());
            if let Some(current) = installed.get_mut(key) {
                current["enabled"] = json!(row["enabled"] != false);
                if !row["order"].is_null() {
                    current["order"] = row["order"].clone();
                }
            }
        }
        let opts = object(data.get("settings").cloned())?;
        let private = object(data.get("data").cloned())?;
        let jars = object(data.get("cookies").cloned())?;
        let keys: BTreeSet<_> = opts
            .keys()
            .chain(private.keys())
            .chain(jars.keys())
            .chain(incoming.keys())
            .collect();
        for key in keys {
            if !valid_key(key) {
                return Err(Error::Invalid);
            }
            let file = format!("source_state_{key}.json");
            let mut state = object(read_json(&root.join(&file))?)?;
            if let Some(v) = opts.get(key) {
                let mut local = object(state.remove("settings"))?;
                local.extend(v.as_object().ok_or(Error::Invalid)?.clone());
                state.insert("settings".into(), local.into());
            }
            if let Some(v) = private.get(key) {
                let mut local = if jars.contains_key(key) {
                    Object::new()
                } else {
                    object(state.remove("data"))?
                };
                local.extend(v.as_object().ok_or(Error::Invalid)?.clone());
                state.insert("data".into(), local.into());
            }
            if let Some(v) = jars.get(key) {
                let after: Files = super::desktop::cookie_files(v)?;
                cookies::parse(&after, true).map_err(|_| Error::Invalid)?;
                if let Some(record) = installed.get(key) {
                    let before = sources::cookies_on_disk(root, key)?;
                    if !before.is_empty() || !after.is_empty() {
                        let allowed = domains(key, record["sha256"].as_str().unwrap_or(""))?;
                        cookies::validate_domains(&after, &allowed).map_err(|_| Error::Invalid)?;
                        self.cookies.insert(
                            key.clone(),
                            Partition {
                                before,
                                after,
                                domains: allowed,
                            },
                        );
                    }
                    state.remove("restoreCookies");
                } else {
                    state.insert("restoreCookies".into(), v.clone());
                }
            }
            if !installed.contains_key(key) {
                if let Some(meta) = incoming.get(key) {
                    state.insert("restoreMetadata".into(), meta.clone());
                }
            }
            self.files.insert(file, state.into());
        }
        registry.insert("removedOfficial".into(), json!(removed));
        registry.insert(
            "sources".into(),
            installed.into_values().collect::<Vec<_>>().into(),
        );
        self.files.insert("sources.json".into(), registry.into());
        Ok(())
    }
    pub fn merge(&self, tx: &Transaction<'_>, cancelled: impl Fn() -> bool) -> Result<bool> {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        if !self.data["tavern"].is_null() {
            super::merge_tavern(tx, &self.data["tavern"], &cancelled)?;
        }
        if !self.data["subscriptions"].is_null() {
            subscriptions(tx, &self.data["subscriptions"])?;
        }
        let changed = if self.data["library"].is_null() {
            false
        } else {
            super::merge_library(tx, &self.data["library"], &cancelled)?
        };
        if cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(changed)
    }
}
