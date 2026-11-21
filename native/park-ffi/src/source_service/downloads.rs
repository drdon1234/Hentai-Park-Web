use super::*;
mod bundle;
use crate::{ProgressStore, StorageError};
use park_media::Priority;
use super::comic::page_identity;
use park_storage::downloads as queue;
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

/// 下载任务的持久书目。媒体授权只在进程内使用。
#[derive(uniffi::Record)]
pub struct DownloadBook {
    pub source: String,
    pub work: String,
    pub title: String,
    pub media: String,
    pub domain: String,
    pub cover: Option<String>,
    pub single_chapter_id: Option<String>,
}

#[uniffi::export]
impl ProgressStore {
    pub fn enqueue_download(
        &self, book: DownloadBook, chapters: Vec<crate::OfflineChapter>, directory: String,
        lines: Option<Arc<VideoLines>>, index: Option<u32>,
    ) -> std::result::Result<(), StorageError> {
        use rusqlite::TransactionBehavior;
        let invalid = StorageError::InvalidDownload;
        if book.source.is_empty() || book.work.is_empty() || book.title.trim().is_empty()
            || book.domain == "live" || chapters.is_empty()
            || chapters.iter().any(|c| c.id.is_empty() || c.title.trim().is_empty())
            || book.single_chapter_id.as_ref().is_some_and(|id| chapters.len() != 1 || &chapters[0].id != id) {
            return Err(invalid);
        }
        let choice = match (book.media.as_str(), lines, index) {
            ("video", Some(lines), Some(index)) if !lines.live && lines.owner == book.source
                && lines.episode.as_ref() == Some(&(book.work.clone(), chapters[0].id.clone())) => {
                Some(park_media::download::selection(&lines.grant, index as usize, &chapters[0].id)
                    .map_err(|_| StorageError::InvalidDownload)?)
            }
            ("comic" | "novel", None, None) => None,
            _ => return Err(invalid),
        };
        let chapters: Vec<_> = chapters.iter().map(|c| json!({"id": c.id, "title": c.title})).collect();
        let mut item = json!({"id": book.work, "title": book.title, "mediaKind": book.media,
                              "contentDomain": book.domain});
        if let Some(cover) = book.cover { item["cover"] = cover.into(); }
        if let Some(chapter) = book.single_chapter_id { item["singleChapterId"] = chapter.into(); }
        else { item["chapters"] = json!(chapters); }
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        queue::enqueue_chapters(&tx, &book.source, &item, &chapters, Path::new(&directory),
            &book.media, choice.as_ref(), crate::tavern::now()?)?;
        tx.commit()?;
        Ok(())
    }
}

fn io<T>(result: std::io::Result<T>) -> std::result::Result<T, queue::Error> {
    result.map_err(|_| queue::Error::Unavailable)
}
pub(super) fn checked(value: Value) -> Result<Value> {
    if value["ok"] == true {
        Ok(value["value"].clone())
    } else {
        Err(match value["error"]["code"].as_str() {
            Some("LOGIN_REQUIRED") => SourceFailure::LoginRequired,
            Some("CONFIG") => SourceFailure::Configuration,
            Some("UNSUPPORTED") => SourceFailure::Unsupported,
            Some("CONFIRM_REQUIRED" | "VERIFICATION_REQUIRED" | "CLOUDFLARE") => SourceFailure::ConfirmationRequired,
            Some("CANCELLED") => SourceFailure::Cancelled,
            _ => SourceFailure::Unavailable,
        })
    }
}
fn storage(_: StorageError) -> SourceFailure {
    SourceFailure::Storage
}
fn atomic(
    dir: &Path,
    name: &str,
    write: impl FnOnce(&mut fs::File) -> std::io::Result<()>,
) -> std::result::Result<(), queue::Error> {
    let mut file = io(tempfile::NamedTempFile::new_in(dir))?;
    io(write(file.as_file_mut()))?;
    io(file.as_file().sync_all())?;
    file.persist(dir.join(name))
        .map_err(|_| queue::Error::Unavailable)?;
    Ok(())
}
struct ChapterJob<'a> {
    store: &'a ProgressStore,
    service: &'a SourceService,
    source: &'a str,
    work: &'a str,
    chapter: &'a str,
    root: &'a Path,
    selection: Option<&'a Value>,
    request: &'a Request,
}
impl ChapterJob<'_> {
    fn write<T>(
        &self,
        op: impl FnOnce(&Path) -> std::result::Result<T, queue::Error>,
    ) -> Result<T> {
        self.request.check().map_err(|_| SourceFailure::Cancelled)?;
        let mut db = self.store.lock().map_err(storage)?;
        queue::with_chapter(&mut db, self.source, self.work, self.chapter, self.root, self.selection, op)
            .map_err(|_| SourceFailure::Storage)
    }
    fn call(&self, method: &str, args: &[Value]) -> Result<Value> {
        let (_, value) = self.service.catalog.invoke(
            self.source,
            method,
            args,
            Invocation::child(Duration::from_secs(60), &self.request.cancelled),
        )?;
        checked(value)
    }
    fn novel(&self) -> Result<u64> {
        let body = self.service.novel_body(self.source, self.work, self.chapter, self.request)?;
        let bytes = serde_json::to_vec(&body).map_err(|_| SourceFailure::Unsupported)?;
        self.write(|dir| atomic(dir, "text.json", |file| file.write_all(&bytes)))?;
        Ok(bytes.len() as u64)
    }
    fn video(&self, plan: &queue::DownloadPlan) -> Result<(u64, String)> {
        let req = Arc::new(Request {
            cancelled: self.request.cancelled.child_token(),
        });
        let lines = self.service.video_lines(
            self.source.to_owned(),
            self.work.to_owned(),
            self.chapter.to_owned(),
            req,
        )?;
        let line = park_media::download::select(
            &lines.grant,
            &plan.selection,
            plan.quality,
            self.chapter,
        )?;
        let name = format!(
            "video_{}p.{}",
            line.quality,
            park_media::download::extension(line, false)?
        );
        let (path, size) = self.media(line, &name, false)?;
        let relative = path
            .strip_prefix(self.root)
            .ok()
            .and_then(Path::to_str)
            .ok_or(SourceFailure::Storage)?;
        Ok((size, relative.to_owned()))
    }
    fn media(&self, line: &VideoSource, name: &str, playlist: bool) -> Result<(PathBuf, u64)> {
        let dir = self.write(|dir| Ok(dir.to_owned()))?;
        let host = &self.service.routes;
        let session = host
            .sessions
            .partition(self.source, Purpose::Media)
            .map_err(|_| SourceFailure::Storage)?;
        let token = self.request.cancelled.child_token();
        let part = host.runtime.block_on(async {
            let save = park_media::download::save(
                host.transport.clone(), session, line.clone(), &dir, name, playlist, self.selection, &token,
            );
            tokio::pin!(save);
            if let Some(selection) = self.selection {
                // 另一个进程也能重新选线。等待网络数据期间检测任务，取消后等传输退出再交还作品锁。
                loop {
                    if let Ok(result) = tokio::time::timeout(Duration::from_secs(1), &mut save).await {
                        return result.map_err(SourceFailure::from);
                    }
                    let current = {
                        let db = self.store.lock().map_err(storage)?;
                        queue::video_pending(&db, self.source, self.work, selection)
                    };
                    if !matches!(current, Ok(true)) {
                        token.cancel();
                        let _ = save.await;
                        return Err(SourceFailure::Cancelled);
                    }
                }
            }
            save.await.map_err(SourceFailure::from)
        })?;
        self.write(|dir| {
            if part.parent() != Some(dir) {
                return Err(queue::Error::Invalid);
            }
            let file = dir.join(name);
            io(fs::rename(&part, &file))?;
            let mut size = io(file.metadata())?.len();
            if name.ends_with(".m3u8") {
                for asset in
                    crate::offline::local_playlist(dir, name).map_err(|_| queue::Error::Invalid)?
                {
                    size = size
                        .checked_add(io(fs::metadata(asset))?.len())
                        .ok_or(queue::Error::Invalid)?;
                }
            }
            Ok((file, size))
        })
    }
    fn comic(&self, title: &str, workers: usize) -> Result<u64> {
        let chapter = self.service.comic_pages(self.store, crate::ChapterKey {
            source: self.source.into(), work_id: self.work.into(), chapter_id: self.chapter.into(),
        }, self.request)?;
        let pages = &chapter.pages;
        let dir = self.write(|dir| Ok(dir.to_owned()))?;
        let old = fs::File::open(dir.join("pending.json"))
            .ok()
            .and_then(|f| serde_json::from_reader::<_, Value>(f.take(16 * 1024 * 1024)).ok());
        let cursor = AtomicUsize::new(0);
        let cancel = self.request.cancelled.child_token();
        let results = std::thread::scope(|threads| {
            let mut joins = Vec::new();
            for _ in 0..workers.min(pages.len()) {
                joins.push(threads.spawn(|| {
                    let mut result = Vec::new();
                    loop {
                        if cancel.is_cancelled() {
                            return Err(SourceFailure::Cancelled);
                        }
                        let index = cursor.fetch_add(1, Ordering::Relaxed);
                        if index >= pages.len() { break; }
                        let item = self.image(
                            &chapter,
                            &dir,
                            index,
                            old.as_ref().and_then(|p| p["pages"].get(index)),
                            &cancel,
                        );
                        match item {
                            Ok((name, bytes)) => result.push((index, name, bytes)),
                            Err(e) => {
                                cancel.cancel();
                                return Err(e);
                            }
                        }
                    }
                    Ok(result)
                }));
            }
            let mut results = Vec::new();
            let mut failure = None;
            for join in joins {
                match join.join().unwrap_or(Err(SourceFailure::Unavailable)) {
                    Ok(mut items) => results.append(&mut items),
                    Err(e)
                        if failure.is_none()
                            || matches!(failure, Some(SourceFailure::Cancelled)) =>
                    {
                        failure = Some(e)
                    }
                    Err(_) => (),
                }
            }
            failure.map_or(Ok(results), Err)
        })?;
        let mut entries = vec![Value::Null; pages.len()];
        let mut size = 0;
        for (i, file, bytes) in results {
            let mut record = json!({"index":i,"file":file});
            for field in ["width", "height", "transform"] {
                if let Some(v) = pages[i].get(field) {
                    record[field] = v.clone();
                }
            }
            entries[i] = record;
            size += bytes;
        }
        let bytes = serde_json::to_vec(&json!({"title":title,"pages":entries}))
            .map_err(|_| SourceFailure::Storage)?;
        self.write(|dir| atomic(dir, "pages.json", |file| file.write_all(&bytes)))?;
        Ok(size)
    }
    fn image(
        &self,
        chapter: &RemoteChapter,
        dir: &Path,
        index: usize,
        old: Option<&Value>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(String, u64)> {
        let digest = page_identity(&chapter.pages[index]);
        let name = format!("{:04}-{digest}.img", index + 1);
        let mut names = vec![name.clone()];
        if old.is_some_and(|p| page_identity(p) == digest) {
            names.push(format!("{:04}.img", index + 1));
        }
        for possible in names {
            let path = dir.join(&possible);
            if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
                && path.canonicalize().ok().as_ref() == Some(&path)
                && park_media::prepare(
                    park_media::PageFile::Original(path.clone()),
                    0,
                    false,
                    &self.service.routes.cache,
                    cancel,
                )
                .is_ok()
            {
                return Ok((
                    possible,
                    fs::metadata(path)
                        .map_err(|_| SourceFailure::Storage)?
                        .len(),
                ));
            }
        }
        let ticket = chapter.ticket(index, 0, false, Priority::Background);
        let image = ticket.wait(|| cancel.is_cancelled())?;
        let size = self.write(|dir| {
            let mut input = io(fs::File::open(image.file.path()))?;
            let size = io(input.metadata())?.len();
            atomic(dir, &name, |file| {
                std::io::copy(&mut input, file)?;
                Ok(())
            })?;
            Ok(size)
        })?;
        Ok((name, size))
    }
}
#[uniffi::export]
impl SourceService {
    pub fn run_download(
        &self,
        store: Arc<ProgressStore>,
        source: String,
        work: String,
        sites: u32,
        works: u32,
        pages: u32,
        request: Arc<Request>,
    ) -> Result<bool> {
        let Some(_slot) = queue::DownloadSlot::acquire(
            &store.root,
            &source,
            &work,
            sites as usize,
            works as usize,
        )
        .map_err(|_| SourceFailure::Storage)?
        else {
            return Ok(false);
        };
        let plan = {
            let mut db = store.lock().map_err(storage)?;
            match queue::plan(&db, &source, &work) {
                Ok(plan) => plan,
                Err(_) => {
                    let _ = queue::record_failure(&mut db, &source, &work, None);
                    return Err(SourceFailure::Storage);
                }
            }
        };
        let selection = (plan.media == "video").then_some(&plan.selection);
        let result = (|| {
            if plan.chapters.is_empty() {
                return Err(SourceFailure::Unsupported);
            }
            let mut complete = false;
            for (chapter, title) in &plan.chapters {
                let job = ChapterJob {
                    store: &store,
                    service: self,
                    source: &source,
                    work: &work,
                    chapter,
                    root: &plan.directory,
                    selection,
                    request: &request,
                };
                let (bytes, video) = match plan.media.as_str() {
                    "comic" => (job.comic(title, pages.clamp(1, 8) as usize)?, None),
                    "novel" => (job.novel()?, None),
                    "video" => {
                        let (size, path) = job.video(&plan)?;
                        (size, Some(path))
                    }
                    "bundle" => (job.bundle(&plan)?, None),
                    _ => return Err(SourceFailure::Unsupported),
                };
                request.check().map_err(|_| SourceFailure::Cancelled)?;
                let mut db = store.lock().map_err(storage)?;
                complete = queue::finish_chapter(&mut db, &source, &work, chapter, bytes, video.as_deref(), selection)
                    .map_err(|_| SourceFailure::Storage)?;
            }
            Ok(complete)
        })();
        if result.is_err() && !request.cancelled.is_cancelled() {
            let mut db = store.lock().map_err(storage)?;
            if !queue::record_failure(&mut db, &source, &work, selection).map_err(|_| SourceFailure::Storage)? {
                // 书库已接受其他窗口的选择，由调度器领取最新任务。
                return Ok(false);
            }
        }
        result
    }
}
