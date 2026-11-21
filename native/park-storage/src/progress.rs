use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChapterKey {
    pub source: String,
    pub work_id: String,
    pub chapter_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageTurn {
    pub key: ChapterKey,
    pub history_chapter_id: Option<String>,
    pub chapter_title: Option<String>,
    pub page_index: i64,
    pub page_count: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressSnapshot {
    pub revision: i64,
    pub chapters: Vec<ChapterProgress>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPosition {
    pub key: ChapterKey,
    pub chapter_title: String,
    pub seconds: i64,
    pub duration_seconds: Option<i64>,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    pub revision: i64,
    pub chapter: ChapterProgress,
}

#[derive(Debug, Clone)]
pub enum Changes {
    Events {
        revision: i64,
        items: Vec<ProgressEvent>,
    },
    SnapshotRequired {
        revision: i64,
    },
}

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::sync::{Mutex, MutexGuard};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database")]
    Database(#[from] rusqlite::Error),
    #[error("corrupt_database")]
    CorruptDatabase,
    #[error("io_failure")]
    IoFailure,
    #[error("invalid_progress")]
    InvalidProgress,
    #[error("missing_work")]
    MissingWork,
    #[error("conflict")]
    Conflict,
    #[error("cancelled")]
    Cancelled,
}
type Result<T> = std::result::Result<T, Error>;
fn check(cancelled: &impl Fn() -> bool) -> Result<()> {
    if cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
/// 两个宿主原有关系模型在 SQL 边界映射到同一套进度冲突及事件规则。
pub(crate) struct Layout {
    pub chapters: &'static str,
    pub work_exists: &'static str,
    pub download_work: bool,
    pub set_page: &'static str,
    pub page_history: &'static str,
    pub set_position: &'static str,
    pub position_history: &'static str,
}
pub struct Ledger {
    connection: Mutex<Connection>,
    layout: &'static Layout,
}
impl Ledger {
    pub fn new(db: Connection) -> Result<Self> {
        Self::with_layout(db, &LIBRARY)
    }
    pub(crate) fn with_layout(db: Connection, layout: &'static Layout) -> Result<Self> {
        db.execute_batch("CREATE TABLE IF NOT EXISTS park_progress_events(revision INTEGER PRIMARY KEY,payload TEXT NOT NULL)")?;
        Ok(Self {
            connection: Mutex::new(db),
            layout,
        })
    }
    pub fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| Error::IoFailure)
    }
}
fn row_progress(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChapterProgress> {
    Ok(ChapterProgress {
        key: ChapterKey {
            source: row.get(0)?,
            work_id: row.get(1)?,
            chapter_id: row.get(2)?,
        },
        page_index: row.get(3)?,
        page_count: row.get(4)?,
        page_updated_ms: row.get(5)?,
        position_seconds: row.get(6)?,
        duration_seconds: row.get(7)?,
        position_updated_ms: row.get(8)?,
        read: row.get(9)?,
        date_read_ms: row.get(10)?,
    })
}

pub const CHAPTERS: &str =
    "SELECT source,work_id,chapter_id,last_page,page_count,progress_updated_at,
    position,duration,position_updated_at,read,date_read FROM chapters";

fn lookup(db: &Connection, layout: &Layout, key: &ChapterKey) -> Result<Option<ChapterProgress>> {
    Ok(db
        .query_row(
            &format!(
                "{} WHERE source=?1 AND work_id=?2 AND chapter_id=?3",
                layout.chapters
            ),
            params![key.source, key.work_id, key.chapter_id],
            row_progress,
        )
        .optional()?)
}

fn ensure_work(db: &Connection, layout: &Layout, key: &ChapterKey) -> Result<()> {
    let exists: bool = db.query_row(layout.work_exists, params![key.source, key.work_id], |r| {
        r.get(0)
    })?;
    if exists {
        return Ok(());
    }
    if layout.download_work {
        let raw: Option<String> = db
            .query_row(
                "SELECT json FROM download_tasks WHERE source=? AND uid=?",
                params![key.source, format!("{}:{}", key.source, key.work_id)],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(task) = raw.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
            let item = &task["item"];
            let completed = task["completed"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == &key.chapter_id));
            let deleting = task["deletingChapters"]
                .as_array()
                .is_some_and(|ids| ids.is_empty() || ids.iter().any(|id| id == &key.chapter_id));
            if item["id"] == key.work_id && item["source"] == key.source && completed && !deleting {
                db.execute(
                    "INSERT INTO works(source,id,title,cover,json) VALUES(?,?,?,?,?)",
                    params![
                        key.source,
                        key.work_id,
                        item["title"].as_str().unwrap_or(&key.work_id),
                        item["cover"].as_str().unwrap_or(""),
                        item.to_string()
                    ],
                )?;
                return Ok(());
            }
        }
    }
    Err(Error::MissingWork)
}
fn revision(db: &Connection) -> Result<i64> {
    Ok(db.query_row(
        "SELECT COALESCE(MAX(revision),0) FROM park_progress_events",
        [],
        |row| row.get(0),
    )?)
}

impl Ledger {
    pub fn set_reader_mode(&self, key: &ChapterKey, mode: Option<i32>) -> Result<()> {
        if !self.layout.download_work || mode.is_some_and(|m| !(0..=2).contains(&m)) {
            return Err(Error::InvalidProgress);
        }
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_work(&tx, self.layout, key)?;
        tx.execute(
            "UPDATE works SET reader_mode=? WHERE source=? AND id=?",
            params![mode, key.source, key.work_id],
        )?;
        Ok(tx.commit()?)
    }
    pub fn read(&self, key: ChapterKey) -> Result<Option<ChapterProgress>> {
        let db = self.lock()?;
        lookup(&db, self.layout, &key)
    }

    pub fn snapshot(&self, cancelled: &impl Fn() -> bool) -> Result<ProgressSnapshot> {
        let mut db = self.lock()?;
        check(cancelled)?;
        let tx = db.transaction()?;
        let revision = revision(&tx)?;
        let mut chapters = Vec::new();
        let mut statement = tx.prepare(&format!(
            "{} ORDER BY source,work_id,chapter_id",
            self.layout.chapters
        ))?;
        for chapter in statement.query_map([], row_progress)? {
            check(cancelled)?;
            chapters.push(chapter?);
        }
        check(cancelled)?;
        Ok(ProgressSnapshot { revision, chapters })
    }

    pub fn changes_since(&self, after: i64) -> Result<Changes> {
        let mut db = self.lock()?;
        let tx = db.transaction()?;
        let revision = revision(&tx)?;
        let earliest: Option<i64> =
            tx.query_row("SELECT MIN(revision) FROM park_progress_events", [], |r| {
                r.get(0)
            })?;
        if after < earliest.unwrap_or(1) - 1 || after > revision {
            return Ok(Changes::SnapshotRequired { revision });
        }
        let mut stmt = tx.prepare(
            "SELECT revision,payload FROM park_progress_events WHERE revision>? ORDER BY revision",
        )?;
        let mut rows = stmt.query([after])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            let payload: String = row.get(1)?;
            if payload == "null" {
                return Ok(Changes::SnapshotRequired { revision });
            }
            items.push(ProgressEvent {
                revision: row.get(0)?,
                chapter: serde_json::from_str(&payload).map_err(|_| Error::CorruptDatabase)?,
            });
        }
        Ok(Changes::Events { revision, items })
    }

    pub fn turn_page(
        &self,
        turn: PageTurn,
        cancelled: &impl Fn() -> bool,
    ) -> Result<ChapterProgress> {
        if turn.page_index < 0 || turn.page_index >= turn.page_count || turn.updated_ms < 0 {
            return Err(Error::InvalidProgress);
        }
        let mut db = self.lock()?;
        check(cancelled)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = &turn.key;
        let old = lookup(&tx, self.layout, key)?;
        if let Some(old) = old {
            if old.page_updated_ms >= Some(turn.updated_ms) {
                return if old.page_updated_ms == Some(turn.updated_ms)
                    && old.page_index == turn.page_index
                    && old.page_count == turn.page_count
                {
                    Ok(old)
                } else {
                    Err(Error::Conflict)
                };
            }
        }
        ensure_work(&tx, self.layout, key)?;
        let finished = turn.page_index == turn.page_count - 1;
        tx.execute(
            self.layout.set_page,
            params![
                key.source,
                key.work_id,
                key.chapter_id,
                turn.chapter_title,
                turn.page_index,
                turn.page_count,
                turn.updated_ms,
                finished
            ],
        )?;
        tx.execute(
            self.layout.page_history,
            params![
                key.source,
                key.work_id,
                turn.updated_ms,
                turn.history_chapter_id,
                turn.chapter_title,
                turn.page_index,
                turn.page_count
            ],
        )?;
        commit_progress(tx, self.layout, key, cancelled)
    }
    pub fn save_position(
        &self,
        position: MediaPosition,
        cancelled: &impl Fn() -> bool,
    ) -> Result<ChapterProgress> {
        if position.seconds < 0
            || position.duration_seconds.is_some_and(|n| n < 0)
            || position.updated_ms < 0
        {
            return Err(Error::InvalidProgress);
        }
        let key = &position.key;
        let mut db = self.lock()?;
        check(cancelled)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check(cancelled)?;
        let duration = position.duration_seconds.unwrap_or(0);
        if let Some(old) = lookup(&tx, self.layout, key)? {
            if old.position_updated_ms >= Some(position.updated_ms) {
                return if old.position_updated_ms == Some(position.updated_ms)
                    && old.position_seconds == position.seconds
                    && (duration == 0 || old.duration_seconds == duration)
                {
                    Ok(old)
                } else {
                    Err(Error::Conflict)
                };
            }
        }
        ensure_work(&tx, self.layout, key)?;
        let finished = duration > 0 && position.seconds >= duration;
        tx.execute(
            self.layout.set_position,
            params![
                key.source,
                key.work_id,
                key.chapter_id,
                position.chapter_title,
                position.seconds,
                duration,
                position.updated_ms,
                finished
            ],
        )?;
        tx.execute(
            self.layout.position_history,
            params![
                key.source,
                key.work_id,
                position.updated_ms,
                key.chapter_id,
                position.chapter_title,
                position.seconds
            ],
        )?;
        commit_progress(tx, self.layout, key, cancelled)
    }
}
fn commit_progress(
    tx: rusqlite::Transaction<'_>,
    layout: &Layout,
    key: &ChapterKey,
    cancelled: &impl Fn() -> bool,
) -> Result<ChapterProgress> {
    let chapter = record_chapter(&tx, layout, key)?;
    check(cancelled)?;
    tx.commit()?;
    Ok(chapter)
}

pub(crate) fn record_chapter(
    tx: &rusqlite::Transaction<'_>,
    layout: &Layout,
    key: &ChapterKey,
) -> Result<ChapterProgress> {
    let chapter = lookup(tx, layout, key)?.ok_or(Error::CorruptDatabase)?;
    let payload = serde_json::to_string(&chapter).map_err(|_| Error::InvalidProgress)?;
    tx.execute(
        "INSERT INTO park_progress_events(payload) VALUES(?)",
        [payload],
    )?;
    let revision = tx.last_insert_rowid();
    tx.execute(
        "DELETE FROM park_progress_events WHERE revision<=?",
        [revision - 256],
    )?;
    Ok(chapter)
}

const LIBRARY:Layout=Layout {
download_work:true,
chapters: CHAPTERS,
work_exists: "SELECT EXISTS(SELECT 1 FROM works WHERE source=? AND id=?)",
set_page: "INSERT INTO chapters(source,work_id,chapter_id,title,last_page,page_count,progress_updated_at,read,date_read)
            VALUES(?1,?2,?3,COALESCE(?4,''),?5,?6,?7,?8,CASE WHEN ?8 THEN ?7 END)
            ON CONFLICT(source,work_id,chapter_id) DO UPDATE SET last_page=excluded.last_page,
            page_count=excluded.page_count,progress_updated_at=excluded.progress_updated_at,
            read=chapters.read OR excluded.read,date_read=COALESCE(chapters.date_read,excluded.date_read)",
page_history: "INSERT INTO history(source,work_id,time,chapter_id,chapter_title,page,page_count)
            VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(source,work_id) DO UPDATE SET
            time=excluded.time,chapter_id=excluded.chapter_id,chapter_title=excluded.chapter_title,
            page=excluded.page,page_count=excluded.page_count WHERE excluded.time>=history.time",
set_position: "INSERT INTO chapters(source,work_id,chapter_id,title,position,duration,position_updated_at,read,date_read) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,CASE WHEN ?8 THEN ?7 END)
            ON CONFLICT(source,work_id,chapter_id) DO UPDATE SET position=excluded.position,duration=CASE WHEN excluded.duration>0 THEN excluded.duration ELSE chapters.duration END,
            position_updated_at=excluded.position_updated_at,read=chapters.read OR excluded.read,date_read=COALESCE(chapters.date_read,excluded.date_read)",
position_history: "INSERT INTO history(source,work_id,time,chapter_id,chapter_title,position) VALUES(?1,?2,?3,?4,?5,?6)
            ON CONFLICT(source,work_id) DO UPDATE SET time=excluded.time,chapter_id=excluded.chapter_id,chapter_title=excluded.chapter_title,position=excluded.position,page=0,page_count=0 WHERE excluded.time>=history.time",
};
