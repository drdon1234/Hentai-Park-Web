use crate::progress::{self, Layout, Ledger};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::path::Path;
pub mod history;

/// Web 继续使用自身的作品、文本历史、资源归属及下载表。两个新时钟均源自旧进度行。
pub fn open(path: &Path) -> Result<Ledger, progress::Error> {
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "foreign_keys", true)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let declared: i64 = tx.query_row("SELECT max(version) FROM schema_migrations", [], |r| {
        r.get(0)
    })?;
    if version != 16 || declared != 16 {
        return Err(progress::Error::CorruptDatabase);
    }
    let migrated: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('chapters') WHERE name='page_updated_ms')",
        [],
        |r| r.get(0),
    )?;
    if !migrated {
        tx.execute_batch("ALTER TABLE chapters ADD COLUMN page_count INTEGER;
            ALTER TABLE chapters ADD COLUMN page_updated_ms INTEGER;
            ALTER TABLE chapters ADD COLUMN position_updated_ms INTEGER;
            ALTER TABLE chapters ADD COLUMN date_read_ms INTEGER;
            UPDATE chapters AS c SET
              page_count=(SELECT h.page_count FROM history h WHERE h.source=c.source AND h.work_id=c.work_id AND h.chapter_id=c.chapter_id ORDER BY h.updated_at DESC LIMIT 1),
              page_updated_ms=CASE WHEN page_progress IS NOT NULL THEN CAST(round(unixepoch(progress_updated_at,'subsec')*1000) AS INTEGER) END,
              position_updated_ms=CASE WHEN video_position IS NOT NULL THEN CAST(round(unixepoch(progress_updated_at,'subsec')*1000) AS INTEGER) END,
              date_read_ms=CASE WHEN read_state='read' THEN CAST(round(unixepoch(progress_updated_at,'subsec')*1000) AS INTEGER) END;
            CREATE TABLE park_progress_events(revision INTEGER PRIMARY KEY,payload TEXT NOT NULL);")?;
        tx.execute_batch("CREATE TRIGGER park_web_new_progress AFTER INSERT ON chapters
            WHEN NEW.progress_updated_at IS NOT NULL AND NEW.page_updated_ms IS NULL AND NEW.position_updated_ms IS NULL
            BEGIN UPDATE chapters SET page_updated_ms=CASE WHEN NEW.page_progress IS NOT NULL THEN CAST(round(unixepoch(NEW.progress_updated_at,'subsec')*1000) AS INTEGER) END,
              position_updated_ms=CASE WHEN NEW.video_position IS NOT NULL THEN CAST(round(unixepoch(NEW.progress_updated_at,'subsec')*1000) AS INTEGER) END,
              date_read_ms=CASE WHEN NEW.read_state='read' THEN CAST(round(unixepoch(NEW.progress_updated_at,'subsec')*1000) AS INTEGER) END WHERE rowid=NEW.rowid;END;
            CREATE TRIGGER park_web_restored_progress AFTER UPDATE OF page_progress,video_position,read_state ON chapters
            WHEN NEW.progress_updated_at IS NOT NULL AND (NEW.page_progress IS NOT OLD.page_progress OR NEW.video_position IS NOT OLD.video_position OR NEW.read_state IS NOT OLD.read_state)
            BEGIN UPDATE chapters SET page_updated_ms=CASE WHEN NEW.page_progress IS NOT OLD.page_progress AND NEW.page_updated_ms IS OLD.page_updated_ms THEN CAST(round(unixepoch(NEW.progress_updated_at,'subsec')*1000) AS INTEGER) ELSE NEW.page_updated_ms END,
              position_updated_ms=CASE WHEN NEW.video_position IS NOT OLD.video_position AND NEW.position_updated_ms IS OLD.position_updated_ms THEN CAST(round(unixepoch(NEW.progress_updated_at,'subsec')*1000) AS INTEGER) ELSE NEW.position_updated_ms END,
              date_read_ms=CASE WHEN NEW.read_state='read' THEN coalesce(NEW.date_read_ms,CAST(round(unixepoch(NEW.progress_updated_at,'subsec')*1000) AS INTEGER)) ELSE NULL END WHERE rowid=NEW.rowid;END;
            CREATE TRIGGER park_web_new_history AFTER INSERT ON history WHEN NEW.page_count IS NOT NULL
            BEGIN UPDATE chapters SET page_count=NEW.page_count WHERE source=NEW.source AND work_id=NEW.work_id AND chapter_id=NEW.chapter_id
                AND (page_updated_ms IS NULL OR page_updated_ms<=round(unixepoch(NEW.updated_at,'subsec')*1000));END;
            CREATE TRIGGER park_web_restored_history AFTER UPDATE OF page_count ON history WHEN NEW.page_count IS NOT NULL
            BEGIN UPDATE chapters SET page_count=NEW.page_count WHERE source=NEW.source AND work_id=NEW.work_id AND chapter_id=NEW.chapter_id
                AND (page_updated_ms IS NULL OR page_updated_ms<=round(unixepoch(NEW.updated_at,'subsec')*1000));END;")?;
    }
    tx.prepare(&format!("{} LIMIT 0", WEB.chapters))?;
    tx.commit()?;
    Ledger::with_layout(db, &WEB)
}

const WEB:Layout=Layout{
    download_work:false,
    chapters:"SELECT source,work_id,chapter_id,coalesce(page_progress,0),coalesce(page_count,0),page_updated_ms,
        CAST(coalesce(video_position,0) AS INTEGER),CAST(coalesce(duration,0) AS INTEGER),position_updated_ms,read_state='read',date_read_ms FROM chapters",
    work_exists:"SELECT EXISTS(SELECT 1 FROM works WHERE source=? AND work_id=?)",
    set_page:"INSERT INTO chapters(source,work_id,chapter_id,title,sort,page_progress,page_count,page_updated_ms,progress_updated_at,read_state,date_read_ms)
        VALUES(?1,?2,?3,coalesce(?4,''),0,?5,?6,?7,strftime('%Y-%m-%dT%H:%M:%fZ',?7/1000.0,'unixepoch'),CASE WHEN ?8 THEN 'read' ELSE 'reading' END,CASE WHEN ?8 THEN ?7 END)
        ON CONFLICT(source,work_id,chapter_id) DO UPDATE SET page_progress=excluded.page_progress,page_count=excluded.page_count,
        page_updated_ms=excluded.page_updated_ms,progress_updated_at=max(coalesce(chapters.progress_updated_at,''),excluded.progress_updated_at),
        read_state=CASE WHEN chapters.read_state='read' THEN 'read' ELSE excluded.read_state END,date_read_ms=coalesce(chapters.date_read_ms,excluded.date_read_ms),version=chapters.version+1",
    page_history:"INSERT INTO history(source,work_id,updated_at,chapter_id,chapter_title,page,page_count,scope_key)
        VALUES(?1,?2,strftime('%Y-%m-%dT%H:%M:%fZ',?3/1000.0,'unixepoch'),?4,?5,?6,?7,json_array(?1,?2,?4))
        ON CONFLICT(scope_key) DO UPDATE SET updated_at=excluded.updated_at,chapter_title=excluded.chapter_title,page=excluded.page,page_count=excluded.page_count
        WHERE excluded.updated_at>=history.updated_at",
    set_position:"INSERT INTO chapters(source,work_id,chapter_id,title,sort,video_position,duration,position_updated_ms,progress_updated_at,read_state,date_read_ms)
        VALUES(?1,?2,?3,coalesce(?4,''),0,?5,?6,?7,strftime('%Y-%m-%dT%H:%M:%fZ',?7/1000.0,'unixepoch'),CASE WHEN ?8 THEN 'read' ELSE 'reading' END,CASE WHEN ?8 THEN ?7 END)
        ON CONFLICT(source,work_id,chapter_id) DO UPDATE SET video_position=excluded.video_position,duration=CASE WHEN excluded.duration>0 THEN excluded.duration ELSE chapters.duration END,
        position_updated_ms=excluded.position_updated_ms,progress_updated_at=max(coalesce(chapters.progress_updated_at,''),excluded.progress_updated_at),
        read_state=CASE WHEN chapters.read_state='read' THEN 'read' ELSE excluded.read_state END,date_read_ms=coalesce(chapters.date_read_ms,excluded.date_read_ms),version=chapters.version+1",
    position_history:"INSERT INTO history(source,work_id,updated_at,chapter_id,chapter_title,position,scope_key)
        VALUES(?1,?2,strftime('%Y-%m-%dT%H:%M:%fZ',?3/1000.0,'unixepoch'),?4,?5,?6,json_array(?1,?2,?4))
        ON CONFLICT(scope_key) DO UPDATE SET updated_at=excluded.updated_at,chapter_title=excluded.chapter_title,position=excluded.position WHERE excluded.updated_at>=history.updated_at",
};
