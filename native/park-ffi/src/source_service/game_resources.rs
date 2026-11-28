use super::*;
use serde_json::{json, Value};
use std::collections::HashSet;
mod desktop;
pub use desktop::{open_host_game_offers, HostGameChoice, HostGameOffers};

fn field<'a>(value: &'a Value, name: &str, limit: usize) -> Result<&'a str> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(""),
        Some(Value::String(s)) if s.len() <= limit && !s.contains('\0') => Ok(s),
        _ => Err(SourceFailure::Unsupported),
    }
}

fn id(value: &Value) -> Result<&str> {
    let s = field(value, "id", 2048)?;
    if s.trim().is_empty() {
        return Err(SourceFailure::Unsupported);
    }
    Ok(s)
}

fn labels(value: &Value, key: &str, max: usize) -> Result<Vec<String>> {
    if value[key].is_null() {
        return Ok(Vec::new());
    }
    value[key]
        .as_array()
        .filter(|a| a.len() <= 100)
        .ok_or(SourceFailure::Unsupported)?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .filter(|s| s.len() <= max && !s.contains('\0'))
                .map(str::to_owned)
                .ok_or(SourceFailure::Unsupported)
        })
        .collect()
}

fn offers(raw: Value) -> Result<Vec<Value>> {
    let rows = raw
        .as_array()
        .filter(|v| v.len() <= 1000)
        .ok_or(SourceFailure::Unsupported)?;
    let mut resource_ids = HashSet::new();
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let resource_id = id(row)?;
        if !resource_ids.insert(resource_id) {
            return Err(SourceFailure::Unsupported);
        }
        let choices = row["channels"]
            .as_array()
            .filter(|v| v.len() <= 100)
            .ok_or(SourceFailure::Unsupported)?;
        let mut channels = Vec::with_capacity(choices.len());
        let mut channel_ids = HashSet::new();
        for choice in choices {
            let channel_id = id(choice)?;
            let kind = field(choice, "kind", 16)?;
            let requires_reply = choice
                .get("requiresReply")
                .map(Value::as_bool)
                .unwrap_or(Some(false))
                .ok_or(SourceFailure::Unsupported)?;
            let reply = field(choice, "replyText", 64000)?;
            if !channel_ids.insert(channel_id)
                || !matches!(kind, "external" | "direct")
                || requires_reply && reply.trim().is_empty()
            {
                return Err(SourceFailure::Unsupported);
            }
            channels.push(json!({"id":channel_id,"kind":kind,"label":field(choice,"label",8192)?,
                "size":field(choice,"size",1024)?,"requiresReply":requires_reply,"replyText":reply}));
        }
        result.push(json!({"id":resource_id,"title":field(row,"title",8192)?,"version":field(row,"version",2048)?,
            "size":field(row,"size",1024)?,"notes":field(row,"notes",262144)?,"testStatus":field(row,"testStatus",32)?,
            "platforms":labels(row,"platforms",1024)?,"languages":labels(row,"languages",1024)?,
            "expired":row["status"]=="expired","channels":channels}));
    }
    Ok(result)
}

fn external_address(raw: &str) -> Result<Url> {
    if raw.len() > 16384 || raw.chars().any(char::is_control) {
        return Err(SourceFailure::Unsupported);
    }
    let url = Url::parse(raw).map_err(|_| SourceFailure::Unsupported)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || !matches!(
            url.scheme(),
            "https" | "http" | "ftps" | "ftp" | "magnet" | "ed2k" | "thunder"
        )
    {
        return Err(SourceFailure::Unsupported);
    }
    Ok(url)
}

fn available_channel<'a>(rows: &'a [Value], resource: &str, channel: &str) -> Result<&'a Value> {
    rows.iter()
        .find(|r| r["id"] == resource && r["expired"] == false)
        .and_then(|row| row["channels"].as_array())
        .and_then(|channels| channels.iter().find(|c| c["id"] == channel))
        .ok_or(SourceFailure::Unsupported)
}

fn confirmed_choice<'a>(
    rows: &'a [Value],
    resource: &str,
    channel: &str,
    confirmed_reply: Option<&str>,
) -> Result<&'a Value> {
    let choice = available_channel(rows, resource, channel)?;
    let reply = if choice["requiresReply"] == true {
        choice["replyText"].as_str()
    } else {
        None
    };
    if confirmed_reply != reply {
        return Err(SourceFailure::Unsupported);
    }
    Ok(choice)
}

fn validate_choice(choice: &Value, latest: &[Value], resource: &str, channel: &str) -> Result<()> {
    let current = available_channel(latest, resource, channel)?;
    if ["kind", "requiresReply", "replyText"]
        .iter()
        .any(|key| choice[*key] != current[*key])
    {
        return Err(SourceFailure::Unsupported);
    }
    Ok(())
}

#[derive(Object)]
pub struct GameOffers {
    catalog: Arc<SourceCatalog>,
    source: String,
    work: String,
    digest: String,
    rows: Vec<Value>,
    host: Arc<RouteHost>,
}

#[derive(Object, Clone)]
pub struct GameLinks {
    details: Value,
    direct: Option<VideoSource>,
    source: String,
    work: String,
    edition: String,
    channel: String,
    label: String,
    host: Arc<RouteHost>,
}

#[derive(Object)]
pub struct GameFile {
    path: PathBuf,
    _directory: tempfile::TempDir,
}

#[uniffi::export]
impl SourceService {
    pub fn game_offers(
        &self,
        source: String,
        work: String,
        request: Arc<Request>,
    ) -> Result<Arc<GameOffers>> {
        let (manifest, digest) = self
            .catalog
            .manifest_with_digest(&source, plugins::budget(&request))?;
        if manifest["category"] != "game"
            || manifest["capabilities"]["gameResources"] != true
            || manifest["capabilities"]["resolveGameResource"] != true
        {
            return Err(SourceFailure::Unsupported);
        }
        let (_, raw) = self.catalog.invoke_installed(
            &source,
            "gameResources",
            &[work.clone().into()],
            plugins::budget(&request),
            Some(&digest),
        )?;
        Ok(Arc::new(GameOffers {
            catalog: self.catalog.clone(),
            source,
            work,
            digest,
            rows: offers(downloads::checked(raw)?)?,
            host: self.routes.clone(),
        }))
    }
}

#[uniffi::export]
impl GameOffers {
    pub fn entries(&self) -> String {
        Value::Array(self.rows.clone()).to_string()
    }

    pub fn resolve(
        &self,
        resource: String,
        channel: String,
        confirmed_reply: Option<String>,
        request: Arc<Request>,
    ) -> Result<Arc<GameLinks>> {
        let choice = confirmed_choice(&self.rows, &resource, &channel, confirmed_reply.as_deref())?;
        let (_, preview) = self.catalog.invoke_installed(
            &self.source,
            "gameResources",
            &[self.work.clone().into()],
            plugins::budget(&request),
            Some(&self.digest),
        )?;
        let latest = offers(downloads::checked(preview)?)?;
        validate_choice(choice, &latest, &resource, &channel)?;
        let (manifest, envelope) = self.catalog.invoke_installed(
            &self.source,
            "resolveGameResource",
            &[
                self.work.clone().into(),
                resource.clone().into(),
                channel.clone().into(),
            ],
            plugins::budget(&request),
            Some(&self.digest),
        )?;
        build_links(
            &self.host,
            &self.source,
            &self.work,
            resource,
            channel,
            choice,
            &manifest,
            envelope,
        )
    }
}

fn build_links(
    host: &Arc<RouteHost>,
    source: &str,
    work: &str,
    resource: String,
    channel: String,
    choice: &Value,
    manifest: &Value,
    envelope: Value,
) -> Result<Arc<GameLinks>> {
    let mut direct = None;
    let details = if envelope["ok"] == false && envelope["error"]["code"] == "CONFIRM_REQUIRED" {
        let original = manual_read::confirmation_page(manifest, &envelope["error"]["url"]);
        json!({"confirmRequired":true,"original":original.map(|u| u.to_string())})
    } else {
        let value = downloads::checked(envelope)?;
        if value["kind"] != choice["kind"] {
            return Err(SourceFailure::Unsupported);
        }
        let urls = if value["kind"] == "direct" {
            // 原有动态媒体域授权适用于本次解析；传输分区不携带原站凭据。
            let address = field(&value, "url", 16384)?;
            let origin = manifest["media"]["origin"]
                .as_str()
                .ok_or(SourceFailure::Unsupported)?;
            direct = Some(VideoSource::from_plugin(
                manifest,
                &json!({"url":address,
                    "headers":{"referer":format!("{}/", origin.trim_end_matches('/'))}}),
            )?);
            vec![address.to_owned()]
        } else {
            let values = if value["urls"].is_null() {
                vec![field(&value, "url", 16384)?.to_owned()]
            } else {
                labels(&value, "urls", 16384)?
            };
            if values.is_empty() {
                return Err(SourceFailure::Unsupported);
            }
            values
                .iter()
                .map(|s| external_address(s).map(|url| url.to_string()))
                .collect::<Result<Vec<_>>>()?
        };
        let codes = labels(&value, "extractionCodes", 16384)?;
        if !value["extractionCodes"].is_null() && codes.len() != urls.len() {
            return Err(SourceFailure::Unsupported);
        }
        json!({"urls":urls,"direct":direct.is_some(),"filename":field(&value,"filename",2048)?,
                "extractionCodes":codes,"extractionCode":field(&value,"extractionCode",16384)?,"archivePassword":field(&value,"archivePassword",16384)?})
    };
    Ok(Arc::new(GameLinks {
        details,
        direct,
        source: source.to_owned(),
        work: work.to_owned(),
        edition: resource,
        channel,
        label: choice["label"].as_str().unwrap_or_default().into(),
        host: host.clone(),
    }))
}

#[uniffi::export]
impl GameLinks {
    pub fn enqueue_download(
        &self,
        store: Arc<ProgressStore>,
        directory: String,
        title: String,
    ) -> Result<Arc<GameLinks>> {
        if self.direct.is_none() {
            return Err(SourceFailure::Unsupported);
        }
        let now: String = store
            .lock()
            .map_err(|_| SourceFailure::Storage)?
            .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')", [], |r| {
                r.get(0)
            })
            .map_err(|_| SourceFailure::Storage)?;
        let item = json!({"source":self.source,"id":self.work,"title":title,"mediaKind":"game","contentDomain":"game"});
        let name = park_storage::game_downloads::safe_filename(&self.label);
        park_storage::game_downloads::enqueue(
            &store.root,
            std::path::Path::new(&directory),
            item,
            &self.edition,
            &self.channel,
            &name,
            &now,
        )
        .map_err(|_| SourceFailure::Storage)?;
        Ok(Arc::new(self.clone()))
    }
    pub fn details(&self) -> String {
        self.details.to_string()
    }

    /// 每次显式保存均从头传输，校验通过才把私有临时文件交给系统导出。
    pub fn download(&self, request: Arc<Request>) -> Result<Arc<GameFile>> {
        self.transfer(request, None)
    }
}

impl GameLinks {
    fn transfer(
        &self,
        request: Arc<Request>,
        progress: Option<park_media::download::GameProgress<'_>>,
    ) -> Result<Arc<GameFile>> {
        let line = self.direct.as_ref().ok_or(SourceFailure::Unsupported)?;
        let session = self
            .host
            .sessions
            .partition(&self.source, Purpose::Media)
            .map_err(|_| SourceFailure::Storage)?;
        std::fs::create_dir_all(&self.host.cache).map_err(|_| SourceFailure::Storage)?;
        let dir = tempfile::Builder::new()
            .prefix("game-")
            .tempdir_in(&self.host.cache)
            .map_err(|_| SourceFailure::Storage)?;
        let path = self.host.runtime.block_on(park_media::download::game_file(
            &self.host.transport,
            &session,
            line,
            dir.path(),
            &request.cancelled,
            progress,
        ))?;
        Ok(Arc::new(GameFile {
            path,
            _directory: dir,
        }))
    }
}

#[uniffi::export]
impl GameFile {
    pub fn path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

#[uniffi::export]
impl ProgressStore {
    pub fn game_downloads(&self) -> std::result::Result<String, crate::StorageError> {
        park_storage::game_downloads::list(&self.root)
            .map(|rows| Value::Array(rows).to_string())
            .map_err(|_| crate::StorageError::InvalidDownload)
    }
    pub fn change_game_download(
        &self,
        identity: Vec<String>,
        action: String,
    ) -> std::result::Result<(), crate::StorageError> {
        let ids: [String; 4] = identity
            .try_into()
            .map_err(|_| crate::StorageError::InvalidDownload)?;
        park_storage::game_downloads::change(
            &self.root,
            ids.each_ref().map(String::as_str),
            &action,
        )
        .map_err(|_| crate::StorageError::InvalidDownload)
    }
}

/// 一次持久化队列尝试；宿主解析失败、请求撤销或离开进程前均可释放占用。
#[derive(Object)]
pub struct GameTransfer {
    store: Arc<ProgressStore>,
    ids: [String; 4],
    attempt: u64,
    request: Arc<Request>,
    finished: std::sync::Mutex<bool>,
}

#[uniffi::export]
impl ProgressStore {
    pub fn claim_game_download(
        self: Arc<Self>,
        identity: Vec<String>,
        request: Arc<Request>,
    ) -> Result<Arc<GameTransfer>> {
        let ids: [String; 4] = identity
            .try_into()
            .map_err(|_| SourceFailure::Unsupported)?;
        let attempt =
            park_storage::game_downloads::claim(&self.root, ids.each_ref().map(String::as_str))
                .map_err(|_| SourceFailure::Storage)?;
        Ok(Arc::new(GameTransfer {
            store: self,
            ids,
            attempt,
            request,
            finished: std::sync::Mutex::new(false),
        }))
    }
}

#[uniffi::export]
impl GameTransfer {
    pub fn save(&self, links: Arc<GameLinks>) -> Result<()> {
        use park_storage::game_downloads as queue;
        let mut finished = self.finished.lock().map_err(|_| SourceFailure::Storage)?;
        let key = self.ids.each_ref().map(String::as_str);
        if *finished
            || key
                != [
                    links.source.as_str(),
                    links.work.as_str(),
                    links.edition.as_str(),
                    links.channel.as_str(),
                ]
        {
            return Err(SourceFailure::Unsupported);
        }
        let report = |bytes, total| {
            queue::progress(&self.store.root, key, self.attempt, bytes, total)
                .map_err(|_| park_media::Error::Storage)
        };
        let file = links.transfer(self.request.clone(), Some(&report))?;
        queue::publish(
            &self.store.root,
            key,
            self.attempt,
            &file.path,
            &links.details,
            || self.request.cancelled.is_cancelled(),
        )
        .map_err(|_| SourceFailure::Storage)?;
        *finished = true;
        Ok(())
    }
}

impl Drop for GameTransfer {
    fn drop(&mut self) {
        if !*self
            .finished
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
        {
            let _ = park_storage::game_downloads::finish_error(
                &self.store.root,
                self.ids.each_ref().map(String::as_str),
                self.attempt,
                self.request.cancelled.is_cancelled(),
            );
        }
    }
}

#[uniffi::export]
impl SourceService {
    pub fn run_game_download(
        &self,
        store: Arc<ProgressStore>,
        identity: Vec<String>,
        resolved: Option<Arc<GameLinks>>,
        request: Arc<Request>,
    ) -> Result<()> {
        let transfer = store.claim_game_download(identity, request.clone())?;
        let links = match resolved {
            Some(links) => links,
            None => {
                let [source, work, resource, channel] = &transfer.ids;
                let menu = self.game_offers(source.clone(), work.clone(), request.clone())?;
                // 后台重试从不授权新的原站回复。若站点重新要求确认，用户须回到资源入口。
                menu.resolve(resource.clone(), channel.clone(), None, request)?
            }
        };
        transfer.save(links)
    }
}
