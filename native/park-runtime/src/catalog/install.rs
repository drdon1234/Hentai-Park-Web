use super::*;
use park_net::Purpose;
use park_storage::{
    plugins::{registry, InstallJournal},
    recovery::RecoveryBrowser,
};
use serde_json::json;

pub fn release(input: &str) -> Result<[u64; 3], Error> {
    let mut parts = input.split('.');
    let mut numbers = [0; 3];
    for n in &mut numbers {
        let text = parts.next().ok_or(Error::Config)?;
        if text.is_empty()
            || !text.bytes().all(|b| b.is_ascii_digit())
            || text.len() > 1 && text.starts_with('0')
        {
            return Err(Error::Config);
        }
        *n = text.parse().map_err(|_| Error::Config)?;
    }
    if parts.next().is_some() {
        return Err(Error::Config);
    }
    Ok(numbers)
}
pub struct PluginPreview {
    pub manifest: Value,
    pub sha256: String,
    pub url: String,
    pub existing: Option<Value>,
    code: String,
}
fn domains(manifest: &Value) -> Vec<String> {
    park_storage::cookies::declared_domains(manifest)
}
impl SourceCatalog {
    pub fn move_site(&self, key: &str, anchor: &str, after: bool, call: &Invocation) -> Result<(), Error> {
        let _lock = self.lock(true, call)?;
        park_storage::plugins::order_sources(&self.root, |rows| {
            let locate = |key: &str| rows.iter().position(|r| r["key"] == key)
                .ok_or(park_storage::recovery::Error::InvalidJournal);
            let from = locate(key)?;
            let to = locate(anchor)?;
            if from == to { return Ok(()); }
            let row = rows.remove(from);
            rows.insert(to - usize::from(from < to) + usize::from(after), row);
            Ok(())
        }).map_err(|_| Error::Storage)
    }

    pub fn reset_order(&self, bundled: &[String], call: &Invocation) -> Result<(), Error> {
        let _lock = self.lock(true, call)?;
        let positions: HashMap<_, _> = bundled.iter().enumerate().map(|(i,k)| (k.as_str(), i)).collect();
        park_storage::plugins::order_sources(&self.root, |rows| {
            rows.sort_by_key(|r| positions.get(r["key"].as_str().unwrap_or("")).copied().unwrap_or(usize::MAX));
            Ok(())
        }).map_err(|_| Error::Storage)
    }

    pub fn set_enabled(&self, key: &str, enabled: bool, call: &Invocation) -> Result<(), Error> {
        let _lock = self.lock(true, call)?;
        park_storage::plugins::set_enabled(&self.root, key, enabled).map_err(|_| Error::Storage)
    }
    pub fn seed(
        &self,
        index: &Value,
        asset: impl FnMut(&str) -> Option<Vec<u8>>,
        call: &Invocation,
    ) -> Result<(), Error> {
        let _lock = self.lock(true, call)?;
        park_storage::plugins::seed(&self.root, index, asset).map_err(|_| Error::Storage)
    }
    pub(super) fn lock(&self, exclusive: bool, call: &Invocation) -> Result<fs::File, Error> {
        let file = park_storage::recovery::gate_file(&self.root).map_err(|_| Error::Storage)?;
        loop {
            call.check()?;
            match park_storage::recovery::try_lock_file(&file, exclusive) {
                Ok(true) => {
                    if self
                        .root
                        .join("backup_restore.json")
                        .try_exists()
                        .map_err(|_| Error::Storage)?
                        || park_storage::plugins::pending(&self.root).map_err(|_| Error::Storage)?
                    {
                        return Err(Error::Storage);
                    }
                    return Ok(file);
                }
                Ok(false) => std::thread::sleep(Duration::from_millis(8)),
                Err(_) => return Err(Error::Storage),
            }
        }
    }
    pub fn management(&self, call: &Invocation) -> Result<Value, Error> {
        let _lock = self.lock(false, call)?;
        registry(&self.root).map_err(|_| Error::Storage)
    }
    pub fn preview(
        &self,
        bytes: Vec<u8>,
        url: String,
        expected: Option<&str>,
        api: u64,
        app: &str,
        call: Invocation,
    ) -> Result<PluginPreview, Error> {
        if bytes.is_empty() || bytes.len() > 4 * 1024 * 1024 {
            return Err(Error::Config);
        }
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if expected.is_some_and(|v| !sha256.eq_ignore_ascii_case(v)) {
            return Err(Error::Config);
        }
        let code = String::from_utf8(bytes).map_err(|_| Error::Script)?;
        let manifest = SourceRuntime::inspect_script(
            None,
            code.trim_start_matches('\u{feff}'),
            &self.polyfills,
            call.clone(),
        )?;
        if manifest["apiVersion"].as_u64() != Some(api)
            || release(manifest["minAppVersion"].as_str().ok_or(Error::Config)?)? > release(app)?
        {
            return Err(Error::Unsupported);
        }
        release(manifest["version"].as_str().ok_or(Error::Config)?)?;
        let list = self.management(&call)?;
        let existing = list["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == manifest["key"])
            .cloned();
        Ok(PluginPreview {
            manifest,
            sha256,
            url,
            existing,
            code,
        })
    }
    pub fn install(
        &self,
        preview: &PluginPreview,
        origin: &str,
        browser: Option<&dyn RecoveryBrowser>,
        call: Invocation,
    ) -> Result<(), Error> {
        let key = preview.manifest["key"].as_str().ok_or(Error::Config)?;
        let snapshot = {
            let _lock = self.lock(false, &call)?;
            InstallJournal::capture(&self.root, key).map_err(|_| Error::Storage)?
        };
        if snapshot.source != preview.existing {
            return Err(Error::Config);
        }
        if origin == "official" {
            let old = snapshot.source.as_ref().ok_or(Error::Config)?;
            if !park_storage::plugins::is_official(old)
                || release(old["version"].as_str().ok_or(Error::Config)?)?
                    >= release(preview.manifest["version"].as_str().ok_or(Error::Config)?)?
            {
                return Err(Error::Config);
            }
        } else if !matches!(origin, "url" | "builtin") {
            return Err(Error::Config);
        }
        let tmp = loop {
            call.check()?;
            if let Some(stage) =
                park_storage::plugins::Stage::try_create(&self.root).map_err(|_| Error::Storage)?
            {
                break stage;
            }
            std::thread::sleep(Duration::from_millis(8));
        };
        snapshot.stage(tmp.path()).map_err(|_| Error::Storage)?;
        let mut state = snapshot.state.clone();
        let restored = state
            .as_object_mut()
            .ok_or(Error::Storage)?
            .remove("restoreMetadata");
        if let Some(meta) = &restored {
            if meta["key"] != key {
                return Err(Error::Config);
            }
            fs::write(
                tmp.path().join(format!("source_state_{key}.json")),
                serde_json::to_vec(&state).map_err(|_| Error::Storage)?,
            )
            .map_err(|_| Error::Storage)?;
        }
        let scope = domains(&preview.manifest);
        let host = Rc::new(NativeHost::open(
            tmp.path(),
            key,
            scope.clone(),
            &Sessions::new(tmp.path()),
            self.transport.clone(),
        )?);
        let engine = SourceRuntime::open(
            key,
            preview.code.trim_start_matches('\u{feff}'),
            &self.polyfills,
            &state,
            host,
            call.clone(),
            true,
        )?;
        if engine.manifest() != &preview.manifest {
            return Err(Error::Config);
        }
        engine.close(call.clone())?;
        let _lock = self.lock(true, &call)?;
        let list = registry(&self.root).map_err(|_| Error::Storage)?;
        let mut row = snapshot.source.clone().unwrap_or_else(
            || json!({"key":key,"order":list["sources"].as_array().unwrap().len(),"enabled":true}),
        );
        if let Some(meta) = restored.filter(|_| snapshot.source.is_none()) {
            row["enabled"] = json!(meta["enabled"] != false);
            if meta["order"].is_i64() {
                row["order"] = meta["order"].clone();
            }
        }
        row["version"] = preview.manifest["version"].clone();
        row["origin"] = json!(origin);
        row["url"] = json!(preview.url);
        row["sha256"] = json!(preview.sha256);
        let mut generation = [0u8; 16];
        getrandom::fill(&mut generation).map_err(|_| Error::Storage)?;
        row["installGeneration"] = json!(u128::from_be_bytes(generation).to_string());
        call.check()?;
        self.sessions
            .partition(key, Purpose::Source)?
            .invalidate()?;
        snapshot
            .commit(
                &self.root,
                Some(tmp.path()),
                Some(&row),
                Some(preview.code.as_bytes()),
                scope,
                browser,
            )
            .map_err(|_| Error::Storage)
    }
    pub fn withdraw(
        &self,
        expected: &Value,
        browser: Option<&dyn RecoveryBrowser>,
        call: Invocation,
    ) -> Result<(), Error> {
        let key = expected["key"].as_str().ok_or(Error::Config)?;
        let _lock = self.lock(true, &call)?;
        let snapshot = InstallJournal::capture(&self.root, key).map_err(|_| Error::Storage)?;
        if snapshot.source.as_ref() != Some(expected)
            || !park_storage::plugins::is_official(expected)
        {
            return Err(Error::Config);
        }
        let manifest = installed_manifest(
            &self.root,
            key,
            expected["sha256"].as_str().unwrap_or(""),
            &self.polyfills,
            call.clone(),
        )?;
        call.check()?;
        self.sessions
            .partition(key, Purpose::Source)?
            .invalidate()?;
        snapshot
            .commit(&self.root, None, None, None, domains(&manifest), browser)
            .map_err(|_| Error::Storage)
    }
}
