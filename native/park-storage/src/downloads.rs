use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
mod queue;
pub use queue::{
    change_status, finish_chapter, plan, record_failure, video_pending, with_chapter, DownloadPlan, DownloadSlot,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid_download")]
    Invalid,
    #[error("unavailable_download")]
    Unavailable,
    #[error("database")]
    Database(#[from] rusqlite::Error),
}
type Result<T> = std::result::Result<T, Error>;

fn hash(id: &str) -> String {
    format!("{:x}", Sha256::digest(id.as_bytes()))
}

/// 调用方持有写事务。重复添加同一章不会清除完成记录或移动原文件。
pub fn enqueue_chapters(
    db: &rusqlite::Transaction<'_>,
    source: &str,
    item: &Value,
    chapters: &[Value],
    default_root: &Path,
    media: &str,
    selection: Option<&Value>,
    now: i64,
) -> Result<()> {
    if chapters.is_empty() || !matches!(media, "comic" | "video" | "novel") {
        return Err(Error::Invalid);
    }
    let work = item["id"].as_str().ok_or(Error::Invalid)?;
    let uid = format!("{source}:{work}");
    let existing: Option<(String, String)> = db.query_row(
        "SELECT dir,json FROM download_tasks WHERE uid=?", [&uid],
        |r| Ok((r.get(0)?, r.get(1)?))
    ).optional()?;
    let (dir, mut data) = match existing {
        Some((dir, raw)) => {
            let data: Value = serde_json::from_str(&raw).map_err(|_| Error::Invalid)?;
            if data["item"]["source"] != source || data["item"]["id"] != work
                || data["media"] != media || !data["deletingChapters"].is_null() {
                return Err(Error::Invalid);
            }
            (PathBuf::from(dir), data)
        }
        None => {
            if !default_root.is_absolute() || default_root.components().any(|c| c == Component::ParentDir) {
                return Err(Error::Invalid);
            }
            fs::create_dir_all(default_root).map_err(|_| Error::Unavailable)?;
            let mut record = item.clone();
            record["source"] = source.into();
            (default_root.join(hash(source)).join(hash(work)), serde_json::json!({
                "item": record, "media": media, "chapters": [], "completed": [], "status": 0,
                "done": 0, "total": 0, "bytes": 0
            }))
        }
    };
    let list = data["chapters"].as_array_mut().ok_or(Error::Invalid)?;
    let previous = list.len();
    let mut ids = list.iter().map(|v| v["id"].as_str().map(str::to_owned).ok_or(Error::Invalid))
        .collect::<Result<BTreeSet<_>>>()?;
    for chapter in chapters {
        let key = chapter["id"].as_str().ok_or(Error::Invalid)?;
        if ids.insert(key.into()) { list.push(chapter.clone()); }
    }
    let total = list.len();
    if total == previous && data["status"] == 3 { return Ok(()); }
    if let Some(choice) = selection {
        data["videoQuality"] = choice["quality"].clone();
        data["videoSelection"] = choice.clone();
    }
    let owned = owned_directory(db, source, work, &dir)?;
    if data["status"] != 1 { data["status"] = 0.into(); }
    data["total"] = total.into();
    data["error"] = Value::Null;
    db.execute(
        "INSERT INTO download_tasks(uid,source,json,dir,created_at) VALUES(?,?,?,?,?)
         ON CONFLICT(uid) DO UPDATE SET json=excluded.json",
        params![uid, source, data.to_string(), owned.to_str().ok_or(Error::Invalid)?, now]
    )?;
    Ok(())
}

/// 基线在每条任务记录内保存所选卷。只在能够确认卷、站点和作品归属后操作文件。
pub fn owned_directory(db: &Connection, source: &str, work: &str, dir: &Path) -> Result<PathBuf> {
    let base = dir.parent().and_then(Path::parent).ok_or(Error::Invalid)?;
    let source_dir = hash(source);
    let work_dir = hash(work);
    if !dir.is_absolute()
        || dir.components().any(|c| c == Component::ParentDir)
        || base.join(&source_dir).join(&work_dir) != dir
    {
        return Err(Error::Invalid);
    }
    let base = base.canonicalize().map_err(|_| Error::Unavailable)?;
    if !base.is_dir() {
        return Err(Error::Unavailable);
    }
    let real = base.join(source_dir).join(work_dir);
    let mut other = db.prepare("SELECT dir FROM download_tasks WHERE uid != ?1")?;
    for path in other.query_map([format!("{source}:{work}")], |r| r.get::<_, String>(0))? {
        let path = PathBuf::from(path?);
        if dir.starts_with(&path) || path.starts_with(dir) {
            return Err(Error::Invalid);
        }
        if let Ok(path) = path.canonicalize() {
            if real.starts_with(&path) || path.starts_with(&real) {
                return Err(Error::Invalid);
            }
        }
    }
    for path in [real.parent().ok_or(Error::Invalid)?, &real] {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() && path.canonicalize().ok().as_deref() == Some(path) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Ok(_) => return Err(Error::Invalid),
            Err(_) => return Err(Error::Unavailable),
        }
    }
    Ok(real)
}

fn task(db: &Connection, uid: &str, source: &str, work: &str) -> Result<(PathBuf, Value)> {
    let (dir, raw) = db
        .query_row(
            "SELECT dir,json FROM download_tasks WHERE uid=? AND source=?",
            params![uid, source],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or(Error::Unavailable)?;
    let value: Value = serde_json::from_str(&raw).map_err(|_| Error::Invalid)?;
    if value["item"]["source"].as_str() != Some(source)
        || value["item"]["id"].as_str() != Some(work)
    {
        return Err(Error::Invalid);
    }
    Ok((dir.into(), value))
}

fn remove_owned(path: &Path, directory: bool) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(Error::Unavailable),
    };
    if path.canonicalize().ok().as_deref() != Some(path) {
        return Err(Error::Invalid);
    }
    match (directory, meta.is_dir(), meta.is_file()) {
        (true, true, _) => fs::remove_dir_all(path),
        (false, _, true) => fs::remove_file(path),
        _ => return Err(Error::Invalid),
    }
    .map_err(|_| Error::Unavailable)
}

/// 分两次提交。首次提交后即使进程终止，仍可用同一任务重试原范围。
pub fn delete(db: &mut Connection, source: &str, work: &str, requested: &[String]) -> Result<bool> {
    let uid = format!("{source}:{work}");
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (_, mut data) = task(&tx, &uid, source, work)?;
    let ids: BTreeSet<&str> = data["chapters"]
        .as_array()
        .ok_or(Error::Invalid)?
        .iter()
        .map(|c| c["id"].as_str().ok_or(Error::Invalid))
        .collect::<Result<_>>()?;
    let mut scope: BTreeSet<String> = requested.iter().cloned().collect();
    let mut whole = scope.is_empty();
    if let Some(pending) = data.get("deletingChapters").filter(|v| !v.is_null()) {
        let pending = pending.as_array().ok_or(Error::Invalid)?;
        whole |= pending.is_empty();
        for id in pending {
            scope.insert(id.as_str().ok_or(Error::Invalid)?.to_owned());
        }
    }
    if scope.iter().any(|id| !ids.contains(id.as_str())) {
        return Err(Error::Invalid);
    }
    if whole || scope.len() == ids.len() {
        scope.clear();
    }
    data["deletingChapters"] = serde_json::json!(scope);
    tx.execute(
        "UPDATE download_tasks SET json=? WHERE uid=?",
        params![data.to_string(), uid],
    )?;
    tx.commit()?;

    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // 重新读取最新待删集合，避免并发请求或另一个进程覆盖后来的章节。
    let (dir, mut data) = match task(&tx, &uid, source, work) {
        Ok(row) => row,
        Err(Error::Unavailable) => return Ok(true),
        Err(e) => return Err(e),
    };
    let Some(pending) = data.get("deletingChapters") else {
        return Ok(false);
    };
    let scope: BTreeSet<String> =
        serde_json::from_value(pending.clone()).map_err(|_| Error::Invalid)?;
    let root = owned_directory(&tx, source, work, &dir)?;
    if scope.is_empty() {
        remove_owned(&root, true)?;
        tx.execute("DELETE FROM download_tasks WHERE uid=?", [&uid])?;
    } else {
        let video = data["media"] == "video";
        for id in &scope {
            let folder = if video {
                format!("episode_{}", hash(id))
            } else {
                hash(id)
            };
            if video {
                if let Some(name) = data["videoFiles"].get(id).and_then(Value::as_str) {
                    let file = Path::new(name);
                    if !file.components().all(|c| matches!(c, Component::Normal(_))) {
                        return Err(Error::Invalid);
                    }
                    let other_uses = data["videoFiles"].as_object().is_some_and(|files| {
                        files
                            .iter()
                            .any(|(key, v)| !scope.contains(key) && v == name)
                    });
                    if !other_uses && file.parent() == Some(Path::new("")) {
                        remove_owned(&root.join(file), false)?;
                    } else if !other_uses && file.parent() != Some(Path::new(&folder)) {
                        return Err(Error::Invalid);
                    }
                }
            }
            remove_owned(&root.join(folder), true)?;
        }
        data["chapters"]
            .as_array_mut()
            .ok_or(Error::Invalid)?
            .retain(|c| !c["id"].as_str().is_some_and(|id| scope.contains(id)));
        if let Some(completed) = data["completed"].as_array_mut() {
            completed.retain(|c| !c.as_str().is_some_and(|id| scope.contains(id)));
        }
        for name in ["videoFiles", "bundleNodes"] {
            if let Some(map) = data[name].as_object_mut() {
                map.retain(|id, _| !scope.contains(id));
            }
        }
        data.as_object_mut()
            .ok_or(Error::Invalid)?
            .remove("deletingChapters");
        tx.execute(
            "UPDATE download_tasks SET json=? WHERE uid=?",
            params![data.to_string(), uid],
        )?;
    }
    tx.commit()?;
    Ok(scope.is_empty())
}
