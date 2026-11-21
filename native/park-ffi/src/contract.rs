pub use park_storage::progress::{
    Changes, ChapterKey, ChapterProgress, MediaPosition, PageTurn, ProgressEvent, ProgressSnapshot,
};
#[uniffi::remote(Record)]
pub struct ChapterKey {
    pub source: String,
    pub work_id: String,
    pub chapter_id: String,
}

#[uniffi::remote(Record)]
pub struct ChapterProgress {
    pub key: ChapterKey,
    pub page_index: i64,
    pub page_count: i64,
    pub page_updated_ms: Option<i64>,
    pub position_seconds: i64,
    pub duration_seconds: i64,
    pub position_updated_ms: Option<i64>,
    pub read: bool,
    pub date_read_ms: Option<i64>,
}

#[uniffi::remote(Record)]
pub struct PageTurn {
    pub key: ChapterKey,
    pub history_chapter_id: Option<String>,
    pub chapter_title: Option<String>,
    pub page_index: i64,
    pub page_count: i64,
    pub updated_ms: i64,
}

#[uniffi::remote(Record)]
pub struct ProgressSnapshot {
    pub revision: i64,
    pub chapters: Vec<ChapterProgress>,
}

#[uniffi::remote(Record)]
pub struct MediaPosition {
    pub key: ChapterKey,
    pub chapter_title: String,
    pub seconds: i64,
    pub duration_seconds: Option<i64>,
    pub updated_ms: i64,
}

#[uniffi::remote(Record)]
pub struct ProgressEvent {
    pub revision: i64,
    pub chapter: ChapterProgress,
}

#[uniffi::remote(Enum)]
pub enum Changes {
    Events {
        revision: i64,
        items: Vec<ProgressEvent>,
    },
    SnapshotRequired {
        revision: i64,
    },
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum StorageError {
    #[error("missing_database")]
    MissingDatabase,
    #[error("unsupported_schema")]
    UnsupportedSchema,
    #[error("corrupt_database")]
    CorruptDatabase,
    #[error("pending_recovery")]
    PendingRecovery,
    #[error("invalid_journal")]
    InvalidJournal,
    #[error("invalid_backup")]
    InvalidBackup,
    #[error("io_failure")]
    IoFailure,
    #[error("invalid_progress")]
    InvalidProgress,
    #[error("missing_work")]
    MissingWork,
    #[error("unavailable_download")]
    UnavailableDownload,
    #[error("invalid_download")]
    InvalidDownload,
    #[error("invalid_tavern_resource")]
    InvalidTavernResource,
    #[error("conflict")]
    Conflict,
    #[error("cancelled")]
    Cancelled,
}

impl From<rusqlite::Error> for StorageError {
    fn from(error: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode;
        match error.sqlite_error_code() {
            Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => Self::CorruptDatabase,
            _ => Self::IoFailure,
        }
    }
}

impl From<park_storage::downloads::Error> for StorageError {
    fn from(error: park_storage::downloads::Error) -> Self {
        use park_storage::downloads::Error;
        match error {
            Error::Invalid => Self::InvalidDownload,
            Error::Unavailable => Self::UnavailableDownload,
            Error::Database(e) => e.into(),
        }
    }
}

impl From<park_storage::progress::Error> for StorageError {
    fn from(e: park_storage::progress::Error) -> Self {
        use park_storage::progress::Error;
        match e {
            Error::Database(e) => e.into(),
            Error::CorruptDatabase => Self::CorruptDatabase,
            Error::IoFailure => Self::IoFailure,
            Error::InvalidProgress => Self::InvalidProgress,
            Error::MissingWork => Self::MissingWork,
            Error::Conflict => Self::Conflict,
            Error::Cancelled => Self::Cancelled,
        }
    }
}
