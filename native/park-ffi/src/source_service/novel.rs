use super::*;
use crate::{novel::NovelDocument, ChapterKey, ProgressStore};
use serde_json::{json, Value};

impl SourceService {
    pub(super) fn novel_body(
        &self,
        source: &str,
        work: &str,
        chapter: &str,
        request: &Request,
    ) -> Result<Value> {
        let chapter = (chapter != "_").then_some(chapter);
        let (_, reply) = self.catalog.invoke(
            source,
            "textChapter",
            &[json!(work), json!(chapter)],
            Invocation::child(Duration::from_secs(60), &request.cancelled),
        )?;
        let body = downloads::checked(reply)?;
        let paragraphs = body["paragraphs"]
            .as_array()
            .ok_or(SourceFailure::Unsupported)?;
        if !paragraphs.iter().all(Value::is_string)
            || !paragraphs
                .iter()
                .any(|p| p.as_str().is_some_and(|s| !s.trim().is_empty()))
        {
            return Err(SourceFailure::Unsupported);
        }
        Ok(body)
    }
}

#[uniffi::export]
impl SourceService {
    pub fn novel_chapter(
        &self,
        store: Arc<ProgressStore>,
        key: ChapterKey,
        locale: String,
        engine: String,
        request: Arc<Request>,
    ) -> Result<NovelDocument> {
        let body = self.novel_body(&key.source, &key.work_id, &key.chapter_id, &request)?;
        let chapter = serde_json::from_value(body).map_err(|_| SourceFailure::Unsupported)?;
        request.check().map_err(|_| SourceFailure::Cancelled)?;
        store
            .novel_document(&key, chapter, &locale, &engine)
            .map_err(|_| SourceFailure::Storage)
    }
}
