use crate::{BrowserTransport, Error, Invocation, NativeHost, SourceRuntime};
mod install;
mod settings;
pub use install::{release, PluginPreview};
use park_net::{Sessions, Transport};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

#[derive(Deserialize)]
pub struct Installed {
    pub key: String,
    #[serde(default)]
    pub order: i64,
    #[serde(default)]
    pub sha256: String,
    #[serde(default, rename = "installGeneration")]
    pub install_generation: Option<String>,
    pub enabled: Option<bool>,
}
#[derive(Deserialize)]
struct Registry {
    sources: Vec<Installed>,
}
pub struct SourceCatalog {
    root: PathBuf,
    polyfills: String,
    sessions: Arc<Sessions>,
    transport: Arc<Transport>,
    browser: Option<Arc<dyn BrowserTransport>>,
    writers: Mutex<HashMap<String, Weak<Mutex<()>>>>,
}
fn read_script(root: &Path, key: &str, digest: &str) -> Result<String, Error> {
    if !crate::source_key(key) {
        return Err(Error::Config);
    }
    if root
        .join(format!("source_install_{key}.json"))
        .try_exists()
        .map_err(|_| Error::Storage)?
    {
        return Err(Error::Storage);
    }
    let directory = root.join("sources");
    let path = directory.join(format!("{key}.js"));
    if fs::symlink_metadata(&directory)
        .map_err(|_| Error::Storage)?
        .file_type()
        .is_symlink()
        || !fs::symlink_metadata(&path)
            .map_err(|_| Error::Storage)?
            .is_file()
    {
        return Err(Error::Storage);
    }
    let raw = fs::read(&path).map_err(|_| Error::Storage)?;
    if !digest.is_empty() && !digest.eq_ignore_ascii_case(&format!("{:x}", Sha256::digest(&raw))) {
        return Err(Error::Config);
    }
    String::from_utf8(raw).map_err(|_| Error::Script)
}

pub fn installed_manifest(
    root: &Path,
    key: &str,
    digest: &str,
    polyfills: &str,
    call: Invocation,
) -> Result<Value, Error> {
    let script = read_script(root, key, digest)?;
    SourceRuntime::inspect(key, script.trim_start_matches('\u{feff}'), polyfills, call)
}
impl SourceCatalog {
    pub fn new(
        root: &Path,
        polyfills: String,
        sessions: Arc<Sessions>,
        transport: Arc<Transport>,
    ) -> Self {
        Self {
            root: root.to_owned(),
            polyfills,
            sessions,
            transport,
            browser: None,
            writers: Mutex::default(),
        }
    }
    pub fn new_with_browser(
        root: &Path,
        polyfills: String,
        sessions: Arc<Sessions>,
        transport: Arc<Transport>,
        browser: Arc<dyn BrowserTransport>,
    ) -> Self {
        Self {
            root: root.to_owned(),
            polyfills,
            sessions,
            transport,
            browser: Some(browser),
            writers: Mutex::default(),
        }
    }
    fn registered(&self) -> Result<Vec<Installed>, Error> {
        let raw = match fs::read(self.root.join("sources.json")) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(Error::Storage),
        };
        let mut entries: Registry = serde_json::from_slice(&raw).map_err(|_| Error::Storage)?;
        if entries.sources.iter().any(|s| !crate::source_key(&s.key)) {
            return Err(Error::Config);
        }
        entries.sources.sort_by_key(|s| s.order);
        Ok(entries.sources)
    }
    pub fn installed(&self) -> Result<Vec<Installed>, Error> {
        let mut entries = self.registered()?;
        entries.retain(|s| s.enabled != Some(false));
        Ok(entries)
    }
    pub fn management_manifest(&self, key: &str, call: Invocation) -> Result<Value, Error> {
        let _gate = self.lock(false, &call)?;
        let entry = self.registered()?.into_iter().find(|s| s.key == key).ok_or(Error::Config)?;
        installed_manifest(&self.root, key, &entry.sha256, &self.polyfills, call)
    }
    pub fn library_source_kinds(&self, keys: &[String], call: Invocation) -> Result<HashMap<String, String>, Error> {
        let _gate = self.lock(false, &call)?;
        let wanted: std::collections::HashSet<_> = keys.iter().collect();
        let mut kinds = HashMap::new();
        for entry in self.registered()?.into_iter().filter(|entry| wanted.contains(&entry.key)) {
            let manifest = installed_manifest(&self.root, &entry.key, &entry.sha256, &self.polyfills, call.clone())?;
            if let Some(kind) = manifest["type"].as_str() {
                kinds.insert(entry.key, kind.to_owned());
            }
        }
        Ok(kinds)
    }
    pub fn manifest(&self,key:&str,call:Invocation)->Result<Value,Error>{
        let _gate=self.lock(false,&call)?;
        let source=self.installed()?.into_iter().find(|s|s.key==key).ok_or(Error::Config)?;
        installed_manifest(&self.root,key,&source.sha256,&self.polyfills,call)
    }
    pub fn manifest_with_digest(&self, key: &str, call: Invocation) -> Result<(Value, String), Error> {
        let _gate = self.lock(false, &call)?;
        let entry = self.installed()?.into_iter().find(|r| r.key == key).ok_or(Error::Config)?;
        let bytes = read_script(&self.root, key, &entry.sha256)?;
        let digest = format!("{:x}", Sha256::digest(bytes.as_bytes()));
        let manifest = SourceRuntime::inspect(key, bytes.trim_start_matches('\u{feff}'), &self.polyfills, call)?;
        Ok((manifest, digest))
    }
    pub fn invoke(
        &self,
        key: &str,
        method: &str,
        args: &[Value],
        call: Invocation,
    ) -> Result<(Value, Value), Error> {
        self.invoke_installed(key, method, args, call, None)
    }

    /// 显式操作可以绑定到用户预览时的脚本，更新后的脚本需要重新展示操作。
    pub fn invoke_installed(
        &self,
        key: &str,
        method: &str,
        args: &[Value],
        call: Invocation,
        expected_digest: Option<&str>,
    ) -> Result<(Value, Value), Error> {
        let (manifest, response, _) = self.invoke_identified(key, method, args, call, expected_digest)?;
        Ok((manifest, response))
    }

    pub fn invoke_identified(
        &self, key: &str, method: &str, args: &[Value], call: Invocation, expected_digest: Option<&str>,
    ) -> Result<(Value, Value, String), Error> {
        self.run_method(key, method, args, call, expected_digest, None)
    }

    pub fn set_managed_account(&self, key: &str, enabled: bool, call: Invocation) -> Result<Value, Error> {
        let (_, result, _) = self.run_method(key, "account.enableManaged", &[], call, None, Some(enabled))?;
        Ok(result)
    }

    fn run_method(
        &self, key: &str, method: &str, args: &[Value], call: Invocation, expected_digest: Option<&str>,
        managed: Option<bool>,
    ) -> Result<(Value, Value, String), Error> {
        call.check()?;
        let _restore = self.lock(managed.is_some() || method == "account.logout", &call)?;
        if !crate::source_key(key) {
            return Err(Error::Config);
        }
        let gate = {
            let mut writers = self.writers.lock().map_err(|_| Error::Storage)?;
            writers.retain(|_, gate| gate.strong_count() > 0);
            if let Some(g) = writers.get(key).and_then(Weak::upgrade) {
                g
            } else {
                let g = Arc::new(Mutex::new(()));
                writers.insert(key.into(), Arc::downgrade(&g));
                g
            }
        };
        let _exclusive = loop {
            call.check()?;
            match gate.try_lock() {
                Ok(g) => break g,
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(8))
                }
                Err(_) => return Err(Error::Storage),
            }
        };
        let source = self
            .installed()?
            .into_iter()
            .find(|s| s.key == key)
            .ok_or(Error::Config)?;
        let bytes = read_script(&self.root, key, &source.sha256)?;
        let digest = if source.sha256.is_empty() {
            format!("{:x}", Sha256::digest(bytes.as_bytes()))
        } else {
            source.sha256.clone()
        };
        if expected_digest.is_some_and(|expected| !expected.eq_ignore_ascii_case(&digest)) {
            return Err(Error::Config);
        }
        let script = bytes.trim_start_matches('\u{feff}');
        let manifest = SourceRuntime::inspect(key, script, &self.polyfills, call.clone())?;
        if method == "account.enableManaged" {
            let enabled = managed.ok_or(Error::Unsupported)?;
            if manifest["account"]["managedAccount"] != true {
                return Err(Error::Unsupported);
            }
            let mut state = crate::SourceState::open(&self.root, key)?;
            if enabled && !state.snapshot()["data"]["credentials"].is_null() {
                return Err(Error::Config);
            }
            state.set_option("managedAccount", enabled.into(), &call)?;
            if !enabled {
                return Ok((manifest, serde_json::json!({"ok":true,"value":null}), digest));
            }
        }
        let domains = manifest["domains"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let host = Rc::new(NativeHost::registered_with_browser(
            &self.root,
            key,
            domains,
            manifest["chat"]["domains"].clone(),
            &self.sessions,
            self.transport.clone(),
            source
                .install_generation
                .as_deref()
                .or(Some(source.sha256.as_str())),
            self.browser.clone(),
        )?);
        let mut engine = SourceRuntime::open(
            key,
            script,
            &self.polyfills,
            &host.snapshot(),
            host.clone(),
            call.clone(),
            false,
        )?;
        let result = if manifest["account"]["required"] == true && !method.starts_with("account.") {
            let status = engine.invoke("account.status", &[], call.clone())?;
            if status["ok"] != true {
                status
            } else if status["value"]["loggedIn"] != true {
                serde_json::json!({"ok":false,"error":{"code":"LOGIN_REQUIRED"}})
            } else {
                engine.invoke(method, args, call.clone())?
            }
        } else {
            engine.invoke(method, args, call.clone())?
        };
        engine.close(call.clone())?;
        if result["ok"] == true {
            let partition = || self.sessions.partition(key, park_net::Purpose::Source);
            match method {
                "account.logout" => {
                    partition()?.clear()?;
                    if manifest["account"]["managedAccount"] == true {
                        let mut state = crate::SourceState::open(&self.root, key)?;
                        if state.snapshot()["settings"]["managedAccount"] == true {
                            state.set_option("managedAccount", false.into(), &call)?;
                        }
                    }
                }
                "account.enableManaged" => partition()?.invalidate()?,
                "account.login" => partition()?.invalidate()?,
                _ => {}
            }
        }
        Ok((manifest, result, digest))
    }
}
