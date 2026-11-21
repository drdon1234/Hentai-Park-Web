use crate::contract::*;
use park_storage::progress::{Ledger, CHAPTERS};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::{Arc, MutexGuard};
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, StorageError>;

#[derive(uniffi::Object, Default)]
pub struct Request {
    pub(crate) cancelled: CancellationToken,
}

#[uniffi::export]
impl Request {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn cancel(&self) {
        self.cancelled.cancel();
    }
}

impl Request {
    pub(crate) fn check(&self) -> Result<()> {
        if self.cancelled.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        Ok(())
    }
}

#[derive(uniffi::Object)]
pub struct ProgressStore {
    ledger: Ledger,
    pub(crate) chats:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Weak<crate::TavernChat>>>,
    pub(crate) images: Arc<park_media::ImagePool>,
    pub(crate) image_cache_limit: u64,
    pub(crate) root: std::path::PathBuf,
}

impl ProgressStore {
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        Ok(self.ledger.lock()?)
    }
}

#[uniffi::export]
impl ProgressStore {
    #[uniffi::constructor]
    pub fn open_windows_database(file: String) -> Result<Arc<Self>> {
        let path = Path::new(&file);
        let root = path
            .parent()
            .filter(|p| p.is_absolute())
            .ok_or(StorageError::MissingDatabase)?
            .to_owned();
        std::fs::create_dir_all(&root).map_err(|_| StorageError::IoFailure)?;
        let _gate = park_storage::recovery::gate(&root, true)?;
        let db = park_storage::desktop::open(path).map_err(|e| match e {
            park_storage::desktop::Error::Version => StorageError::UnsupportedSchema,
            park_storage::desktop::Error::Database(e) => e.into(),
            park_storage::desktop::Error::Missing => StorageError::MissingDatabase,
            park_storage::desktop::Error::Invalid => StorageError::CorruptDatabase,
            park_storage::desktop::Error::Io => StorageError::IoFailure,
        })?;
        Self::from_root(root, db, None)
    }

    pub fn set_reader_mode(&self, key: ChapterKey, mode: Option<i32>) -> Result<()> {
        Ok(self.ledger.set_reader_mode(&key, mode)?)
    }

    /// 新装先在同卷准备完整库；旧目录中缺失的库不能用空库覆盖。
    #[uniffi::constructor]
    pub fn open_library(support_directory: String) -> Result<Arc<Self>> {
        Self::open_library_inner(support_directory, None)
    }

    #[uniffi::constructor]
    pub fn open_library_with_browser(
        support_directory: String,
        browser: Box<dyn crate::RecoveryBrowser>,
    ) -> Result<Arc<Self>> {
        Self::open_library_inner(support_directory, Some(browser.as_ref()))
    }

    #[uniffi::constructor]
    pub fn open(support_directory: String) -> Result<Arc<Self>> {
        park_domain::install_crash_diagnostics();
        Self::open_existing(support_directory, None)
    }
}

impl ProgressStore {
    fn open_library_inner(
        support_directory: String,
        browser: Option<&dyn crate::RecoveryBrowser>,
    ) -> Result<Arc<Self>> {
        park_domain::install_crash_diagnostics();
        let support = Path::new(&support_directory);
        let root = support.join("park");
        let path = root.join("park.db");
        if !path.try_exists().map_err(|_| StorageError::IoFailure)? {
            for legacy in ["downloads", "backups"] {
                if support
                    .join(legacy)
                    .try_exists()
                    .map_err(|_| StorageError::IoFailure)?
                {
                    return Err(StorageError::MissingDatabase);
                }
            }
            std::fs::create_dir_all(&root).map_err(|_| StorageError::IoFailure)?;
            let mut entries = root.read_dir().map_err(|_| StorageError::IoFailure)?;
            if entries.next().is_some() {
                return Self::open_existing(support_directory, browser);
            }
            let temp =
                tempfile::NamedTempFile::new_in(support).map_err(|_| StorageError::IoFailure)?;
            {
                let mut db = Connection::open(temp.path())?;
                let tx = db.transaction()?;
                tx.execute_batch(park_storage::SCHEMA)?;
                tx.commit()?;
            }
            if let Err(error) = temp.persist_noclobber(path) {
                if error.error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(StorageError::IoFailure);
                }
            }
        }
        Self::open_existing(support_directory, browser)
    }

    /// 只在识别已有格式并完成恢复后交付句柄。
    fn open_existing(
        support_directory: String,
        browser: Option<&dyn crate::RecoveryBrowser>,
    ) -> Result<Arc<Self>> {
        let root = Path::new(&support_directory).join("park");
        let path = root.join("park.db");
        if !path.try_exists().map_err(|_| StorageError::IoFailure)? {
            return Err(StorageError::MissingDatabase);
        }
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let _gate = park_storage::recovery::gate(&root, true)?;
        Self::from_root(root, db, browser)
    }
    fn from_root(
        root: std::path::PathBuf,
        db: Connection,
        browser: Option<&dyn crate::RecoveryBrowser>,
    ) -> Result<Arc<Self>> {
        let schema: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if schema != 10 {
            return Err(StorageError::UnsupportedSchema);
        }
        db.prepare(&format!("{CHAPTERS} LIMIT 0"))?;
        db.prepare("SELECT source,work_id,time,chapter_id,chapter_title,page,page_count,position FROM history LIMIT 0")?;
        crate::recovery::recover(&root, &db, browser)?;
        let settings = std::fs::read(root.join("settings.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .unwrap_or_default();
        let image_concurrency = settings["imageConcurrency"].as_u64()
            .unwrap_or(6)
            .clamp(2, 16);
        Ok(Arc::new(Self {
            ledger: Ledger::new(db)?,
            chats: Default::default(),
            images: park_media::ImagePool::new(16, image_concurrency as usize),
            image_cache_limit:settings["cacheLimitMb"].as_u64().unwrap_or(1024).min(1048576)*1048576,
            root,
        }))
    }
}

#[uniffi::export]
impl ProgressStore {
    pub fn read(&self, key: ChapterKey) -> Result<Option<ChapterProgress>> {
        Ok(self.ledger.read(key)?)
    }
    pub fn snapshot(&self, request: Arc<Request>) -> Result<ProgressSnapshot> {
        Ok(self.ledger.snapshot(&|| request.cancelled.is_cancelled())?)
    }
    pub fn changes_since(&self, after: i64) -> Result<Changes> {
        Ok(self.ledger.changes_since(after)?)
    }
    pub fn turn_page(&self, turn: PageTurn, request: Arc<Request>) -> Result<ChapterProgress> {
        Ok(self
            .ledger
            .turn_page(turn, &|| request.cancelled.is_cancelled())?)
    }
    pub fn save_position(
        &self,
        position: MediaPosition,
        request: Arc<Request>,
    ) -> Result<ChapterProgress> {
        Ok(self
            .ledger
            .save_position(position, &|| request.cancelled.is_cancelled())?)
    }
}
