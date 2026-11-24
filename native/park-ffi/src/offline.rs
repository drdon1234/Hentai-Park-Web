use crate::{ChapterKey, ProgressStore, StorageError};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::path::{Component, Path, PathBuf};
mod bundle;
pub use bundle::*;

#[derive(Debug, uniffi::Record)]
pub struct LibraryWork {
    pub downloaded: bool,
    pub invalid_download: bool,
    pub pending_deletion: Option<Vec<String>>,
    pub download_status: Option<i32>,
    pub reader_mode: Option<i32>,
    pub media: String,
    pub source: String,
    pub work_id: String,
    pub title: String,
    pub category_names: Vec<String>,
    pub favorite: bool,
    pub history_ms: Option<i64>,
    pub history_chapter: Option<String>,
    pub history_page: i64,
    pub chapters: Vec<OfflineChapter>,
    pub content_domain: Option<String>,
    pub item_kind: Option<String>,
}

#[derive(Debug, Deserialize, uniffi::Record)]
pub struct OfflineChapter {
    pub id: String,
    pub title: String,
}

#[derive(Deserialize)]
struct WorkIdentity {
    source: String,
    id: String,
    #[serde(default)]
    title: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Download {
    item: WorkIdentity,
    media: String,
    chapters: Vec<OfflineChapter>,
    #[serde(default)]
    completed: Vec<String>,
    deleting_chapters: Option<Vec<String>>,
    status: i32,
}

impl Download {
    fn from_record(raw: &str, source: &str, work: &str) -> Option<Self> {
        serde_json::from_str::<Self>(raw).ok().filter(|task| {
            task.item.source == source
                && task.item.id == work
                && (0..=4).contains(&task.status)
                && matches!(task.media.as_str(), "comic" | "video" | "novel" | "bundle")
        })
    }

    fn available(&self, id: &str) -> bool {
        matches!(self.media.as_str(), "comic" | "novel" | "video" | "bundle")
            && (self.media == "bundle" || self.completed.iter().any(|key| key == id))
            && self
                .deleting_chapters
                .as_ref()
                .is_none_or(|deleted| !deleted.is_empty() && !deleted.iter().any(|key| key == id))
    }
}

#[derive(Deserialize)]
struct Manifest {
    pages: Vec<Page>,
}
#[derive(Deserialize)]
struct Page {
    file: String,
    transform: Option<serde_json::Value>,
    width: Option<f64>,
    height: Option<f64>,
}

pub(crate) fn asset(dir: &Path, relative: &str) -> Result<PathBuf, StorageError> {
    let path = Path::new(relative);
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(StorageError::InvalidDownload);
    }
    let actual = dir
        .join(path)
        .canonicalize()
        .map_err(|_| StorageError::UnavailableDownload)?;
    if !actual.starts_with(dir) || !actual.is_file() {
        return Err(StorageError::InvalidDownload);
    }
    Ok(actual)
}

#[uniffi::export]
impl ProgressStore {
    pub fn last_read_chapter(&self, source: String, work: String) -> Result<Option<String>, StorageError> {
        Ok(self.lock()?.query_row("SELECT chapter_id FROM history WHERE source=? AND work_id=?",
            params![source, work],|row|row.get::<_,Option<String>>(0)).optional()?.flatten())
    }
    pub fn stored_work(&self, source: String, work: String) -> Result<Option<String>, StorageError> {
        Ok(self.lock()?.query_row("SELECT json FROM works WHERE source=? AND id=?",params![source,work],|r|r.get(0)).optional()?)
    }
    pub fn is_favorite(&self, source: String, work: String) -> Result<bool, StorageError> {
        Ok(self.lock()?.query_row("SELECT favorite FROM works WHERE source=? AND id=?",params![source,work],|r|r.get(0)).optional()?.unwrap_or(false))
    }
    pub fn save_work_summary(&self, source: String, summary: String, favorite: Option<bool>) -> Result<bool, StorageError> {
        if summary.len() > 1024*1024 { return Err(StorageError::InvalidProgress); }
        let summary=serde_json::from_str(&summary).map_err(|_|StorageError::InvalidProgress)?;
        let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_|StorageError::IoFailure)?.as_millis();
        let mut db=self.lock()?;
        Ok(park_storage::library::save_summary(&mut db,&source,&summary,favorite,now.try_into().map_err(|_|StorageError::IoFailure)?)?)
    }
    pub fn library_works(&self) -> Result<Vec<LibraryWork>, StorageError> {
        let mut db = self.lock()?;
        let tx = db.transaction()?;
        let mut query = tx.prepare(
            "WITH ids(source,id) AS (SELECT source,id FROM works UNION SELECT source,substr(uid,length(source)+2) FROM download_tasks WHERE substr(uid,1,length(source)+1)=source||':')
            SELECT d.json,ids.source,ids.id,w.title,COALESCE(w.favorite,0),h.time,h.chapter_id,h.page,w.json,w.reader_mode,
            COALESCE(json_extract(CASE WHEN json_valid(w.json) THEN w.json END,'$.contentDomain'),
                     json_extract(CASE WHEN json_valid(d.json) THEN d.json END,'$.item.contentDomain')),
            CASE WHEN d.uid IS NOT NULL THEN
                CASE WHEN json_type(CASE WHEN json_valid(d.json) THEN d.json END,'$.item.mediaKind')='text' THEN json_extract(d.json,'$.item.mediaKind') END
            WHEN json_type(CASE WHEN json_valid(w.json) THEN w.json END,'$.mediaKind')='text' THEN json_extract(w.json,'$.mediaKind') END
            FROM ids LEFT JOIN works w ON ids.source=w.source AND ids.id=w.id
            LEFT JOIN download_tasks d ON d.uid=ids.source||':'||ids.id AND d.source=ids.source
            LEFT JOIN history h ON h.source=ids.source AND h.work_id=ids.id
            WHERE d.uid IS NOT NULL OR h.time IS NOT NULL OR w.favorite=1
            ORDER BY h.time DESC,w.favorited_at DESC,w.title,ids.source,ids.id",
        )?;
        let mut categories = tx.prepare(
            "SELECT c.name FROM work_categories wc JOIN categories c ON c.id=wc.category_id
            WHERE wc.source=? AND wc.work_id=? ORDER BY c.sort,c.id",
        )?;
        let mut rows = query.query([])?;
        let mut works = Vec::new();
        while let Some(row) = rows.next()? {
            let source: String = row.get(1)?;
            let work_id: String = row.get(2)?;
            let raw: Option<String> = row.get(0)?;
            let mut title: Option<String> = row.get(3)?;
            let mut invalid_download = false;
            let mut pending_deletion = None;
            let mut download_status = None;
            let (media, downloaded, chapters) = if let Some(raw) = raw {
                if let Some(mut d) = Download::from_record(&raw, &source, &work_id) {
                    let chapters = std::mem::take(&mut d.chapters)
                        .into_iter()
                        .filter(|c| d.available(&c.id))
                        .collect::<Vec<_>>();
                    if title.is_none() && !d.item.title.is_empty() {
                        title = Some(d.item.title);
                    }
                    pending_deletion = d.deleting_chapters;
                    download_status = Some(d.status);
                    (d.media, true, chapters)
                } else {
                    invalid_download = true;
                    (String::new(), true, Vec::new())
                }
            } else {
                let item: serde_json::Value = serde_json::from_str(&row.get::<_, String>(8)?)
                    .map_err(|_| StorageError::CorruptDatabase)?;
                let mut list=tx.prepare("SELECT chapter_id,title FROM chapters WHERE source=?1 AND work_id=?2 ORDER BY sort,chapter_id")?;
                let mut chapters = list
                    .query_map(params![source, work_id], |r| {
                        Ok(OfflineChapter {
                            id: r.get(0)?,
                            title: r.get(1)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if chapters.is_empty() {
                    chapters.push(OfflineChapter {
                        id: row
                            .get::<_, Option<String>>(6)?
                            .unwrap_or_else(|| "_".into()),
                        title: row.get(3)?,
                    });
                }
                (
                    if source == "tavern-session" {
                        "tavern"
                    } else {
                        item["mediaKind"].as_str().unwrap_or("unknown")
                    }
                    .into(),
                    false,
                    chapters,
                )
            };
            let category_names = categories
                .query_map(params![source, work_id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            works.push(LibraryWork {
                downloaded,
                invalid_download,
                pending_deletion,
                download_status,
                reader_mode: row.get(9)?,
                media,
                source,
                title: title.unwrap_or_else(|| work_id.clone()),
                work_id,
                favorite: row.get(4)?,
                history_ms: row.get(5)?,
                history_chapter: row.get(6)?,
                history_page: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                chapters,
                category_names,
                content_domain: row.get(10)?,
                item_kind: row.get::<_, Option<String>>(11)?.filter(|kind| matches!(kind.as_str(),
                    "comic" | "video" | "voice" | "novel" | "tavern" | "game" | "article" | "mixed" | "unknown")),
            });
        }
        Ok(works)
    }

    pub fn favorite_updates(&self,category:Option<i64>) -> Result<String,StorageError> {
        let db=self.lock()?;
        Ok(park_storage::library::updates::view(&db,category)?.to_string())
    }

    pub fn download_chapters(&self, source: String, work: String) -> Result<Vec<OfflineChapter>, StorageError> {
        let raw: String = self.lock()?.query_row(
            "SELECT json FROM download_tasks WHERE source=? AND uid=?",
            params![source, format!("{source}:{work}")], |row| row.get(0),
        ).optional()?.ok_or(StorageError::UnavailableDownload)?;
        let task = Download::from_record(&raw, &source, &work).ok_or(StorageError::InvalidDownload)?;
        Ok(task.chapters)
    }

    pub fn remove_invalid_download(&self, source: String, work: String) -> Result<(), StorageError> {
        let uid = format!("{source}:{work}");
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let raw: Option<String> = tx.query_row(
            "SELECT json FROM download_tasks WHERE source=? AND uid=?",
            params![source, uid], |row| row.get(0),
        ).optional()?;
        if let Some(raw) = raw {
            // 用户确认期间记录可能已恢复。此操作永远不访问下载目录。
            if Download::from_record(&raw, &source, &work).is_some() {
                return Err(StorageError::InvalidDownload);
            }
            tx.execute("DELETE FROM download_tasks WHERE source=? AND uid=?", params![source, uid])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_download_queued(
        &self,
        source: String,
        work: String,
        resume: bool,
    ) -> Result<(), StorageError> {
        let mut db = self.lock()?;
        Ok(park_storage::downloads::change_status(
            &mut db, &source, &work, resume,
        )?)
    }

    pub fn delete_download(
        &self,
        source: String,
        work_id: String,
        chapters: Vec<String>,
    ) -> Result<bool, StorageError> {
        let mut db = self.lock()?;
        Ok(park_storage::downloads::delete(
            &mut db, &source, &work_id, &chapters,
        )?)
    }

    pub fn video_file(&self, key: ChapterKey) -> Result<BundleFile, StorageError> {
        let root = self.download_directory(&key, "video")?;
        let raw: String = self.lock()?.query_row(
            "SELECT json FROM download_tasks WHERE source=? AND uid=?",
            params![key.source, format!("{}:{}", key.source, key.work_id)],
            |r| r.get(0),
        )?;
        let task: serde_json::Value =
            serde_json::from_str(&raw).map_err(|_| StorageError::InvalidDownload)?;
        let name = task["videoFiles"][&key.chapter_id]
            .as_str()
            .ok_or(StorageError::UnavailableDownload)?;
        let path = asset(&root, name)?;
        if path != root.join(name) {
            return Err(StorageError::InvalidDownload);
        }
        let resources = if path.extension().is_some_and(|s| s == "m3u8") {
            bundle::local_playlist(
                path.parent().ok_or(StorageError::InvalidDownload)?,
                path.file_name()
                    .and_then(|s| s.to_str())
                    .ok_or(StorageError::InvalidDownload)?,
            )?
        } else {
            Vec::new()
        };
        Ok(BundleFile {
            path: path
                .into_os_string()
                .into_string()
                .map_err(|_| StorageError::InvalidDownload)?,
            resources,
        })
    }
    pub fn chapter_images(
        &self,
        key: ChapterKey,
        cache_directory: String,
    ) -> Result<std::sync::Arc<ChapterImages>, StorageError> {
        let dir = self.chapter_directory(&key, "comic")?;
        let manifest: Manifest = serde_json::from_reader(
            File::open(asset(&dir, "pages.json")?)
                .map_err(|_| StorageError::UnavailableDownload)?,
        )
        .map_err(|_| StorageError::InvalidDownload)?;
        if manifest.pages.is_empty() || manifest.pages.len() > i32::MAX as usize {
            return Err(StorageError::InvalidDownload);
        }
        Ok(std::sync::Arc::new(ChapterImages(ChapterData::Files(OfflineImages {
            source: key.source,
            dir,
            pages: manifest.pages,
            pool: self.images.clone(),
            cache: PathBuf::from(cache_directory),
        }))))
    }
}

impl ProgressStore {
    fn download_directory(&self, key: &ChapterKey, media: &str) -> Result<PathBuf, StorageError> {
        let mut db = self.lock()?;
        let tx = db.transaction()?;
        let (directory, json): (String, String) = tx
            .query_row(
                "SELECT dir,json FROM download_tasks WHERE source=? AND uid=?",
                params![key.source, format!("{}:{}", key.source, key.work_id)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(StorageError::UnavailableDownload)?;
        let download: Download =
            serde_json::from_str(&json).map_err(|_| StorageError::InvalidDownload)?;
        if download.media != media
            || download.item.source != key.source
            || download.item.id != key.work_id
            || !download.chapters.iter().any(|c| c.id == key.chapter_id)
            || !download.available(&key.chapter_id)
        {
            return Err(StorageError::UnavailableDownload);
        }
        let root = park_storage::downloads::owned_directory(
            &tx,
            &key.source,
            &key.work_id,
            Path::new(&directory),
        )?;
        if !root.is_dir() {
            return Err(StorageError::UnavailableDownload);
        }
        Ok(root)
    }
    pub(crate) fn chapter_directory(
        &self,
        key: &ChapterKey,
        media: &str,
    ) -> Result<PathBuf, StorageError> {
        let root = self.download_directory(key, media)?;
        let expected = root.join(format!("{:x}", Sha256::digest(key.chapter_id.as_bytes())));
        let dir = expected
            .canonicalize()
            .map_err(|_| StorageError::UnavailableDownload)?;
        if dir != expected {
            return Err(StorageError::InvalidDownload);
        }
        Ok(dir)
    }
}

#[derive(uniffi::Object)]
pub struct ChapterImages(ChapterData);
enum ChapterData {
    Files(OfflineImages),
    Source(crate::source_service::RemoteChapter),
}
struct OfflineImages {
    source: String,
    dir: PathBuf,
    pages: Vec<Page>,
    pool: std::sync::Arc<park_media::ImagePool>,
    cache: PathBuf,
}
#[derive(uniffi::Object)]
pub struct PageImage {
    pub(crate) asset: std::sync::Arc<park_media::ImageAsset>,
    pub(crate) disk: Option<park_media::image_disk::ImageCacheEntry>,
    pub(crate) pool: std::sync::Arc<park_media::ImagePool>,
    pub(crate) key: park_media::ImageKey,
}
#[uniffi::export]
impl PageImage {
    pub fn invalidate(&self) {
        self.pool.invalidate(&self.key, &self.asset);
        if let Some(entry)=&self.disk {
            if let Ok(digest)=park_media::image_disk::checksum(self.asset.file.path()){entry.remove_if(&digest)}
        }
    }
    pub fn path(&self) -> Result<String, StorageError> {
        self.asset
            .file
            .path()
            .to_str()
            .map(str::to_owned)
            .ok_or(StorageError::InvalidDownload)
    }
}
#[uniffi::export]
impl ChapterImages {
    pub fn aspects(&self) -> Vec<f64> {
        let ratio = |size: (Option<f64>, Option<f64>)| match size {
                (Some(w), Some(h)) if w > 0.0 && h > 0.0 && (w / h).is_finite() => w / h,
                _ => 0.0,
        };
        match &self.0 {
            ChapterData::Files(files) => files.pages.iter().map(|p| ratio((p.width,p.height))).collect(),
            ChapterData::Source(chapter) => chapter.pages.iter().map(|p| ratio((p["width"].as_f64(),p["height"].as_f64()))).collect(),
        }
    }
    pub fn count(&self) -> i32 {
        match &self.0 {
            ChapterData::Files(files) => files.pages.len() as i32,
            ChapterData::Source(chapter) => chapter.pages.len() as i32,
        }
    }
    pub fn image(
        &self,
        index: i32,
        crop: bool,
        preload: bool,
        request: std::sync::Arc<crate::Request>,
    ) -> Result<std::sync::Arc<PageImage>, StorageError> {
        request.check()?;
        let index = usize::try_from(index).map_err(|_|StorageError::InvalidDownload)?;
        let priority = if preload {park_media::Priority::Preload} else {park_media::Priority::Visible};
        match &self.0 {
            ChapterData::Files(files) => files.image(index,crop,priority,request),
            ChapterData::Source(chapter) => {
                let page = chapter.pages.get(index).ok_or(StorageError::InvalidDownload)?;
                let ticket = chapter.ticket(index, strips(page.get("transform"))?, crop, priority);
                Ok(std::sync::Arc::new(PageImage {asset:ticket.wait(||request.cancelled.is_cancelled())?,disk:None,
                    pool:chapter.pool.clone(),key:ticket.key().clone()}))
            }
        }
    }
}
impl ChapterImages {
    pub(crate) fn remote(chapter: crate::source_service::RemoteChapter) -> Self {Self(ChapterData::Source(chapter))}
}
fn strips(transform: Option<&serde_json::Value>) -> Result<u32,StorageError> {
    transform.filter(|v| v["type"] == "unscramble-strips").and_then(|v|v["segments"].as_u64())
        .unwrap_or(0).try_into().map_err(|_|StorageError::InvalidDownload)
}
impl OfflineImages {
    fn image(&self,index:usize,crop:bool,priority:park_media::Priority,request:std::sync::Arc<crate::Request>) -> Result<std::sync::Arc<PageImage>, StorageError> {
        let page = self.pages.get(index).ok_or(StorageError::InvalidDownload)?;
        let path = asset(&self.dir, &page.file)?;
        let meta = path
            .metadata()
            .map_err(|_| StorageError::UnavailableDownload)?;
        let stamp = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let strips = strips(page.transform.as_ref())?;
        let identity = format!(
            "{}:{}:{}",
            path.to_str().ok_or(StorageError::InvalidDownload)?,
            meta.len(),
            stamp
        );
        let key = park_media::ImageKey {
            source: self.source.clone(),
            identity,
            strips,
            crop,
        };
        let cache = self.cache.clone();
        let ticket = self.pool.request(
            key.clone(),
            park_media::Route {
                host: String::from("local"),
                browser_session: None,
            },
            priority,
            move |token| {
                park_media::prepare(
                    park_media::PageFile::Original(path),
                    strips,
                    crop,
                    &cache,
                    token,
                )
            },
        );
        let asset = ticket.wait(|| request.check().is_err())?;
        Ok(std::sync::Arc::new(PageImage {
            asset,
            disk: None,
            pool: self.pool.clone(),
            key,
        }))
    }
}
