use crate::{ChapterKey, ProgressStore, Request};
use park_media::{PlaybackRoute, VideoSource};
use park_net::{Purpose, Sessions, Transport};
use park_runtime::{Invocation, SourceCatalog};
use std::{path::PathBuf, sync::Arc, time::Duration};
use uniffi::{Object, Record};
use url::Url;
mod plugins;
mod dictionaries;
pub use dictionaries::DictionaryIndex;
mod host_video;
pub use host_video::open_host_video;
mod accounts;
pub use accounts::SiteAccount;
mod game;
mod tavern_resource;
pub use tavern_resource::{SourceTavernResource, TavernOriginal};
pub use game::OnlineGame;
mod game_resources;
pub use game_resources::{GameFile, GameLinks, GameOffers};
mod downloads;
pub use downloads::DownloadBook;
mod comic;
mod novel;
mod translation;
pub(crate) use comic::RemoteChapter;
mod discovery;
mod manual_read;
pub use manual_read::{BrowseResult, ManualRead};
pub use discovery::{BrowseMethod, SitePreference, SourceImages};
mod reverse;
pub use reverse::{ReverseCandidates, ReverseQuery};
mod updates;
mod library_updates;
mod subscriptions;
pub use subscriptions::{SubscriptionDimension, SubscriptionSite};
mod voice;
pub use voice::VoiceCue;
mod recordings;
pub use recordings::LiveRecording;
mod live_chat;
pub use live_chat::{ChatMessage,LiveChatOutput};
pub use updates::{AppRelease,UpdateProgress};
pub use plugins::{BundledScripts, PluginChange, PluginOffer, PluginUpdates};

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SourceFailure {
    #[error("unavailable")]
    Unavailable,
    #[error("cancelled")]
    Cancelled,
    #[error("unsupported")]
    Unsupported,
    #[error("storage")]
    Storage,
    #[error("login_required")]
    LoginRequired,
    #[error("configuration")]
    Configuration,
    #[error("confirmation_required")]
    ConfirmationRequired,
}
type Result<T> = std::result::Result<T, SourceFailure>;
impl From<park_media::Error> for SourceFailure {
    fn from(error: park_media::Error) -> Self {
        match error {
            park_media::Error::Cancelled => Self::Cancelled,
            park_media::Error::Storage => Self::Storage,
            _ => Self::Unavailable,
        }
    }
}
impl From<park_runtime::Error> for SourceFailure {
    fn from(e: park_runtime::Error) -> Self {
        match e {
            park_runtime::Error::Cancelled => Self::Cancelled,
            park_runtime::Error::Storage => Self::Storage,
            park_runtime::Error::Unsupported => Self::Unsupported,
            _ => Self::Unavailable,
        }
    }
}
#[derive(Object)]
pub struct SourceService {
    catalog: Arc<SourceCatalog>,
    routes: Arc<RouteHost>,
}

#[uniffi::export(callback_interface)]
pub trait TavernOutput: Send + Sync {
    fn delta(&self, text: String);
}

#[uniffi::export]
impl SourceService {
    pub fn generate_tavern(
        &self,
        base: String,
        model: String,
        key: String,
        messages: String,
        temperature: f64,
        max_tokens: u32,
        request: Arc<Request>,
        output: Box<dyn TavernOutput>,
    ) -> Result<()> {
        let parts: Vec<serde_json::Value> =
            serde_json::from_str(&messages).map_err(|_| SourceFailure::Unsupported)?;
        if model.trim().is_empty()
            || model.len() > 200
            || !(0.0..=2.0).contains(&temperature)
            || !(1..=32768).contains(&max_tokens)
            || parts.is_empty()
            || parts.iter().any(|m| {
                !matches!(m["role"].as_str(), Some("system" | "user" | "assistant"))
                    || !m["content"].is_string()
            })
        {
            return Err(SourceFailure::Unsupported);
        }
        let payload=serde_json::to_vec(&serde_json::json!({"model":model,"messages":parts,"temperature":temperature,"max_tokens":max_tokens,"stream":true})).map_err(|_|SourceFailure::Unsupported)?;
        self.routes
            .runtime
            .block_on(self.routes.transport.model_reply(
                &base,
                &key,
                payload,
                &request.cancelled,
                |s| output.delta(s),
            ))
            .map_err(|e| {
                if e == park_net::Error::Cancelled {
                    SourceFailure::Cancelled
                } else {
                    SourceFailure::Unavailable
                }
            })
    }
}
struct RouteHost {
    cache: PathBuf,
    sessions: Arc<Sessions>,
    transport: Arc<Transport>,
    runtime: Arc<tokio::runtime::Runtime>,
}
#[derive(Object)]
pub struct VideoLines {
    live: bool,
    episode: Option<(String, String)>,
    web: Option<Arc<WebPlayback>>,
    owner: String,
    grant: Vec<VideoSource>,
    host: Arc<RouteHost>,
}
#[derive(Object)]
pub struct WebPlayback(park_net::WebNavigation);

#[uniffi::export]
impl WebPlayback {
    pub fn url(&self) -> String {
        self.0.initial.to_string()
    }
    pub fn allows_navigation(&self, url: String, subframe: bool) -> bool {
        self.0.allows(&url, subframe)
    }
    pub fn allows_resource(&self, url: String) -> bool {
        self.0.allows_resource(&url)
    }
}
#[derive(Record)]
pub struct VideoLine {
    pub label: String,
    pub preferred: bool,
}
#[derive(Object)]
pub struct PlaybackSession {
    inner: PlaybackRoute,
    _runtime: Arc<tokio::runtime::Runtime>,
}
#[uniffi::export]
impl SourceService {
    #[uniffi::constructor]
    pub fn new(
        support: String,
        cache_directory: String,
        polyfills: String,
        proxy: String,
        system_roots: Vec<Vec<u8>>,
    ) -> Result<Arc<Self>> {
        park_domain::install_crash_diagnostics();
        let root = PathBuf::from(support).join("park");
        let sessions = Arc::new(Sessions::new(&root));
        let routes = RouteHost::open(cache_directory, sessions.clone(), proxy, system_roots)?;
        Ok(Arc::new(Self {
            catalog: Arc::new(SourceCatalog::new(&root, polyfills, sessions, routes.transport.clone())),
            routes,
        }))
    }
    pub fn video_lines(
        &self,
        source: String,
        work: String,
        chapter: String,
        request: Arc<Request>,
    ) -> Result<Arc<VideoLines>> {
        let budget = Invocation::child(Duration::from_secs(45), &request.cancelled);
        let args = [work.as_str().into(), chapter.as_str().into()];
        let (manifest, result) = self
            .catalog
            .invoke(&source, "videos", &args, budget.clone())?;
        let code = result["error"]["code"].as_str();
        let live = manifest["type"] == "video" && manifest["category"] == "live";
        if code == Some("LOGIN_REQUIRED") {
            return Err(SourceFailure::LoginRequired);
        }
        if manifest["capabilities"]["webVideo"] == true
            && (manifest["capabilities"]["videos"] != true
                || code == Some("UNSUPPORTED")
                || result["value"].as_array().is_some_and(Vec::is_empty))
        {
            let (_, response) = self.catalog.invoke(&source, "webVideo", &args, budget)?;
            if response["ok"] != true {
                return Err(SourceFailure::Unavailable);
            }
            let navigation = park_net::WebNavigation::from_plugin(&response["value"])
                .map_err(|_| SourceFailure::Unsupported)?;
            return Ok(Arc::new(VideoLines {
                live,
                episode: None,
                owner: source,
                grant: Vec::new(),
                web: Some(Arc::new(WebPlayback(navigation))),
                host: self.routes.clone(),
            }));
        }
        if result["ok"] != true {
            return Err(SourceFailure::Unavailable);
        }
        self.routes.lines(source, &manifest, &result["value"], live, Some((work, chapter)), &request)
    }
}

fn playback_runtime() -> Result<Arc<tokio::runtime::Runtime>> {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()
        .map(Arc::new).map_err(|_| SourceFailure::Unavailable)
}

#[uniffi::export]
impl ProgressStore {
    pub fn offline_video(&self, key: ChapterKey, request: Arc<Request>) -> Result<Arc<PlaybackSession>> {
        request.check().map_err(|_| SourceFailure::Cancelled)?;
        let file = self.video_file(key).map_err(|_| SourceFailure::Storage)?;
        offline_route(file, &request)
    }

    pub fn offline_bundle_media(&self, key: ChapterKey, node: String, request: Arc<Request>) -> Result<Arc<PlaybackSession>> {
        request.check().map_err(|_| SourceFailure::Cancelled)?;
        let file = self.checked_bundle_file(&key, &node, true).map_err(|_| SourceFailure::Storage)?;
        offline_route(file, &request)
    }
}

fn offline_route(file: crate::BundleFile, request: &Request) -> Result<Arc<PlaybackSession>> {
    let runtime = playback_runtime()?;
    let inner = runtime.block_on(PlaybackRoute::offline(file.path.into(), file.resources, request.cancelled.child_token()))?;
    request.check().map_err(|_| SourceFailure::Cancelled)?;
    Ok(Arc::new(PlaybackSession { inner, _runtime: runtime }))
}

impl RouteHost {
    fn open(cache: String, sessions: Arc<Sessions>, proxy: String, system_roots: Vec<Vec<u8>>) -> Result<Arc<Self>> {
        let proxy = if proxy.trim().is_empty() {
            None
        } else {
            let url = if proxy.contains("://") {
                proxy
            } else {
                format!("http://{proxy}")
            };
            Some(Url::parse(&url).map_err(|_| SourceFailure::Unavailable)?)
        };
        let transport =
            Arc::new(Transport::with_system_trust(proxy, system_roots).map_err(|_| SourceFailure::Unavailable)?);
        let runtime = playback_runtime()?;
        Ok(Arc::new(Self { cache: cache.into(), sessions, transport, runtime }))
    }

    fn lines(self: &Arc<Self>, source: String, manifest: &serde_json::Value,
             response: &serde_json::Value, live: bool, episode: Option<(String, String)>, request: &Request) -> Result<Arc<VideoLines>> {
        let values = response
            .as_array()
            .ok_or(SourceFailure::Unavailable)?;
        let mut grant = Vec::new();
        for entry in values {
            let line = VideoSource::from_plugin(manifest, entry)
                .map_err(|_| SourceFailure::Unsupported)?;
            if line.hls {
                let session = self
                    .sessions
                    .partition(&source, Purpose::Media)
                    .map_err(|_| SourceFailure::Storage)?;
                let hls =
                    park_media::hls::HlsClient::new(self.transport.clone(), session, line)?;
                grant.extend(
                    self
                        .runtime
                        .block_on(hls.variants(live, &request.cancelled))?,
                );
            } else {
                grant.push(line);
            }
        }
        if grant.is_empty() {
            return Err(SourceFailure::Unsupported);
        }
        Ok(Arc::new(VideoLines {
            live,
            episode,
            web: None,
            owner: source,
            grant,
            host: self.clone(),
        }))
    }
}
#[uniffi::export]
impl VideoLines {
    pub fn web_playback(&self) -> Option<Arc<WebPlayback>> {
        self.web.clone()
    }
    pub fn options(&self) -> Vec<VideoLine> {
        self.grant
            .iter()
            .map(|g| VideoLine {
                label: g.label.clone(),
                preferred: g.preferred,
            })
            .collect()
    }
    pub fn play(&self, index: i32, request: Arc<Request>) -> Result<Arc<PlaybackSession>> {
        let grant = self
            .grant
            .get(usize::try_from(index).map_err(|_| SourceFailure::Unavailable)?)
            .ok_or(SourceFailure::Unavailable)?
            .clone();
        let session = self
            .host
            .sessions
            .partition(&self.owner, Purpose::Media)
            .map_err(|_| SourceFailure::Storage)?;
        let token = request.cancelled.child_token();
        let inner = if grant.hls {
            let client = Arc::new(park_media::hls::HlsClient::new(
                self.host.transport.clone(),
                session,
                grant,
            )?);
            if self.live { self.host.runtime.block_on(PlaybackRoute::live(client, token))? }
            else {self.host.runtime.block_on(PlaybackRoute::hls(
                client,
                token,
                true,
            ))?}
        } else {
            self.host.runtime.block_on(PlaybackRoute::direct(
                self.host.transport.clone(),
                session,
                grant,
                &self.host.cache,
                token,
            ))?
        };
        Ok(Arc::new(PlaybackSession {
            inner,
            _runtime: self.host.runtime.clone(),
        }))
    }
}
#[uniffi::export]
impl PlaybackSession {
    pub fn local_url(&self) -> String {
        self.inner.url().to_owned()
    }
    pub fn revoke(&self) {
        self.inner.close();
    }
}
