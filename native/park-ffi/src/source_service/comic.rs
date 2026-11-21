use super::*;
use crate::{ChapterImages, ChapterKey, ProgressStore};
use park_media::{ImagePool, ImageTicket, Priority, RemoteImages, ResolvedImage, Route};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(super) fn page_identity(page: &Value) -> String {
    let entry = page["cacheKey"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(|v| json!([v, &page["transform"]]))
        .unwrap_or_else(|| json!([&page["url"], &page["token"], &page["transform"]]));
    format!("{:x}", Sha256::digest(entry.to_string().as_bytes()))
}

pub(crate) struct RemoteChapter {
    pub(crate) pages: Vec<Value>,
    pub(crate) pool: Arc<ImagePool>,
    loader: Arc<RemoteImages>,
    catalog: Arc<SourceCatalog>,
    key: ChapterKey,
    digest: String,
    headers: Value,
}

impl RemoteChapter {
    pub(crate) fn ticket(
        &self,
        index: usize,
        strips: u32,
        crop: bool,
        priority: Priority,
    ) -> ImageTicket {
        let page = self.pages[index].clone();
        let host = page["url"]
            .as_str()
            .and_then(|s| Url::parse(s).ok())
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| self.key.source.clone());
        let stable = page_identity(&page);
        let key = self.key.clone();
        let headers = self.headers.clone();
        let catalog = self.catalog.clone();
        let digest = self.digest.clone();
        self.loader.load(&stable, strips, crop, Route { host, browser_session: None }, priority, move |retry, token| {
            if page["url"].as_str().is_some_and(|url| !url.is_empty()) {
                return ResolvedImage::from_page(&page, &[&headers, &page["headers"]]);
            }
            let chapter = (key.chapter_id != "_").then_some(&key.chapter_id);
            let args = json!({"id":key.work_id,"chapterId":chapter,"index":index,"token":page["token"],"retry":retry});
            let (_, reply) = catalog.invoke_installed(&key.source, "resolveImage", &[args],
                Invocation::child(Duration::from_secs(45),token), Some(&digest)).map_err(|_| park_media::Error::Unavailable)?;
            if reply["ok"] != true { return Err(park_media::Error::Unavailable); }
            let resolved = &reply["value"];
            ResolvedImage::from_page(resolved, &[&headers, &page["headers"], &resolved["headers"]])
        })
    }
}

impl SourceService {
    pub(super) fn comic_pages(
        &self,
        store: &ProgressStore,
        key: ChapterKey,
        request: &Request,
    ) -> Result<RemoteChapter> {
        let chapter = (key.chapter_id != "_").then_some(&key.chapter_id);
        let (manifest, reply, digest) = self.catalog.invoke_identified(
            &key.source,
            "chapter",
            &[json!(key.work_id), json!(chapter)],
            Invocation::child(Duration::from_secs(60), &request.cancelled),
            None,
        )?;
        let mut result = downloads::checked(reply)?;
        let Some(Value::Array(pages)) = result.get_mut("pages").map(Value::take) else {
            return Err(SourceFailure::Unsupported);
        };
        if pages.is_empty() || pages.len() > 100_000 {
            return Err(SourceFailure::Unsupported);
        }
        let domains = manifest["domains"]
            .as_array()
            .ok_or(SourceFailure::Unsupported)?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let loader = RemoteImages::new(
            store.images.clone(),
            self.routes.transport.clone(),
            &self.routes.sessions,
            &key.source,
            domains,
            self.routes.cache.clone(),
            &manifest["imageDecrypt"],
        )?;
        Ok(RemoteChapter {
            pages,
            pool: store.images.clone(),
            loader,
            catalog: self.catalog.clone(),
            key,
            digest,
            headers: manifest["imageHeaders"].clone(),
        })
    }
}

#[uniffi::export]
impl SourceService {
    pub fn comic_chapter(
        &self,
        store: Arc<ProgressStore>,
        key: ChapterKey,
        request: Arc<Request>,
    ) -> Result<Arc<ChapterImages>> {
        Ok(Arc::new(ChapterImages::remote(
            self.comic_pages(&store, key, &request)?,
        )))
    }
}
