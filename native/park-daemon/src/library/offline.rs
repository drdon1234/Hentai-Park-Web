use super::{
    changed, file_index, missing, owned_directory, owned_file, read, storage, ApiError, App,
    OfflineQuery,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

struct Entry {
    index: u32,
    bytes: u64,
    metadata: Value,
    path: PathBuf,
}

impl Entry {
    fn kind(&self) -> &str {
        self.metadata["kind"].as_str().unwrap_or_default()
    }
    fn url(&self) -> &Value {
        &self.metadata["url"]
    }

    async fn text(&self, max: u64) -> Result<String, ApiError> {
        if self.bytes > max {
            return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT"));
        }
        let mut bytes = Vec::with_capacity(self.bytes as usize);
        tokio::fs::File::open(&self.path)
            .await
            .map_err(|_| missing())?
            .take(self.bytes + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| changed())?;
        if bytes.len() as u64 != self.bytes {
            return Err(changed());
        }
        String::from_utf8(bytes).map_err(|_| changed())
    }
}

pub async fn describe(
    State(app): State<App>,
    Path(task): Path<String>,
    Query(query): Query<OfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let root = app.downloads.clone();
    let id = task.clone();
    let partial = query.allow_incomplete;
    let (mut result,mut files)=read(app,move|db|{
        let (task_key,meta):(String,String)=db.query_row("SELECT t.resource_key,json_object(
            'source',t.source,'workId',t.work_id,'chapterId',t.chapter_id,'workTitle',coalesce(w.title,t.work_id),
            'title',coalesce(c.title,t.chapter_id),'pageProgress',c.page_progress,'videoPosition',c.video_position,
            'duration',c.duration,'textOffset',coalesce(c.text_offset,0),'version',coalesce(c.version,1),
            'offline',json('true'),'incomplete',json(CASE WHEN t.status='completed' THEN 'false' ELSE 'true' END),
            'totalCount',t.total_count,'mediaKind',w.media_kind)
            FROM download_tasks t LEFT JOIN works w ON w.source=t.source AND w.work_id=t.work_id
            LEFT JOIN chapters c ON c.source=t.source AND c.work_id=t.work_id AND c.chapter_id=t.chapter_id
            WHERE t.task_id=? AND (t.status='completed' OR (? AND t.status IN ('running','paused','queued')))",
            params![id,partial],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_|storage())?.ok_or_else(missing)?;
        let encoded=url::form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>().replace('+',"%20");
        let mut stmt=db.prepare("SELECT resource_key,id,bytes,metadata_json FROM download_files WHERE task_id=? AND status='completed'").map_err(|_|storage())?;
        let mut rows=stmt.query([id]).map_err(|_|storage())?;
        let mut entries=Vec::new();
        while let Some(row)=rows.next().map_err(|_|storage())? {
            let key:String=row.get(0).map_err(|_|storage())?;
            let id:i64=row.get(1).map_err(|_|storage())?;
            let raw:String=row.get(3).map_err(|_|storage())?;
            let mut metadata:Value=serde_json::from_str(&raw).map_err(|_|storage())?;
            if !metadata.is_object() {return Err(changed());}
            metadata["id"]=json!(id);
            metadata["url"]=json!(format!("/api/v1/offline/{encoded}/files/{id}{}",if partial {"?allowIncomplete=true"}else{""}));
            entries.push(Entry{index:file_index(&task_key,&key)?,bytes:row.get(2).map_err(|_|storage())?,metadata,path:PathBuf::new()});
        }
        if entries.is_empty(){return Err(missing());}
        entries.sort_by_key(|e|e.index);
        Ok((serde_json::from_str::<Value>(&meta).map_err(|_|storage())?,entries))
    }).await?;

    let directory = owned_directory(&root, &task).await?;
    for file in &mut files {
        file.path = owned_file(&directory, file.index, file.bytes).await?;
    }
    add_content(&mut result, &files).await?;
    result["files"] = Value::Array(files.into_iter().map(|f| f.metadata).collect());
    Ok(Json(result))
}

async fn add_content(result: &mut Value, files: &[Entry]) -> Result<(), ApiError> {
    result["previewOnly"] = json!(files.iter().any(|f| f.metadata["previewOnly"] == true));
    let mut article: Vec<_> = files
        .iter()
        .filter_map(|f| Some((f.metadata["blockIndex"].as_u64()?, f)))
        .collect();
    if !article.is_empty() {
        article.sort_by_key(|(n, _)| *n);
        let mut blocks = Vec::with_capacity(article.len());
        for (_, f) in article {
            blocks.push(match f.kind() {
                "script" => json!({"type":"text","text":f.text(4_000_000).await?}),
                "video" | "manifest" => json!({"type":"video","url":f.url()}),
                "image" => json!({"type":"image","url":f.url()}),
                _ => return Err(ApiError(StatusCode::CONFLICT, "INVALID_OFFLINE_ARTICLE")),
            });
        }
        result["mediaKind"] = json!("article");
        result["blocks"] = Value::Array(blocks);
        result["pages"] = json!([]);
        return Ok(());
    }
    let novel = files.iter().find(|f| f.kind() == "novel");
    let media = files.iter().find(|f| matches!(f.kind(), "audio" | "video"));
    let manifest = files.iter().find(|f| f.kind() == "manifest");
    if let Some(f) = novel {
        let text = f.text(2 * 1024 * 1024).await?;
        let doc: Value = serde_json::from_str(&text).map_err(|_| changed())?;
        let paragraphs = doc["paragraphs"]
            .as_array()
            .filter(|a| a.iter().all(Value::is_string))
            .ok_or_else(changed)?;
        result["paragraphs"] = json!(paragraphs);
        if doc["title"].is_string() {
            result["title"] = doc["title"].clone();
        }
    } else if let Some(f) = files.iter().find(|f| {
        f.kind() == "script"
            && matches!(f.metadata["format"].as_str(), Some("txt" | "text"))
            && f.bytes <= 2 * 1024 * 1024
    }) {
        if let Ok(script) = f.text(2 * 1024 * 1024).await {
            result["paragraphs"] =
                json!(script.replace("\r\n", "\n").split('\n').collect::<Vec<_>>());
        }
    }
    let voice = media.is_some_and(|f| f.kind() == "audio") || result["mediaKind"] == "voice";
    result["mediaKind"] = json!(if novel.is_some() {
        "novel"
    } else if manifest.is_none() && media.is_none() {
        "comic"
    } else if voice {
        "voice"
    } else {
        "video"
    });
    if let Some(f) = manifest.or(media) {
        result["mediaUrl"] = f.url().clone();
        result["format"] = json!(if manifest.is_some() {
            "hls"
        } else {
            f.metadata["format"]
                .as_str()
                .unwrap_or(if f.kind() == "audio" { "audio" } else { "mp4" })
        });
    }
    let mut volumes: Vec<_> = files
        .iter()
        .filter(|f| f.kind() == "manifest")
        .filter_map(|f| Some((f.metadata["recording"]["volume"].as_u64()?, f.url())))
        .collect();
    volumes.sort_by_key(|(v, _)| *v);
    result["volumes"] = Value::Array(
        volumes
            .iter()
            .map(|(v, url)| json!({"volume":v,"url":url}))
            .collect(),
    );
    result["pages"] = Value::Array(
        files
            .iter()
            .filter(|f| f.kind() == "image")
            .map(|f| {
                let mut page = json!({"index":f.index,"url":f.url()});
                if let Some(v) = f.metadata.get("transform") {
                    page["transform"] = v.clone();
                }
                page
            })
            .collect(),
    );
    Ok(())
}
