use crate::{tavern::now, ProgressStore, StorageError};
use park_domain::tavern_chat as chat;
use park_storage::tavern as store;
use serde_json::Value;
use std::sync::{Arc, Mutex, MutexGuard};

type Result<T> = std::result::Result<T, StorageError>;
fn parse(s: &str) -> Result<Value> {
    serde_json::from_str(s).map_err(|_| StorageError::InvalidTavernResource)
}
fn new_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| StorageError::IoFailure)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
#[derive(uniffi::Object)]
pub struct TavernChat {
    db: Arc<ProgressStore>,
    saved: Mutex<String>,
    private: bool,
}
impl TavernChat {
    pub(crate) fn from_private_card(
        db: Arc<ProgressStore>,
        uid: &str,
        title: &str,
        card: Value,
        greeting: usize,
    ) -> Result<Arc<Self>> {
        let mut session = chat::new_session(&new_id()?, uid, title, card, greeting);
        session["updatedAt"] = now()?.into();
        Ok(Self::new(db, session.to_string(), true))
    }
    fn local(&self) -> Result<MutexGuard<'_, String>> {
        self.saved.lock().map_err(|_| StorageError::IoFailure)
    }
    fn new(db: Arc<ProgressStore>, saved: String, private: bool) -> Arc<Self> {
        Arc::new(Self {
            db,
            saved: Mutex::new(saved),
            private,
        })
    }
}
#[uniffi::export]
impl ProgressStore {
    pub fn open_tavern_chat(self: Arc<Self>, id: String, private: bool) -> Result<Arc<TavernChat>> {
        let mut cache = self.chats.lock().map_err(|_| StorageError::IoFailure)?;
        cache.retain(|_, v| v.strong_count() > 0);
        if !private {
            if let Some(live) = cache.get(&id).and_then(|v| v.upgrade()) {
                return Ok(live);
            }
        }
        let raw = {
            let db = self.lock()?;
            store::session(&db, &id)?
        };
        let recover = parse(&raw)?
            .get("pendingReply")
            .is_some_and(|v| !v.is_null());
        let handle = TavernChat::new(self.clone(), raw, private);
        if recover {
            handle.change(r#"{"action":"recover"}"#.into())?;
        }
        if !private {
            cache.insert(id, Arc::downgrade(&handle));
        }
        Ok(handle)
    }
    pub fn create_tavern_chat(
        self: Arc<Self>,
        uid: String,
        private: bool,
    ) -> Result<Arc<TavernChat>> {
        self.chat_with_greeting(uid, private, 0)
    }
}
impl ProgressStore {
    pub(crate) fn chat_with_greeting(
        self: Arc<Self>,
        uid: String,
        private: bool,
        greeting: usize,
    ) -> Result<Arc<TavernChat>> {
        let raw = {
            let mut db = self.lock()?;
            let card = store::resource(&db, &uid, "character")?;
            let title=db.query_row("SELECT COALESCE(json_extract(item_json,'$.title'),'') FROM tavern_resources WHERE uid=?",[&uid],|r|r.get::<_,String>(0))?;
            let mut doc = chat::new_session(&new_id()?, &uid, &title, card, greeting);
            store::updated_session(&mut db, None, &mut doc, now()?, private)?
        };
        if !private {
            return self.open_tavern_chat(
                parse(&raw)?["id"]
                    .as_str()
                    .ok_or(StorageError::InvalidTavernResource)?
                    .into(),
                false,
            );
        }
        Ok(TavernChat::new(self, raw, true))
    }
}
#[uniffi::export]
impl TavernChat {
    pub fn document(&self) -> Result<String> {
        Ok(self.local()?.clone())
    }
    pub fn change(&self, operation: String) -> Result<String> {
        let mut current = self.local()?;
        let mut next = parse(&current)?;
        let mut op = parse(&operation)?;
        if op["action"] == "input"
            && op["expected"].as_str() != Some(next["inputDraft"].as_str().unwrap_or(""))
        {
            return Err(StorageError::Conflict);
        }
        let mut db = self.db.lock()?;
        if op["action"] == "speaker" && op["uid"].as_str().is_some_and(|id| !id.is_empty()) {
            let id = op["uid"].as_str().unwrap();
            if next["cast"].get(id).is_none() {
                op["card"] = store::resource(&db, id, "character")?;
            }
        }
        if op["action"] == "link" && op["enabled"] == true {
            store::resource(
                &db,
                op["uid"]
                    .as_str()
                    .ok_or(StorageError::InvalidTavernResource)?,
                "lorebook",
            )?;
        }
        chat::apply(&mut next, &op).map_err(|_| StorageError::InvalidTavernResource)?;
        *current =
            store::updated_session(&mut db, Some(&current), &mut next, now()?, self.private)?;
        Ok(current.clone())
    }
    pub fn branch(&self, index: u32) -> Result<Arc<TavernChat>> {
        let mut next = chat::branch(&parse(&self.document()?)?, &new_id()?, index as usize)
            .map_err(|_| StorageError::InvalidTavernResource)?;
        let mut db = self.db.lock()?;
        let raw = store::updated_session(&mut db, None, &mut next, now()?, self.private)?;
        drop(db);
        if !self.private {
            return self.db.clone().open_tavern_chat(
                next["id"]
                    .as_str()
                    .ok_or(StorageError::InvalidTavernResource)?
                    .into(),
                false,
            );
        }
        Ok(Self::new(self.db.clone(), raw, self.private))
    }
    pub fn prompt(&self, config: String, terms: String, mode: String) -> Result<String> {
        let doc = parse(&self.document()?)?;
        let books = {
            let db = self.db.lock()?;
            doc["books"]
                .as_array()
                .ok_or(StorageError::InvalidTavernResource)?
                .iter()
                .map(|uid| {
                    Ok(store::resource(
                        &db,
                        uid.as_str().ok_or(StorageError::InvalidTavernResource)?,
                        "lorebook",
                    )?)
                })
                .collect::<Result<Vec<_>>>()?
        };
        let mut failed = false;
        let prompt = chat::prompt(
            &doc,
            &books,
            &parse(&config)?,
            &parse(&terms)?,
            &mode,
            || {
                let mut bytes = [0; 4];
                if getrandom::getrandom(&mut bytes).is_err() {
                    failed = true;
                }
                u32::from_le_bytes(bytes) as f64 / (u32::MAX as f64 + 1.0)
            },
        )
        .map_err(|_| StorageError::InvalidTavernResource)?;
        if failed {
            return Err(StorageError::IoFailure);
        }
        Ok(prompt.to_string())
    }
}
