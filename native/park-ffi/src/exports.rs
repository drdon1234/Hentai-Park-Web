use crate::{ChapterKey, ProgressStore, Request, StorageError};
use park_media::exports::{self, ComicInfo};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use std::{collections::HashMap, path::Path, sync::Arc};

#[derive(uniffi::Enum)]
pub enum ComicFormat {
    Cbz,
    Pdf,
}

fn tags(value: &Value, namespace: &str, output: &mut Vec<String>) {
    if let Some(list) = value.as_array() {
        for entry in list {
            if let Some(name) = entry.as_str() {
                output.push(format!("{namespace}{name}"));
                continue;
            }
            let prefix = entry["namespace"]
                .as_str()
                .filter(|n| !n.is_empty())
                .map(|n| format!("{namespace}{n}:"))
                .unwrap_or_else(|| namespace.to_owned());
            if entry["tags"].is_array() {
                tags(&entry["tags"], &prefix, output)
            } else if let Some(name) = entry["name"].as_str() {
                output.push(format!("{prefix}{name}"));
            }
        }
    }
}
impl From<park_media::Error> for StorageError {
    fn from(error: park_media::Error) -> Self {
        match error {
            park_media::Error::Cancelled => Self::Cancelled,
            park_media::Error::Unavailable => Self::UnavailableDownload,
            park_media::Error::Storage => Self::IoFailure,
            _ => Self::InvalidDownload,
        }
    }
}
#[uniffi::export]
impl ProgressStore {
    /// 将单集已完成的视频复制到系统分享暂存区。HLS 仅打包清单引用的本地文件。
    pub fn export_video_episode(&self, key: ChapterKey, destination: String, request: Arc<Request>)
        -> Result<String, StorageError> {
        use std::io::{Read, Write};
        use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};
        let episode = self.video_file(key)?;
        let main = Path::new(&episode.path);
        let origin = main.parent().ok_or(StorageError::InvalidDownload)?;
        let name = main.file_name().ok_or(StorageError::InvalidDownload)?;
        let dir = Path::new(&destination).canonicalize().map_err(|_| StorageError::IoFailure)?;
        let mut staged = tempfile::NamedTempFile::new_in(&dir).map_err(|_| StorageError::IoFailure)?;
        let copy = |path: &Path, output: &mut dyn Write| -> Result<(), StorageError> {
            let mut input = std::fs::File::open(path).map_err(|_| StorageError::UnavailableDownload)?;
            let mut buffer = [0; 65536];
            loop {
                request.check().map_err(|_| StorageError::Cancelled)?;
                let n = input.read(&mut buffer).map_err(|_| StorageError::IoFailure)?;
                if n == 0 { return Ok(()); }
                output.write_all(&buffer[..n]).map_err(|_| StorageError::IoFailure)?;
            }
        };
        let mut target = dir.join(name);
        if main.extension().is_some_and(|ext| ext == "m3u8") {
            let mut archive = ZipWriter::new(staged.as_file_mut());
            let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored).large_file(true);
            for path in std::iter::once(main).chain(episode.resources.iter().map(Path::new)) {
                let entry = path.strip_prefix(origin).ok().and_then(Path::to_str).ok_or(StorageError::InvalidDownload)?;
                archive.start_file(entry.replace(std::path::MAIN_SEPARATOR, "/"), options).map_err(|_| StorageError::IoFailure)?;
                copy(path, &mut archive)?;
            }
            archive.finish().map_err(|_| StorageError::IoFailure)?;
            target.set_extension("zip");
        } else { copy(main, staged.as_file_mut())?; }
        staged.as_file().sync_all().map_err(|_| StorageError::IoFailure)?;
        request.check().map_err(|_| StorageError::Cancelled)?;
        staged.persist_noclobber(&target).map_err(|_| StorageError::IoFailure)?;
        target.into_os_string().into_string().map_err(|_| StorageError::InvalidDownload)
    }

    /// Windows 每章拥有一条下载记录和一个独立的完成清单。
    pub fn export_windows_comic(
        &self,
        source: String,
        work_id: String,
        chapters: Vec<String>,
        format: ComicFormat,
        max_pages: i32,
        download_directory: String,
        locale: String,
        request: Arc<Request>,
    ) -> Result<Vec<String>, StorageError> {
        if chapters.is_empty() || !(0..=5000).contains(&max_pages) {
            return Err(StorageError::InvalidDownload);
        }
        let root = Path::new(&download_directory)
            .canonicalize()
            .map_err(|_| StorageError::UnavailableDownload)?;
        let output = root.join("exports");
        std::fs::create_dir_all(&output).map_err(|_| StorageError::IoFailure)?;
        if output.canonicalize().map_err(|_| StorageError::IoFailure)? != output {
            return Err(StorageError::InvalidDownload);
        }
        let stage = tempfile::Builder::new()
            .prefix(".park-export-")
            .tempdir_in(&output)
            .map_err(|_| StorageError::IoFailure)?;
        let mut staged = Vec::new();
        let copy = park_domain::LocalizedCopy::new(&locale, Value::Null);
        let mut seen = std::collections::HashSet::new();
        let check = || request.check().map_err(|_| park_media::Error::Cancelled);
        for chapter in chapters {
            request.check()?;
            if !seen.insert(chapter.clone()) {
                return Err(StorageError::InvalidDownload);
            }
            let (raw, dir): (String, String) = self
                .lock()?
                .query_row(
                    "SELECT json,dir FROM download_tasks WHERE uid=?1 AND source=?2",
                    params![format!("{source}:{work_id}:{chapter}"), source],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or(StorageError::UnavailableDownload)?;
            let task: Value =
                serde_json::from_str(&raw).map_err(|_| StorageError::InvalidDownload)?;
            let work = &task["work"];
            if task["status"] != "Completed"
                || task["chapterId"] != chapter
                || work["id"] != work_id
                || work["mediaKind"].as_str().unwrap_or("comic") != "comic"
            {
                return Err(StorageError::InvalidDownload);
            }
            let folder = Path::new(&dir)
                .canonicalize()
                .map_err(|_| StorageError::UnavailableDownload)?;
            if !folder.starts_with(&root) || folder == root || folder.starts_with(&output) {
                return Err(StorageError::InvalidDownload);
            }
            let owned = |name: &str| -> Result<_, StorageError> {
                let path = folder.join(name);
                if path
                    .canonicalize()
                    .map_err(|_| StorageError::UnavailableDownload)?
                    != path
                    || !path.is_file()
                {
                    return Err(StorageError::InvalidDownload);
                }
                Ok(path)
            };
            let manifest: Value = serde_json::from_reader(
                std::fs::File::open(owned("pages.json")?)
                    .map_err(|_| StorageError::UnavailableDownload)?,
            )
            .map_err(|_| StorageError::InvalidDownload)?;
            let count = manifest["count"]
                .as_u64()
                .filter(|n| (1..=100_000).contains(n))
                .ok_or(StorageError::InvalidDownload)?;
            if manifest["completed"] != true {
                return Err(StorageError::UnavailableDownload);
            }
            let mut pages = Vec::new();
            for index in 0..count {
                let page = park_media::prepare(
                    park_media::PageFile::Original(owned(&format!("{index:04}.img"))?),
                    0,
                    false,
                    stage.path(),
                    &request.cancelled,
                )?;
                pages.push(Arc::new(page));
            }
            let title = work["title"].as_str().unwrap_or(&work_id);
            let label = task["chapterTitle"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(&chapter);
            let basename = format!(
                "{} - {}",
                exports::filename(title),
                exports::filename(label)
            );
            match format {
                ComicFormat::Cbz => {
                    let mut names = Vec::new();
                    tags(&work["tags"], "", &mut names);
                    let writer = work["authors"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let info = ComicInfo {
                        title: label.into(),
                        series: title.into(),
                        writer,
                        tags: names.join(", "),
                        web: work["url"].as_str().unwrap_or("").into(),
                    };
                    staged.push(exports::cbz(
                        stage.path(),
                        &basename,
                        &info,
                        &pages,
                        &check,
                    )?);
                }
                ComicFormat::Pdf => staged.extend(exports::pdf_volumes(
                    stage.path(),
                    &basename,
                    &format!("{title} - {label}"),
                    &pages,
                    max_pages as usize,
                    |n| {
                        copy.text(
                            "第 {value0} 部分",
                            &HashMap::from([("value0".to_owned(), n.to_string())]),
                        )
                    },
                    &check,
                )?),
            }
        }
        Ok(exports::publish(&output, staged, &check)?
            .into_iter()
            .filter_map(|p| p.file_name()?.to_str().map(str::to_owned))
            .collect())
    }

    pub fn export_comic(
        &self,
        source: String,
        work_id: String,
        format: ComicFormat,
        max_pages: i32,
        directory: String,
        cache_directory: String,
        locale: String,
        request: Arc<Request>,
    ) -> Result<Vec<String>, StorageError> {
        if !(0..=5000).contains(&max_pages) {
            return Err(StorageError::InvalidDownload);
        }
        request.check()?;
        let raw: String = self
            .lock()?
            .query_row(
                "SELECT json FROM download_tasks WHERE uid=?1 AND source=?2",
                params![format!("{source}:{work_id}"), source],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StorageError::UnavailableDownload)?;
        let task: Value = serde_json::from_str(&raw).map_err(|_| StorageError::InvalidDownload)?;
        let item = &task["item"];
        if task["media"] != "comic" || item["source"] != source || item["id"] != work_id {
            return Err(StorageError::InvalidDownload);
        }
        let chapters = task["chapters"]
            .as_array()
            .ok_or(StorageError::InvalidDownload)?;
        let completed = task["completed"]
            .as_array()
            .ok_or(StorageError::UnavailableDownload)?;
        let chapters: Vec<_> = chapters
            .iter()
            .filter(|c| completed.contains(&c["id"]))
            .collect();
        if chapters.is_empty() {
            return Err(StorageError::UnavailableDownload);
        }
        let output = Path::new(&directory);
        std::fs::create_dir_all(output).map_err(|_| StorageError::IoFailure)?;
        let staging = tempfile::Builder::new()
            .prefix(".park-export-")
            .tempdir_in(output)
            .map_err(|_| StorageError::IoFailure)?;
        let copy = park_domain::LocalizedCopy::new(&locale, Value::Null);
        let part = |n: usize| {
            copy.text(
                "第 {value0} 部分",
                &HashMap::from([("value0".to_owned(), n.to_string())]),
            )
        };
        let check = || request.check().map_err(|_| park_media::Error::Cancelled);
        let title = item["title"].as_str().unwrap_or("");
        let writer = item["authors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let mut tag_list = Vec::new();
        tags(&item["tags"], "", &mut tag_list);
        let mut staged = Vec::new();
        for chapter in &chapters {
            let key = ChapterKey {
                source: source.clone(),
                work_id: work_id.clone(),
                chapter_id: chapter["id"]
                    .as_str()
                    .ok_or(StorageError::InvalidDownload)?
                    .to_owned(),
            };
            let images = self.chapter_images(key, cache_directory.clone())?;
            let mut pages = Vec::new();
            for index in 0..images.count() {
                pages.push(
                    images
                        .image(index, false, false, request.clone())?
                        .asset
                        .clone(),
                );
            }
            let chapter_title = chapter["title"].as_str().unwrap_or("");
            let (basename, label) = if chapters.len() > 1 {
                (
                    format!(
                        "{} - {}",
                        exports::filename(title),
                        exports::filename(chapter_title)
                    ),
                    chapter_title,
                )
            } else {
                (exports::filename(title), title)
            };
            match format {
                ComicFormat::Cbz => {
                    let info = ComicInfo {
                        title: label.into(),
                        series: title.into(),
                        writer: writer.clone(),
                        tags: tag_list.join(", "),
                        web: item["url"].as_str().unwrap_or("").into(),
                    };
                    staged.push(exports::cbz(
                        staging.path(),
                        &basename,
                        &info,
                        &pages,
                        &check,
                    )?);
                }
                ComicFormat::Pdf => {
                    let label = if chapters.len() > 1 {
                        format!("{title} - {chapter_title}")
                    } else {
                        title.to_owned()
                    };
                    staged.extend(exports::pdf_volumes(
                        staging.path(),
                        &basename,
                        &label,
                        &pages,
                        max_pages as usize,
                        part,
                        &check,
                    )?);
                }
            }
        }
        Ok(exports::publish(output, staged, &check)?
            .into_iter()
            .filter_map(|p| p.file_name()?.to_str().map(str::to_owned))
            .collect())
    }
}
