use crate::progress::{self, ChapterKey, Ledger};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryWrite {
    source: String,
    work_id: String,
    chapter_id: Option<String>,
    chapter_title: Option<String>,
    page: Option<i64>,
    page_count: Option<i64>,
    position: Option<f64>,
    duration: Option<f64>,
    text_offset: Option<i64>,
    version: Option<i64>,
    request_key: String,
    fingerprint: String,
    request_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid input")]
    Invalid,
    #[error("version required")]
    VersionRequired,
    #[error("missing record")]
    Missing,
    #[error("concurrent change")]
    Conflict(Option<i64>),
    #[error("database unavailable")]
    Storage(#[from] rusqlite::Error),
    #[error("ledger unavailable")]
    Ledger(#[from] progress::Error),
}

impl HistoryWrite {
    pub fn apply(self, ledger: &Ledger, cancelled: impl Fn() -> bool) -> Result<Value, Error> {
        if self
            .page
            .is_some_and(|p| p < 0 || self.page_count.is_some_and(|n| p >= n))
            || self.page_count.is_some_and(|n| n <= 0)
            || self.position.is_some_and(|p| !p.is_finite() || p < 0.0)
            || self
                .duration
                .is_some_and(|d| !(0.0..=31536000.0).contains(&d))
            || self.request_key.is_empty()
            || self.request_key.encode_utf16().count() > 256
            || self.fingerprint.len() != 64
            || !self.fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Error::Invalid);
        }
        let mut db = ledger.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if cancelled() {
            return Err(progress::Error::Cancelled.into());
        }
        let private: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM app_settings WHERE key='incognito' AND value_json='true')",
            [],
            |r| r.get(0),
        )?;
        if private {
            return Ok(json!({"saved":false,"reason":"INCOGNITO","requestId":self.request_id}));
        }
        let (kind, category): (String, String) = tx
            .query_row(
                "SELECT media_kind,category FROM works WHERE source=? AND work_id=?",
                params![self.source, self.work_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(Error::Missing)?;
        if category == "live" {
            return Ok(json!({"saved":false,"reason":"LIVE","requestId":self.request_id}));
        }
        let prior:Option<(String,String)>=tx.query_row("SELECT request_fingerprint,response_json FROM idempotency_records WHERE operation='history.write' AND request_key=?",
            [&self.request_key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((hash, response)) = prior {
            if hash != self.fingerprint {
                return Err(Error::Conflict(None));
            }
            return serde_json::from_str(&response)
                .map_err(|_| progress::Error::CorruptDatabase.into());
        }
        let now: String = tx.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')", [], |r| {
            r.get(0)
        })?;
        let scope = json!([self.source, self.work_id, self.chapter_id]).to_string();
        let new_version = if let Some(id) = &self.chapter_id {
            let (version,state,image,length):(i64,String,bool,Option<i64>)=tx.query_row(
                "SELECT version,read_state,coalesce(node_type='image',0),CASE WHEN json_type(node_metadata_json,'$.textLength')='integer' THEN json_extract(node_metadata_json,'$.textLength') END FROM chapters WHERE source=? AND work_id=? AND chapter_id=?",
                params![self.source,self.work_id,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.ok_or(Error::Missing)?;
            if self.version.ok_or(Error::VersionRequired)? != version {
                return Err(Error::Conflict(Some(version)));
            }
            if self
                .text_offset
                .is_some_and(|n| kind != "novel" || n < 0 || length.is_none_or(|l| n > l))
            {
                return Err(Error::Invalid);
            }
            let comic = kind == "comic" || image;
            let complete = self
                .page
                .zip(self.page_count)
                .is_some_and(|(p, n)| p >= n - 1)
                || self
                    .position
                    .zip(self.duration)
                    .is_some_and(|(p, d)| d > 0.0 && p >= 0.99 * d)
                || self.text_offset.zip(length).is_some_and(|(p, n)| p >= n);
            let state = if complete || (comic && state == "read") {
                "read"
            } else if self.page.is_none() && self.position.is_none() && self.text_offset.is_none() {
                &state
            } else {
                "reading"
            };
            tx.execute("UPDATE chapters SET text_offset=coalesce(?1,text_offset),page_progress=coalesce(?2,page_progress),page_count=coalesce(?3,page_count),
                video_position=coalesce(?4,video_position),duration=coalesce(?5,duration),read_state=?6,progress_updated_at=?7,
                page_updated_ms=CASE WHEN ?2 IS NULL THEN page_updated_ms ELSE cast(round(unixepoch(?7,'subsec')*1000) AS INTEGER) END,
                position_updated_ms=CASE WHEN ?4 IS NULL THEN position_updated_ms ELSE cast(round(unixepoch(?7,'subsec')*1000) AS INTEGER) END,
                date_read_ms=CASE WHEN ?6='read' THEN coalesce(date_read_ms,cast(round(unixepoch(?7,'subsec')*1000) AS INTEGER)) END,
                version=version+1 WHERE source=?8 AND work_id=?9 AND chapter_id=?10",
                params![self.text_offset,self.page,self.page_count,self.position,self.duration,state,now,self.source,self.work_id,id])?;
            progress::record_chapter(
                &tx,
                &super::WEB,
                &ChapterKey {
                    source: self.source.clone(),
                    work_id: self.work_id.clone(),
                    chapter_id: id.clone(),
                },
            )?;
            Some(version + 1)
        } else {
            None
        };
        tx.execute("INSERT INTO history(source,work_id,chapter_id,chapter_title,page,page_count,position,updated_at,scope_key) VALUES(?,?,?,?,?,?,?,?,?)
            ON CONFLICT(scope_key) DO UPDATE SET chapter_title=excluded.chapter_title,page=excluded.page,page_count=excluded.page_count,position=excluded.position,updated_at=excluded.updated_at",
            params![self.source,self.work_id,self.chapter_id,self.chapter_title,self.page,self.page_count,self.position,now,scope])?;
        let event = json!({"source":self.source,"workId":self.work_id,"chapterId":self.chapter_id,"page":self.page,"pageCount":self.page_count,"position":self.position});
        tx.execute("WITH next_ver(v) AS (SELECT coalesce(max(event_version),0)+1 FROM domain_events WHERE aggregate_type='chapter' AND aggregate_id=?2)
            INSERT INTO domain_events(event_id,sequence,event_type,aggregate_type,aggregate_id,aggregate_version,event_version,created_at,payload_json)
            SELECT ?1,(SELECT coalesce(max(sequence),0)+1 FROM domain_events),'chapter.progressed','chapter',?2,coalesce(?3,v),v,?4,?5 FROM next_ver",
            params![self.request_id,scope,new_version,now,event.to_string()])?;
        tx.execute(
            "INSERT INTO event_outbox(event_id) VALUES(?)",
            [&self.request_id],
        )?;
        let mut response = event;
        response["saved"] = json!(true);
        response["textOffset"] = json!(self.text_offset);
        response["updatedAt"] = json!(now);
        response["requestId"] = json!(self.request_id);
        if let Some(version) = new_version {
            response["version"] = json!(version);
        }
        tx.execute("INSERT INTO idempotency_records(operation,request_key,request_fingerprint,response_status,response_json,created_at)
            VALUES('history.write',?,?,200,?,?)",params![self.request_key,self.fingerprint,response.to_string(),now])?;
        if cancelled() {
            return Err(progress::Error::Cancelled.into());
        }
        tx.commit()?;
        Ok(response)
    }
}
