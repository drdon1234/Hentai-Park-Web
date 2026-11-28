use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use crate::{PageImage, ProgressStore};
use park_media::{ImageKey, PageFile, Priority, Route};
use serde_json::{json, Value};
use std::io::Write;

pub(super) fn media_chunk(part: &Value, remaining: u64) -> Result<Vec<u8>> {
    let encoded = part["base64"].as_str().filter(|s| s.len() <= 174764).ok_or(SourceFailure::Unsupported)?;
    let bytes = STANDARD.decode(encoded).map_err(|_| SourceFailure::Unsupported)?;
    if bytes.len() as u64 != remaining.min(131072) {
        return Err(SourceFailure::Unsupported);
    }
    Ok(bytes)
}

#[uniffi::export]
impl SourceService {
    pub fn album_preview(&self, source: String, resource: String, store: Arc<ProgressStore>, request: Arc<Request>) -> Result<Arc<PageImage>> {
        if resource.len() > 2 * 1024 * 1024 { return Err(SourceFailure::Unsupported); }
        let resource: Value = serde_json::from_str(&resource).map_err(|_| SourceFailure::Unsupported)?;
        let size = resource["size"].as_u64().filter(|n| (1..=2097152).contains(n)).ok_or(SourceFailure::Unsupported)?;
        let name = resource["cacheKey"].as_str().filter(|s| !s.is_empty() && s.len()<=512).ok_or(SourceFailure::Unsupported)?;
        let manifest = self.catalog.manifest(&source, plugins::budget(&request))?;
        if manifest["capabilities"]["album"] != true { return Err(SourceFailure::Unsupported); }
        let entry = self.catalog.installed()?.into_iter().find(|s| s.key==source && s.enabled!=Some(false)).ok_or(SourceFailure::Unavailable)?;
        let identity = json!(["album-preview",self.context_hash(&store,&source,&manifest)?,entry.sha256,entry.install_generation,name,size]).to_string();
        let key = ImageKey { source:source.clone(), identity:identity.clone(), strips:0, crop:false };
        let cache = self.routes.cache.clone();
        let disk = park_media::image_disk::ImageCacheEntry::new(&cache,&source,&identity,&Value::Null,store.image_cache_limit);
        let writer = disk.clone();
        let catalog = self.catalog.clone();
        let ticket = store.images.request(key.clone(),Route{host:source.clone(),browser_session:None},Priority::Visible, move |token| {
            if let Some(image)=writer.read(&cache,token)? { return Ok(image); }
            let mut bytes=Vec::with_capacity(size as usize);
            while (bytes.len() as u64)<size {
                let offset=bytes.len() as u64;
                if token.is_cancelled() { return Err(park_media::Error::Cancelled); }
                let (_,reply)=catalog.invoke_installed(&source,"subscription.mediaChunk",&[resource.clone(),json!(offset)],
                    Invocation::child(Duration::from_secs(45),token),Some(&entry.sha256)).map_err(|_|park_media::Error::Unavailable)?;
                let part=downloads::checked(reply).map_err(|_|park_media::Error::Unavailable)?;
                bytes.extend(media_chunk(&part,size-offset).map_err(|_|park_media::Error::InvalidImage)?);
            }
            let mut file=tempfile::NamedTempFile::new_in(&cache).map_err(|_|park_media::Error::Storage)?;
            file.write_all(&bytes).map_err(|_|park_media::Error::Storage)?;
            let image=park_media::prepare(PageFile::Temporary(file),0,false,&cache,token)?;
            writer.store(&bytes,token);
            Ok(image)
        });
        let asset=ticket.wait(||request.cancelled.is_cancelled())?;
        Ok(Arc::new(PageImage{ asset, disk:Some(disk), pool:store.images.clone(), key }))
    }
}
