use super::{
    plugins::{base, CONTRACT},
    *,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const LIMIT: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct Entry {
    source: String,
    locale: String,
    version: String,
    url: String,
    sha256: String,
}

#[derive(Object)]
pub struct DictionaryIndex {
    entries: BTreeMap<(String, String), Entry>,
}

#[uniffi::export]
impl DictionaryIndex {
    pub fn packages(&self) -> String {
        let rows: Vec<_> = self.entries.values().map(|e|
            serde_json::json!({"source": e.source, "locale": e.locale, "version": e.version})
        ).collect();
        serde_json::Value::Array(rows).to_string()
    }
    #[uniffi::constructor]
    pub fn parse(bytes: Vec<u8>) -> Result<Arc<Self>> {
        if bytes.len() > LIMIT {
            return Err(SourceFailure::Unsupported);
        }
        #[derive(Deserialize)]
        struct Index {
            schema: u32,
            packs: Vec<Entry>,
        }
        let index: Index =
            serde_json::from_slice(&bytes).map_err(|_| SourceFailure::Unsupported)?;
        if index.schema != 1 || index.packs.len() > 10_000 {
            return Err(SourceFailure::Unsupported);
        }
        let mut entries = BTreeMap::new();
        for entry in index.packs {
            let (commit, path) = entry
                .url
                .strip_prefix(base())
                .and_then(|s| s.strip_prefix('/'))
                .and_then(|s| s.split_once('/'))
                .ok_or(SourceFailure::Unsupported)?;
            if !park_storage::recovery::valid_key(&entry.source)
                || !CONTRACT["languages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|l| l["id"] == entry.locale)
                || park_runtime::release(&entry.version).is_err()
                || entry.sha256.len() != 64
                || !entry.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                || commit.len() != 40
                || !commit.bytes().all(|b| b.is_ascii_hexdigit())
                || path
                    != format!(
                        "assets/tag_translations/{}/{}.json",
                        entry.source, entry.locale
                    )
            {
                return Err(SourceFailure::Unsupported);
            }
            if entries
                .insert((entry.source.clone(), entry.locale.clone()), entry)
                .is_some()
            {
                return Err(SourceFailure::Unsupported);
            }
        }
        Ok(Arc::new(DictionaryIndex { entries }))
    }
    pub fn asset_url(&self, source: String, locale: String) -> Result<String> {
        Ok(self.entry(source, locale)?.url.clone())
    }
    pub fn verify(&self, source: String, locale: String, bytes: Vec<u8>) -> Result<()> {
        verify_entry(self.entry(source, locale)?, &bytes)
    }
}
impl DictionaryIndex {
    fn entry(&self, source: String, locale: String) -> Result<&Entry> {
        self.entries
            .get(&(source, locale))
            .ok_or(SourceFailure::Unsupported)
    }
}
fn verify_entry(entry: &Entry, bytes: &[u8]) -> Result<()> {
    if bytes.len() > LIMIT {
        return Err(SourceFailure::Unsupported);
    }
    if !format!("{:x}", Sha256::digest(&bytes)).eq_ignore_ascii_case(&entry.sha256) {
        return Err(SourceFailure::Unsupported);
    }
    let pack: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| SourceFailure::Unsupported)?;
    if pack["source"] != entry.source
        || pack["locale"] != entry.locale
        || pack["version"] != entry.version
    {
        return Err(SourceFailure::Unsupported);
    }
    Ok(())
}

#[uniffi::export]
impl SourceService {
    pub fn official_dictionaries(&self, request: Arc<Request>) -> Result<Arc<DictionaryIndex>> {
        let channel = CONTRACT["officialRepositories"]["tagTranslations"]
            .as_str()
            .unwrap();
        let bytes = self.repository_resource(
            &format!("{}/{channel}/index.json", base()),
            true,
            &request,
            LIMIT,
        )?;
        DictionaryIndex::parse(bytes)
    }
    pub fn official_dictionary(
        &self,
        index: Arc<DictionaryIndex>,
        source: String,
        locale: String,
        request: Arc<Request>,
    ) -> Result<Vec<u8>> {
        let entry = index.entry(source, locale)?;
        let bytes = self.repository_resource(&entry.url, true, &request, LIMIT)?;
        verify_entry(entry, &bytes)?;
        Ok(bytes)
    }
}
