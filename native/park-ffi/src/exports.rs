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
