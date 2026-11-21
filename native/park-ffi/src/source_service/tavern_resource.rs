use super::*;
use crate::{ProgressStore, StorageError, TavernChat, TavernEntry};
use serde_json::Value;
use std::io::Write;

const LIMIT: usize = 12 * 1024 * 1024;

#[derive(Object)]
pub struct SourceTavernResource {
    uid: String,
    item: Value,
    kind: String,
    format: String,
    card: Value,
    original: Vec<u8>,
    saved: bool,
}

#[derive(Object)]
pub struct TavernOriginal(tempfile::NamedTempFile);

#[uniffi::export]
impl TavernOriginal {
    pub fn path(&self) -> std::result::Result<String, StorageError> {
        self.0
            .path()
            .to_str()
            .map(str::to_owned)
            .ok_or(StorageError::IoFailure)
    }
}

#[uniffi::export]
impl SourceTavernResource {
    pub fn kind(&self) -> String {
        self.kind.clone()
    }
    pub fn format(&self) -> String {
        self.format.clone()
    }
    pub fn saved(&self) -> bool {
        self.saved
    }
    pub fn content(&self) -> String {
        self.card.to_string()
    }
    pub fn summary(&self) -> String {
        self.item.to_string()
    }
    pub fn save(&self, store: Arc<ProgressStore>) -> std::result::Result<String, StorageError> {
        let db = store.lock()?;
        Ok(park_storage::tavern::save_source_resource(
            &db,
            &self.item,
            &self.kind,
            &self.format,
            &self.card,
            &self.original,
            crate::tavern::now()?,
        )?)
    }
    pub fn original_file(
        &self,
        directory: String,
    ) -> std::result::Result<Arc<TavernOriginal>, StorageError> {
        let mut file =
            tempfile::NamedTempFile::new_in(directory).map_err(|_| StorageError::IoFailure)?;
        file.write_all(&self.original)
            .map_err(|_| StorageError::IoFailure)?;
        Ok(Arc::new(TavernOriginal(file)))
    }
    pub fn sessions(
        &self,
        store: Arc<ProgressStore>,
    ) -> std::result::Result<Vec<TavernEntry>, StorageError> {
        let db = store.lock()?;
        Ok(park_storage::tavern::resource_sessions(&db, &self.uid)?
            .into_iter()
            .map(|e| TavernEntry {
                id: e.id,
                kind: e.kind,
                title: e.title,
            })
            .collect())
    }
    pub fn start_chat(
        &self,
        store: Arc<ProgressStore>,
        private: bool,
        greeting: u32,
    ) -> std::result::Result<Arc<TavernChat>, StorageError> {
        if self.kind != "character" {
            return Err(StorageError::InvalidTavernResource);
        }
        if private {
            return TavernChat::from_private_card(
                store,
                &self.uid,
                self.item["title"].as_str().unwrap_or(""),
                self.card.clone(),
                greeting as usize,
            );
        }
        let uid = self.save(store.clone())?;
        store.chat_with_greeting(uid, false, greeting as usize)
    }
}

#[uniffi::export]
impl SourceService {
    pub fn tavern_resource(
        &self,
        store: Arc<ProgressStore>,
        source: String,
        work: String,
        summary: String,
        request: Arc<Request>,
    ) -> Result<Arc<SourceTavernResource>> {
        if summary.len() > 1024 * 1024 || work.is_empty() {
            return Err(SourceFailure::Unsupported);
        }
        let mut item: Value =
            serde_json::from_str(&summary).map_err(|_| SourceFailure::Unsupported)?;
        if !item.is_object() {
            return Err(SourceFailure::Unsupported);
        }
        item["source"] = source.clone().into();
        item["id"] = work.clone().into();
        item["mediaKind"] = "tavern".into();
        let uid = format!("{source}:{work}");
        let saved = {
            let db = store.lock().map_err(|_| SourceFailure::Storage)?;
            park_storage::tavern::saved_resource(&db, &uid).map_err(|_| SourceFailure::Storage)?
        };
        if let Some(saved) = saved {
            let card = serde_json::from_str(&saved.card).map_err(|_| SourceFailure::Storage)?;
            if !park_domain::tavern::valid_content(&saved.kind, &card) || saved.bytes.len() > LIMIT
            {
                return Err(SourceFailure::Storage);
            }
            complete_item(&mut item, &card, &saved.kind);
            return Ok(Arc::new(SourceTavernResource {
                uid,
                item,
                kind: saved.kind,
                format: saved.format,
                card,
                original: saved.bytes,
                saved: true,
            }));
        }
        let (manifest, digest) = self
            .catalog
            .manifest_with_digest(&source, plugins::budget(&request))?;
        if manifest["type"] != "tavern" || manifest["capabilities"]["tavernAsset"] != true {
            return Err(SourceFailure::Unsupported);
        }
        let (_, raw) = self.catalog.invoke_installed(
            &source,
            "tavernAsset",
            &[work.into()],
            plugins::budget(&request),
            Some(&digest),
        )?;
        let raw = downloads::checked(raw)?;
        let kind = raw["kind"]
            .as_str()
            .filter(|v| matches!(*v, "character" | "lorebook"))
            .ok_or(SourceFailure::Unsupported)?;
        let format = raw["format"]
            .as_str()
            .filter(|v| matches!(*v, "png" | "json"))
            .ok_or(SourceFailure::Unsupported)?;
        let original = if format == "json" {
            if !raw["data"].is_object() {
                return Err(SourceFailure::Unsupported);
            }
            serde_json::to_vec(&raw["data"]).map_err(|_| SourceFailure::Unsupported)?
        } else if kind == "character" {
            let address = manual_read::confirmation_page(&manifest, &raw["url"])
                .ok_or(SourceFailure::Unsupported)?;
            let domains = manifest["domains"]
                .as_array()
                .ok_or(SourceFailure::Unsupported)?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let session = self
                .routes
                .sessions
                .partition(&source, Purpose::Source)
                .map_err(|_| SourceFailure::Storage)?;
            let req = park_net::Request {
                url: address,
                method: "GET".parse().unwrap(),
                headers: Default::default(),
                body: None,
                connect_address: None,
                follow_redirects: true,
            };
            let reply = self
                .routes
                .runtime
                .block_on(self.routes.transport.execute(
                    &session,
                    &domains,
                    req,
                    LIMIT,
                    Duration::from_secs(30),
                    &request.cancelled,
                ))
                .map_err(|e| {
                    if e == park_net::Error::Cancelled {
                        SourceFailure::Cancelled
                    } else {
                        SourceFailure::Unavailable
                    }
                })?;
            if !reply.status.is_success() {
                return Err(SourceFailure::Unavailable);
            }
            reply.body.to_vec()
        } else {
            return Err(SourceFailure::Unsupported);
        };
        let card = park_domain::tavern::parse_resource(kind, format, &original)
            .map_err(|_| SourceFailure::Unsupported)?;
        complete_item(&mut item, &card, kind);
        Ok(Arc::new(SourceTavernResource {
            uid,
            item,
            kind: kind.into(),
            format: format.into(),
            card,
            original,
            saved: false,
        }))
    }
}

fn complete_item(item: &mut Value, card: &Value, kind: &str) {
    item["tavernKind"] = kind.into();
    item["contentDomain"] = "tavern".into();
    if item["title"].as_str().unwrap_or("").is_empty() {
        item["title"] = card["data"]["name"]
            .as_str()
            .or_else(|| card["name"].as_str())
            .unwrap_or_else(|| item["id"].as_str().unwrap_or(""))
            .into();
    }
}
