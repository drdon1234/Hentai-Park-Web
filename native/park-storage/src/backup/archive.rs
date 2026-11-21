use super::{encode, object, read_json, Error, Result, Scope};
use crate::recovery;
use flate2::read::MultiGzDecoder;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::{
    fs,
    io::{self, BufReader},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) fn now_ms() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Invalid)?
        .as_millis()
        .try_into()
        .map_err(|_| Error::Invalid)
}

pub(super) fn ready(root: &Path) -> Result<()> {
    if root.join("backup_restore.json").try_exists()?
        || crate::plugins::pending(root).map_err(|_| Error::Invalid)?
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
fn store_json(root: &Path, name: &str, value: &Value) -> Result<()> {
    recovery::atomic(root, name, &serde_json::to_vec(value)?)
        .and_then(|_| recovery::sync_directory(root))
        .map_err(|_| io::Error::other("backup_write"))?;
    Ok(())
}
/// 调用方持有恢复独占锁，逐键合并构成同一份配置的所有写入。
pub fn set_preferences(root: &Path, values: impl IntoIterator<Item = (String, Value)>) -> Result<()> {
    ready(root)?;
    let mut current = object(read_json(&root.join("settings.json"))?)?;
    current.extend(values);
    store_json(root, "settings.json", &current.into())
}

pub(super) fn persist(
    dir: &Path,
    timestamp: &str,
    write: impl FnOnce(&mut fs::File) -> Result<()>,
    cancelled: &impl Fn() -> bool,
) -> Result<String> {
    if timestamp.len() != 15
        || !timestamp.bytes().enumerate().all(|(i, b)| {
            if i == 8 {
                b == b'_'
            } else {
                b.is_ascii_digit()
            }
        })
    {
        return Err(Error::Invalid);
    }
    fs::create_dir_all(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    write(temp.as_file_mut())?;
    temp.as_file().sync_all()?;
    for counter in 1u32.. {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let suffix = if counter == 1 {
            String::new()
        } else {
            format!("_{counter:06}")
        };
        let name = format!("hentaipark_{timestamp}{suffix}.parkbak");
        match temp.persist_noclobber(dir.join(&name)) {
            Ok(_) => {
                recovery::sync_directory(dir).map_err(|_| io::Error::other("backup_sync"))?;
                return Ok(name);
            }
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => temp = e.file,
            Err(e) => return Err(e.error.into()),
        }
    }
    Err(Error::Invalid)
}

pub fn save(
    db: &Connection,
    root: &Path,
    dir: &Path,
    timestamp: &str,
    scope: Scope,
    library: bool,
    settings: bool,
    sources: bool,
    cancelled: impl Fn() -> bool,
) -> Result<String> {
    ready(root)?;
    persist(
        dir,
        timestamp,
        |out| encode::write(db, root, out, scope, library, settings, sources, &cancelled),
        &cancelled,
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    format: String,
    version: u64,
    origin: String,
    created_at: i64,
}

fn automatic_files(dir: &Path, cancelled: &impl Fn() -> bool) -> Result<Vec<(i64, PathBuf)>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let entry = entry?;
        let p = entry.path();
        if !entry.file_type()?.is_file()
            || p.extension().and_then(|v| v.to_str()) != Some("parkbak")
        {
            continue;
        }
        // 读到 gzip 尾部并验证 CRC，未知版本和损坏文件全部保留。
        let stream = BufReader::new(MultiGzDecoder::new(BufReader::new(fs::File::open(&p)?)));
        if let Ok(h) = serde_json::from_reader::<_, Header>(stream) {
            if h.format == "parkbak" && h.version == 1 && h.origin == "automatic" {
                files.push((h.created_at, p));
            }
        }
    }
    files.sort_unstable_by(|a, b| b.cmp(a));
    Ok(files)
}

pub fn automatic_if_due(
    db: &Connection,
    root: &Path,
    dir: &Path,
    timestamp: &str,
    cancelled: impl Fn() -> bool,
) -> Result<Option<String>> {
    ready(root)?;
    let mut settings = object(read_json(&root.join("settings.json"))?)?;
    let int = |key: &str, fallback: i64| {
        settings
            .get(key)
            .and_then(Value::as_i64)
            .unwrap_or(fallback)
    };
    let hours = int("autoBackupHours", 24);
    let previous = int("lastAutoBackup", 0);
    let keep = int("autoBackupKeep", 3).clamp(1, 50) as usize;
    let now = now_ms()?;
    if hours <= 0
        || previous > 0 && previous <= now && now - previous < hours.saturating_mul(3_600_000)
    {
        return Ok(None);
    }
    fs::create_dir_all(dir)?;
    let pending = "auto_backup_pending.json";
    let interrupted = read_json(&root.join(pending))?.and_then(|v| v.as_i64());
    let mut files = automatic_files(dir, &cancelled)?;
    // 仅承接本机已登记的写入。归档、保留数清理、时间戳三个阶段可独立重试。
    let recovered = interrupted.and_then(|stamp| files.iter().find(|(t, _)| *t == stamp));
    let (created, path) = if let Some(pair) = recovered {
        pair.clone()
    } else {
        store_json(root, pending, &now.into())?;
        let name = persist(
            dir,
            timestamp,
            |out| {
                encode::write_at(
                    db,
                    root,
                    out,
                    Scope::Automatic,
                    true,
                    true,
                    true,
                    now as u128,
                    &cancelled,
                )
            },
            &cancelled,
        )?;
        (now, dir.join(name))
    };
    files.retain(|(_, p)| p != &path);
    for (_, old) in files.iter().skip(keep - 1) {
        fs::remove_file(old)?;
    }
    recovery::sync_directory(dir).map_err(|_| io::Error::other("backup_sync"))?;
    settings.insert("lastAutoBackup".into(), created.min(now).into());
    store_json(root, "settings.json", &settings.into())?;
    fs::remove_file(root.join(pending))?;
    Ok(path.file_name().and_then(|p| p.to_str()).map(str::to_owned))
}
