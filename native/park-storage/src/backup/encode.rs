use super::{object, portable_settings, read_json, sources, Error, Result, Scope};
use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::{write::GzEncoder, Compression};
use rusqlite::{Connection, Transaction};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

// 行直接写入压缩流，收藏、章节和图片原件均不构成整库内存快照。
const LIBRARY: [(&str,&str);4] = [
    ("categories","SELECT json_object('name',name,'sort',sort) FROM categories ORDER BY rowid"),
    ("works", "SELECT json_object('item',json(w.json),'source',source,'id',id,
        'favorite',json(iif(favorite,'true','false')),'favoritedAt',favorited_at,'readerMode',reader_mode,
        'categories',(SELECT json_group_array(c.name) FROM work_categories wc JOIN categories c ON c.id=wc.category_id
            WHERE wc.source=w.source AND wc.work_id=w.id)) FROM works w
        WHERE favorite OR reader_mode IS NOT NULL OR EXISTS(SELECT 1 FROM work_categories wc WHERE wc.source=w.source AND wc.work_id=w.id) ORDER BY w.rowid"),
    ("chapters", "SELECT json_object('source',source,'workId',work_id,'chapterId',chapter_id,'title',title,'sort',sort,
        'read',json(iif(read,'true','false')),'lastPage',last_page,'pageCount',page_count,'progressUpdatedAt',progress_updated_at,
        'dateRead',date_read,'position',position,'duration',duration,'positionUpdatedAt',position_updated_at)
        FROM chapters WHERE read OR progress_updated_at IS NOT NULL OR position_updated_at IS NOT NULL ORDER BY rowid"),
    ("history", "SELECT json_object('item',json(w.json),'source',h.source,'workId',h.work_id,'time',h.time,
        'chapterId',h.chapter_id,'chapterTitle',h.chapter_title,'page',h.page,'pageCount',h.page_count,'position',h.position)
        FROM history h JOIN works w ON w.source=h.source AND w.id=h.work_id ORDER BY h.rowid"),
];
const TAVERN: [(&str,&str);3] = [
    ("resources","SELECT json_object('item',json(item_json),'kind',kind,'format',format,'savedAt',saved_at), original FROM tavern_resources ORDER BY rowid"),
    ("edits","SELECT json_object('revision',revision,'uid',uid,'savedAt',saved_at,'content',json(content)) FROM
        (SELECT *,row_number() OVER(PARTITION BY uid ORDER BY saved_at DESC,rowid DESC) AS n FROM tavern_edits) WHERE n<=8 ORDER BY saved_at DESC"),
    ("sessions","SELECT json(data_json) FROM tavern_sessions ORDER BY rowid"),
];
const SUBSCRIPTIONS: [(&str,&str);1] = [("items","SELECT json_object('source',source,'target',target,'label',label,'dimension',dimension,
    'enabled',json(iif(enabled,'true','false')),'notify',json(iif(notify,'true','false'))) FROM subscriptions ORDER BY created_at,rowid")];

fn group(
    out: &mut impl Write,
    db: &Transaction<'_>,
    name: &str,
    sections: &[(&str, &str)],
    cancelled: &impl Fn() -> bool,
) -> Result<()> {
    write!(out, ",\"{name}\":{{")?;
    for (i, (section, sql)) in sections.iter().enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
        write!(out, "\"{section}\":[")?;
        let mut statement = db.prepare(sql)?;
        let mut rows = statement.query([])?;
        let mut comma = false;
        while let Some(row) = rows.next()? {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            if comma {
                out.write_all(b",")?;
            }
            let data = row.get_ref(0)?.as_str().map_err(|_| Error::Invalid)?;
            if *section == "resources" {
                let mut item: Value = serde_json::from_str(data)?;
                item["original"] = STANDARD
                    .encode(row.get_ref(1)?.as_blob().map_err(|_| Error::Invalid)?)
                    .into();
                serde_json::to_writer(&mut *out, &item)?;
            } else {
                out.write_all(data.as_bytes())?;
            }
            comma = true;
        }
        out.write_all(b"]")?;
    }
    out.write_all(b"}")?;
    Ok(())
}

pub fn write(
    db: &Connection,
    root: &Path,
    dest: impl Write,
    scope: Scope,
    library: bool,
    settings: bool,
    plugins: bool,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    if cancelled() {
        return Err(Error::Cancelled);
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Invalid)?
        .as_millis();
    write_at(
        db, root, dest, scope, library, settings, plugins, stamp, cancelled,
    )
}

pub(super) fn write_at(
    db: &Connection,
    root: &Path,
    dest: impl Write,
    scope: Scope,
    library: bool,
    settings: bool,
    plugins: bool,
    stamp: u128,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    write_sections(
        db, root, dest, scope, library, settings, plugins, stamp, cancelled, None,
    )
}
pub(super) fn write_sections(
    db: &Connection,
    root: &Path,
    dest: impl Write,
    scope: Scope,
    library: bool,
    settings: bool,
    plugins: bool,
    stamp: u128,
    cancelled: impl Fn() -> bool,
    desktop_sources: Option<&Value>,
) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    let mut out = GzEncoder::new(dest, Compression::default());
    let origin = if scope == Scope::Automatic {
        "automatic"
    } else {
        "manual"
    };
    write!(
        out,
        "{{\"format\":\"parkbak\",\"version\":1,\"createdAt\":{stamp},\"origin\":\"{origin}\""
    )?;
    if settings {
        out.write_all(b",\"settings\":")?;
        let mut local = portable_settings(object(read_json(&root.join("settings.json"))?)?);
        if desktop_sources.is_some() {
            local.remove("onboarded");
        }
        serde_json::to_writer(&mut out, &local)?;
    }
    if plugins {
        out.write_all(b",\"sources\":")?;
        if let Some(snapshot) = desktop_sources {
            serde_json::to_writer(&mut out, snapshot)?;
        } else {
            serde_json::to_writer(
                &mut out,
                &sources::snapshot(root, scope == Scope::WithCredentials)?,
            )?;
        }
    }
    if library {
        out.write_all(b",\"readingNotes\":")?;
        let notes = read_json(&root.join("reading_notes.json"))?.unwrap_or_else(|| json!([]));
        if !notes.is_array() {
            return Err(Error::Invalid);
        }
        serde_json::to_writer(&mut out, &notes)?;
        if desktop_sources.is_some() {
            group(&mut out,&tx,"subscriptions",&[("items",SUBSCRIPTIONS[0].1),("targets","SELECT json_object('identity',identity,'source',source,'dimension',dimension,'target',target,'label',label,'enabled',json(iif(enabled,'true','false')),'notify',json(iif(notify,'true','false')),'createdAt',created_at) FROM subscriptions ORDER BY created_at,rowid")],&cancelled)?;
        } else {
            group(&mut out, &tx, "subscriptions", &SUBSCRIPTIONS, &cancelled)?;
        }
        group(&mut out, &tx, "tavern", &TAVERN, &cancelled)?;
        group(&mut out, &tx, "library", &LIBRARY, &cancelled)?;
    }
    out.write_all(b"}")?;
    out.finish()?.flush()?;
    tx.commit()?;
    Ok(())
}
