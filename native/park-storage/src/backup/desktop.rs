use super::{
    archive, encode, object, portable_settings, read_json, Error, Import, Object, Result, Scope,
};
use crate::cookies::{self, Files};
use crate::recovery::valid_key;
use cookie::{time::OffsetDateTime, Cookie};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeSet, io::Read, path::Path};

pub(super) fn portable_theme(settings: &mut Object) -> Result<()> {
    if let Some(theme) = settings.get("themeMode").and_then(Value::as_str) {
        let n = match theme {
            "system" => 0,
            "light" => 1,
            "dark" => 2,
            _ => return Err(Error::Invalid),
        };
        settings.insert("themeMode".into(), n.into());
    }
    Ok(())
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DesktopCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    secure: bool,
    http_only: bool,
    expires_at_ms: Option<i64>,
    #[serde(default = "host_only")]
    host_only: bool,
}
fn host_only() -> bool {
    true
}
pub(super) fn cookie_files(value: &Value) -> Result<Files> {
    if !value.is_array() {
        return Ok(serde_json::from_value(value.clone())?);
    }
    let rows: Vec<DesktopCookie> = serde_json::from_value(value.clone())?;
    let mut hosts = Object::new();
    let mut domains = Object::new();
    for row in rows {
        if row.name.is_empty()
            || !row
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            || row.value.contains(';')
            || row.value.chars().any(char::is_control)
        {
            return Err(Error::Invalid);
        }
        let domain = row.domain.trim_start_matches('.').to_ascii_lowercase();
        let mut c = Cookie::new(row.name.clone(), row.value);
        c.set_path(row.path.clone());
        c.set_secure(row.secure);
        c.set_http_only(row.http_only);
        let host = if row.host_only {
            hosts.entry(&domain)
        } else {
            c.set_domain(domain.clone());
            domains.entry(&domain)
        }
        .or_insert_with(|| json!({}));
        if let Some(stamp) = row.expires_at_ms {
            c.set_expires(
                OffsetDateTime::from_unix_timestamp(stamp.div_euclid(1000))
                    .map_err(|_| Error::Invalid)?,
            );
        }
        host[&row.path][&row.name] = format!("{c};_crt=0").into();
    }
    let mut out = Files::new();
    if !hosts.is_empty() {
        out.insert(
            "ie0_ps1/.index".into(),
            serde_json::to_string(&hosts.keys().collect::<Vec<_>>())?,
        );
        for (key, paths) in hosts {
            out.insert(format!("ie0_ps1/{key}"), paths.to_string());
        }
    }
    if !domains.is_empty() {
        out.insert("ie0_ps1/.domains".into(), serde_json::to_string(&domains)?);
    }
    cookies::parse(&out, true).map_err(|_| Error::Invalid)?;
    Ok(out)
}
fn desktop_cookies(files: &Files) -> Result<Value> {
    let jar = cookies::parse(files, true).map_err(|_| Error::Invalid)?;
    let rows = jar
        .into_iter()
        .map(|((domain, path, name, shared), c)| DesktopCookie {
            name,
            value: c.value().into(),
            domain,
            path,
            secure: c.secure() == Some(true),
            http_only: c.http_only() == Some(true),
            expires_at_ms: c.expires_datetime().map(|d| d.unix_timestamp() * 1000),
            host_only: !shared,
        })
        .collect::<Vec<_>>();
    Ok(serde_json::to_value(rows)?)
}
fn snapshot(root: &Path, installed: &Object, credentials: bool) -> Result<Value> {
    let mut keys: BTreeSet<String> = installed.keys().cloned().collect();
    for dir in ["source_state", "cookies"] {
        if dir == "cookies" && !credentials {
            continue;
        }
        let dir = root.join(dir);
        if !std::fs::symlink_metadata(&dir)?.is_dir() {
            return Err(Error::Invalid);
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let key = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or(Error::Invalid)?;
            if key.starts_with("source_install_") {
                continue;
            }
            if !valid_key(key) || !entry.file_type()?.is_file() {
                return Err(Error::Invalid);
            }
            keys.insert(key.into());
        }
    }
    let mut settings = Object::new();
    let mut private = Object::new();
    let mut cookies = Object::new();
    let mut records = Vec::new();
    for k in keys {
        if !valid_key(&k) {
            return Err(Error::Invalid);
        }
        let state = object(read_json(
            &root.join("source_state").join(format!("{k}.json")),
        )?)?;
        let opts = object(state.get("settings").cloned())?;
        if let Some(row) = installed.get(&k) {
            records.push(super::sources::portable_metadata(row));
        } else if let Some(meta) = state.get("restoreMetadata") {
            if super::import::metadata(meta)? != k {
                return Err(Error::Invalid);
            }
            records.push(super::sources::portable_metadata(meta));
        }
        settings.insert(k.clone(), opts.into());
        if credentials {
            private.insert(k.clone(), object(state.get("store").cloned())?.into());
            let rows = if let Some(pending) = state.get("restoreCookies") {
                desktop_cookies(&cookie_files(pending)?)?
            } else {
                read_json(&root.join("cookies").join(format!("{k}.json")))?
                    .unwrap_or_else(|| json!([]))
            };
            cookie_files(&rows)?;
            cookies.insert(k, rows);
        }
    }
    records.sort_by_key(|r| r["order"].as_i64().unwrap_or_default());
    let mut out = json!({"sources":records,"settings":settings});
    if credentials {
        out["data"] = private.into();
        out["cookies"] = cookies.into();
    }
    Ok(out)
}
pub fn save_desktop(
    db: &Connection,
    root: &Path,
    dir: &Path,
    timestamp: &str,
    scope: Scope,
    library: bool,
    settings: bool,
    sources: bool,
    installed: &Object,
    cancelled: impl Fn() -> bool,
) -> Result<String> {
    archive::ready(root)?;
    let snapshot = if sources {
        snapshot(root, installed, scope == Scope::WithCredentials)?
    } else {
        Value::Null
    };
    archive::persist(
        dir,
        timestamp,
        |out| {
            encode::write_sections(
                db,
                root,
                out,
                scope,
                library,
                settings,
                sources,
                archive::now_ms()? as u128,
                &cancelled,
                Some(&snapshot),
            )
        },
        &cancelled,
    )
}

/// Windows 文件属于本机宿主，导入规划与书库、酒馆、进度共享同一事务检查点。
pub fn import_desktop(root: &Path, src: impl Read, installed: &Object) -> Result<Import> {
    let mut doc = Import::decode(src)?;
    let top = doc.as_object_mut().ok_or(Error::Invalid)?;
    let settings = top.remove("settings");
    let sources = top.remove("sources");
    let mut plan = Import::portable(root, doc, |_, _| Err(Error::Invalid))?;
    if let Some(v) = settings.filter(|v| !v.is_null()) {
        let mut incoming = object(Some(v))?;
        portable_theme(&mut incoming)?;
        if !incoming
            .iter()
            .all(|(k, v)| park_domain::settings::valid(k, v))
        {
            return Err(Error::Invalid);
        }
        if let Some(theme) = incoming.get("themeMode").and_then(Value::as_u64) {
            incoming.insert(
                "themeMode".into(),
                match theme {
                    0 => "system",
                    1 => "light",
                    2 => "dark",
                    _ => return Err(Error::Invalid),
                }
                .into(),
            );
        }
        incoming.remove("onboarded");
        let mut merged = object(read_json(&root.join("settings.json"))?)?;
        merged.extend(portable_settings(incoming));
        plan.files.insert("settings.json".into(), merged.into());
    }
    if let Some(src) = sources.filter(|v| !v.is_null()) {
        let src = object(Some(src))?;
        let opts = object(src.get("settings").cloned())?;
        let data = object(src.get("data").cloned())?;
        let jars = object(src.get("cookies").cloned())?;
        let mut records = Object::new();
        if let Some(v) = src.get("sources") {
            for row in v.as_array().ok_or(Error::Invalid)? {
                let key = super::import::metadata(row)?;
                records.insert(key.to_owned(), super::sources::portable_metadata(row));
            }
        }
        let keys: BTreeSet<_> = opts
            .keys()
            .chain(data.keys())
            .chain(jars.keys())
            .chain(records.keys())
            .collect();
        for k in keys {
            if !valid_key(k) {
                return Err(Error::Invalid);
            }
            let file = format!("source_state/{k}.json");
            let path = crate::recovery::asset_path(root, &file).map_err(|_| Error::Invalid)?;
            let mut state = object(read_json(&path)?)?;
            if let Some(v) = opts.get(k) {
                let mut current = object(state.remove("settings"))?;
                current.extend(object(Some(v.clone()))?);
                state.insert("settings".into(), current.into());
            }
            if let Some(v) = data.get(k) {
                state.insert("store".into(), object(Some(v.clone()))?.into());
            }
            if let Some(row) = records.get(k) {
                if installed.contains_key(k) {
                    let mut settings = object(state.remove("settings"))?;
                    settings.insert("__enabled".into(), (row["enabled"] != false).into());
                    state.insert("settings".into(), settings.into());
                } else {
                    state.insert("restoreMetadata".into(), row.clone());
                }
            }
            if let Some(jar) = jars.get(k) {
                let files = cookie_files(jar)?;
                let win = desktop_cookies(&files)?;
                if let Some(domains) = installed.get(k) {
                    let allowed: Vec<String> = serde_json::from_value(domains["domains"].clone())?;
                    cookies::validate_domains(&files, &allowed).map_err(|_| Error::Invalid)?;
                    plan.files.insert(format!("cookies/{k}.json"), win);
                    state.remove("restoreCookies");
                } else {
                    state.insert("restoreCookies".into(), serde_json::to_value(files)?);
                }
            }
            plan.files.insert(file, state.into());
        }
    }
    Ok(plan)
}
