use super::*;
use crate::recovery::{Browser, RecoveryBrowser};
use park_runtime::{release, PluginPreview};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

#[uniffi::export(callback_interface)]
pub trait BundledScripts: Send + Sync {
    fn asset(&self, name: String) -> Option<Vec<u8>>;
}
pub(super) static CONTRACT: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(park_domain::PUBLIC_CONTRACT).expect("public contract"));
fn api() -> u64 {
    CONTRACT["sourceProtocol"]["apiVersion"].as_u64().unwrap()
}
pub(super) fn base() -> &'static str {
    CONTRACT["officialRepositories"]["base"].as_str().unwrap()
}
fn value<'a>(row: &'a Value, field: &str) -> Result<&'a str> {
    row[field].as_str().ok_or(SourceFailure::Unsupported)
}
pub(super) fn budget(request: &Request) -> Invocation {
    Invocation::child(Duration::from_secs(45), &request.cancelled)
}

#[derive(Object)]
pub struct PluginChange {
    old: Value,
    entry: Option<Value>,
}
#[derive(Object)]
pub struct PluginUpdates {
    items: Vec<Arc<PluginChange>>,
    incomplete: bool,
}
#[uniffi::export]
impl PluginUpdates {
    pub fn items(&self) -> Vec<Arc<PluginChange>> {
        self.items.clone()
    }
    pub fn incomplete(&self) -> bool {
        self.incomplete
    }
}
#[uniffi::export]
impl PluginChange {
    pub fn description(&self) -> String {
        json!({"existing":self.old,"entry":self.entry}).to_string()
    }
}
#[derive(Object)]
pub struct PluginOffer {
    preview: Option<PluginPreview>,
    removed: Option<Value>,
    origin: String,
}
#[uniffi::export]
impl PluginOffer {
    pub fn description(&self) -> String {
        match &self.preview {
            Some(p)=>json!({"manifest":p.manifest,"existing":p.existing,"url":p.url,"sha256":p.sha256,"origin":self.origin}).to_string(),
            None=>json!({"existing":self.removed,"withdrawn":true}).to_string()
        }
    }
}

fn valid_entry(row: &Value, channel: &str) -> bool {
    let Some(key) = row["key"]
        .as_str()
        .filter(|s| park_storage::recovery::valid_key(s))
    else {
        return false;
    };
    let Some(raw) = row["url"].as_str() else {
        return false;
    };
    let Some(rest) = raw.strip_prefix(base()).and_then(|s| s.strip_prefix('/')) else {
        return false;
    };
    let Some((commit, path)) = rest.split_once('/') else {
        return false;
    };
    if commit.len() != 40
        || !commit.bytes().all(|b| b.is_ascii_hexdigit())
        || path != format!("assets/sources/{key}.js")
    {
        return false;
    }
    row["name"].as_str().is_some_and(|s| !s.trim().is_empty())
        && ["version", "minAppVersion"]
            .iter()
            .all(|k| row[k].as_str().is_some_and(|s| release(s).is_ok()))
        && row["apiVersion"].as_u64().is_some_and(|v| v > 0)
        && row["sha256"]
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        && ["rankingOnly", "reverseSearchOnly", "metadataOnly"]
            .iter()
            .all(|k| row.get(k).is_none_or(Value::is_boolean))
        && row["rankingOnly"] != true
        && (row["reverseSearchOnly"] == true) == (channel == "reverse-search")
        && (row["metadataOnly"] == true) == (channel == "metadata")
}
impl SourceService {
    fn repository_bytes(
        &self,
        address: &str,
        official: bool,
        request: &Request,
    ) -> Result<Vec<u8>> {
        self.repository_resource(address, official, request, 4 * 1024 * 1024)
    }

    pub(super) fn repository_resource(
        &self, address: &str, official: bool, request: &Request, limit: usize,
    ) -> Result<Vec<u8>> {
        let url = Url::parse(address).map_err(|_| SourceFailure::Unsupported)?;
        if !park_net::public_https(&url) {
            return Err(SourceFailure::Unsupported);
        }
        let domain = url.host_str().unwrap().to_owned();
        let session = self
            .routes
            .sessions
            .partition("_repository", Purpose::Repository)
            .map_err(|_| SourceFailure::Storage)?;
        let response = self
            .routes
            .runtime
            .block_on(self.routes.transport.execute(
                &session,
                &[domain],
                park_net::Request {
                    url: url.clone(),
                    method: "GET".parse().unwrap(),
                    headers: Default::default(),
                    body: None,
                    connect_address: None,
                    follow_redirects: !official,
                },
                limit,
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
        if !response.status.is_success() || official && response.url != url {
            return Err(SourceFailure::Unavailable);
        }
        Ok(response.body.to_vec())
    }
}
#[uniffi::export]
impl SourceService {
    pub fn plugin_registry(&self, request: Arc<Request>) -> Result<String> {
        Ok(self.catalog.management(&budget(&request))?.to_string())
    }
    pub fn library_source_kinds(&self, keys: Vec<String>, request: Arc<Request>) -> Result<std::collections::HashMap<String, String>> {
        Ok(self.catalog.library_source_kinds(&keys, budget(&request))?)
    }
    pub fn clear_site_cookies(&self, key: String, browser: Box<dyn RecoveryBrowser>, request: Arc<Request>) -> Result<()> {
        Ok(self.catalog.clear_site_cookies(&key, &Browser(browser.as_ref()), budget(&request))?)
    }
    pub fn move_plugin(&self, key: String, anchor: String, after: bool, request: Arc<Request>) -> Result<()> {
        Ok(self.catalog.move_site(&key, &anchor, after, &budget(&request))?)
    }
    pub fn reset_plugin_order(&self, bundled: Vec<String>, request: Arc<Request>) -> Result<()> {
        Ok(self.catalog.reset_order(&bundled, &budget(&request))?)
    }
    pub fn set_plugin_enabled(
        &self,
        key: String,
        enabled: bool,
        request: Arc<Request>,
    ) -> Result<()> {
        Ok(self.catalog.set_enabled(&key, enabled, &budget(&request))?)
    }
    pub fn plugin_repository(&self, address: String, request: Arc<Request>) -> Result<String> {
        let base = Url::parse(&address).map_err(|_| SourceFailure::Unsupported)?;
        let doc: Value = serde_json::from_slice(&self.repository_bytes(&address, false, &request)?)
            .map_err(|_| SourceFailure::Unsupported)?;
        let rows = doc
            .get("sources")
            .unwrap_or(&doc)
            .as_array()
            .filter(|rows| rows.len() <= 4096)
            .ok_or(SourceFailure::Unsupported)?;
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let path = row["url"]
                .as_str()
                .or_else(|| row["file"].as_str())
                .ok_or(SourceFailure::Unsupported)?;
            let url = base.join(path).map_err(|_| SourceFailure::Unsupported)?;
            if !park_net::public_https(&url) {
                return Err(SourceFailure::Unsupported);
            }
            if row.get("sha256").is_some()
                && !row["sha256"]
                    .as_str()
                    .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return Err(SourceFailure::Unsupported);
            }
            result.push(json!({"name":row["name"].as_str().or(row["key"].as_str()).unwrap_or(path),"url":url.as_str(),"sha256":row["sha256"]}));
        }
        Ok(json!(result).to_string())
    }
    pub fn load_bundled_plugins(
        &self,
        index: String,
        assets: Box<dyn BundledScripts>,
        request: Arc<Request>,
    ) -> Result<()> {
        let list: Value = serde_json::from_str(&index).map_err(|_| SourceFailure::Unsupported)?;
        if list["apiVersion"] != api() {
            return Err(SourceFailure::Unsupported);
        }
        self.catalog
            .seed(&list, |name| assets.asset(name.into()), &budget(&request))?;
        Ok(())
    }
    pub fn preview_plugin_file(
        &self,
        bytes: Vec<u8>,
        app: String,
        request: Arc<Request>,
    ) -> Result<Arc<PluginOffer>> {
        let p = self
            .catalog
            .preview(bytes, String::new(), None, api(), &app, budget(&request))?;
        Ok(Arc::new(PluginOffer {
            preview: Some(p),
            removed: None,
            origin: "url".into(),
        }))
    }
    pub fn preview_plugin_url(
        &self,
        url: String,
        expected_sha256: Option<String>,
        app: String,
        request: Arc<Request>,
    ) -> Result<Arc<PluginOffer>> {
        let bytes = self.repository_bytes(&url, false, &request)?;
        let p = self.catalog.preview(
            bytes,
            url,
            expected_sha256.as_deref(),
            api(),
            &app,
            budget(&request),
        )?;
        Ok(Arc::new(PluginOffer {
            preview: Some(p),
            removed: None,
            origin: "url".into(),
        }))
    }
    pub fn check_plugin_updates(&self, request: Arc<Request>) -> Result<Arc<PluginUpdates>> {
        let mut entries = HashMap::new();
        let mut seen = HashSet::new();
        let mut incomplete = false;
        for channel in CONTRACT["officialRepositories"]["channels"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
        {
            let bytes = match self.repository_bytes(
                &format!("{}/{channel}/index.json", base()),
                true,
                &request,
            ) {
                Ok(v) => v,
                Err(SourceFailure::Cancelled) => return Err(SourceFailure::Cancelled),
                Err(_) => {
                    incomplete = true;
                    continue;
                }
            };
            let index: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let Some(rows) = index["sources"]
                .as_array()
                .filter(|_| index["apiVersion"] == api())
            else {
                incomplete = true;
                continue;
            };
            for row in rows {
                let key = row["key"].as_str().unwrap_or("");
                if !seen.insert(key.to_owned()) {
                    entries.remove(key);
                    incomplete = true;
                    continue;
                }
                if !valid_entry(row, channel) {
                    incomplete = true;
                    continue;
                }
                entries.insert(key.to_owned(), row.clone());
            }
        }
        let list = self.catalog.management(&budget(&request))?;
        let mut items = Vec::new();
        for old in list["sources"].as_array().unwrap() {
            if !park_storage::plugins::is_official(old) {
                continue;
            }
            let entry = entries.get(value(old, "key")?);
            match entry {
                Some(e)
                    if release(value(e, "version")?)?
                        > release(old["version"].as_str().unwrap_or("0.0.0"))? =>
                {
                    ()
                }
                None if !incomplete => (),
                _ => continue,
            }
            items.push(Arc::new(PluginChange {
                old: old.clone(),
                entry: entry.cloned(),
            }));
        }
        Ok(Arc::new(PluginUpdates { items, incomplete }))
    }
    pub fn preview_plugin_change(
        &self,
        change: Arc<PluginChange>,
        app: String,
        request: Arc<Request>,
    ) -> Result<Arc<PluginOffer>> {
        let registry = self.catalog.management(&budget(&request))?;
        if !registry["sources"]
            .as_array()
            .unwrap()
            .contains(&change.old)
        {
            return Err(SourceFailure::Unavailable);
        }
        let Some(entry) = &change.entry else {
            return Ok(Arc::new(PluginOffer {
                preview: None,
                removed: Some(change.old.clone()),
                origin: "official".into(),
            }));
        };
        let url = value(entry, "url")?;
        let p = self.catalog.preview(
            self.repository_bytes(url, true, &request)?,
            url.into(),
            Some(value(entry, "sha256")?),
            api(),
            &app,
            budget(&request),
        )?;
        if p.existing.as_ref() != Some(&change.old)
            || ["key", "version", "apiVersion", "minAppVersion"]
                .iter()
                .any(|k| p.manifest[k] != entry[k])
        {
            return Err(SourceFailure::Unsupported);
        }
        Ok(Arc::new(PluginOffer {
            preview: Some(p),
            removed: None,
            origin: "official".into(),
        }))
    }
    pub fn confirm_plugin(
        &self,
        offer: Arc<PluginOffer>,
        browser: Box<dyn RecoveryBrowser>,
        request: Arc<Request>,
    ) -> Result<()> {
        let bridge = Browser(browser.as_ref());
        if let Some(p) = &offer.preview {
            self.catalog
                .install(p, &offer.origin, Some(&bridge), budget(&request))?;
        } else {
            self.catalog.withdraw(
                offer.removed.as_ref().ok_or(SourceFailure::Unsupported)?,
                Some(&bridge),
                budget(&request),
            )?;
        }
        Ok(())
    }
}
