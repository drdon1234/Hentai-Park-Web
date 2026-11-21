use park_domain::tavern::encode_resource;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database")]
    Database(#[from] rusqlite::Error),
    #[error("invalid_resource")]
    Invalid,
    #[error("missing_resource")]
    Missing,
    #[error("edit_conflict")]
    Conflict,
}
type Result<T> = std::result::Result<T, Error>;
pub struct Entry {
    pub id: String,
    pub kind: String,
    pub title: String,
}
pub struct Editor {
    pub key: String,
    pub content: String,
    pub recovered: bool,
}

pub fn catalog(db: &Connection) -> Result<Vec<Entry>> {
    let mut query=db.prepare("SELECT uid,kind,COALESCE(json_extract(item_json,'$.title'),uid) FROM tavern_resources
        UNION ALL SELECT id,'session',json_extract(data_json,'$.title') FROM tavern_sessions ORDER BY 3,1")?;
    let rows = query.query_map([], |r| {
        Ok(Entry {
            id: r.get(0)?,
            kind: r.get(1)?,
            title: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
pub struct Asset {
    pub kind: String,
    pub format: String,
    pub card: String,
    pub bytes: Vec<u8>,
}
pub fn saved_resource(db: &Connection, uid: &str) -> Result<Option<Asset>> {
    Ok(db
        .query_row(
            "SELECT kind,format,card_json,original FROM tavern_resources WHERE uid=?1",
            [uid],
            |row| {
                Ok(Asset {
                    kind: row.get(0)?,
                    format: row.get(1)?,
                    card: row.get(2)?,
                    bytes: row.get(3)?,
                })
            },
        )
        .optional()?)
}

pub fn save_source_resource(
    db: &Connection,
    item: &Value,
    kind: &str,
    format: &str,
    card: &Value,
    bytes: &[u8],
    now: i64,
) -> Result<String> {
    let source = item["source"].as_str().ok_or(Error::Invalid)?;
    let work = item["id"].as_str().ok_or(Error::Invalid)?;
    let uid = format!("{source}:{work}");
    // 已经保存的卡片可能正在被其他窗口编辑。只有首次保存才插入原始快照。
    db.execute(
        "INSERT INTO tavern_resources(uid,item_json,kind,format,card_json,original,saved_at)
        VALUES(?,?,?,?,?,?,?) ON CONFLICT(uid) DO NOTHING",
        params![
            uid,
            item.to_string(),
            kind,
            format,
            card.to_string(),
            bytes,
            now
        ],
    )?;
    Ok(uid)
}

pub fn resource_sessions(db: &Connection, uid: &str) -> Result<Vec<Entry>> {
    let mut query = db.prepare("SELECT id,COALESCE(json_extract(data_json,'$.title'),'') FROM tavern_sessions WHERE resource_uid=? ORDER BY updated_at DESC,id")?;
    let entries = query
        .query_map([uid], |row| {
            Ok(Entry {
                id: row.get(0)?,
                kind: "session".into(),
                title: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(entries)
}
pub fn import_card(db: &mut Connection, bytes: &[u8], now: i64) -> Result<String> {
    let format = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else {
        "json"
    };
    let (kind, raw) = ["character", "lorebook"]
        .into_iter()
        .find_map(|kind| {
            park_domain::tavern::parse_resource(kind, format, bytes)
                .ok()
                .map(|raw| (kind, raw))
        })
        .ok_or(Error::Invalid)?;
    let fields = raw.get("data").filter(|v| v.is_object()).unwrap_or(&raw);
    let title = fields["name"].as_str().unwrap_or_default();
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id: String = tx.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
    let uid = format!("tavern-local:{id}");
    let item = json!({"source":"tavern-local","id":id,"mediaKind":"tavern","tavernKind":kind,"title":title});
    tx.execute(
        "INSERT INTO tavern_resources VALUES(?,?,?,?,?,?,?)",
        params![
            uid,
            item.to_string(),
            kind,
            format,
            raw.to_string(),
            bytes,
            now
        ],
    )?;
    tx.commit()?;
    Ok(uid)
}
impl Asset {
    fn key(&self, uid: &str) -> Result<String> {
        let raw: Value = serde_json::from_str(&self.card).map_err(|_| Error::Invalid)?;
        Ok(format!(
            "asset:{uid}:{:x}:{:x}",
            Sha256::digest(&self.bytes),
            Sha256::digest(raw.to_string().as_bytes())
        ))
    }
}
fn asset(db: &Connection, uid: &str) -> Result<Asset> {
    saved_resource(db, uid)?.ok_or(Error::Missing)
}
fn blank(kind: &str) -> Result<Value> {
    match kind {
        "character" => Ok(json!({"spec":"chara_card_v2","spec_version":"2.0","data":{
            "name":"","description":"","personality":"","scenario":"","first_mes":"","mes_example":"","creator_notes":"",
            "system_prompt":"","post_history_instructions":"","creator":"","character_version":"","alternate_greetings":[],"tags":[],"extensions":{}}})),
        "lorebook" => Ok(json!({"name":"","extensions":{},"entries":[]})),
        _ => Err(Error::Invalid),
    }
}
pub fn editor(db: &Connection, uid: Option<&str>, kind: &str, private: bool) -> Result<Editor> {
    let (key, base) = if let Some(uid) = uid {
        let saved = asset(db, uid)?;
        if saved.kind != kind {
            return Err(Error::Invalid);
        }
        (saved.key(uid)?, saved.card)
    } else {
        (format!("new:{kind}"), blank(kind)?.to_string())
    };
    let draft = if private {
        None
    } else {
        db.query_row(
            "SELECT content FROM tavern_drafts WHERE id=?",
            [&key],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    };
    Ok(Editor {
        key,
        recovered: draft.is_some(),
        content: draft.unwrap_or(base),
    })
}

pub fn write_draft(
    db: &Connection,
    key: &str,
    content: &str,
    previous: Option<&str>,
) -> Result<()> {
    if !serde_json::from_str::<Value>(content).is_ok_and(|v| v.is_object()) {
        return Err(Error::Invalid);
    }
    let changed = if let Some(old) = previous {
        db.execute(
            "UPDATE tavern_drafts SET content=? WHERE id=? AND content=?",
            params![content, key, old],
        )?
    } else {
        db.execute(
            "INSERT INTO tavern_drafts(id,content) VALUES(?,?) ON CONFLICT DO NOTHING",
            params![key, content],
        )?
    };
    if changed == 0 {
        return Err(Error::Conflict);
    }
    Ok(())
}

pub fn save_card(
    db: &mut Connection,
    uid: Option<&str>,
    kind: &str,
    key: &str,
    content: &str,
    private: bool,
    now: i64,
) -> Result<String> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let raw: Value = serde_json::from_str(content).map_err(|_| Error::Invalid)?;
    let saved = uid.map(|id| asset(&tx, id)).transpose()?;
    if let (Some(id), Some(a)) = (uid, &saved) {
        if a.kind != kind || a.key(id)? != key {
            return Err(Error::Conflict);
        }
    } else if key != format!("new:{kind}") {
        return Err(Error::Invalid);
    }
    let draft: Option<(String, Option<Vec<u8>>)> = if private {
        None
    } else {
        tx.query_row(
            "SELECT content,portrait FROM tavern_drafts WHERE id=?",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    };
    if let Some((original, _)) = &draft {
        if original != content {
            return Err(Error::Conflict);
        }
    }
    let image = draft.as_ref().and_then(|(_, p)| p.as_deref()).or_else(|| {
        saved
            .as_ref()
            .filter(|a| a.format == "png")
            .map(|a| a.bytes.as_slice())
    });
    let bytes = encode_resource(kind, image, &raw).map_err(|_| Error::Invalid)?;
    let format = if image.is_some() { "png" } else { "json" };
    let fields = raw.get("data").filter(|v| v.is_object()).unwrap_or(&raw);
    let title = fields["name"].as_str().unwrap_or("").trim();
    let id = match uid {
        Some(id) => {
            tx.execute("UPDATE tavern_resources SET item_json=CASE WHEN ?1='' THEN item_json ELSE json_set(item_json,'$.title',?1) END,
                card_json=?2,original=?3,saved_at=?4,format=?5 WHERE uid=?6",params![title,raw.to_string(),bytes,now,format,id])?;
            id.to_owned()
        }
        None => {
            let id: String = tx.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
            let uid = format!("tavern-local:{id}");
            let item = json!({"source":"tavern-local","id":id,"mediaKind":"tavern","tavernKind":kind,"title":title});
            tx.execute(
                "INSERT INTO tavern_resources VALUES(?,?,?,?,?,?,?)",
                params![
                    uid,
                    item.to_string(),
                    kind,
                    format,
                    raw.to_string(),
                    bytes,
                    now
                ],
            )?;
            uid
        }
    };
    if !private {
        tx.execute("DELETE FROM tavern_drafts WHERE id=?", [key])?;
    }
    tx.commit()?;
    Ok(id)
}

pub fn session(db: &Connection, id: &str) -> Result<String> {
    db.query_row(
        "SELECT data_json FROM tavern_sessions WHERE id=?",
        [id],
        |r| r.get(0),
    )
    .optional()?
    .ok_or(Error::Missing)
}
fn visit(tx: &Connection, id: &str, data: &Value, now: i64) -> Result<()> {
    let title = data["title"].as_str().ok_or(Error::Invalid)?;
    let work = json!({"source":"tavern-session","id":id,"title":title,"contentDomain":"tavern","mediaKind":"tavern"});
    tx.execute("INSERT INTO works(source,id,title,json) VALUES('tavern-session',?,?,?) ON CONFLICT(source,id) DO UPDATE SET title=excluded.title",
        params![id,title,work.to_string()])?;
    tx.execute("INSERT INTO history(source,work_id,time) VALUES('tavern-session',?,?) ON CONFLICT(source,work_id) DO UPDATE SET time=excluded.time",params![id,now])?;
    Ok(())
}
pub fn resource(db: &Connection, id: &str, kind: &str) -> Result<Value> {
    let saved = asset(db, id)?;
    if saved.kind != kind {
        return Err(Error::Invalid);
    }
    serde_json::from_str(&saved.card).map_err(|_| Error::Invalid)
}
pub fn updated_session(
    db: &mut Connection,
    old: Option<&str>,
    data: &mut Value,
    now: i64,
    private: bool,
) -> Result<String> {
    data["updatedAt"] = now.into();
    let id = data["id"].as_str().ok_or(Error::Invalid)?;
    let uid = data["resourceUid"].as_str().ok_or(Error::Invalid)?;
    let serialized = data.to_string();
    if private {
        return Ok(serialized);
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(old) = old {
        if tx.execute(
            "UPDATE tavern_sessions SET data_json=?,updated_at=? WHERE id=? AND data_json=?",
            params![serialized, now, id, old],
        )? != 1
        {
            return Err(Error::Conflict);
        }
    } else {
        tx.execute(
            "INSERT INTO tavern_sessions(id,resource_uid,data_json,updated_at) VALUES(?,?,?,?)",
            params![id, uid, serialized, now],
        )?;
    }
    visit(&tx, id, data, now)?;
    tx.commit()?;
    Ok(serialized)
}
pub fn delete_session(db: &mut Connection, id: &str) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM tavern_sessions WHERE id=?", [id])?;
    tx.execute(
        "DELETE FROM history WHERE source='tavern-session' AND work_id=?",
        [id],
    )?;
    tx.commit()?;
    Ok(())
}
