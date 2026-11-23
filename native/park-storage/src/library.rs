use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;
pub mod updates;
pub mod migration;

/// 详情页只持久化书目字段。正文、临时播放地址和插件私有对象不写入书库。
pub fn save_summary(
    db: &mut Connection,
    source: &str,
    detail: &Value,
    favorite: Option<bool>,
    now: i64,
) -> rusqlite::Result<bool> {
    save_classified_summary(db, source, detail, favorite, now, None)
}

pub fn save_classified_summary(
    db: &mut Connection,
    source: &str,
    detail: &Value,
    favorite: Option<bool>,
    now: i64,
    categories: Option<&[i64]>,
) -> rusqlite::Result<bool> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = upsert_summary(&tx, source, detail)?;
    match favorite {
        Some(value) => {
            tx.execute("UPDATE works SET favorite=?,favorited_at=? WHERE source=? AND id=? AND favorite!=?",params![value,value.then_some(now),source,id,value])?;
        }
        None => {
            tx.execute("INSERT INTO history(source,work_id,time) VALUES(?,?,?) ON CONFLICT(source,work_id) DO UPDATE SET time=MAX(time,excluded.time)",params![source,id,now])?;
        }
    }
    if let Some(categories) = categories {
        tx.execute("DELETE FROM work_categories WHERE source=? AND work_id=?", params![source,id])?;
        for category in categories {
            if tx.execute("INSERT INTO work_categories(source,work_id,category_id)
                SELECT ?,?,id FROM categories WHERE id=?", params![source,id,category])? != 1 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
        }
    }
    let current = tx.query_row(
        "SELECT favorite FROM works WHERE source=? AND id=?",
        params![source, id],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(current)
}

fn upsert_summary<'a>(tx:&Transaction<'_>,source:&str,detail:&'a Value) -> rusqlite::Result<&'a str> {
    let id = detail["id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or(rusqlite::Error::InvalidQuery)?;
    let title = detail["title"]
        .as_str()
        .filter(|v| !v.trim().is_empty())
        .ok_or(rusqlite::Error::InvalidQuery)?;
    let previous: Option<String> = tx
        .query_row(
            "SELECT json FROM works WHERE source=? AND id=?",
            params![source, id],
            |r| r.get(0),
        )
        .optional()?;
    let mut item = match previous {
        Some(v) => serde_json::from_str::<Value>(&v).map_err(|_| rusqlite::Error::InvalidQuery)?,
        None => serde_json::json!({}),
    };
    if !item.is_object() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    for key in [
        "id",
        "title",
        "cover",
        "coverImages",
        "coverAspect",
        "coverBlurred",
        "uploaded",
        "tags",
        "authors",
        "subtitle",
        "mediaKind",
        "contentDomain",
        "singleChapterId",
        "voiceNodeType",
        "workCode",
        "editions",
    ] {
        if let Some(value) = detail.get(key).filter(|v| !v.is_null()) {
            item[key] = value.clone();
        }
    }
    if detail["mediaKind"] == "voice" && detail["chaptersComplete"] == false && detail["chapters"].is_array() {
        for key in ["chapters","chaptersComplete","voiceTrackKinds","voiceSubtitles"] {
            if let Some(value)=detail.get(key){item[key]=value.clone()}
        }
    }
    item["source"] = source.into();
    tx.execute("INSERT INTO works(source,id,title,cover,json) VALUES(?,?,?,?,?) ON CONFLICT(source,id) DO UPDATE SET title=excluded.title,cover=excluded.cover,json=excluded.json",
        params![source,id,title,item["cover"].as_str().unwrap_or(""),item.to_string()])?;
    Ok(id)
}
