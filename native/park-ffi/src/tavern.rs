use crate::{ProgressStore, StorageError};
use park_storage::tavern as store;
use std::time::{SystemTime, UNIX_EPOCH};

#[uniffi::export]
pub fn review_tavern_proposal(
    kind: String,
    base: String,
    generated: String,
    omitted: Vec<String>,
) -> std::result::Result<String, StorageError> {
    let parse = |s: &str| serde_json::from_str(s).map_err(|_| StorageError::InvalidTavernResource);
    let incoming = generated
        .trim()
        .strip_prefix("```json")
        .or_else(|| generated.trim().strip_prefix("```"))
        .unwrap_or(generated.trim())
        .trim_end_matches("```")
        .trim();
    park_domain::tavern_proposal::review(&kind, parse(&base)?, &parse(incoming)?, &omitted)
        .map(|v| v.to_string())
        .map_err(|_| StorageError::InvalidTavernResource)
}

/// 按路径编辑已选字段。宿主显示层不重新编码未知 JSON 数字或其他扩展。
#[uniffi::export]
pub fn edit_tavern_field(
    content: String,
    path: Vec<String>,
    value: String,
) -> std::result::Result<String, StorageError> {
    use serde_json::Value;
    let error = || StorageError::InvalidTavernResource;
    let mut root: Value = serde_json::from_str(&content).map_err(|_| error())?;
    let replacement: Value = serde_json::from_str(&value).map_err(|_| error())?;
    let (key, parents) = path.split_last().ok_or_else(error)?;
    let mut target = &mut root;
    for part in parents {
        target = match target {
            Value::Object(o) => o.get_mut(part),
            Value::Array(a) => part.parse::<usize>().ok().and_then(|n| a.get_mut(n)),
            _ => None,
        }
        .ok_or_else(error)?;
    }
    match target {
        Value::Object(o) => {
            o.insert(key.clone(), replacement);
        }
        Value::Array(a) if key == "-" => a.push(replacement),
        Value::Array(a) => {
            let idx = key.parse::<usize>().map_err(|_| error())?;
            *a.get_mut(idx).ok_or_else(error)? = replacement;
        }
        _ => return Err(error()),
    }
    Ok(root.to_string())
}

impl From<store::Error> for StorageError {
    fn from(e: store::Error) -> Self {
        match e {
            store::Error::Database(db) => db.into(),
            store::Error::Missing => Self::MissingWork,
            store::Error::Conflict => Self::Conflict,
            store::Error::Invalid => Self::InvalidTavernResource,
        }
    }
}
type Result<T> = std::result::Result<T, StorageError>;
#[derive(uniffi::Record)]
pub struct TavernEntry {
    pub id: String,
    pub kind: String,
    pub title: String,
}
#[derive(uniffi::Record)]
pub struct TavernEditor {
    pub key: String,
    pub content: String,
    pub recovered: bool,
}
pub(crate) fn now() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::IoFailure)?
        .as_millis() as i64)
}
#[uniffi::export]
impl ProgressStore {
    pub fn import_tavern_card(&self, bytes: Vec<u8>) -> Result<String> {
        let mut db = self.lock()?;
        Ok(store::import_card(&mut db, &bytes, now()?)?)
    }
    pub fn tavern_catalog(&self) -> Result<Vec<TavernEntry>> {
        let db = self.lock()?;
        Ok(store::catalog(&db)?
            .into_iter()
            .map(|r| TavernEntry {
                id: r.id,
                kind: r.kind,
                title: r.title,
            })
            .collect())
    }
    pub fn tavern_editor(
        &self,
        uid: Option<String>,
        kind: String,
        private: bool,
    ) -> Result<TavernEditor> {
        let db = self.lock()?;
        let r = store::editor(&db, uid.as_deref(), &kind, private)?;
        Ok(TavernEditor {
            key: r.key,
            content: r.content,
            recovered: r.recovered,
        })
    }
    pub fn write_tavern_draft(
        &self,
        key: String,
        content: String,
        previous: Option<String>,
    ) -> Result<()> {
        let db = self.lock()?;
        Ok(store::write_draft(
            &db,
            &key,
            &content,
            previous.as_deref(),
        )?)
    }
    pub fn save_tavern_card(
        &self,
        uid: Option<String>,
        kind: String,
        key: String,
        content: String,
        private: bool,
    ) -> Result<String> {
        let mut db = self.lock()?;
        Ok(store::save_card(
            &mut db,
            uid.as_deref(),
            &kind,
            &key,
            &content,
            private,
            now()?,
        )?)
    }
    pub fn delete_tavern_session(&self, id: String) -> Result<()> {
        let mut db = self.lock()?;
        Ok(store::delete_session(&mut db, &id)?)
    }
}
