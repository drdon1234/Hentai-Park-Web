use super::{
    library::{required, rows},
    Error, Result,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use park_domain::tavern::{parse_resource, valid_content};
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};

// 同一份备份重试时应找回已建的冲突副本；不修改已有资源或分支的标识。
fn copy_id(parts: &[&[u8]]) -> String {
    let mut sha = Sha256::new();
    for p in parts {
        sha.update((p.len() as u64).to_be_bytes());
        sha.update(p);
    }
    let bytes = sha.finalize();
    bytes[..16].iter().map(|b| format!("{b:02x}")).collect()
}
fn saved(row: &Value, key: &str) -> Result<i64> {
    row[key].as_i64().filter(|i| *i >= 0).ok_or(Error::Invalid)
}
struct Resource {
    uid: String,
    kind: String,
}
fn resource(tx: &Transaction<'_>, row: &Value) -> Result<(String, Resource)> {
    let mut item = row["item"].clone();
    let source = required(&item, "source")?.to_owned();
    let id = required(&item, "id")?;
    let old = format!("{source}:{id}");
    let mut uid = old.clone();
    let kind = required(row, "kind")?;
    let format = required(row, "format")?;
    let original = required(row, "original")?;
    if original.len() > 16 * 1024 * 1024 {
        return Err(Error::Invalid);
    }
    let bytes = STANDARD.decode(original).map_err(|_| Error::Invalid)?;
    let card = parse_resource(kind, format, &bytes).map_err(|_| Error::Invalid)?;
    let existing = |uid: &str| -> Result<Option<bool>> {
        let state = tx
            .query_row(
                "SELECT format,card_json,original FROM tavern_resources WHERE uid=?",
                [uid],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?;
        state
            .map(|(f, j, b)| {
                Ok(f == format && b == bytes && serde_json::from_str::<Value>(&j)? == card)
            })
            .transpose()
    };
    if existing(&old)? == Some(false) {
        let id = format!(
            "local-{}",
            copy_id(&[old.as_bytes(), format.as_bytes(), &bytes])
        );
        uid = format!("{source}:{id}");
        if existing(&uid)? == Some(false) {
            return Err(Error::Invalid);
        }
        item["id"] = id.into();
        item["mediaKind"] = "tavern".into();
    }
    tx.execute("INSERT INTO tavern_resources(uid,item_json,kind,format,card_json,original,saved_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(uid) DO NOTHING",
        params![uid,item.to_string(),kind,format,card.to_string(),bytes,saved(row,"savedAt")?])?;
    Ok((
        old,
        Resource {
            uid,
            kind: kind.to_owned(),
        },
    ))
}
fn validate_session(row: &Value) -> Result<()> {
    required(row, "id")?;
    required(row, "resourceUid")?;
    saved(row, "updatedAt")?;
    if !row["title"].is_string()
        || !row["books"].is_array()
        || !valid_content("character", &row["card"])
    {
        return Err(Error::Invalid);
    }
    let messages = row["messages"].as_array().ok_or(Error::Invalid)?;
    if messages.iter().any(|m| {
        !matches!(m["role"].as_str(), Some("assistant" | "user")) || !m["content"].is_string()
    }) {
        return Err(Error::Invalid);
    }
    if let Some(a) = row.get("alternatives").filter(|a| !a.is_null()) {
        for (k, v) in a.as_object().ok_or(Error::Invalid)? {
            if k.parse::<usize>().ok().is_none_or(|i| i >= messages.len()) {
                return Err(Error::Invalid);
            }
            for m in v.as_array().ok_or(Error::Invalid)? {
                if m["role"] != "assistant"
                    || !m["content"].is_string()
                    || !m
                        .as_object()
                        .is_some_and(|o| o.values().all(Value::is_string))
                {
                    return Err(Error::Invalid);
                }
            }
        }
    }
    if row.get("cast").filter(|v| !v.is_null()).is_some_and(|v| {
        !v.as_object()
            .is_some_and(|o| o.len() <= 20 && o.values().all(Value::is_object))
    }) {
        return Err(Error::Invalid);
    }
    Ok(())
}
pub fn merge_tavern(
    tx: &Transaction<'_>,
    data: &Value,
    cancelled: &impl Fn() -> bool,
) -> Result<()> {
    let assets = data["resources"].as_array().ok_or(Error::Invalid)?;
    let sessions = data["sessions"].as_array().ok_or(Error::Invalid)?;
    let edits = rows(data, "edits")?;
    if assets.len() > 10000 || sessions.len() > 10000 || edits.len() > 80000 {
        return Err(Error::Invalid);
    }
    let mut resources = HashMap::new();
    for row in assets {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let (old, r) = resource(tx, row)?;
        if resources.insert(old, r).is_some() {
            return Err(Error::Invalid);
        }
    }
    for row in edits {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let before = required(row, "uid")?;
        let asset = resources.get(before).ok_or(Error::Invalid)?;
        let rev = required(row, "revision")?;
        if rev.len() != 32
            || !rev
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !valid_content(&asset.kind, &row["content"])
        {
            return Err(Error::Invalid);
        }
        let id = if asset.uid == before {
            rev.to_owned()
        } else {
            copy_id(&[asset.uid.as_bytes(), rev.as_bytes()])
        };
        tx.execute("INSERT INTO tavern_edits(revision,uid,saved_at,content) VALUES(?,?,?,?) ON CONFLICT(revision) DO NOTHING",
            params![id,asset.uid,saved(row,"savedAt")?,row["content"].to_string()])?;
    }
    let mut remapped = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut queue = VecDeque::new();
    let mut records = HashMap::new();
    for row in sessions {
        validate_session(row)?;
        let id = required(row, "id")?.to_owned();
        let r = resources
            .get(required(row, "resourceUid")?)
            .filter(|r| r.kind == "character")
            .ok_or(Error::Invalid)?;
        let mut row = row.clone();
        row["resourceUid"] = r.uid.clone().into();
        for book in row["books"].as_array_mut().ok_or(Error::Invalid)? {
            let asset = resources
                .get(book.as_str().ok_or(Error::Invalid)?)
                .filter(|r| r.kind == "lorebook")
                .ok_or(Error::Invalid)?;
            *book = asset.uid.clone().into();
        }
        if let Some(parent) = row["parentId"].as_str() {
            children
                .entry(parent.to_owned())
                .or_default()
                .push(id.clone());
        }
        queue.push_back(id.clone());
        if records.insert(id, row).is_some() {
            return Err(Error::Invalid);
        }
    }
    let current = |key: &str| -> Result<Option<Value>> {
        let raw = tx
            .query_row(
                "SELECT data_json FROM tavern_sessions WHERE id=?",
                [key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        raw.map(|s| serde_json::from_str(&s).map_err(Error::from))
            .transpose()
    };
    while let Some(id) = queue.pop_front() {
        if remapped.contains_key(&id) {
            continue;
        }
        let mut candidate = records[&id].clone();
        if let Some(mapped) = candidate["parentId"]
            .as_str()
            .and_then(|p| remapped.get(p))
            .cloned()
        {
            candidate["parentId"] = Value::String(mapped);
        }
        if current(&id)?.is_some_and(|old| old != candidate) {
            let new_id = copy_id(&[id.as_bytes(), records[&id].to_string().as_bytes()]);
            remapped.insert(id.clone(), new_id);
            queue.extend(children.get(&id).into_iter().flatten().cloned());
        }
    }
    let mut ids = HashSet::new();
    for (id, mut row) in records {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let id = remapped.get(&id).unwrap_or(&id);
        if !ids.insert(id.clone()) {
            return Err(Error::Invalid);
        }
        row["id"] = id.clone().into();
        if let Some(parent) = row["parentId"].as_str().and_then(|p| remapped.get(p)) {
            row["parentId"] = parent.clone().into();
        }
        if current(id)?.is_some_and(|old| old != row) {
            return Err(Error::Invalid);
        }
        tx.execute("INSERT INTO tavern_sessions(id,resource_uid,data_json,updated_at) VALUES(?,?,?,?) ON CONFLICT(id) DO NOTHING",
            params![id,required(&row,"resourceUid")?,row.to_string(),saved(&row,"updatedAt")?])?;
    }
    Ok(())
}
