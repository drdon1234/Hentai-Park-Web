use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::{fs, path::Path};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported_schema")]
    Version,
    #[error("missing_database")]
    Missing,
    #[error("invalid_database")]
    Invalid,
    #[error("storage_io")]
    Io,
    #[error("storage_database")]
    Database(#[from] rusqlite::Error),
}

pub fn open(path: &Path) -> Result<Connection, Error> {
    let root = path.parent().ok_or(Error::Invalid)?;
    if !path.try_exists().map_err(|_| Error::Io)? {
        // 已存在的库资产指向丢失的数据库；保留目录供重新挂载或恢复。
        if root
            .join("backup_restore.json")
            .try_exists()
            .map_err(|_| Error::Io)?
        {
            return Err(Error::Missing);
        }
        for name in ["downloads", "backups", "cookies", "source_state"] {
            match fs::read_dir(root.join(name)) {
                Ok(mut files) => {
                    if files.next().transpose().map_err(|_| Error::Io)?.is_some() {
                        return Err(Error::Missing);
                    }
                }
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(Error::Io),
                _ => (),
            }
        }
        let temp = tempfile::NamedTempFile::new_in(root).map_err(|_| Error::Io)?;
        {
            let mut db = Connection::open(temp.path())?;
            let tx = db.transaction()?;
            tx.execute_batch(crate::SCHEMA)?;
            tx.commit()?;
        }
        temp.persist_noclobber(path).map_err(|_| Error::Io)?;
    }
    if !fs::symlink_metadata(path).map_err(|_| Error::Io)?.is_file() {
        return Err(Error::Invalid);
    }
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    match version {
        10 => (),
        7 => {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // 基线 Windows 的同名业务表字段与移动端一致，仅新增订阅元数据和酒馆编辑历史。
            for sql in [
                "SELECT source,id,json,favorite FROM works LIMIT 0",
                "SELECT source,work_id,chapter_id,progress_updated_at,position_updated_at FROM chapters LIMIT 0",
                "SELECT uid,dir,json FROM download_tasks LIMIT 0",
                "SELECT uid,original,card_json FROM tavern_resources LIMIT 0",
                "SELECT id,resource_uid,data_json FROM tavern_sessions LIMIT 0",
                "SELECT identity,target,created_at FROM subscriptions LIMIT 0",
                "SELECT id,token FROM restore_checkpoints LIMIT 0",
            ]{tx.prepare(sql).map_err(|_|Error::Invalid)?;}
            tx.execute_batch(crate::SCHEMA)?;
            tx.commit()?;
        }
        _ => return Err(Error::Version),
    }
    db.pragma_update(None, "foreign_keys", true)?;
    db.pragma_update(None, "journal_mode", "wal")?;
    Ok(db)
}
