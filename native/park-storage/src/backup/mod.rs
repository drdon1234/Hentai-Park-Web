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
