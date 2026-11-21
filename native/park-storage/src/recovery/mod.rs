mod cookies;
use base64::prelude::{Engine, BASE64_STANDARD};
pub use cookies::{Partition, RecoveryBrowser};
use rusqlite::{Connection, OptionalExtension};
use rusqlite::{Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;
use Error as StorageError;

pub fn checkpoint(root: &Path) -> Result<Option<String>, Error> {
    let path = root.join("park.db");
    if !path.try_exists().map_err(|_| Error::IoFailure)? {
        return Ok(None);
    }
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(db
        .query_row(
            "SELECT token FROM restore_checkpoints WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn gate_file(root: &Path) -> Result<fs::File, Error> {
    fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("restore.lock"))
        .map_err(|_| Error::IoFailure)
}

pub fn gate(root: &Path, exclusive: bool) -> Result<fs::File, Error> {
    let file = gate_file(root)?;
    if exclusive {
        fs2::FileExt::lock_exclusive(&file)
    } else {
        fs2::FileExt::lock_shared(&file)
    }
    .map_err(|_| Error::IoFailure)?;
    Ok(file)
}

/// 与阻塞恢复入口使用同一套文件锁，在 Android 和桌面均可取消等待。
pub fn try_lock_file(file: &fs::File, exclusive: bool) -> Result<bool, Error> {
    let result = if exclusive {
        fs2::FileExt::try_lock_exclusive(file)
    } else {
        fs2::FileExt::try_lock_shared(file)
    };
    match result {
        Ok(()) => Ok(true),
        Err(e)
            if e.kind() == ErrorKind::WouldBlock
                || e.raw_os_error().is_some_and(|code| {
                    Some(code) == fs2::lock_contended_error().raw_os_error()
                }) =>
        {
            Ok(false)
        }
        Err(_) => Err(Error::IoFailure),
    }
}

pub(crate) fn atomic(root: &Path, name: &str, data: &[u8]) -> Result<(), Error> {
    let mut temp = tempfile::NamedTempFile::new_in(root).map_err(|_| Error::IoFailure)?;
    temp.write_all(data)
        .and_then(|_| temp.as_file().sync_all())
        .map_err(|_| Error::IoFailure)?;
    let dest = root.join(name);
    temp.persist(&dest).map_err(|_| Error::IoFailure)?;
    if let Some(dir) = dest.parent().filter(|d| *d != root) {
        sync_directory(dir)?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid_journal")]
    InvalidJournal,
    #[error("pending_recovery")]
    PendingRecovery,
    #[error("io_failure")]
    IoFailure,
    #[error("storage_database")]
    Database(#[from] rusqlite::Error),
    #[error("backup_invalid")]
    Backup(#[from] crate::backup::Error),
}
#[derive(Deserialize, Serialize)]
struct Journal {
    token: String,
    before: BTreeMap<String, Option<String>>,
    after: BTreeMap<String, Option<String>>,
    #[serde(default)]
    cookies: Option<BTreeMap<String, Partition>>,
}

pub(crate) fn asset_path(root: &Path, name: &str) -> Result<std::path::PathBuf, Error> {
    if let Some((dir, file)) = name.split_once('/') {
        if !matches!(dir, "source_state" | "cookies")
            || !file.strip_suffix(".json").is_some_and(valid_key)
        {
            return Err(Error::InvalidJournal);
        }
        let data = root.join(dir);
        if !fs::symlink_metadata(&data)
            .map_err(|_| Error::IoFailure)?
            .is_dir()
        {
            return Err(Error::InvalidJournal);
        }
        return Ok(data.join(file));
    }
    if !allowed(name) {
        return Err(Error::InvalidJournal);
    }
    Ok(root.join(name))
}
fn allowed(name: &str) -> bool {
    if matches!(
        name,
        "settings.json" | "sources.json" | "reading_notes.json" | "subscriptions.json"
    ) {
        return true;
    }
    let Some(key) = name
        .strip_prefix("source_state_")
        .and_then(|s| s.strip_suffix(".json"))
    else {
        return false;
    };
    valid_key(key)
}

pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 100
        && key != "_app"
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn decode(value: &Option<String>) -> Result<Option<Vec<u8>>, StorageError> {
    value
        .as_ref()
        .map(|text| {
            BASE64_STANDARD
                .decode(text)
                .map_err(|_| StorageError::InvalidJournal)
        })
        .transpose()
}

pub fn recover(
    root: &Path,
    db: &Connection,
    browser: Option<&dyn RecoveryBrowser>,
) -> Result<(), StorageError> {
    let journal = root.join("backup_restore.json");
    let data = match fs::read(&journal) {
        Ok(data) => data,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(StorageError::IoFailure),
    };
    let record: Journal =
        serde_json::from_slice(&data).map_err(|_| StorageError::InvalidJournal)?;
    if record.token.is_empty() || record.before.keys().ne(record.after.keys()) {
        return Err(StorageError::InvalidJournal);
    }
    let mut before = BTreeMap::new();
    for (name, old) in record.before.iter() {
        if asset_path(root, name).is_err() {
            return Err(StorageError::InvalidJournal);
        }
        before.insert(name, decode(old)?);
        decode(&record.after[name])?;
    }
    let cookies = record.cookies.unwrap_or_default();
    for (source, partition) in &cookies {
        if !valid_key(source) {
            return Err(StorageError::InvalidJournal);
        }
        partition.validate()?;
    }
    let marker: Option<String> = db
        .query_row(
            "SELECT token FROM restore_checkpoints WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if marker.as_ref() != Some(&record.token) {
        for (source, partition) in &cookies {
            partition.restore(root, source, browser, false)?;
        }
        for (name, bytes) in before {
            let target = asset_path(root, &name)?;
            if let Some(bytes) = bytes {
                atomic(root, &name, &bytes)?;
            } else if let Err(e) = fs::remove_file(&target) {
                if e.kind() != ErrorKind::NotFound {
                    return Err(StorageError::IoFailure);
                }
            }
        }
        sync_directory(root)?;
    }
    fs::remove_file(journal).map_err(|_| StorageError::IoFailure)?;
    sync_directory(root)
}

/// 调用方持有跨进程恢复锁。检查点和业务合并共用事务，日志清理失败允许重启补齐。
pub fn apply(
    root: &Path,
    db: &mut Connection,
    files: BTreeMap<String, serde_json::Value>,
    partitions: BTreeMap<String, Partition>,
    browser: Option<&dyn RecoveryBrowser>,
    update: impl FnOnce(&Transaction<'_>) -> Result<(), crate::backup::Error>,
) -> Result<(), Error> {
    recover(root, db, browser)?;
    let token: String = db.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
    let mut before = BTreeMap::new();
    let mut after = BTreeMap::new();
    for (name, value) in &files {
        if asset_path(root, name).is_err() {
            return Err(Error::InvalidJournal);
        }
        let path = asset_path(root, name)?;
        let old = match fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => {
                Some(BASE64_STANDARD.encode(fs::read(path).map_err(|_| Error::IoFailure)?))
            }
            Ok(_) => return Err(Error::InvalidJournal),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(_) => return Err(Error::IoFailure),
        };
        before.insert(name.clone(), old);
        let bytes = serde_json::to_vec(value).map_err(|_| Error::InvalidJournal)?;
        after.insert(name.clone(), Some(BASE64_STANDARD.encode(bytes)));
    }
    for (source, cookie) in &partitions {
        if !valid_key(source) {
            return Err(Error::InvalidJournal);
        }
        cookie.validate()?;
    }
    let record = Journal {
        before,
        after,
        token,
        cookies: Some(partitions),
    };
    let journal = serde_json::to_vec(&record).map_err(|_| Error::InvalidJournal)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    atomic(root, "backup_restore.json", &journal)?;
    sync_directory(root)?;
    let result = (|| -> Result<(), Error> {
        update(&tx)?;
        for (name, value) in files {
            atomic(
                root,
                &name,
                &serde_json::to_vec(&value).map_err(|_| Error::InvalidJournal)?,
            )?;
        }
        for (source, partition) in record.cookies.into_iter().flatten() {
            partition.restore(root, &source, browser, true)?;
        }
        sync_directory(root)?;
        tx.execute("INSERT INTO restore_checkpoints(id,token) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET token=excluded.token",[record.token])?;
        tx.commit()?;
        Ok(())
    })();
    if result.is_err() {
        recover(root, db, browser)?;
        return result;
    }
    if fs::remove_file(root.join("backup_restore.json")).is_ok() {
        let _ = sync_directory(root);
    }
    Ok(())
}

pub fn sync_directory(root: &Path) -> Result<(), StorageError> {
    #[cfg(unix)]
    {
        fs::File::open(root)
            .and_then(|file| file.sync_all())
            .map_err(|_| StorageError::IoFailure)?;
    }
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}
