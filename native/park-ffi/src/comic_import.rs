use crate::{ProgressStore, Request, StorageError};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

type Result<T> = std::result::Result<T, StorageError>;

fn io<T>(result: std::io::Result<T>) -> Result<T> {
    result.map_err(|_| StorageError::IoFailure)
}

fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn natural(mut left: &str, mut right: &str) -> Ordering {
    fn part(s: &str) -> (&str, &str) {
        let digit = s.as_bytes()[0].is_ascii_digit();
        let end = s
            .char_indices()
            .find(|(_, ch)| ch.is_ascii_digit() != digit)
            .map_or(s.len(), |(offset, _)| offset);
        s.split_at(end)
    }
    while !left.is_empty() && !right.is_empty() {
        let (a, rest_a) = part(left);
        let (b, rest_b) = part(right);
        let order = if a.as_bytes()[0].is_ascii_digit() && b.as_bytes()[0].is_ascii_digit() {
            let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
            a.len().cmp(&b.len()).then_with(|| a.cmp(b))
        } else {
            a.cmp(b)
        };
        if order != Ordering::Equal {
            return order;
        }
        left = rest_a;
        right = rest_b;
    }
    left.len().cmp(&right.len())
}

fn info_field(xml: &str, key: &str) -> String {
    xml.split_once(&format!("<{key}>"))
        .and_then(|(_, rest)| rest.split_once(&format!("</{key}>")))
        .map(|(text, _)| {
            text.trim()
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&amp;", "&")
        })
        .unwrap_or_default()
}

fn stream(reader: &mut impl Read, writer: &mut impl Write, request: &Request) -> Result<u64> {
    let mut buffer = [0; 65536];
    let mut bytes = 0;
    loop {
        request.check()?;
        let n = io(reader.read(&mut buffer))?;
        if n == 0 {
            return Ok(bytes);
        }
        io(writer.write_all(&buffer[..n]))?;
        bytes += n as u64;
    }
}

struct ComicArchive {
    id: String,
    zip: zip::ZipArchive<File>,
    images: Vec<(String, usize)>,
    xml: String,
    title: String,
}

fn read_archive(archive_path: &str, filename: &str, request: &Request) -> Result<ComicArchive> {
    let mut file = io(File::open(archive_path))?;
    let mut digest = Sha1::new();
    stream(&mut file, &mut digest, request)?;
    let id = format!("{:x}", digest.finalize())[..16].to_owned();
    io(file.seek(SeekFrom::Start(0)))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| StorageError::InvalidDownload)?;
    let mut images = Vec::new();
    let mut info = None;
    for index in 0..zip.len() {
        request.check()?;
        let entry = zip
            .by_index_raw(index)
            .map_err(|_| StorageError::InvalidDownload)?;
        let name = entry.name().replace('\\', "/").to_lowercase();
        let basename = name.rsplit('/').next().unwrap_or("");
        if !entry.is_file() || basename.starts_with('.') {
            continue;
        }
        if basename == "comicinfo.xml" {
            info.get_or_insert(index);
        }
        if matches!(
            basename.rsplit_once('.').map(|(_, ext)| ext),
            Some("jpg" | "jpeg" | "png" | "webp" | "gif" | "avif" | "bmp")
        ) {
            images.push((name, index));
        }
    }
    if images.is_empty() {
        return Err(StorageError::InvalidDownload);
    }
    images.sort_by(|a, b| natural(&a.0, &b.0));
    let mut xml = Vec::new();
    if let Some(index) = info {
        let entry = zip
            .by_index(index)
            .map_err(|_| StorageError::InvalidDownload)?;
        if entry.size() > 2 * 1024 * 1024 {
            return Err(StorageError::InvalidDownload);
        }
        stream(&mut entry.take(2 * 1024 * 1024 + 1), &mut xml, request)?;
        if xml.len() > 2 * 1024 * 1024 {
            return Err(StorageError::InvalidDownload);
        }
    }
    let xml = String::from_utf8_lossy(&xml).into_owned();
    let mut title = info_field(&xml, "Title");
    if title.is_empty() {
        title = Path::new(filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or(StorageError::InvalidDownload)?
            .to_owned();
    }
    Ok(ComicArchive {
        id,
        zip,
        images,
        xml,
        title,
    })
}

fn extract_pages(
    zip: &mut zip::ZipArchive<File>,
    images: &[(String, usize)],
    files: &Path,
    first: usize,
    request: &Request,
) -> Result<(Vec<Value>, u64)> {
    let mut pages = Vec::with_capacity(images.len());
    let mut bytes = 0u64;
    for (position, (_, index)) in images.iter().enumerate() {
        let mut image = zip
            .by_index(*index)
            .map_err(|_| StorageError::InvalidDownload)?;
        let filename = format!("{:04}.img", position + first);
        bytes += stream(
            &mut image,
            &mut io(File::create(files.join(&filename)))?,
            request,
        )?;
        pages.push(json!({"index":position, "file":filename}));
    }
    Ok((pages, bytes))
}

fn imported_work(id: &str, title: &str, xml: &str, count: usize, cover: &Path) -> Result<Value> {
    let writer = info_field(xml, "Writer");
    let authors: Vec<_> = writer
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let tags = [info_field(xml, "Tags"), info_field(xml, "Genre")].join(",");
    let tags: Vec<_> = tags
        .split([',', '，'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let cover = url::Url::from_file_path(cover).map_err(|_| StorageError::InvalidDownload)?;
    Ok(
        json!({"source":"local","id":id,"title":title,"cover":cover.as_str(),"pages":count,"tags":tags,
              "authors":authors,"subtitle":writer,"description":info_field(xml,"Summary"),"mediaKind":"comic"}),
    )
}

#[uniffi::export]
impl ProgressStore {
    /// WinUI 按章节登记，页文件从零编号，完成清单与下载队列共用。
    pub fn import_windows_comic(
        &self,
        archive_path: String,
        filename: String,
        directory: String,
        chapter_title: String,
        request: Arc<Request>,
    ) -> Result<String> {
        let ComicArchive {
            id,
            mut zip,
            images,
            xml,
            title,
        } = read_archive(&archive_path, &filename, &request)?;
        let count = images.len();
        if count > 100_000 {
            return Err(StorageError::InvalidDownload);
        }
        let uid = format!("local:{id}:_");
        let leaf = hash(&uid)[..24].to_owned();
        let configured = Path::new(&directory);
        if !configured.is_absolute() {
            return Err(StorageError::InvalidDownload);
        }
        io(fs::create_dir_all(configured))?;
        let root = io(configured.canonicalize())?;
        let parent = root.join("local");
        io(fs::create_dir_all(&parent))?;
        if io(parent.canonicalize())? != parent {
            return Err(StorageError::InvalidDownload);
        }
        let stage = io(tempfile::Builder::new()
            .prefix(".park-import-")
            .tempdir_in(&parent))?;
        extract_pages(&mut zip, &images, stage.path(), 0, &request)?;
        let manifest = json!({"count":count,"completed":true});
        io(fs::write(
            stage.path().join("pages.json"),
            manifest.to_string(),
        ))?;

        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(String, String, String)> = tx
            .query_row(
                "SELECT source,dir,json FROM download_tasks WHERE uid=?",
                [&uid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let default_dir = configured.join("local").join(&leaf);
        let dest = if let Some((source, stored, raw)) = &previous {
            let prior: Value = serde_json::from_str(raw).map_err(|_| StorageError::Conflict)?;
            if source != "local"
                || prior["work"]["source"] != "local"
                || prior["work"]["id"] != id
                || prior["chapterId"] != "_"
                || prior["status"] != "Completed"
            {
                return Err(StorageError::Conflict);
            }
            let p = Path::new(stored);
            if !p.is_absolute()
                || p.file_name() != Some(leaf.as_ref())
                || p.parent().and_then(Path::file_name) != Some("local".as_ref())
            {
                return Err(StorageError::Conflict);
            }
            p
        } else {
            default_dir.as_path()
        };
        let canonical_parent = io(dest.parent().ok_or(StorageError::Conflict)?.canonicalize())?;
        let owned = canonical_parent.join(&leaf);
        if owned.exists() {
            if io(owned.canonicalize())? != owned {
                return Err(StorageError::Conflict);
            }
            let actual: Value = serde_json::from_reader(io(File::open(crate::offline::asset(
                &owned,
                "pages.json",
            )?))?)
            .map_err(|_| StorageError::Conflict)?;
            if actual != manifest {
                return Err(StorageError::Conflict);
            }
            for index in 0..count {
                let name = format!("{index:04}.img");
                let mut old = Sha256::new();
                let mut new = Sha256::new();
                stream(
                    &mut io(File::open(crate::offline::asset(&owned, &name)?))?,
                    &mut old,
                    &request,
                )?;
                stream(
                    &mut io(File::open(stage.path().join(name)))?,
                    &mut new,
                    &request,
                )?;
                if old.finalize() != new.finalize() {
                    return Err(StorageError::Conflict);
                }
            }
            if previous.is_some() {
                return Ok(id);
            }
        } else {
            if canonical_parent != parent || io(parent.canonicalize())? != parent {
                return Err(StorageError::Conflict);
            }
            request.check()?;
            io(fs::rename(stage.path(), &owned))?;
        }
        let item = imported_work(&id, &title, &xml, count, &dest.join("0000.img"))?;
        let task = json!({"work":item,"chapterId":"_","chapterTitle":chapter_title,
                        "status":"Completed","completed":count,"total":count});
        tx.execute("INSERT INTO download_tasks(uid,source,json,dir,created_at) VALUES(?1,'local',?2,?3,CAST(strftime('%s','now') AS INTEGER)*1000)
                    ON CONFLICT(uid) DO UPDATE SET json=excluded.json,dir=excluded.dir",params![uid,task.to_string(),dest.to_str().ok_or(StorageError::InvalidDownload)?])?;
        park_storage::library::upsert_summary(&tx, "local", &item)?;
        tx.commit()?;
        Ok(id)
    }

    pub fn import_comic(
        &self,
        archive_path: String,
        filename: String,
        directory: String,
        chapter_title: String,
        request: Arc<Request>,
    ) -> Result<String> {
        let ComicArchive {
            id,
            mut zip,
            images,
            xml,
            title,
        } = read_archive(&archive_path, &filename, &request)?;
        let uid = format!("local:{id}");
        let configured = Path::new(&directory);
        if !configured.is_absolute() {
            return Err(StorageError::InvalidDownload);
        }
        io(fs::create_dir_all(configured))?;
        let root = io(configured.canonicalize())?;
        let recorded: Option<String> = self
            .lock()?
            .query_row("SELECT dir FROM download_tasks WHERE uid=?", [&uid], |r| {
                r.get(0)
            })
            .optional()?;
        let dest = recorded
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(hash("local")).join(hash(&id)));
        let dest = {
            let db = self.lock()?;
            park_storage::downloads::owned_directory(&db, "local", &id, &dest)?
        };
        let parent = dest.parent().ok_or(StorageError::InvalidDownload)?;
        io(fs::create_dir_all(parent))?;
        let stage = io(tempfile::Builder::new()
            .prefix(".park-import-")
            .tempdir_in(parent))?;
        let chapter = hash("_");
        let files = stage.path().join(&chapter);
        io(fs::create_dir(&files))?;
        let count = images.len();
        let (pages, bytes) = extract_pages(&mut zip, &images, &files, 1, &request)?;
        io(fs::write(
            files.join("pages.json"),
            json!({"title":title,"pages":pages}).to_string(),
        ))?;
        let item = imported_work(
            &id,
            &title,
            &xml,
            count,
            &dest.join(&chapter).join("0001.img"),
        )?;
        io(fs::write(stage.path().join("work.json"), item.to_string()))?;
        let task = json!({"item":item,"media":"comic","chapters":[{"id":"_","title":chapter_title}],
                          "completed":["_"],"status":3,"done":count,"total":count,"bytes":bytes});
        request.check()?;
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        park_storage::downloads::owned_directory(&tx, "local", &id, &dest)?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT dir,json FROM download_tasks WHERE uid=?",
                [&uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((directory, raw)) = &existing {
            let old: Value =
                serde_json::from_str(raw).map_err(|_| StorageError::InvalidDownload)?;
            if Path::new(directory) != dest
                || old["item"]["source"] != "local"
                || old["item"]["id"] != id
                || old["status"] != 3
                || !old["deletingChapters"].is_null()
            {
                return Err(StorageError::Conflict);
            }
        }
        if dest.exists() {
            // 重入只复用内容相同的资产。数据库提交前中断留下的完整目录也可由同一个包认领。
            let manifest = dest.join(&chapter).join("pages.json");
            if !manifest.is_file() || manifest.canonicalize().ok().as_ref() != Some(&manifest) {
                return Err(StorageError::Conflict);
            }
            let old: Value = serde_json::from_reader(io(File::open(manifest))?)
                .map_err(|_| StorageError::InvalidDownload)?;
            let records = old["pages"]
                .as_array()
                .filter(|p| p.len() == count)
                .ok_or(StorageError::Conflict)?;
            for (index, page) in records.iter().enumerate() {
                let filename = format!("{:04}.img", index + 1);
                if page["file"] != filename {
                    return Err(StorageError::Conflict);
                }
                let stored = crate::offline::asset(&dest.join(&chapter), &filename)?;
                let mut before = Sha256::new();
                let mut after = Sha256::new();
                stream(&mut io(File::open(stored))?, &mut before, &request)?;
                stream(
                    &mut io(File::open(files.join(filename)))?,
                    &mut after,
                    &request,
                )?;
                if before.finalize() != after.finalize() {
                    return Err(StorageError::Conflict);
                }
            }
            if existing.is_some() {
                return Ok(id);
            }
        } else {
            io(fs::rename(stage.path(), &dest))?;
        }
        tx.execute("INSERT INTO download_tasks(uid,source,json,dir,created_at) VALUES(?1,'local',?2,?3,CAST(strftime('%s','now') AS INTEGER)*1000)
                    ON CONFLICT(uid) DO UPDATE SET json=excluded.json,dir=excluded.dir",params![uid,task.to_string(),dest.to_str().ok_or(StorageError::InvalidDownload)?])?;
        park_storage::library::upsert_summary(&tx, "local", &item)?;
        tx.commit()?;
        Ok(id)
    }
}
