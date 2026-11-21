use crate::cookies::Files;
mod withdrawal;
use crate::recovery::{atomic, sync_directory, valid_key, Error, Partition, RecoveryBrowser};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};
use withdrawal::Withdrawal;
mod stage;
pub use stage::Stage;

type Result<T> = std::result::Result<T, Error>;

fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(info) if info.is_file() => fs::read(path).map(Some).map_err(|_| Error::IoFailure),
        Ok(_) => Err(Error::InvalidJournal),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(_) => Err(Error::IoFailure),
    }
}

fn json_file(root: &Path, name: &str) -> Result<Option<Value>> {
    read(&root.join(name))?
        .map(|v| serde_json::from_slice(&v).map_err(|_| Error::InvalidJournal))
        .transpose()
}

pub fn registry(root: &Path) -> Result<Value> {
    let mut value = json_file(root, "sources.json")?.unwrap_or_else(|| json!({}));
    if !value.is_object() {
        return Err(Error::InvalidJournal);
    }
    for field in ["sources", "removedOfficial"] {
        if value.get(field).is_none() {
            value[field] = json!([]);
        }
        let rows = value[field].as_array().ok_or(Error::InvalidJournal)?;
        let mut keys = HashSet::new();
        for row in rows {
            let key = if field == "sources" {
                row["key"].as_str()
            } else {
                row.as_str()
            };
            let key = key
                .filter(|key| valid_key(key))
                .ok_or(Error::InvalidJournal)?;
            if !keys.insert(key) {
                return Err(Error::InvalidJournal);
            }
        }
    }
    Ok(value)
}

pub fn is_official(row: &Value) -> bool {
    row.get("origin").is_none() || matches!(row["origin"].as_str(), Some("official" | "builtin"))
}

fn directory(root: &Path) -> Result<PathBuf> {
    let dir = root.join("sources");
    match fs::symlink_metadata(&dir) {
        Ok(m) if m.is_dir() => (),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            fs::create_dir(&dir).map_err(|_| Error::IoFailure)?
        }
        Ok(_) => return Err(Error::InvalidJournal),
        Err(_) => return Err(Error::IoFailure),
    }
    Ok(dir)
}
fn put(root: &Path, name: &str, bytes: Option<&[u8]>) -> Result<()> {
    if let Some(data) = bytes {
        atomic(root, name, data)?;
    } else if let Err(e) = fs::remove_file(root.join(name)) {
        if e.kind() != ErrorKind::NotFound {
            return Err(Error::IoFailure);
        }
    }
    sync_directory(root)
}
fn put_json(root: &Path, name: &str, value: &Value) -> Result<()> {
    put(
        root,
        name,
        Some(&serde_json::to_vec(value).map_err(|_| Error::InvalidJournal)?),
    )
}
/// 安装包中的初始脚本先于登记写入，退出后可以用相同字节重试。
pub fn seed(
    root: &Path,
    index: &Value,
    mut asset: impl FnMut(&str) -> Option<Vec<u8>>,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut list = registry(root)?;
    let mut seen = HashSet::new();
    let dir = directory(root)?;
    let mut changed = false;
    for entry in index["sources"].as_array().ok_or(Error::InvalidJournal)? {
        let key = entry["key"]
            .as_str()
            .filter(|k| valid_key(k))
            .ok_or(Error::InvalidJournal)?;
        let name = format!("{key}.js");
        let version = entry["version"].as_str().ok_or(Error::InvalidJournal)?;
        if !seen.insert(key) || entry["file"] != name {
            return Err(Error::InvalidJournal);
        }
        if list["removedOfficial"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == key)
            || list["sources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["key"] == key)
        {
            continue;
        }
        let bytes = asset(&name).ok_or(Error::IoFailure)?;
        if bytes.is_empty() || bytes.len() > 4 * 1024 * 1024 {
            return Err(Error::InvalidJournal);
        }
        match read(&dir.join(&name))? {
            Some(old) if old != bytes => return Err(Error::InvalidJournal),
            None => put(&dir, &name, Some(&bytes))?,
            _ => (),
        }
        let state = json_file(root, &format!("source_state_{key}.json"))?;
        let meta = state.as_ref().and_then(|v| v.get("restoreMetadata"));
        if meta.is_some_and(|m| m["key"] != key) {
            return Err(Error::InvalidJournal);
        }
        let order = meta
            .and_then(|m| m["order"].as_i64())
            .unwrap_or(list["sources"].as_array().unwrap().len() as i64);
        let row = json!({"key":key,"origin":"builtin","url":"","version":version,"sha256":format!("{:x}",Sha256::digest(bytes)),
            "enabled":meta.is_none_or(|m|m["enabled"]!=false),"order":order});
        replace_record(&mut list, key, Some(&row), None)?;
        changed = true;
    }
    if changed {
        put_json(root, "sources.json", &list)?;
    }
    Ok(())
}
fn replace_record(
    registry: &mut Value,
    key: &str,
    record: Option<&Value>,
    removed: Option<bool>,
) -> Result<()> {
    let rows = registry["sources"]
        .as_array_mut()
        .ok_or(Error::InvalidJournal)?;
    let pos = rows.iter().position(|r| r["key"] == key);
    match (pos, record) {
        (Some(i), Some(r)) => rows[i] = r.clone(),
        (None, Some(r)) => rows.push(r.clone()),
        (Some(i), None) => {
            rows.remove(i);
        }
        (None, None) => (),
    }
    if let Some(removed) = removed {
        let list = registry["removedOfficial"]
            .as_array_mut()
            .ok_or(Error::InvalidJournal)?;
        list.retain(|k| k != key);
        if removed {
            list.push(json!(key));
        }
    }
    Ok(())
}

pub fn set_enabled(root: &Path, key: &str, enabled: bool) -> Result<()> {
    if !valid_key(key) {
        return Err(Error::InvalidJournal);
    }
    let mut list = registry(root)?;
    let row = list["sources"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["key"] == key)
        .ok_or(Error::InvalidJournal)?;
    if row["enabled"].as_bool().unwrap_or(true) != enabled {
        row["enabled"] = json!(enabled);
        put_json(root, "sources.json", &list)?;
    }
    Ok(())
}

/// 调用方持有登记表写锁。只更新顺序，保留并发安装或配置留下的全部字段。
pub fn order_sources(root: &Path, change: impl FnOnce(&mut Vec<Value>) -> Result<()>) -> Result<()> {
    let mut list = registry(root)?;
    let rows = list["sources"].as_array_mut().ok_or(Error::InvalidJournal)?;
    rows.sort_by_key(|row| row["order"].as_i64().unwrap_or(0));
    change(rows)?;
    for (index, row) in rows.iter_mut().enumerate() {
        row["order"] = index.into();
    }
    put_json(root, "sources.json", &list)
}

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InstallJournal {
    key: String,
    pub source: Option<Value>,
    code: Option<String>,
    pub state: Value,
    cookies: Files,
    domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_cookies: Option<Files>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    withdrawal: Option<Withdrawal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    was_removed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    had_state: Option<bool>,
}

impl InstallJournal {
    pub fn capture(root: &Path, key: &str) -> Result<Self> {
        if !valid_key(key) {
            return Err(Error::InvalidJournal);
        }
        for name in [
            "backup_restore.json".into(),
            "source_install.json".into(),
            format!("source_install_{key}.json"),
        ] {
            if root.join(name).try_exists().map_err(|_| Error::IoFailure)? {
                return Err(Error::PendingRecovery);
            }
        }
        let list = registry(root)?;
        let source = list["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["key"] == key)
            .cloned();
        let was_removed = list["removedOfficial"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == key);
        let code = match fs::symlink_metadata(root.join("sources")) {
            Ok(info) if info.is_dir() => read(&root.join("sources").join(format!("{key}.js")))?,
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            _ => return Err(Error::IoFailure),
        }
        .map(String::from_utf8)
        .transpose()
        .map_err(|_| Error::InvalidJournal)?;
        let state = json_file(root, &format!("source_state_{key}.json"))?;
        if state.as_ref().is_some_and(|v| !v.is_object()) {
            return Err(Error::InvalidJournal);
        }
        Ok(Self {
            key: key.into(),
            source,
            code,
            cookies: crate::backup::cookies_on_disk(root, key)?,
            domains: Vec::new(),
            next_cookies: None,
            withdrawal: None,
            was_removed: Some(was_removed),
            had_state: Some(state.is_some()),
            state: state.unwrap_or_else(|| json!({"data":{},"settings":{}})),
        })
    }

    /// 初始化在独立的临时目录中进行。探测或初始化失败时没有业务文件需要撤销。
    pub fn stage(&self, temp: &Path) -> Result<()> {
        put_json(
            temp,
            &format!("source_state_{}.json", self.key),
            &self.state,
        )?;
        Partition {
            before: Files::new(),
            after: self.cookies.clone(),
            domains: vec![],
        }
        .restore(temp, &self.key, None, true)
    }

    /// 调用方持有独占锁。重新核对该源的脚本、登记和私有数据，允许其他源独立变化。
    pub fn commit(
        mut self,
        root: &Path,
        staged: Option<&Path>,
        record: Option<&Value>,
        code: Option<&[u8]>,
        domains: Vec<String>,
        browser: Option<&dyn RecoveryBrowser>,
    ) -> Result<()> {
        if self != Self::capture(root, &self.key)? {
            return Err(Error::PendingRecovery);
        }
        if record.is_some_and(|r| r["key"] != self.key.as_str())
            || staged.is_some() != record.is_some()
            || code.is_some() != record.is_some()
        {
            return Err(Error::InvalidJournal);
        }
        let name = format!("source_install_{}.json", self.key);
        let state_name = format!("source_state_{}.json", self.key);
        let (next, state) = match staged {
            Some(dir) => (
                crate::backup::cookies_on_disk(dir, &self.key)?,
                json_file(dir, &state_name)?,
            ),
            None => (Files::new(), None),
        };
        let changed = self.cookies != next;
        self.domains = domains;
        self.next_cookies = Some(next.clone());
        if record.is_none() {
            self.withdrawal = Some(Withdrawal::capture(root, &self.key)?);
        }
        let mut list = registry(root)?;
        replace_record(&mut list, &self.key, record, Some(record.is_none()))?;
        let dir = directory(root)?;
        put(
            root,
            &name,
            Some(&serde_json::to_vec(&self).map_err(|_| Error::InvalidJournal)?),
        )?;
        let written = (|| {
            put(&dir, &format!("{}.js", self.key), code)?;
            match state {
                Some(v) if v.is_object() => put_json(root, &state_name, &v)?,
                Some(_) => return Err(Error::InvalidJournal),
                None => put(root, &state_name, None)?,
            }
            if changed {
                Partition {
                    before: self.cookies.clone(),
                    after: next,
                    domains: self.domains.clone(),
                }
                .restore(root, &self.key, browser, true)?;
            }
            if let Some(w) = &self.withdrawal {
                w.apply(root, &self.key)?;
            }
            put_json(root, "sources.json", &list)
        })();
        if let Err(e) = written {
            self.restore(root, browser)?;
            put(root, &name, None)?;
            return Err(e);
        }
        if let Some(w) = &self.withdrawal {
            let done = format!("source_withdraw_{}.done", self.key);
            fs::rename(root.join(&name), root.join(&done)).map_err(|_| Error::IoFailure)?;
            sync_directory(root)?;
            w.finish(root, &self.key)?;
            put(root, &done, None)
        } else {
            put(root, &name, None)
        }
    }

    fn restore(&self, root: &Path, browser: Option<&dyn RecoveryBrowser>) -> Result<()> {
        let key = &self.key;
        if !valid_key(key)
            || self
                .source
                .as_ref()
                .is_some_and(|s| s["key"] != key.as_str())
            || !self.state.is_object()
        {
            return Err(Error::InvalidJournal);
        }
        let mut source_list = registry(root)?;
        replace_record(
            &mut source_list,
            key,
            self.source.as_ref(),
            self.was_removed,
        )?;
        let source_dir = directory(root)?;
        if let Some(w) = &self.withdrawal {
            w.rollback(root, key)?;
        }
        if self.next_cookies.as_ref() != Some(&self.cookies) {
            let after = match &self.next_cookies {
                Some(next) => next.clone(),
                None => crate::backup::cookies_on_disk(root, key)?,
            };
            let partition = Partition {
                before: self.cookies.clone(),
                after,
                domains: self.domains.clone(),
            };
            partition.validate()?;
            partition.restore(root, key, browser, false)?;
        }
        put(
            &source_dir,
            &format!("{key}.js"),
            self.code.as_ref().map(|s| s.as_bytes()),
        )?;
        let state_name = format!("source_state_{key}.json");
        if self.had_state == Some(false) {
            put(root, &state_name, None)?;
        } else {
            put_json(root, &state_name, &self.state)?;
        }
        put_json(root, "sources.json", &source_list)
    }
}

/// 调用方持有实例恢复锁。兼容基线写出的单源日志，保留未完成的恢复以供下次重试。
fn journals(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for item in fs::read_dir(root).map_err(|_| Error::IoFailure)? {
        let item = item.map_err(|_| Error::IoFailure)?;
        let Some(name) = item.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == "source_install.json"
            || (name.starts_with("source_install_") && name.ends_with(".json"))
            || (name.starts_with("source_withdraw_") && name.ends_with(".done"))
        {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}
pub fn pending(root: &Path) -> Result<bool> {
    Ok(!journals(root)?.is_empty())
}
pub fn recover(root: &Path, browser: Option<&dyn RecoveryBrowser>) -> Result<()> {
    stage::clean_abandoned(root)?;
    for name in journals(root)? {
        let raw = read(&root.join(&name))?.ok_or(Error::InvalidJournal)?;
        let prior: InstallJournal =
            serde_json::from_slice(&raw).map_err(|_| Error::InvalidJournal)?;
        if name == format!("source_withdraw_{}.done", prior.key) {
            prior
                .withdrawal
                .as_ref()
                .ok_or(Error::InvalidJournal)?
                .finish(root, &prior.key)?;
            put(root, &name, None)?;
            continue;
        }
        if name != "source_install.json" && name != format!("source_install_{}.json", prior.key) {
            return Err(Error::InvalidJournal);
        }
        prior.restore(root, browser)?;
        put(root, &name, None)?;
    }
    Ok(())
}
