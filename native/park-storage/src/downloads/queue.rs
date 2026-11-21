use super::*;
use fs2::FileExt;
use std::fs::OpenOptions;

use tempfile::NamedTempFile;

/// 每个下载持有一个系统文件锁，后台入口和前台使用同一额度，进程终止自动释放。
pub struct DownloadSlot {
    _lease: NamedTempFile,
}
impl DownloadSlot {
    pub fn acquire(
        root: &Path,
        source: &str,
        work: &str,
        site_limit: usize,
        work_limit: usize,
    ) -> Result<Option<Self>> {
        let dir = root.join("download_workers");
        fs::create_dir_all(&dir).map_err(|_| Error::Unavailable)?;
        if fs::symlink_metadata(&dir)
            .map_err(|_| Error::Unavailable)?
            .file_type()
            .is_symlink()
        {
            return Err(Error::Invalid);
        }
        let gate = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("queue.lock"))
            .map_err(|_| Error::Unavailable)?;
        FileExt::lock_exclusive(&gate).map_err(|_| Error::Unavailable)?;
        let site = hash(source);
        let owner = format!("{site}-{}", hash(work));
        let mut sites = BTreeSet::new();
        let mut works = 0;
        for entry in fs::read_dir(&dir).map_err(|_| Error::Unavailable)? {
            let entry = entry.map_err(|_| Error::Unavailable)?;
            if !entry.file_name().to_string_lossy().starts_with("active-") {
                continue;
            }
            if !entry.file_type().map_err(|_| Error::Unavailable)?.is_file() {
                return Err(Error::Invalid);
            }
            let path = entry.path();
            let file = match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(Error::Unavailable),
            };
            if crate::recovery::try_lock_file(&file, true).map_err(|_| Error::Unavailable)? {
                drop(file);
                let _ = fs::remove_file(path);
                continue;
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or(Error::Invalid)?;
            let record = name
                .strip_prefix("active-")
                .and_then(|v| v.rsplit_once('-'))
                .map(|(record, _)| record)
                .ok_or(Error::Invalid)?;
            if record == owner {
                return Ok(None);
            }
            let (s, _) = record.split_once('-').ok_or(Error::Invalid)?;
            sites.insert(s.to_owned());
            if s == site {
                works += 1;
            }
        }
        if works >= work_limit.clamp(1, 8) || works == 0 && sites.len() >= site_limit.clamp(1, 8) {
            return Ok(None);
        }
        let lease = tempfile::Builder::new()
            .prefix(&format!("active-{owner}-"))
            .tempfile_in(&dir)
            .map_err(|_| Error::Unavailable)?;
        FileExt::lock_exclusive(lease.as_file()).map_err(|_| Error::Unavailable)?;
        Ok(Some(Self { _lease: lease }))
    }
}

pub struct DownloadPlan {
    pub directory: PathBuf,
    pub media: String,
    pub chapters: Vec<(String, String)>,
    pub item: Value,
    pub selection: Value,
    pub quality: u32,
    pub bundle_nodes: Value,
}
fn queued(data: &Value) -> bool {
    matches!(data["status"].as_i64(), Some(0 | 1))
        && data.get("deletingChapters").is_none_or(Value::is_null)
}
/// 只监视正在传输的视频。暂停、移除或更换逻辑线路时放弃旧请求。
pub fn video_pending(db: &Connection, source: &str, work: &str, selection: &Value) -> Result<bool> {
    let (_, row) = task(db, &format!("{source}:{work}"), source, work)?;
    Ok(queued(&row) && row["media"] == "video" && row["videoSelection"] == *selection)
}

pub fn plan(db: &Connection, source: &str, work: &str) -> Result<DownloadPlan> {
    let uid = format!("{source}:{work}");
    let (dir, task) = task(db, &uid, source, work)?;
    if !queued(&task) {
        return Err(Error::Unavailable);
    }
    let directory = owned_directory(db, source, work, &dir)?;
    let completed = task["completed"].as_array();
    let chapters = task["chapters"]
        .as_array()
        .ok_or(Error::Invalid)?
        .iter()
        .filter(|chapter| !completed.is_some_and(|list| list.contains(&chapter["id"])))
        .map(|c| {
            Ok((
                c["id"].as_str().ok_or(Error::Invalid)?.to_owned(),
                c["title"].as_str().ok_or(Error::Invalid)?.to_owned(),
            ))
        })
        .collect::<Result<_>>()?;
    Ok(DownloadPlan {
        directory,
        media: task["media"].as_str().ok_or(Error::Invalid)?.into(),
        chapters,
        item: task["item"].clone(),
        selection: task["videoSelection"].clone(),
        quality: task["videoQuality"]
            .as_u64()
            .and_then(|v| v.try_into().ok())
            .unwrap_or(0),
        bundle_nodes: task["bundleNodes"].clone(),
    })
}

pub fn with_chapter<T>(
    db: &mut Connection,
    source: &str,
    work: &str,
    chapter: &str,
    expected: &Path,
    selection: Option<&Value>,
    operation: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let plan = plan(&tx, source, work)?;
    if plan.directory != expected || selection.is_some_and(|s| *s != plan.selection)
        || !plan.chapters.iter().any(|(id, _)| id == chapter) {
        return Err(Error::Unavailable);
    }
    let dir = if plan.media == "video" {
        let single = plan.item["singleChapterId"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or("_");
        if !plan.item["chapters"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
            && chapter == single
        {
            plan.directory
        } else {
            plan.directory.join(format!("episode_{}", hash(chapter)))
        }
    } else {
        plan.directory.join(hash(chapter))
    };
    if !dir.try_exists().map_err(|_| Error::Unavailable)? {
        fs::create_dir_all(&dir).map_err(|_| Error::Unavailable)?;
    }
    if !dir.is_dir() || dir.canonicalize().map_err(|_| Error::Unavailable)? != dir {
        return Err(Error::Invalid);
    }
    let result = operation(&dir)?;
    tx.commit()?;
    Ok(result)
}

pub fn change_status(db: &mut Connection, source: &str, work: &str, resume: bool) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let uid = format!("{source}:{work}");
    let (_, mut t) = task(&tx, &uid, source, work)?;
    if !matches!(t["status"].as_i64(), Some(0 | 1 | 2 | 4)) || !t["deletingChapters"].is_null() {
        return Err(Error::Invalid);
    }
    t["status"] = Value::from(if resume { 0 } else { 2 });
    t["error"] = Value::Null;
    tx.execute(
        "UPDATE download_tasks SET json=? WHERE uid=?",
        params![t.to_string(), uid],
    )?;
    Ok(tx.commit()?)
}
pub fn record_failure(db: &mut Connection, source: &str, work: &str, selection: Option<&Value>) -> Result<bool> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let uid = format!("{source}:{work}");
    let (_, mut t) = task(&tx, &uid, source, work)?;
    let current = queued(&t) && selection.is_none_or(|choice| t["videoSelection"] == *choice);
    if current {
        t["status"] = Value::from(4);
        t["error"] = Value::Null;
        tx.execute(
            "UPDATE download_tasks SET json=? WHERE uid=?",
            params![t.to_string(), uid],
        )?;
    }
    tx.commit()?;
    Ok(current)
}
/// 文件已完整同步后才添加到 completed。重启可重用已写文件，部分章节不会标成完成。
pub fn finish_chapter(
    db: &mut Connection,
    source: &str,
    work: &str,
    chapter: &str,
    bytes: u64,
    video_file: Option<&str>,
    selection: Option<&Value>,
) -> Result<bool> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let uid = format!("{source}:{work}");
    let (_, mut t) = task(&tx, &uid, source, work)?;
    if !queued(&t) || selection.is_some_and(|choice| t["videoSelection"] != *choice)
        || !t["chapters"]
            .as_array()
            .is_some_and(|cs| cs.iter().any(|c| c["id"] == chapter))
    {
        return Err(Error::Unavailable);
    }
    if t["completed"].is_null() {
        t["completed"] = serde_json::json!([]);
    }
    if let Some(path) = video_file {
        if t["media"] != "video"
            || !Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(Error::Invalid);
        }
        t["videoFiles"][chapter] = Value::from(path);
    }
    let completed = t["completed"].as_array_mut().ok_or(Error::Invalid)?;
    if !completed.iter().any(|c| c == chapter) {
        completed.push(serde_json::json!(chapter));
        t["bytes"] = Value::from(t["bytes"].as_u64().unwrap_or(0).saturating_add(bytes));
    }
    let count = t["completed"].as_array().ok_or(Error::Invalid)?.len();
    let all = t["chapters"].as_array().ok_or(Error::Invalid)?;
    let total = all.len();
    let finished = all
        .iter()
        .all(|c| t["completed"].as_array().unwrap().contains(&c["id"]));
    if finished {
        t["status"] = Value::from(3);
    }
    t["total"] = Value::from(total);
    t["done"] = Value::from(count);
    t["error"] = Value::Null;
    tx.execute(
        "UPDATE download_tasks SET json=? WHERE uid=?",
        params![t.to_string(), uid],
    )?;
    tx.commit()?;
    Ok(finished)
}
