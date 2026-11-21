use super::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, PartialEq)]
pub(super) struct Withdrawal {
    preferences: BTreeMap<String, Value>,
    manifest: Option<Value>,
    tags: bool,
}

fn matches_key(key: &str, value: &str) -> bool {
    value == key
        || value
            .strip_prefix(key)
            .is_some_and(|rest| rest.starts_with(':'))
}
fn without_references(key: &str, name: &str, mut value: Value) -> Value {
    let filter = |v: &mut Value| {
        if let Some(map) = v.as_object_mut() {
            map.retain(|k, v| {
                !matches_key(key, k) && !v.as_str().is_some_and(|s| matches_key(key, s))
            });
        }
    };
    match name {
        "homeSources" | "ignoredSourceUpdates" => filter(&mut value),
        "searchHistories" => {
            if let Some(map) = value.as_object_mut() {
                map.remove(&json!(["source", key]).to_string());
            }
        }
        "rankingSelection" => {
            if let Some(map) = value.as_object_mut() {
                if map
                    .get("selected")
                    .and_then(Value::as_str)
                    .is_some_and(|v| matches_key(key, v))
                {
                    map.remove("selected");
                }
                for field in ["options", "navigation", "detailSources", "searchSources"] {
                    if let Some(v) = map.get_mut(field) {
                        filter(v);
                    }
                }
            }
        }
        _ => {
            if let Some(list) = value.as_array_mut() {
                list.retain(|v| !v.as_str().is_some_and(|s| matches_key(key, s)));
            }
        }
    }
    value
}
fn checked_dir(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => Ok(true),
        Ok(_) => Err(Error::InvalidJournal),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(_) => Err(Error::IoFailure),
    }
}
fn paths(root: &Path, key: &str) -> Result<(PathBuf, PathBuf, PathBuf)> {
    if !valid_key(key) {
        return Err(Error::InvalidJournal);
    }
    let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    let dir = root.join("tag_translations");
    checked_dir(&dir)?;
    Ok((
        dir.clone(),
        dir.join(&hex),
        dir.join(format!(".withdraw-{hex}")),
    ))
}
impl Withdrawal {
    pub fn capture(root: &Path, key: &str) -> Result<Self> {
        let settings = json_file(root, "settings.json")?.unwrap_or_else(|| json!({}));
        let settings = settings.as_object().ok_or(Error::InvalidJournal)?;
        let preferences = [
            "homeSources",
            "ignoredSourceUpdates",
            "searchHistories",
            "homeHiddenSources",
            "libraryFilterSources",
            "aggregateExcluded",
            "tagDownloadIgnoredSources",
            "replyWithoutAsking",
            "rankingSelection",
        ]
        .into_iter()
        .filter_map(|k| {
            settings
                .get(k)
                .filter(|v| without_references(key, k, (*v).clone()) != **v)
                .map(|v| (k.to_string(), v.clone()))
        })
        .collect();
        let cache = json_file(root, "source_manifests.json")?.unwrap_or_else(|| json!({}));
        let manifest = cache
            .as_object()
            .ok_or(Error::InvalidJournal)?
            .get(key)
            .cloned();
        let (_, source, detached) = paths(root, key)?;
        if checked_dir(&detached)? {
            return Err(Error::PendingRecovery);
        }
        Ok(Self {
            preferences,
            manifest,
            tags: checked_dir(&source)?,
        })
    }
    fn metadata(&self, root: &Path, key: &str, restore: bool) -> Result<()> {
        if !self.preferences.is_empty() {
            let mut settings = json_file(root, "settings.json")?.ok_or(Error::InvalidJournal)?;
            let map = settings.as_object_mut().ok_or(Error::InvalidJournal)?;
            for (k, v) in &self.preferences {
                map.insert(
                    k.clone(),
                    if restore {
                        v.clone()
                    } else {
                        without_references(key, k, v.clone())
                    },
                );
            }
            put_json(root, "settings.json", &settings)?;
        }
        if let Some(old) = &self.manifest {
            let mut cache = json_file(root, "source_manifests.json")?.unwrap_or_else(|| json!({}));
            let map = cache.as_object_mut().ok_or(Error::InvalidJournal)?;
            if restore {
                map.insert(key.to_string(), old.clone());
            } else {
                map.remove(key);
            }
            put_json(root, "source_manifests.json", &cache)?;
        }
        Ok(())
    }
    pub fn apply(&self, root: &Path, key: &str) -> Result<()> {
        if self.tags {
            let (dir, source, detached) = paths(root, key)?;
            fs::rename(source, detached).map_err(|_| Error::IoFailure)?;
            sync_directory(&dir)?;
        }
        self.metadata(root, key, false)
    }
    pub fn rollback(&self, root: &Path, key: &str) -> Result<()> {
        if self.tags {
            let (dir, source, detached) = paths(root, key)?;
            match (checked_dir(&source)?, checked_dir(&detached)?) {
                (false, true) => {
                    fs::rename(detached, source).map_err(|_| Error::IoFailure)?;
                    sync_directory(&dir)?;
                }
                (true, false) => (),
                _ => return Err(Error::InvalidJournal),
            }
        }
        self.metadata(root, key, true)
    }
    pub fn finish(&self, root: &Path, key: &str) -> Result<()> {
        if self.tags {
            finish_tags(root, key)?;
        }
        Ok(())
    }
}
pub(super) fn finish_tags(root: &Path, key: &str) -> Result<()> {
    let (dir, _, detached) = paths(root, key)?;
    if checked_dir(&detached)? {
        // 已确认撤回且登记提交后，才清理从该源原目录移出的词包。
        fs::remove_dir_all(&detached).map_err(|_| Error::IoFailure)?;
        sync_directory(&dir)?;
    }
    Ok(())
}
