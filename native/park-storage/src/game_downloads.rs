use crate::backup::Error;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs, io,
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};

type Result<T> = std::result::Result<T, Error>;
const INDEX: &str = "game_downloads.json";
// 仅本次进程明确排队的任务可自动启动。旧版或上一进程的未完成任务等待用户继续。
static SESSION: LazyLock<String> =
    LazyLock::new(|| format!("{}-{:?}", std::process::id(), std::time::SystemTime::now()));

fn identity(row: &Value) -> Option<[&str; 4]> {
    Some([
        row["item"]["source"].as_str()?,
        row["item"]["id"].as_str()?,
        row["resourceId"].as_str()?,
        row["channelId"].as_str()?,
    ])
}

fn suffix(key: &[&str; 4]) -> PathBuf {
    let mut path = PathBuf::from("games");
    for id in key {
        path.push(format!("{:x}", Sha256::digest(id.as_bytes())));
    }
    path
}

pub fn directory(row: &Value) -> Option<PathBuf> {
    let path = Path::new(row["directory"].as_str()?);
    if !path.is_absolute()
        || path.components().any(|c| c == Component::ParentDir)
        || !path.ends_with(suffix(&identity(row)?))
    {
        return None;
    }
    for dir in path.ancestors().take(5) {
        match fs::symlink_metadata(dir) {
            Ok(meta) if meta.is_dir() => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            _ => return None,
        }
    }
    Some(path.to_owned())
}

fn filename(name: &str) -> bool {
    safe_filename(name) == name
}

pub fn safe_filename(label: &str) -> String {
    let leaf = label.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = leaf
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim().trim_end_matches([' ', '.']);
    if clean.is_empty() {
        return "game.bin".into();
    }
    let prefix = clean.split('.').next().unwrap().to_ascii_uppercase();
    let reserved = matches!(prefix.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || prefix
            .strip_prefix("COM")
            .or_else(|| prefix.strip_prefix("LPT"))
            .is_some_and(|v| matches!(v, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"));
    let mut name = if reserved {
        format!("_{clean}")
    } else {
        clean.to_owned()
    };
    if name.encode_utf16().count() > 180 {
        fn prefix(s: &str, limit: usize) -> String {
            let mut n = 0;
            s.chars()
                .take_while(|c| {
                    n += c.len_utf16();
                    n <= limit
                })
                .collect()
        }
        let ext = std::path::Path::new(&name)
            .extension()
            .and_then(|v| v.to_str())
            .map(|s| prefix(&format!(".{s}"), 20))
            .unwrap_or_default();
        name = format!("{}{}", prefix(&name, 150), ext);
    }
    name
}

fn target(row: &Value) -> Option<PathBuf> {
    let name = row["filename"].as_str()?;
    filename(name).then_some(())?;
    Some(directory(row)?.join(name))
}

fn read(root: &Path) -> Result<Vec<Value>> {
    let path = root.join(INDEX);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(serde_json::from_reader(fs::File::open(path)?)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
        _ => Err(Error::Invalid),
    }
}

fn edit<T>(root: &Path, action: impl FnOnce(&mut Vec<Value>) -> Result<T>) -> Result<T> {
    let _gate = crate::recovery::gate(root, true).map_err(|_| Error::Invalid)?;
    let mut rows = read(root)?;
    let result = action(&mut rows)?;
    crate::recovery::atomic(root, INDEX, &serde_json::to_vec(&rows)?)
        .map_err(|_| Error::Invalid)?;
    Ok(result)
}

fn regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

fn published(row: &Value, path: &Path) -> bool {
    let (Some(len), Some(expected)) = (
        row["publishingBytes"].as_u64(),
        row["publishingSha256"].as_str(),
    ) else {
        return false;
    };
    if !fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() == len) {
        return false;
    }
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut hash = Sha256::new();
    io::copy(&mut file, &mut hash).is_ok() && format!("{:x}", hash.finalize()) == expected
}

fn status(row: &Value, file: &Path) -> Option<&'static str> {
    match row["status"].as_str()? {
        "done" => Some(if regular(file) { "done" } else { "failed" }),
        // 文件发布后、台账落盘前退出，通过已经验证的目标文件恢复完成状态。
        "running" if published(row, file) => Some("done"),
        "queued" | "running" | "failed"
            if row["queueSession"].as_str() == Some(SESSION.as_str()) =>
        {
            match row["status"].as_str()? {
                "queued" => Some("queued"),
                "running" => Some("running"),
                _ => Some("failed"),
            }
        }
        "queued" | "running" | "paused" | "failed" => Some("paused"),
        _ => None,
    }
}

pub fn list(root: &Path) -> Result<Vec<Value>> {
    let _gate = crate::recovery::gate(root, false).map_err(|_| Error::Invalid)?;
    let mut out = vec![];
    let mut seen = HashSet::new();
    for mut row in read(root)? {
        let Some(file) = target(&row) else { continue };
        let Some(state) = status(&row, &file) else {
            continue;
        };
        let key = identity(&row).unwrap().map(str::to_owned);
        if !seen.insert(key.clone()) {
            continue;
        }
        row["missing"] = json!(row["status"] == "done" && state == "failed");
        row["status"] = json!(state);
        row["key"] = json!(key);
        if state == "done" {
            row["file"] = json!(file);
        }
        out.push(row);
    }
    out.sort_by(|a, b| b["createdAt"].as_str().cmp(&a["createdAt"].as_str()));
    Ok(out)
}

fn index(rows: &[Value], key: &[&str; 4]) -> Result<usize> {
    rows.iter()
        .position(|r| {
            identity(r).as_ref() == Some(key) && target(r).and_then(|p| status(r, &p)).is_some()
        })
        .ok_or(Error::Invalid)
}

pub fn change(root: &Path, key: [&str; 4], action: &str) -> Result<()> {
    edit(root, |rows| {
        let i = index(rows, &key)?;
        let row = &mut rows[i];
        let state = status(row, &target(row).ok_or(Error::Invalid)?).ok_or(Error::Invalid)?;
        match action {
            "remove" if state != "running" && state != "queued" => {
                rows.remove(i);
            }
            "pause" if state == "queued" || state == "running" => {
                row["status"] = json!("paused");
            }
            "resume" if state == "failed" || state == "paused" => {
                row["status"] = json!("queued");
                row["queueSession"] = json!(*SESSION);
                row["bytes"] = json!(0);
                row["total"] = json!(0);
                row.as_object_mut().unwrap().remove("publishingBytes");
                row.as_object_mut().unwrap().remove("publishingSha256");
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    })
}

pub fn enqueue(
    root: &Path,
    downloads: &Path,
    item: Value,
    edition: &str,
    channel: &str,
    name: &str,
    created: &str,
) -> Result<()> {
    let key = [
        item["source"].as_str().ok_or(Error::Invalid)?,
        item["id"].as_str().ok_or(Error::Invalid)?,
        edition,
        channel,
    ];
    if !downloads.is_absolute() || !filename(name) || key.iter().any(|v| v.is_empty()) {
        return Err(Error::Invalid);
    }
    edit(root, |rows| {
        if let Ok(i) = index(rows, &key) {
            let row = &mut rows[i];
            let state = status(row, &target(row).ok_or(Error::Invalid)?).ok_or(Error::Invalid)?;
            if matches!(state, "done" | "queued" | "running") {
                return Ok(());
            }
            row["queueSession"] = json!(*SESSION);
            row["status"] = json!("queued");
            row["bytes"] = json!(0);
            row["total"] = json!(0);
            row.as_object_mut().unwrap().remove("publishingBytes");
            row.as_object_mut().unwrap().remove("publishingSha256");
        } else {
            rows.insert(0,json!({"item":item,"resourceId":edition,"channelId":channel,"filename":name,
                "directory":downloads.join(suffix(&key)),"createdAt":created,"status":"queued","queueSession":*SESSION,"bytes":0,"total":0}));
        }
        Ok(())
    })
}

pub fn claim(root: &Path, key: [&str; 4]) -> Result<u64> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    edit(root, |rows| {
        let i = index(rows, &key)?;
        let row = &mut rows[i];
        if status(row, &target(row).ok_or(Error::Invalid)?) != Some("queued") {
            return Err(Error::Invalid);
        }
        let attempt = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        row["status"] = json!("running");
        row["attempt"] = json!(attempt);
        Ok(attempt)
    })
}

pub fn finish_error(root: &Path, key: [&str; 4], attempt: u64, interrupted: bool) -> Result<()> {
    edit(root, |rows| {
        let i = index(rows, &key)?;
        let row = &mut rows[i];
        if row["attempt"] == attempt
            && status(row, &target(row).ok_or(Error::Invalid)?) == Some("running")
        {
            row["status"] = json!(if interrupted { "queued" } else { "failed" });
        }
        Ok(())
    })
}

pub fn progress(
    root: &Path,
    key: [&str; 4],
    attempt: u64,
    bytes: u64,
    total: Option<u64>,
) -> Result<()> {
    edit(root, |rows| {
        let i = index(rows, &key)?;
        let row = &mut rows[i];
        if row["attempt"] != attempt
            || status(row, &target(row).ok_or(Error::Invalid)?) != Some("running")
        {
            return Err(Error::Cancelled);
        }
        row["bytes"] = json!(bytes);
        row["total"] = json!(total.unwrap_or(0));
        Ok(())
    })
}

pub fn publish(
    root: &Path,
    key: [&str; 4],
    attempt: u64,
    source: &Path,
    details: &Value,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    let dest = {
        let _gate = crate::recovery::gate(root, false).map_err(|_| Error::Invalid)?;
        let rows = read(root)?;
        target(&rows[index(&rows, &key)?]).ok_or(Error::Invalid)?
    };
    let parent = dest.parent().ok_or(Error::Invalid)?;
    fs::create_dir_all(parent)?;
    // Windows 的原子移动接口需要长路径前缀。两个操作数使用同一实际目录。
    let parent = fs::canonicalize(parent)?;
    let published_file = parent.join(dest.file_name().ok_or(Error::Invalid)?);
    let mut temp = tempfile::NamedTempFile::new_in(&parent)?;
    let mut input = fs::File::open(source)?;
    use std::io::{Read, Write};
    let mut buf = vec![0; 131072];
    let mut digest = Sha256::new();
    loop {
        if cancelled() {
            return Err(Error::Invalid);
        }
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        temp.write_all(&buf[..n])?;
        digest.update(&buf[..n]);
    }
    temp.as_file().sync_all()?;
    let len = temp.as_file().metadata()?.len();
    let _gate = crate::recovery::gate(root, true).map_err(|_| Error::Invalid)?;
    let mut rows = read(root)?;
    let i = index(&rows, &key)?;
    if len == 0
        || cancelled()
        || rows[i]["attempt"] != attempt
        || target(&rows[i]).as_ref() != Some(&dest)
        || status(&rows[i], &dest) != Some("running")
        || dest.try_exists()?
    {
        return Err(Error::Invalid);
    }
    rows[i]["publishingBytes"] = json!(len);
    rows[i]["publishingSha256"] = json!(format!("{:x}", digest.finalize()));
    rows[i]["extractionCode"] = details["extractionCode"].clone();
    rows[i]["archivePassword"] = details["archivePassword"].clone();
    let save = |rows: &[Value]| {
        crate::recovery::atomic(root, INDEX, &serde_json::to_vec(rows)?).map_err(|_| Error::Invalid)
    };
    save(&rows)?;
    temp.persist_noclobber(&published_file).map_err(|e| e.error)?;
    rows[i]["status"] = json!("done");
    rows[i]["bytes"] = json!(len);
    rows[i]["total"] = json!(len);
    rows[i].as_object_mut().unwrap().remove("publishingBytes");
    rows[i].as_object_mut().unwrap().remove("publishingSha256");
    save(&rows)
}
