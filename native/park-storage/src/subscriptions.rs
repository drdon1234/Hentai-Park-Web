use crate::backup::{self, Error};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Write,
    path::Path,
};

mod targets;
pub(crate) use targets::target_identity;
pub use targets::{save_target, set_target_option};

type Result<T> = std::result::Result<T, Error>;

pub fn migrate(db: &mut Connection, root: &Path) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM subscription_meta WHERE name='json_import')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(());
    }
    match fs::read(root.join("subscriptions.json")) {
        Ok(bytes) => {
            let data: Value = serde_json::from_slice(&bytes)?;
            backup::import_subscriptions(&tx, &data)?;
            let snapshot = root.join("subscriptions.json.pre-migration");
            if data["items"].as_array().is_some_and(|a| !a.is_empty()) && !snapshot.try_exists()? {
                let mut temp = tempfile::NamedTempFile::new_in(root)?;
                temp.write_all(&bytes)?;
                temp.as_file().sync_all()?;
                temp.persist_noclobber(snapshot).map_err(|e| e.error)?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    tx.execute(
        "INSERT INTO subscription_meta VALUES('json_import','complete')",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

pub struct Target {
    pub id: String,
    pub source: String,
    pub dimension: String,
    pub target: String,
}
pub fn targets(db: &Connection) -> Result<Vec<Target>> {
    let mut q=db.prepare("SELECT identity,source,dimension,target FROM subscriptions WHERE enabled=1 ORDER BY source,created_at,identity")?;
    let rows = q.query_map([], |r| {
        Ok(Target {
            id: r.get(0)?,
            source: r.get(1)?,
            dimension: r.get(2)?,
            target: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
pub fn acquire(db: &Connection, id: &str, now: i64, force: bool) -> Result<Option<String>> {
    Ok(db.query_row("UPDATE subscription_checks SET lease_owner=lower(hex(randomblob(16))),lease_until=?2+300000
       WHERE identity=?1 AND lease_until<=?2 AND (?3 OR next_due<=?2) AND EXISTS(SELECT 1 FROM subscriptions WHERE identity=?1 AND enabled=1) RETURNING lease_owner",
       params![id,now,force],|r|r.get(0)).optional()?)
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub work_id: String,
    #[serde(default)]
    pub chapter_id: String,
    #[serde(default = "default_kind")]
    pub kind: String,
}
fn default_kind() -> String {
    "work".into()
}
impl Item {
    pub fn valid(&self) -> bool {
        !self.id.is_empty()
            && matches!(self.kind.as_str(), "work" | "post" | "chapter")
            && [&self.id, &self.title, &self.work_id, &self.chapter_id]
                .iter()
                .all(|s| s.encode_utf16().count() <= 4096)
    }
}
#[derive(Deserialize, Serialize, Default)]
pub struct Progress {
    pub cursor: Option<String>,
    #[serde(default)]
    pub items: Vec<Item>,
}
pub struct Check {
    pub baseline: bool,
    pub seen: HashSet<String>,
    pub progress: Progress,
}
pub fn begin(db: &mut Connection, id: &str, lease: &str, context: &str) -> Result<Check> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("UPDATE subscription_checks SET context_hash=?3,baseline=0,progress='{}' WHERE identity=?1 AND lease_owner=?2 AND context_hash<>?3",params![id,lease,context])?;
    let (baseline,count,progress):(bool,usize,String)=tx.query_row("SELECT baseline,seen_count,progress FROM subscription_checks WHERE identity=? AND lease_owner=?",params![id,lease],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let seen: HashSet<String> = tx
        .prepare("SELECT content_id FROM subscription_seen WHERE identity=?")?
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    tx.commit()?;
    Ok(Check {
        baseline: baseline && seen.len() == count,
        seen,
        progress: serde_json::from_str(&progress)?,
    })
}
pub fn commit(
    db: &mut Connection,
    t: &Target,
    lease: &str,
    check: Check,
    complete: bool,
    now: i64,
    hours: i64,
) -> Result<bool> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let notify:Option<bool>=tx.query_row("SELECT s.notify FROM subscription_checks c JOIN subscriptions s USING(identity) WHERE identity=? AND lease_owner=? AND lease_until>=? AND s.enabled=1", params![t.id,lease,now],|r|r.get(0)).optional()?;
    let Some(notify) = notify else {
        return Ok(false);
    };
    let items: BTreeMap<_, _> = check
        .progress
        .items
        .into_iter()
        .map(|i| (i.id.clone(), i))
        .collect();
    if items.len() > 20000 {
        return Err(Error::Invalid);
    }
    if complete {
        for item in items.into_values() {
            let fresh = tx.execute(
                "INSERT OR IGNORE INTO subscription_seen VALUES(?,?)",
                params![t.id, item.id],
            )?;
            if fresh == 0 || !check.baseline {
                continue;
            }
            let key = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&[&t.source, &item.kind, &item.id])?)
            );
            let nid = u32::from_str_radix(&key[..7], 16).map_err(|_| Error::Invalid)? + 20000;
            tx.execute("INSERT INTO subscription_events(event_key,source,content_id,identity,title,route,created_at,notification_id,notify) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(event_key) DO UPDATE SET notify=max(notify,excluded.notify)",
                params![key,t.source,item.id,t.id,item.title,serde_json::to_string(&item)?,now,nid,notify])?;
            tx.execute(
                "INSERT OR IGNORE INTO subscription_event_targets VALUES(?,?)",
                params![key, t.id],
            )?;
        }
        tx.execute("UPDATE subscription_checks SET baseline=1,seen_count=(SELECT count(*) FROM subscription_seen WHERE identity=?1),last_success=?2,next_due=?3,failures=0,error_code='',progress='{}',lease_owner='',lease_until=0 WHERE identity=?1",
            params![t.id,now,now+hours*3600000])?;
    } else {
        let progress = Progress {
            cursor: check.progress.cursor,
            items: items.into_values().collect(),
        };
        tx.execute("UPDATE subscription_checks SET progress=?,next_due=?,lease_owner='',lease_until=0 WHERE identity=?",params![serde_json::to_string(&progress)?,now+60000,t.id])?;
    }
    tx.commit()?;
    Ok(true)
}
pub fn fail(db: &Connection, id: &str, lease: &str, now: i64, code: &str) -> Result<()> {
    let code = match code {
        "HTTP" | "PARSE" | "LOGIN_REQUIRED" | "BLOCKED" | "CLOUDFLARE" | "CONFIG"
        | "UNSUPPORTED" | "CANCELLED" | "NOT_FOUND" => code,
        _ => "NETWORK",
    };
    db.execute("UPDATE subscription_checks SET failures=failures+1,error_code=?3,next_due=?4+(60000 << min(8,failures+1)),lease_owner='',lease_until=0 WHERE identity=?1 AND lease_owner=?2",params![id,lease,code,now])?;
    Ok(())
}
pub fn target_view(db: &Connection) -> Result<Vec<Value>> {
    let mut q=db.prepare("SELECT identity,source,label,enabled,error_code,dimension,target,notify FROM subscriptions JOIN subscription_checks USING(identity) ORDER BY created_at,identity")?;
    let targets=q.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"source":r.get::<_,String>(1)?,"label":r.get::<_,String>(2)?,"enabled":r.get::<_,bool>(3)?,"error":r.get::<_,String>(4)?,"dimension":r.get::<_,String>(5)?,"target":r.get::<_,String>(6)?,"notify":r.get::<_,bool>(7)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(targets)
}
pub fn view(db: &Connection) -> Result<Value> {
    Ok(json!({"targets":target_view(db)?, "events":event_page(db, None, "", 500)?}))
}

pub fn event_page(db: &Connection, after_time: Option<i64>, after_key: &str, limit: u32) -> Result<Vec<Value>> {
    let mut query = db.prepare(
        "SELECT event_key,title,unread,source,route,created_at FROM subscription_events
         WHERE ?1 IS NULL OR created_at < ?1 OR (created_at = ?1 AND event_key > ?2)
         ORDER BY created_at DESC,event_key LIMIT ?3"
    )?;
    let rows = query.query_map(params![after_time, after_key, limit.clamp(1, 500)], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, bool>(2)?,
            row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, i64>(5)?))
    })?;
    rows.map(|row| {
        let (key, title, unread, source, raw, created) = row?;
        Ok(json!({"key":key, "title":title, "unread":unread, "source":source,
                  "route":serde_json::from_str::<Item>(&raw)?, "createdAt":created}))
    }).collect()
}
pub fn claim_notification(db: &Connection, now: i64) -> Result<Option<Value>> {
    Ok(db.query_row("UPDATE subscription_events SET next_attempt=?1+300000 WHERE event_key=(SELECT e.event_key FROM subscription_events e WHERE delivered=0 AND notify=1 AND next_attempt<=?1 AND EXISTS(SELECT 1 FROM subscription_event_targets t JOIN subscriptions s USING(identity) WHERE t.event_key=e.event_key AND s.enabled=1 AND s.notify=1) ORDER BY created_at,event_key LIMIT 1) RETURNING event_key,title,notification_id,next_attempt",[now],|r|Ok(json!({"key":r.get::<_,String>(0)?,"title":r.get::<_,String>(1)?,"id":r.get::<_,i32>(2)?,"claim":r.get::<_,i64>(3)?}))).optional()?)
}
