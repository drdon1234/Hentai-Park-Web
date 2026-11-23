use crate::{ProgressStore, StorageError};
use rusqlite::{params, TransactionBehavior};
use std::collections::HashSet;

#[derive(uniffi::Record)]
pub struct LibraryCategory {
    pub id: i64,
    pub name: String,
    pub count: i64,
    pub selected: bool,
}

#[derive(uniffi::Record)]
pub struct FavoriteIdentity {
    pub source: String,
    pub work: String,
}

#[derive(uniffi::Record)]
pub struct SimilarFavorite {
    pub source: String,
    pub id: String,
    pub title: String,
}

#[derive(uniffi::Enum)]
pub enum FavoriteEdit {
    MarkRead,
    Remove,
    AddCategory { id: i64 },
}

#[uniffi::export]
impl ProgressStore {
    pub fn similar_favorite(&self, source: String, id: String, title: String) -> Result<Option<SimilarFavorite>, StorageError> {
        let db = self.lock()?;
        Ok(park_storage::library::similar_favorite(&db, &source, &id, &title)?
            .map(|(source, id, title)| SimilarFavorite { source, id, title }))
    }
    pub fn read_chapter_ids(&self, source: String, work: String) -> Result<Vec<String>, StorageError> {
        let db = self.lock()?;
        let mut query = db.prepare("SELECT chapter_id FROM chapters WHERE source=? AND work_id=? AND read=1")?;
        let ids = query.query_map(params![source, work], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }
    pub fn mark_chapters_read(&self, source: String, work: String, chapters: Vec<crate::OfflineChapter>, read: bool) -> Result<(), StorageError> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| StorageError::IoFailure)?.as_millis();
        let now = i64::try_from(now).map_err(|_| StorageError::IoFailure)?;
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut write = tx.prepare("INSERT INTO chapters(source,work_id,chapter_id,title,read,date_read,progress_updated_at)
                VALUES(?1,?2,?3,?4,?5,CASE WHEN ?5 THEN ?6 END,?6)
                ON CONFLICT(source,work_id,chapter_id) DO UPDATE SET read=excluded.read,date_read=excluded.date_read,
                progress_updated_at=excluded.progress_updated_at,last_page=CASE WHEN ?5 THEN last_page ELSE 0 END,
                position=CASE WHEN ?5 THEN position ELSE 0 END,
                position_updated_at=CASE WHEN position_updated_at IS NOT NULL THEN ?6 END")?;
            for chapter in chapters {
                write.execute(params![source, work, chapter.id, chapter.title, read, now])?;
            }
        }
        Ok(tx.commit()?)
    }
    pub fn edit_favorites(&self, works: Vec<FavoriteIdentity>, action: FavoriteEdit) -> Result<(), StorageError> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| StorageError::IoFailure)?.as_millis();
        let now = i64::try_from(now).map_err(|_| StorageError::IoFailure)?;
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for work in works {
            let playable: bool = tx.query_row(
                "SELECT COALESCE(json_extract(json,'$.mediaKind') IN ('voice','video'),0)
                 FROM works WHERE source=? AND id=? AND favorite=1",
                params![work.source,work.work], |r| r.get(0))?;
            match action {
                FavoriteEdit::MarkRead => {
                    set_chapters_read(&tx, &work.source, &work.work, now, playable)?;
                }
                FavoriteEdit::Remove => {
                    tx.execute("UPDATE works SET favorite=0,favorited_at=NULL WHERE source=? AND id=?",
                        params![work.source,work.work])?;
                }
                FavoriteEdit::AddCategory { id } => {
                    tx.query_row("SELECT 1 FROM categories WHERE id=?", [id], |_| Ok(()))?;
                    tx.execute("INSERT OR IGNORE INTO work_categories(source,work_id,category_id) VALUES(?,?,?)",
                        params![work.source,work.work,id])?;
                }
            }
        }
        Ok(tx.commit()?)
    }
    pub fn migrate_favorite(&self, from_source: String, from_id: String, to_source: String, detail_json: String,
        categories: bool, progress: bool, remove: bool) -> Result<bool, StorageError> {
        if detail_json.len() > 4 * 1024 * 1024 { return Err(StorageError::InvalidProgress); }
        let detail = serde_json::from_str(&detail_json).map_err(|_| StorageError::InvalidProgress)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| StorageError::IoFailure)?.as_millis();
        let plan = park_storage::library::migration::Migration {
            from_source: &from_source, from_id: &from_id, to_source: &to_source,
            detail: &detail, categories, progress, remove,
        };
        let mut db = self.lock()?;
        Ok(park_storage::library::migration::apply(&mut db, plan, now.try_into().map_err(|_| StorageError::IoFailure)?)?)
    }

    pub fn mark_favorite_read(&self, source: String, work: String, playable: bool) -> Result<(), StorageError> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| StorageError::IoFailure)?.as_millis();
        let now = i64::try_from(now).map_err(|_| StorageError::IoFailure)?;
        let db = self.lock()?;
        set_chapters_read(&db, &source, &work, now, playable)?;
        Ok(())
    }

    pub fn favorite_card_stats(&self) -> Result<String, StorageError> {
        let db = self.lock()?;
        let mut query = db.prepare("SELECT w.source||':'||w.id,COALESCE(w.favorited_at,0),w.one_shot,
            COALESCE(MAX(c.date_fetch),0),COUNT(CASE WHEN c.read=0 THEN 1 END),
            (SELECT COALESCE(json_extract(d.json,'$.status')=3 OR json_extract(d.json,'$.done')>0,0)
             FROM download_tasks d WHERE d.uid=w.source||':'||w.id AND json_valid(d.json)),
            CASE WHEN json_valid(w.json) THEN CASE WHEN json_type(w.json,'$.cover')='text'
                THEN json_extract(w.json,'$.cover') END END
            FROM works w LEFT JOIN chapters c ON c.source=w.source AND c.work_id=w.id
            WHERE w.favorite=1 GROUP BY w.source,w.id")?;
        let rows = query.query_map([], |r| Ok((r.get::<_,String>(0)?,serde_json::json!({
            "added":r.get::<_,i64>(1)?, "oneShot":r.get::<_,bool>(2)?,"updated":r.get::<_,i64>(3)?,
            "unread":r.get::<_,i64>(4)?,"downloaded":r.get::<_,Option<bool>>(5)?.unwrap_or(false),
            "cover":r.get::<_,Option<String>>(6)?
        }))))?.collect::<rusqlite::Result<serde_json::Map<_,_>>>()?;
        Ok(serde_json::Value::Object(rows).to_string())
    }
    pub fn library_categories(
        &self,
        source: Option<String>,
        work: Option<String>,
    ) -> Result<Vec<LibraryCategory>, StorageError> {
        let db = self.lock()?;
        let mut query = db.prepare(
            "SELECT c.id,c.name,COUNT(w.category_id),
            COUNT(CASE WHEN w.source=? AND w.work_id=? THEN 1 END)>0
            FROM categories c LEFT JOIN work_categories w ON w.category_id=c.id
            GROUP BY c.id ORDER BY c.sort,c.id",
        )?;
        let rows = query.query_map(params![source, work], |r| {
            Ok(LibraryCategory {
                id: r.get(0)?,
                name: r.get(1)?,
                count: r.get(2)?,
                selected: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn name_category(&self, id: Option<i64>, name: String) -> Result<(), StorageError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(StorageError::InvalidProgress);
        }
        let db = self.lock()?;
        let changed = match id {
            Some(id) => db.execute("UPDATE OR IGNORE categories SET name=? WHERE id=?",params![name,id])?,
            None => db.execute("INSERT OR IGNORE INTO categories(name,sort) SELECT ?,COALESCE(MAX(sort),-1)+1 FROM categories",[name])?,
        };
        if changed != 1 {
            return Err(StorageError::Conflict);
        }
        Ok(())
    }

    pub fn remove_category(&self, id: i64) -> Result<(), StorageError> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM work_categories WHERE category_id=?", [id])?;
        tx.execute("DELETE FROM categories WHERE id=?", [id])?;
        Ok(tx.commit()?)
    }

    pub fn reorder_categories(&self, ordered: Vec<i64>) -> Result<(), StorageError> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: usize = tx.query_row("SELECT COUNT(*) FROM categories", [], |r| r.get(0))?;
        if count != ordered.len() || ordered.iter().collect::<HashSet<_>>().len() != count {
            return Err(StorageError::Conflict);
        }
        for (sort, id) in ordered.iter().enumerate() {
            if tx.execute("UPDATE categories SET sort=? WHERE id=?", params![sort, id])? != 1 {
                return Err(StorageError::Conflict);
            }
        }
        Ok(tx.commit()?)
    }

    pub fn favorite_with_categories(
        &self,
        source: String,
        summary: String,
        categories: Vec<i64>,
    ) -> Result<bool, StorageError> {
        if summary.len() > 1024 * 1024 {
            return Err(StorageError::InvalidProgress);
        }
        let item = serde_json::from_str(&summary).map_err(|_| StorageError::InvalidProgress)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| StorageError::IoFailure)?
            .as_millis();
        let mut db = self.lock()?;
        Ok(park_storage::library::save_classified_summary(
            &mut db,
            &source,
            &item,
            Some(true),
            now.try_into().map_err(|_| StorageError::IoFailure)?,
            Some(&categories),
        )?)
    }
}

fn set_chapters_read(db: &rusqlite::Connection, source: &str, work: &str, now: i64, playable: bool) -> rusqlite::Result<()> {
    db.execute("UPDATE chapters SET read=1,date_read=?3,progress_updated_at=?3,
        position_updated_at=CASE WHEN ?4 OR position_updated_at IS NOT NULL THEN ?3 ELSE NULL END
        WHERE source=?1 AND work_id=?2 AND EXISTS
        (SELECT 1 FROM works WHERE source=?1 AND id=?2 AND favorite=1)",
        params![source, work, now, playable])?;
    Ok(())
}
