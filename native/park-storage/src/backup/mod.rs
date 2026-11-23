mod desktop;
pub use desktop::{import_desktop, save_desktop};
mod archive;
pub use archive::{automatic_if_due, save, set_preferences};
mod encode;
mod import;
pub use import::Import;
pub(crate) use import::subscriptions as import_subscriptions;
mod library;
mod sources;
pub(crate) use sources::cookies_on_disk;
/// 调用方持有源目录写锁。沿用恢复日志同步 Cookie 分区与浏览器，保留账号配置。
pub fn clear_site_cookies(root: &std::path::Path, key: &str, domains: Vec<String>,
    browser: &dyn crate::recovery::RecoveryBrowser) -> std::result::Result<(), crate::recovery::Error> {
    use crate::recovery::{self, Partition};
    let mut db = rusqlite::Connection::open_with_flags(root.join("park.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    recovery::recover(root, &db, Some(browser))?;
    let before = cookies_on_disk(root, key)?;
    let partition = Partition { before, after: Default::default(), domains };
    recovery::apply(root, &mut db, Default::default(), [(key.to_owned(), partition)].into(), Some(browser), |_| Ok(()))
}
mod tavern;
pub use encode::write;
pub use library::merge_library;
use serde_json::{Map, Value};
use std::{fs, io, path::Path};
pub use tavern::merge_tavern;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Manual,
    WithCredentials,
    Automatic,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("backup_io")]
    Io(#[from] io::Error),
    #[error("backup_database")]
    Database(#[from] rusqlite::Error),
    #[error("backup_json")]
    Json(#[from] serde_json::Error),
    #[error("backup_invalid")]
    Invalid,
    #[error("backup_cancelled")]
    Cancelled,
}
type Result<T> = std::result::Result<T, Error>;
type Object = Map<String, Value>;

fn read_json(path: &Path) -> Result<Option<Value>> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Ok(Some(serde_json::from_reader(io::BufReader::new(
            fs::File::open(path)?,
        ))?)),
        Ok(_) => Err(Error::Invalid),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn object(value: Option<Value>) -> Result<Object> {
    match value {
        None | Some(Value::Null) => Ok(Object::new()),
        Some(Value::Object(v)) => Ok(v),
        _ => Err(Error::Invalid),
    }
}
fn portable_settings(mut settings: Object) -> Object {
    for name in [
        "downloadDirectory",
        "adultConfirmed",
        "appLock",
        "appLockAfter",
        "lastAutoBackup",
        "lastLibraryUpdate",
        "lastSourceUpdate",
    ] {
        settings.remove(name);
    }
    settings
}
