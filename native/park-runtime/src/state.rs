use crate::{Error, Invocation};
use serde_json::{json, Value};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

pub struct SourceState {
    file: PathBuf,
    snapshot: Value,
}

impl SourceState {
    pub fn open(root: &Path, key: &str) -> Result<Self, Error> {
        if !crate::source_key(key) {
            return Err(Error::Config);
        }
        let file = root.join(format!("source_state_{key}.json"));
        let mut snapshot = match std::fs::read(&file) {
            Ok(data) => serde_json::from_slice(&data).map_err(|_| Error::Storage)?,
            Err(e) if e.kind() == ErrorKind::NotFound => json!({"data":{},"settings":{}}),
            Err(_) => return Err(Error::Storage),
        };
        if snapshot.is_null() {
            snapshot = json!({"data":{},"settings":{}});
        }
        if !snapshot.is_object() {
            return Err(Error::Storage);
        }
        Ok(Self { file, snapshot })
    }
    pub fn snapshot(&self) -> &Value {
        &self.snapshot
    }
    pub fn persist(&mut self, data: Value, call: &Invocation) -> Result<(), Error> {
        if !data.is_object() {
            return Err(Error::Config);
        }
        let mut next = self.snapshot.clone();
        next["data"] = data;
        self.replace(next, call)
    }

    pub(crate) fn set_option(
        &mut self,
        key: &str,
        value: Value,
        call: &Invocation,
    ) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        if next["settings"].is_null() {
            next["settings"] = json!({});
        }
        next["settings"]
            .as_object_mut()
            .ok_or(Error::Storage)?
            .insert(key.to_owned(), value);
        self.replace(next, call)
    }

    fn replace(&mut self, next: Value, call: &Invocation) -> Result<(), Error> {
        let dir = self.file.parent().ok_or(Error::Storage)?;
        let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(|_| Error::Storage)?;
        serde_json::to_writer(&mut temp, &next).map_err(|_| Error::Storage)?;
        temp.flush()
            .and_then(|_| temp.as_file().sync_all())
            .map_err(|_| Error::Storage)?;
        call.check()?;
        temp.persist(&self.file).map_err(|_| Error::Storage)?;
        #[cfg(unix)]
        std::fs::File::open(dir)
            .and_then(|f| f.sync_all())
            .map_err(|_| Error::Storage)?;
        self.snapshot = next;
        Ok(())
    }
}
