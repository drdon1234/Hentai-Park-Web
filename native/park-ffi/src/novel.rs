use crate::{offline::asset, ChapterKey, ProgressStore, StorageError};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, io::ErrorKind};

#[derive(uniffi::Record)]
pub struct NovelDocument {
    pub title: String,
    pub text: String,
    pub notes: Vec<NovelNote>,
    pub translation: Vec<TranslatedPassage>,
}

#[derive(uniffi::Record)]
pub struct TranslatedPassage {
    pub start: i64,
    pub end: i64,
    pub translation: String,
}

#[derive(uniffi::Enum)]
pub enum NovelTextMode {
    Original,
    Translated,
    Bilingual,
}

#[derive(uniffi::Object)]
pub struct NovelPresentation(park_domain::NovelText);

#[uniffi::export]
impl NovelPresentation {
    #[uniffi::constructor]
    pub fn new(
        original: String,
        translation: Vec<TranslatedPassage>,
        mode: NovelTextMode,
    ) -> Result<Self, StorageError> {
        let mode = match mode {
            NovelTextMode::Original => park_domain::NovelMode::Original,
            NovelTextMode::Translated => park_domain::NovelMode::Translated,
            NovelTextMode::Bilingual => park_domain::NovelMode::Bilingual,
        };
        park_domain::NovelText::new(
            original,
            mode,
            translation
                .iter()
                .map(|t| (t.start, t.end, t.translation.as_str())),
        )
        .filter(|v| v.valid_offsets())
        .map(Self)
        .ok_or(StorageError::InvalidDownload)
    }
    pub fn text(&self) -> String {
        self.0.text.clone()
    }
    pub fn displayed(&self, offset: i32) -> i32 {
        self.0.displayed(offset.max(0) as usize) as i32
    }
    pub fn source(&self, offset: i32) -> i32 {
        self.0.source(offset.max(0) as usize) as i32
    }
}

#[derive(uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NovelNote {
    pub id: String,
    pub uid: String,
    pub chapter_id: String,
    pub offset: i64,
    pub quote: String,
    pub note: String,
    pub time: i64,
}

#[derive(Deserialize)]
pub(crate) struct ChapterText {
    #[serde(default)]
    title: String,
    paragraphs: Vec<String>,
}

#[uniffi::export]
impl ProgressStore {
    pub fn offline_novel(
        &self,
        key: ChapterKey,
        locale: String,
        engine: String,
    ) -> Result<NovelDocument, StorageError> {
        let directory = self.chapter_directory(&key, "novel")?;
        let file = fs::File::open(asset(&directory, "text.json")?)
            .map_err(|_| StorageError::UnavailableDownload)?;
        let chapter: ChapterText =
            serde_json::from_reader(file).map_err(|_| StorageError::InvalidDownload)?;
        self.novel_document(&key, chapter, &locale, &engine)
    }
}

impl ProgressStore {
    pub(crate) fn novel_document(
        &self,
        key: &ChapterKey,
        chapter: ChapterText,
        locale: &str,
        engine: &str,
    ) -> Result<NovelDocument, StorageError> {
        let text = chapter
            .paragraphs
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if text.is_empty() {
            return Err(StorageError::InvalidDownload);
        }
        let uid = format!("{}:{}", key.source, key.work_id);
        let mut notes = match fs::read(self.root.join("reading_notes.json")) {
            Ok(bytes) => {
                let rows: Vec<serde_json::Value> =
                    serde_json::from_slice(&bytes).map_err(|_| StorageError::IoFailure)?;
                let mut notes = std::collections::BTreeMap::new();
                for note in rows
                    .into_iter()
                    .filter_map(|r| serde_json::from_value::<NovelNote>(r).ok())
                {
                    if note.offset >= 0 && note.time.unsigned_abs() <= 8_640_000_000_000_000 {
                        notes.insert(note.id.clone(), note);
                    }
                }
                notes
                    .into_values()
                    .filter(|n| n.uid == uid && n.chapter_id == key.chapter_id)
                    .collect::<Vec<_>>()
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(_) => return Err(StorageError::IoFailure),
        };
        notes.sort_by_key(|n| std::cmp::Reverse(n.time));
        let cache_key = serde_json::to_string(&[text.as_str(), locale, engine])
            .map_err(|_| StorageError::InvalidDownload)?;
        let cache = self.root.join(format!(
            "novel_translation_{:x}.json",
            Sha256::digest(cache_key.as_bytes())
        ));
        let mut translation = Vec::new();
        if let Ok(file) = fs::File::open(cache) {
            if let Ok(values) = serde_json::from_reader::<_, Vec<serde_json::Value>>(file) {
                let parts = park_domain::translation_segments(&text);
                if values.len() == parts.len() {
                    let mut start = 0;
                    for (part, value) in parts.into_iter().zip(values) {
                        let end = start + text[part.source].encode_utf16().count() as i64;
                        if let Some(translated) =
                            value.as_str().filter(|s| part.translate && !s.is_empty())
                        {
                            translation.push(TranslatedPassage {
                                start,
                                end,
                                translation: translated.to_owned(),
                            });
                        }
                        start = end;
                    }
                }
            }
        }
        Ok(NovelDocument {
            title: chapter.title,
            text,
            notes,
            translation,
        })
    }
}
