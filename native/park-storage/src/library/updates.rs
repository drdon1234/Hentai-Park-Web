use super::upsert_summary;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json,Value};
use std::{collections::{HashMap,HashSet},path::Path};
use sha2::{Digest, Sha256};

pub enum ChapterQueue<'a> {
    Library(&'a Path, &'a str),
    Windows(&'a Path),
}

pub fn view(db:&Connection,category:Option<i64>)->rusqlite::Result<Value>{
    let mut categories=db.prepare("SELECT id,name FROM categories ORDER BY sort,id")?;
    let groups=categories.query_map([],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"name":r.get::<_,String>(1)?})))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut entries=db.prepare("SELECT w.source,w.id,w.title,c.chapter_id,c.title,c.read,c.last_page,c.page_count,c.date_fetch,
        CASE WHEN json_valid(w.json) THEN json_extract(w.json,'$.mediaKind') END FROM chapters c
        JOIN works w ON w.source=c.source AND w.id=c.work_id WHERE w.favorite=1 AND c.date_fetch>COALESCE(w.favorited_at,0)
        AND (?1 IS NULL OR EXISTS(SELECT 1 FROM work_categories g WHERE g.source=w.source AND g.work_id=w.id AND g.category_id=?1))
        ORDER BY c.date_fetch DESC LIMIT 300")?;
    let rows=entries.query_map([category],|r|Ok(json!({"source":r.get::<_,String>(0)?,"id":r.get::<_,String>(1)?,
        "title":r.get::<_,String>(2)?,"chapter":r.get::<_,String>(3)?,"label":r.get::<_,String>(4)?,
        "read":r.get::<_,bool>(5)?,"page":r.get::<_,i64>(6)?,"pages":r.get::<_,i64>(7)?,"fetched":r.get::<_,i64>(8)?,"media":r.get::<_,Option<String>>(9)?})))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"categories":groups,"items":rows}))
}

pub fn targets(db:&Connection,category:Option<i64>,settings:&Value)->rusqlite::Result<Vec<Value>> {
    let mut query=db.prepare("SELECT w.source,w.id,w.json,COUNT(c.chapter_id) AS total FROM works w
        LEFT JOIN chapters c ON c.source=w.source AND c.work_id=w.id
        WHERE w.favorite=1 AND (?1 IS NULL OR (?1=0 AND NOT EXISTS(SELECT 1 FROM work_categories wc
        WHERE wc.source=w.source AND wc.work_id=w.id)) OR EXISTS(SELECT 1 FROM work_categories wc
        WHERE wc.source=w.source AND wc.work_id=w.id AND wc.category_id=?1))
        AND (?2=0 OR w.one_shot=0) GROUP BY w.source,w.id
        HAVING (?3=0 OR COUNT(c.chapter_id)=COALESCE(SUM(c.read),0))
        AND (?4=0 OR COALESCE(SUM(c.read),0)>0 OR EXISTS(SELECT 1 FROM history h WHERE h.source=w.source AND h.work_id=w.id))
        ORDER BY w.source,w.favorited_at DESC,w.id")?;
    let flag=|key:&str,default|settings[key].as_bool().unwrap_or(default);
    let rows=query.query_map(params![category,flag("updateOnlyMultiChapter",true),flag("updateSkipUnread",false),flag("updateSkipNotStarted",false)],|r|{
        let raw:String=r.get(2)?;
        let item:Value=serde_json::from_str(&raw).map_err(|_|rusqlite::Error::InvalidQuery)?;
        Ok(json!({"source":r.get::<_,String>(0)?,"id":r.get::<_,String>(1)?,"item":item,"count":r.get::<_,i64>(3)?}))
    })?;
    rows.collect()
}

/// 同一事务比较旧章节、更新书目并登记自动下载。失败后再次检查仍可发现同一批新章。
pub fn reconcile(db:&mut Connection,source:&str,item:&Value,chapters:Option<&[(String,String)]>,
                 one_shot:bool,complete:bool,download:Option<ChapterQueue<'_>>,now:i64)->rusqlite::Result<Vec<Value>> {
    let id=item["id"].as_str().ok_or(rusqlite::Error::InvalidQuery)?;
    let tx=db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let favorite=tx.query_row("SELECT favorite FROM works WHERE source=? AND id=?",params![source,id],|r|r.get::<_,bool>(0)).optional()?;
    if favorite!=Some(true){return Ok(Vec::new())}
    upsert_summary(&tx,source,item)?;
    let mut fresh=Vec::new();
    if let Some(chapters)=chapters {
        let existing:HashMap<String,i64>={
            let mut q=tx.prepare("SELECT chapter_id,sort FROM chapters WHERE source=? AND work_id=?")?;
            let rows=q.query_map(params![source,id],|r|Ok((r.get(0)?,r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut next_sort=existing.values().copied().max().unwrap_or(-1)+1;
        let mut seen=HashSet::new();
        for (index,(chapter,title)) in chapters.iter().enumerate() {
            if !seen.insert(chapter.as_str()) {continue}
            let sort=index as i64;
            if let Some(old_sort)=existing.get(chapter){
                tx.execute("UPDATE chapters SET title=?4,sort=?5 WHERE source=?1 AND work_id=?2 AND chapter_id=?3
                    AND (title!=?4 OR sort!=?5)",params![source,id,chapter,title,if complete{sort}else{*old_sort}])?;
            } else {
                let fetched=if !complete{0}else if existing.is_empty(){sort}else{now+sort};
                tx.execute("INSERT INTO chapters(source,work_id,chapter_id,title,sort,date_fetch) VALUES(?,?,?,?,?,?)",
                    params![source,id,chapter,title,if complete{sort}else{next_sort},fetched])?;
                next_sort+=1;
                if complete && !existing.is_empty(){fresh.push(json!({"id":chapter,"title":title}));}
            }
        }
        if complete {
            for chapter in existing.keys().filter(|k|!seen.contains(k.as_str())) {
                tx.execute("DELETE FROM chapters WHERE source=? AND work_id=? AND chapter_id=? AND read=0 AND last_page=0
                    AND progress_updated_at IS NULL AND position_updated_at IS NULL",params![source,id,chapter])?;
            }
            tx.execute("UPDATE works SET one_shot=? WHERE source=? AND id=?",params![one_shot,source,id])?;
        }
        match download.filter(|_|!fresh.is_empty()) {
          Some(ChapterQueue::Windows(root)) => {
            if !root.is_absolute() || !crate::recovery::valid_key(source) {
                return Err(rusqlite::Error::InvalidQuery)
            }
            for chapter in &fresh {
                let chapter_id = chapter["id"].as_str().ok_or(rusqlite::Error::InvalidQuery)?;
                let uid = format!("{source}:{id}:{chapter_id}");
                let folder = format!("{:x}", Sha256::digest(uid.as_bytes()));
                let path = root.join(source).join(&folder[..24]);
                let payload = json!({"status":"Queued","work":item,"chapterId":chapter_id,
                    "chapterTitle":chapter["title"],"completed":0,"total":0});
                tx.execute("INSERT INTO download_tasks(uid,source,json,dir,created_at) VALUES(?,?,?,?,?)
                    ON CONFLICT(uid) DO NOTHING",params![uid,source,payload.to_string(),path.to_str().ok_or(rusqlite::Error::InvalidQuery)?,now])?;
            }
          }
          Some(ChapterQueue::Library(root, media)) => {
            crate::downloads::enqueue_chapters(&tx,source,item,&fresh,root,media,None,now).map_err(|error| match error {
                crate::downloads::Error::Database(db) => db,
                _ => rusqlite::Error::InvalidQuery,
            })?;
          }
          None => {}
        }
    }
    tx.execute("UPDATE works SET last_update_check=? WHERE source=? AND id=?",params![now,source,id])?;
    tx.commit()?;
    Ok(fresh)
}
