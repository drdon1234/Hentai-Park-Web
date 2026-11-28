use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
mod queue;
pub use queue::{
    change_queue_batch, change_status, finish_chapter, plan, record_failure, video_pending, with_chapter, ActiveDownloads, DownloadPlan, DownloadSlot,
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
    delete_selected(db, source, work, Some(requested), None).map(|(whole, _)| whole)
}

/// 详情页确认只对当时显示的任务有效。
pub fn delete_confirmed(db: &mut Connection, source: &str, work: &str, created_at: i64) -> Result<bool> {
    delete_selected(db, source, work, Some(&[]), Some(created_at)).map(|(whole, _)| whole)
}

pub fn delete_read(db: &mut Connection) -> Result<u64> {
    let mut query = db.prepare("SELECT DISTINCT c.source,c.work_id FROM chapters c
        JOIN download_tasks d ON d.source=c.source AND d.uid=c.source||':'||c.work_id
        WHERE c.read AND c.source!='local' ORDER BY c.source,c.work_id")?;
    let works = query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>,_>>()?;
    drop(query);
    let mut count = 0;
    for (source, work) in works {
        count += delete_selected(db, &source, &work, None, None)?.1;
    }
    Ok(count)
}

fn delete_selected(db: &mut Connection, source: &str, work: &str, requested: Option<&[String]>, created_at: Option<i64>) -> Result<(bool,u64)> {
    let uid = format!("{source}:{work}");
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(expected) = created_at {
        let current: Option<i64> = tx.query_row("SELECT created_at FROM download_tasks WHERE uid=? AND source=?",
            params![uid,source], |row| row.get(0)).optional()?;
        if current != Some(expected) { return Err(Error::Unavailable); }
    }
    let (_, mut data) = match task(&tx, &uid, source, work) {
        Err(Error::Unavailable) if requested.is_none() => return Ok((false,0)),
        result => result?,
    };
    let ids: BTreeSet<&str> = data["chapters"]
        .as_array()
        .ok_or(Error::Invalid)?
        .iter()
        .map(|c| c["id"].as_str().ok_or(Error::Invalid))
        .collect::<Result<_>>()?;
    let mut scope: BTreeSet<String> = if let Some(ids) = requested {
        ids.iter().cloned().collect()
    } else {
        if source == "local" || data["media"] != "comic" { return Ok((false,0)); }
        let completed: BTreeSet<&str> = data["completed"].as_array().into_iter().flatten()
            .filter_map(Value::as_str).collect();
        let mut query = tx.prepare("SELECT chapter_id FROM chapters WHERE source=? AND work_id=? AND read")?;
        let read = query.query_map(params![source,work], |row| row.get::<_,String>(0))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        read.into_iter().filter(|id| completed.contains(id.as_str()) && ids.contains(id.as_str())).collect()
    };
    if requested.is_none() && scope.is_empty() { return Ok((false,0)); }
    let count = scope.len() as u64;
    let mut whole = scope.is_empty();
    if let Some(pending) = data.get("deletingChapters").filter(|v| !v.is_null()) {
        let pending = pending.as_array().ok_or(Error::Invalid)?;
        if requested.is_none() && ((pending.is_empty() && scope.len()!=ids.len()) ||
            pending.iter().any(|id| !id.as_str().is_some_and(|s|scope.contains(s)))) {
            return Err(Error::Unavailable);
        }
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
        Err(Error::Unavailable) => return Ok((true,count)),
        Err(e) => return Err(e),
    };
    let Some(pending) = data.get("deletingChapters") else {
        return Ok((false,count));
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
    Ok((scope.is_empty(),count))
}
