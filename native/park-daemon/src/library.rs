pub mod offline;
use super::{ApiError, App};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path as FsPath, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

fn storage() -> ApiError {
    ApiError(StatusCode::SERVICE_UNAVAILABLE, "STORAGE")
}
fn missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "NOT_FOUND")
}
fn blocked() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, "BLOCKED")
}
fn changed() -> ApiError {
    ApiError(StatusCode::CONFLICT, "OFFLINE_FILE_CHANGED")
}
async fn read<T: Send + 'static>(
    app: App,
    query: impl FnOnce(&rusqlite::Connection) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(move || {
        let db = app.db.lock().map_err(|_| storage())?;
        query(&db)
    })
    .await
    .map_err(|_| storage())?
}
fn json_rows(
    db: &rusqlite::Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Value>, ApiError> {
    let mut stmt = db.prepare(sql).map_err(|_| storage())?;
    let rows = stmt
        .query_map(params, |r| r.get::<_, String>(0))
        .map_err(|_| storage())?;
    rows.map(|v| serde_json::from_str(&v.map_err(|_| storage())?).map_err(|_| storage()))
        .collect()
}
pub async fn works(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let items=read(app,|db|json_rows(db,"SELECT json_patch(w.metadata_json,json_object('source',w.source,'workId',w.work_id,'title',w.title,'mediaKind',w.media_kind,
        'category',w.category,'sourceCategory',w.source_category,'summary',w.summary,'sourceUrl',w.source_url,'capabilities',json(w.capability_json),
        'sourceVersion',w.source_version,'updatedAt',w.updated_at,'version',w.version,'favorite',json(CASE WHEN c.favorite THEN 'true' ELSE 'false' END),
        'categoryIds',json(coalesce(c.category_ids_json,'[]')))) FROM works w LEFT JOIN work_collections c USING(source,work_id)
        WHERE c.source IS NOT NULL OR EXISTS(SELECT 1 FROM history h WHERE h.source=w.source AND h.work_id=w.work_id) ORDER BY w.source,w.work_id",[])).await?;
    Ok(Json(json!({"items":items})))
}
pub async fn revisions(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let items=read(app,move|db|json_rows(db,"SELECT json_object('revision',revision,'date',updated_at,'data',json(content_json)) FROM tavern_edits
        WHERE asset_id=? ORDER BY updated_at DESC,rowid DESC LIMIT 8",[id])).await?;
    Ok(Json(json!({"items":items})))
}
pub async fn downloads(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let items=read(app,|db|json_rows(db,"SELECT json_object('taskId',t.task_id,'source',t.source,'workId',t.work_id,'chapterId',t.chapter_id,'variant',t.variant,
        'status',t.status,'failureCode',t.failure_code,'workTitle',coalesce(w.title,t.work_id),'chapterTitle',coalesce(c.title,t.chapter_id),
        'mediaKind',w.media_kind,'completedCount',t.completed_count,'totalCount',t.total_count,'bytes',t.bytes,'version',t.version,
        'files',(SELECT json_group_array(json_object('id',f.id,'bytes',f.bytes)) FROM download_files f WHERE f.task_id=t.task_id AND f.status='completed'))
        FROM download_tasks t LEFT JOIN works w ON t.source=w.source AND t.work_id=w.work_id
        LEFT JOIN chapters c ON c.source=t.source AND c.work_id=t.work_id AND c.chapter_id=t.chapter_id ORDER BY t.created_at DESC,t.task_id",[])).await?;
    Ok(Json(json!({"items":items})))
}
fn range(value: &str, len: u64) -> Option<(u64, u64)> {
    let (a, b) = value.strip_prefix("bytes=")?.split_once('-')?;
    if len == 0 || !a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    if a.is_empty() {
        let tail = b.parse::<u64>().ok()?;
        return (tail > 0).then_some((len.saturating_sub(tail), len - 1));
    }
    let start = a.parse().ok()?;
    let end = if b.is_empty() {
        len - 1
    } else {
        b.parse::<u64>().ok()?.min(len - 1)
    };
    (start <= end).then_some((start, end))
}
fn manifest(
    raw: &str,
    files: &HashMap<String, i64>,
    task: &str,
    partial: bool,
) -> Result<String, ApiError> {
    if !raw.starts_with("#EXTM3U") {
        return Err(changed());
    }
    let encode = url::form_urlencoded::byte_serialize(task.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    let local = |name: &str| {
        let (seq, ext) = name.split_once('.').ok_or_else(blocked)?;
        if seq.len() != 6
            || !seq.bytes().all(|c| c.is_ascii_digit())
            || !matches!(ext, "m3u8" | "bin")
        {
            return Err(blocked());
        }
        Ok(format!(
            "/api/v1/offline/{encode}/files/{}{}",
            files.get(name).ok_or_else(missing)?,
            if partial { "?allowIncomplete=true" } else { "" }
        ))
    };
    raw.lines()
        .map(|s| {
            let line = s.trim();
            if line.is_empty() {
                return Ok(String::new());
            }
            if !line.starts_with('#') {
                return local(line);
            }
            if !line.to_ascii_uppercase().contains("URI=") {
                return Ok(line.into());
            }
            if !["#EXT-X-KEY:", "#EXT-X-MAP:", "#EXT-X-MEDIA:"]
                .iter()
                .any(|p| line.starts_with(p))
                || line.to_ascii_uppercase().matches("URI=").count() != 1
            {
                return Err(blocked());
            }
            let (prefix, rest) = line.split_once("URI=\"").ok_or_else(blocked)?;
            let (name, suffix) = rest.split_once('"').ok_or_else(blocked)?;
            Ok(format!("{prefix}URI=\"{}\"{suffix}", local(name)?))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|lines| lines.join("\n"))
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineQuery {
    #[serde(default)]
    allow_incomplete: bool,
}

fn file_index(task_key: &str, file_key: &str) -> Result<u32, ApiError> {
    let raw = file_key
        .strip_prefix(task_key)
        .and_then(|s| s.strip_prefix(":file:"))
        .ok_or_else(blocked)?;
    let number = raw.parse::<u32>().map_err(|_| blocked())?;
    if number > 20000 || raw != number.to_string() {
        return Err(blocked());
    }
    Ok(number)
}

async fn owned_directory(root: &FsPath, task: &str) -> Result<PathBuf, ApiError> {
    let folder = format!("{:x}", Sha256::digest(task.as_bytes()));
    if tokio::fs::symlink_metadata(root)
        .await
        .map_err(|_| missing())?
        .file_type()
        .is_symlink()
    {
        return Err(blocked());
    }
    let directory = tokio::fs::canonicalize(root)
        .await
        .map_err(|_| missing())?
        .join(folder);
    if tokio::fs::canonicalize(&directory)
        .await
        .map_err(|_| missing())?
        != directory
    {
        return Err(blocked());
    }
    Ok(directory)
}

async fn owned_file(directory: &FsPath, index: u32, bytes: u64) -> Result<PathBuf, ApiError> {
    let mut actual = None;
    for extension in ["bin", "m3u8"] {
        let path = directory.join(format!("{index:06}.{extension}"));
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(m) => m,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(storage()),
        };
        if actual.is_some() || metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(blocked());
        }
        if metadata.len() != bytes {
            return Err(changed());
        }
        actual = Some(path);
    }
    actual.ok_or_else(missing)
}
pub async fn file(
    State(app): State<App>,
    Path((task, id)): Path<(String, i64)>,
    Query(query): Query<OfflineQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let root = app.downloads.clone();
    let owner = task.clone();
    let partial = query.allow_incomplete;
    let (index,len,kind,format,related)=read(app,move|db|{
        let (task_key,file_key,len,kind,format):(String,String,u64,String,String)=db.query_row("SELECT t.resource_key,f.resource_key,f.bytes,coalesce(json_extract(f.metadata_json,'$.kind'),''),coalesce(json_extract(f.metadata_json,'$.format'),'')
            FROM download_files f JOIN download_tasks t ON t.task_id=f.task_id WHERE f.id=? AND f.task_id=? AND (t.status='completed' OR (? AND t.status IN ('running','paused','queued'))) AND f.status='completed' AND f.path IS NOT NULL",
            params![id,owner,partial],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(|_|storage())?.ok_or_else(missing)?;
        let mut related=HashMap::new();
        if matches!(kind.as_str(),"manifest"|"child-manifest"){
            let mut stmt=db.prepare("SELECT resource_key,id,json_extract(metadata_json,'$.kind') FROM download_files WHERE task_id=? AND status='completed'").map_err(|_|storage())?;
            let rows=stmt.query_map([owner],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,Option<String>>(2)?))).map_err(|_|storage())?;
            for r in rows {let (key,id,kind)=r.map_err(|_|storage())?;let i=file_index(&task_key,&key)?;
                let ext=if matches!(kind.as_deref(),Some("manifest"|"child-manifest")) {"m3u8"} else {"bin"};
                related.insert(format!("{i:06}.{ext}"),id);}
        }
        Ok((file_index(&task_key,&file_key)?,len,kind,format,related))
    }).await?;
    let directory = owned_directory(&root, &task).await?;
    let path = owned_file(&directory, index, len).await?;
    let mut source = tokio::fs::File::open(path).await.map_err(|_| missing())?;
    let meta = source.metadata().await.map_err(|_| missing())?;
    if !meta.is_file() || meta.len() != len {
        return Err(changed());
    }
    if matches!(kind.as_str(), "manifest" | "child-manifest") {
        if len > 2 * 1024 * 1024 {
            return Err(changed());
        }
        let mut text = String::new();
        source
            .take(len + 1)
            .read_to_string(&mut text)
            .await
            .map_err(|_| changed())?;
        return Ok((
            [
                (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            manifest(&text, &related, &task, partial)?,
        )
            .into_response());
    }
    let mut magic = [0; 192];
    let count = source.read(&mut magic).await.map_err(|_| changed())?;
    let magic = &magic[..count];
    let mime = match kind.as_str() {
        "image" if magic.starts_with(b"\x89PNG") => "image/png",
        "image" if magic.starts_with(b"\xff\xd8") => "image/jpeg",
        "image" if magic.starts_with(b"GIF") => "image/gif",
        "image" if magic.get(8..12) == Some(b"WEBP") => "image/webp",
        "segment" if magic.first() == Some(&0x47) => "video/mp2t",
        "audio" if magic.starts_with(b"OggS") => "audio/ogg",
        "audio" if magic.starts_with(b"fLaC") => "audio/flac",
        "audio" if format == "aac" => "audio/aac",
        "audio" if magic.starts_with(b"RIFF") => "audio/wav",
        "audio" if magic.get(4..8) == Some(b"ftyp") => "audio/mp4",
        "audio" => "audio/mpeg",
        "video" | "segment" if matches!(magic.get(4..8), Some(b"ftyp" | b"styp" | b"moof")) => {
            "video/mp4"
        }
        "novel" => "application/json",
        "subtitle" if format == "srt" => "application/x-subrip",
        "subtitle" if format == "lrc" => "text/plain",
        "subtitle" => "text/vtt",
        "script" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::ACCEPT_RANGES, "bytes");
    let (start, take) = if let Some(value) = headers.get(header::RANGE) {
        let Some((start, end)) = value.to_str().ok().and_then(|s| range(s, len)) else {
            return Ok(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap());
        };
        builder = builder
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
        (start, end - start + 1)
    } else {
        (0, len)
    };
    source
        .seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|_| changed())?;
    builder
        .header(header::CONTENT_LENGTH, take)
        .body(Body::from_stream(ReaderStream::new(source.take(take))))
        .map_err(|_| storage())
}
