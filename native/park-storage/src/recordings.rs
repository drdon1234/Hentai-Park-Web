use crate::backup::Error;
use serde_json::{json, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, Error>;
fn regular(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(Error::Invalid);
    }
    Ok(())
}
fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
fn path_for(row: &Value) -> Result<PathBuf> {
    let id = row["id"].as_str().ok_or(Error::Invalid)?;
    if !valid_id(id) {
        return Err(Error::Invalid);
    }
    let dir = Path::new(row["directory"].as_str().ok_or(Error::Invalid)?);
    let parent = dir.parent().ok_or(Error::Invalid)?;
    if !dir.is_absolute()
        || dir.file_name().and_then(|v| v.to_str()) != Some(id)
        || parent.file_name().and_then(|v| v.to_str()) != Some("recordings")
        || !fs::symlink_metadata(dir)?.is_dir()
        || !fs::symlink_metadata(parent)?.is_dir()
    {
        return Err(Error::Invalid);
    }
    let path = dir.canonicalize()?;
    if !path.starts_with(parent.canonicalize()?) {
        return Err(Error::Invalid);
    }
    verify_marker(&path, id)?;
    Ok(path)
}
fn verify_marker(dir: &Path, id: &str) -> Result<()> {
    let marker = dir.join("recording.json");
    regular(&marker)?;
    let mark: Value = serde_json::from_reader(fs::File::open(marker)?)?;
    if mark["id"] != id {
        return Err(Error::Invalid);
    }
    Ok(())
}
fn gate(root: &Path) -> Result<fs::File> {
    let file = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("recordings.lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}
fn rows(root: &Path) -> Result<Vec<Value>> {
    let path = root.join("recordings.json");
    match regular(&path) {
        Ok(()) => Ok(serde_json::from_reader(fs::File::open(path)?)?),
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}
fn write(root: &Path, rows: &[Value]) -> Result<()> {
    crate::recovery::atomic(root, "recordings.json", &serde_json::to_vec(rows)?)
        .map_err(|_| Error::Invalid)
}
fn lock_record(dir: &Path) -> Result<Option<fs::File>> {
    let lock = dir.join("recording.lock");
    if lock.try_exists()? {
        regular(&lock)?
    }
    let f = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock)?;
    match fs2::FileExt::try_lock_exclusive(&f) {
        Ok(()) => Ok(Some(f)),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub struct Recording {
    pub directory: PathBuf,
    pub id: String,
    root: PathBuf,
    _lock: fs::File,
}
impl Recording {
    pub fn create(
        root: &Path,
        download: &Path,
        id: &str,
        source: &str,
        work: &str,
        title: &str,
        started: &str,
    ) -> Result<Self> {
        if !valid_id(id)
            || !download.is_absolute()
            || !crate::recovery::valid_key(source)
            || work.is_empty()
        {
            return Err(Error::Invalid);
        }
        let _index = gate(root)?;
        let mut rows = rows(root)?;
        if rows.iter().any(|r| r["id"] == id) {
            return Err(Error::Invalid);
        }
        for r in &rows {
            if r["item"]["source"] == source
                && r["item"]["id"] == work
                && matches!(r["status"].as_str(), Some("recording" | "stopping"))
            {
                let dir = path_for(r)?;
                if lock_record(&dir)?.is_none() {
                    return Err(Error::Invalid);
                }
            }
        }
        let folder = download.join("recordings");
        fs::create_dir_all(&folder)?;
        if !fs::symlink_metadata(&folder)?.is_dir() {
            return Err(Error::Invalid);
        }
        let directory = folder.join(id);
        fs::create_dir(&directory)?;
        let result = (|| {
            let lock = lock_record(&directory)?.ok_or(Error::Invalid)?;
            crate::recovery::atomic(
                &directory,
                "recording.json",
                &serde_json::to_vec(&json!({"id":id}))?,
            )
            .map_err(|_| Error::Invalid)?;
            rows.insert(0,json!({"id":id,"directory":directory.to_str().ok_or(Error::Invalid)?,"startedAt":started,
              "item":{"source":source,"id":work,"title":title,"mediaKind":"video","contentDomain":"live"},"status":"recording","seconds":0,"bytes":0}));
            write(root, &rows)?;
            Ok(Self {
                root: root.into(),
                id: id.into(),
                directory: directory.clone(),
                _lock: lock,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(directory.join("recording.lock"));
            let _ = fs::remove_file(directory.join("recording.json"));
            let _ = fs::remove_dir(directory);
        }
        result
    }
    pub fn update(&self, seconds: f64, bytes: u64, status: &str) -> Result<()> {
        let _index = gate(&self.root)?;
        let mut rows = rows(&self.root)?;
        let row = rows
            .iter_mut()
            .find(|r| r["id"] == self.id)
            .ok_or(Error::Invalid)?;
        row["seconds"] = seconds.into();
        row["bytes"] = bytes.into();
        row["status"] = status.into();
        write(&self.root, &rows)
    }
}
pub fn directory(root: &Path, id: &str) -> Result<PathBuf> {
    let row = rows(root)?
        .into_iter()
        .find(|r| r["id"] == id)
        .ok_or(Error::Invalid)?;
    path_for(&row)
}
/// 删除中断后优先识别同一卷内已改名的目录。登记仅在文件处理完成后移除。
pub fn delete(root: &Path, id: &str) -> Result<()> {
    if !valid_id(id) {
        return Err(Error::Invalid);
    }
    let _index = gate(root)?;
    let mut rows = rows(root)?;
    let Some(index) = rows.iter().position(|r| r["id"] == id) else {
        return Ok(());
    };
    let row = &rows[index];
    let path = Path::new(row["directory"].as_str().ok_or(Error::Invalid)?);
    let parent = path.parent().ok_or(Error::Invalid)?;
    if !path.is_absolute()
        || path.file_name().and_then(|v| v.to_str()) != Some(id)
        || parent.file_name().and_then(|v| v.to_str()) != Some("recordings")
        || !fs::symlink_metadata(parent)?.is_dir()
    {
        return Err(Error::Invalid);
    }
    let parent = parent.canonicalize()?;
    let tomb = parent.join(format!(".deleting-{id}"));
    if path.try_exists()? {
        let checked = path_for(row)?;
        if tomb.try_exists()? || checked.parent() != Some(parent.as_path()) {
            return Err(Error::Invalid);
        }
        let lock = lock_record(&checked)?.ok_or(Error::Invalid)?;
        drop(lock);
        fs::rename(&checked, &tomb)?;
    }
    if tomb.try_exists()? {
        if !fs::symlink_metadata(&tomb)?.is_dir() || tomb.canonicalize()? != tomb {
            return Err(Error::Invalid);
        }
        // 只在清空内容后才移除所有权标记。断电留下的空目录可直接重试移除。
        if fs::read_dir(&tomb)?.next().transpose()?.is_some() {
            verify_marker(&tomb, id)?;
            for entry in fs::read_dir(&tomb)? {
                let entry = entry?;
                if entry.file_name() == "recording.json" {
                    continue;
                }
                if entry.file_type()?.is_dir() {
                    fs::remove_dir_all(entry.path())?;
                } else {
                    fs::remove_file(entry.path())?;
                }
            }
            fs::remove_file(tomb.join("recording.json"))?;
        }
        fs::remove_dir(tomb)?;
    }
    rows.remove(index);
    write(root, &rows)
}
pub fn volumes(dir: &Path) -> Result<Vec<u32>> {
    let mut ids = Vec::new();
    for f in fs::read_dir(dir)? {
        let f = f?;
        if !f.file_type()?.is_file() {
            continue;
        }
        if let Some(n) = f
            .file_name()
            .to_str()
            .and_then(|s| s.strip_suffix(".m3u8"))
            .and_then(|s| s.parse::<u32>().ok())
        {
            ids.push(n)
        }
    }
    ids.sort_unstable();
    Ok(ids)
}
pub fn view(
    root: &Path,
    mut stats: impl FnMut(&Path, &[u32]) -> Result<(f64, u64)>,
) -> Result<Vec<Value>> {
    let _index = gate(root)?;
    let mut all = rows(root)?;
    let mut changed = false;
    let mut list = Vec::new();
    for r in &mut all {
        let Some(id) = r["id"].as_str().map(str::to_owned) else {
            continue;
        };
        let mut row = json!({"id":id,"source":r["item"]["source"],"work":r["item"]["id"],"title":r["item"]["title"],
            "status":r["status"],"startedAt":r["startedAt"],"seconds":r["seconds"],"bytes":r["bytes"],"volumes":[]});
        if let Ok(dir) = path_for(r) {
            let ids = volumes(&dir)?;
            if matches!(r["status"].as_str(), Some("recording" | "stopping"))
                && lock_record(&dir)?.is_some()
            {
                if let Ok((seconds, bytes)) = stats(&dir, &ids) {
                    r["seconds"] = seconds.into();
                    r["bytes"] = bytes.into();
                    row["seconds"] = seconds.into();
                    row["bytes"] = bytes.into()
                }
                r["status"] = "interrupted".into();
                row["status"] = "interrupted".into();
                changed = true;
            }
            row["volumes"] = json!(ids);
        } else {
            row["unavailable"] = true.into()
        }
        list.push(row);
    }
    if changed {
        write(root, &all)?
    }
    Ok(list)
}
