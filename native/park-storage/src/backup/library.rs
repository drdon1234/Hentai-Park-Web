use super::{Error, Result};
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::{json, Value};

pub(super) fn rows<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    if !value.is_object() {
        return Err(Error::Invalid);
    }
    match value.get(field) {
        None => Ok(&[]),
        Some(Value::Array(a)) => Ok(a),
        _ => Err(Error::Invalid),
    }
}
pub(super) fn required<'a>(row: &'a Value, name: &str) -> Result<&'a str> {
    row[name]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(Error::Invalid)
}
fn text<'a>(row: &'a Value, name: &str) -> Result<Option<&'a str>> {
    match &row[name] {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s)),
        _ => Err(Error::Invalid),
    }
}
fn int(row: &Value, name: &str) -> Result<Option<i64>> {
    let v = &row[name];
    if v.is_null() {
        return Ok(None);
    }
    v.as_i64()
        .filter(|i| (0..=8640000000000000).contains(i))
        .map(Some)
        .ok_or(Error::Invalid)
}
fn flag(row: &Value, name: &str) -> Result<bool> {
    if row[name].is_null() {
        return Ok(false);
    }
    row[name].as_bool().ok_or(Error::Invalid)
}
fn work(tx: &Transaction<'_>, row: &Value, source: &str, id: &str) -> Result<()> {
    let mut item = match &row["item"] {
        Value::Null => json!({}),
        v if v.is_object() => v.clone(),
        _ => return Err(Error::Invalid),
    };
    item["source"] = source.into();
    item["id"] = id.into();
    tx.execute("INSERT INTO works(source,id,title,cover,json) VALUES(?,?,?,?,?) ON CONFLICT(source,id) DO NOTHING",
        params![source,id,item["title"].as_str().unwrap_or(""),item["cover"].as_str().unwrap_or(""),item.to_string()])?;
    Ok(())
}
fn merge_chapter(tx: &Transaction<'_>, row: &Value) -> Result<bool> {
    let keys = [
        required(row, "source")?,
        required(row, "workId")?,
        required(row, "chapterId")?,
    ];
    let title = text(row, "title")?.unwrap_or("");
    let sort = int(row, "sort")?.unwrap_or(0);
    let read = flag(row, "read")?;
    let page = int(row, "lastPage")?.unwrap_or(0);
    let count = int(row, "pageCount")?.unwrap_or(0);
    let pt = int(row, "progressUpdatedAt")?;
    let pos = int(row, "position")?.unwrap_or(0);
    let duration = int(row, "duration")?.unwrap_or(0);
    let vt = int(row, "positionUpdatedAt")?;
    let date = int(row, "dateRead")?;
    let current=tx.query_row("SELECT progress_updated_at,position_updated_at FROM chapters WHERE source=? AND work_id=? AND chapter_id=?",
        keys,|r|Ok((r.get::<_,Option<i64>>(0)?,r.get::<_,Option<i64>>(1)?))).optional()?;
    if let Some((old_page, old_video)) = current {
        let newer_page = pt > old_page;
        let newer_position = vt > old_video;
        if !newer_page && !newer_position {
            return Ok(false);
        }
        let newer_read = pt.max(vt) > old_page.max(old_video);
        tx.execute("UPDATE chapters SET last_page=iif(?4,?5,last_page),page_count=iif(?4,?6,page_count),
            progress_updated_at=iif(?4,?7,progress_updated_at),position=iif(?8,?9,position),
            duration=iif(?8 AND ?10>0,?10,duration),position_updated_at=iif(?8,?11,position_updated_at),
            read=iif(?12,?13,read),date_read=iif(?12,?14,date_read) WHERE source=?1 AND work_id=?2 AND chapter_id=?3",
            params![keys[0],keys[1],keys[2],newer_page,page,count,pt,newer_position,pos,duration,vt,newer_read,read,if read {date}else{None}])?;
    } else {
        tx.execute("INSERT INTO chapters(source,work_id,chapter_id,title,sort,read,last_page,page_count,progress_updated_at,
            date_read,position,duration,position_updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![keys[0],keys[1],keys[2],title,sort,read,page,count,pt,if vt.is_some() && !read {None}else{date},pos,duration,vt])?;
    }
    Ok(true)
}
pub fn merge_library(
    tx: &Transaction<'_>,
    library: &Value,
    cancelled: &impl Fn() -> bool,
) -> Result<bool> {
    let mut sort: i64 = tx.query_row("SELECT count(*) FROM categories", [], |r| r.get(0))?;
    for row in rows(library, "categories")? {
        let name = required(row, "name")?;
        int(row, "sort")?;
        sort += tx.execute(
            "INSERT INTO categories(name,sort) VALUES(?,?) ON CONFLICT(name) DO NOTHING",
            params![name, sort],
        )? as i64;
    }
    for row in rows(library, "works")? {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let source = required(row, "source")?;
        let id = required(row, "id")?;
        let fav = flag(row, "favorite")?;
        let date = int(row, "favoritedAt")?;
        let mode = int(row, "readerMode")?;
        if mode.is_some_and(|v| v > 2) {
            return Err(Error::Invalid);
        }
        work(tx, row, source, id)?;
        tx.execute("UPDATE works SET favorite=(favorite OR ?3), favorited_at=iif(favorite,favorited_at,coalesce(?4,favorited_at)),
            reader_mode=coalesce(reader_mode,?5) WHERE source=?1 AND id=?2",params![source,id,fav,date,mode])?;
        for category in rows(row, "categories")? {
            tx.execute("INSERT OR IGNORE INTO work_categories(source,work_id,category_id) SELECT ?,?,id FROM categories WHERE name=?",
                params![source,id,category.as_str().ok_or(Error::Invalid)?])?;
        }
    }
    let mut changed = false;
    for row in rows(library, "chapters")? {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        changed |= merge_chapter(tx, row)?;
    }
    for row in rows(library, "history")? {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let source = required(row, "source")?;
        let id = required(row, "workId")?;
        work(tx, row, source, id)?;
        tx.execute("INSERT INTO history(source,work_id,time,chapter_id,chapter_title,page,page_count,position) VALUES(?,?,?,?,?,?,?,?)
            ON CONFLICT(source,work_id) DO UPDATE SET time=excluded.time,chapter_id=excluded.chapter_id,chapter_title=excluded.chapter_title,
            page=excluded.page,page_count=excluded.page_count,position=excluded.position WHERE excluded.time>history.time",
            params![source,id,int(row,"time")?.unwrap_or(0),text(row,"chapterId")?,text(row,"chapterTitle")?,
            int(row,"page")?.unwrap_or(0),int(row,"pageCount")?.unwrap_or(0),int(row,"position")?])?;
    }
    Ok(changed)
}
