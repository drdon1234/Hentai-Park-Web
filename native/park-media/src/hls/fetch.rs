use super::{
    decode, parse, Error, Kind, MediaList, Playlist, Result, Segment, MANIFEST_LIMIT, SEGMENT_LIMIT,
};
use crate::VideoSource;
use http::{HeaderMap, Method};
use park_net::{Purpose, Request, Session, Transport};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use url::Url;

pub struct HlsClient {
    transport: Arc<Transport>,
    session: Session,
    grant: VideoSource,
    keys: Mutex<HashMap<Url, [u8; 16]>>,
    refresh: AtomicUsize,
}
impl HlsClient {
    pub fn new(transport: Arc<Transport>, session: Session, grant: VideoSource) -> Result<Self> {
        if session.purpose() != Purpose::Media || !grant.hls {
            return Err(Error::Unavailable);
        }
        Ok(Self {
            transport,
            session,
            grant,
            keys: Mutex::default(),
            refresh: AtomicUsize::new(0),
        })
    }
    fn authorized(&self, url: &Url) -> bool {
        park_net::public_https(url)
            && (self.grant.session_scope
                || url.host_str().is_some_and(|h| {
                    self.grant
                        .domains
                        .iter()
                        .any(|d| h == d || h.strip_suffix(d).is_some_and(|s| s.ends_with('.')))
                }))
    }
    async fn bytes(
        &self,
        url: &Url,
        limit: usize,
        token: &CancellationToken,
    ) -> Result<(Url, Vec<u8>)> {
        if !self.authorized(url) {
            return Err(Error::Unavailable);
        }
        let mut headers = HeaderMap::new();
        for (k, v) in &self.grant.headers {
            if !k.as_str().starts_with("x-") || url.origin() == self.grant.header_origin {
                headers.insert(k.clone(), v.clone());
            }
        }
        let request = Request {
            url: url.clone(),
            method: Method::GET,
            headers,
            body: None,
            connect_address: None,
            follow_redirects: true,
        };
        let net = |e| {
            if e == park_net::Error::Cancelled {
                Error::Cancelled
            } else {
                Error::Unavailable
            }
        };
        let mut response = self
            .transport
            .open_media(
                &self.session,
                &self.grant.domains,
                request,
                Duration::from_secs(60),
                token,
                self.grant.session_scope,
            )
            .await
            .map_err(net)?;
        if !response.status.is_success()
            || response
                .headers
                .get(http::header::CONTENT_ENCODING)
                .is_some_and(|v| v != "identity")
        {
            return Err(Error::Unavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.next_chunk().await.map_err(net)? {
            if chunk.len() > limit - bytes.len() {
                return Err(Error::Limit);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((response.url, bytes))
    }
    async fn playlist(&self, url: &Url, live: bool, token: &CancellationToken) -> Result<Playlist> {
        let (base, body) = self.bytes(url, MANIFEST_LIMIT, token).await?;
        let parsed = parse(&body, &base, live)?;
        let ok = match &parsed {
            Playlist::Master(variants) => variants.iter().all(|v| {
                self.authorized(&v.url) && v.audio.as_ref().is_none_or(|a| self.authorized(a))
            }),
            Playlist::Media(list) => list.initialization.iter().chain(&list.segments).all(|s| {
                self.authorized(&s.url)
                    && s.encryption
                        .as_ref()
                        .is_none_or(|k| self.authorized(&k.url))
            }),
        };
        if ok {
            Ok(parsed)
        } else {
            Err(Error::Unavailable)
        }
    }
    pub async fn variants(&self, live: bool, token: &CancellationToken) -> Result<Vec<VideoSource>> {
        let Playlist::Master(list) = self.playlist(&self.grant.url, live, token).await? else {
            return Ok(vec![self.grant.clone()]);
        };
        let preferred = list
            .iter()
            .position(|v| v.height <= self.grant.quality)
            .unwrap_or(0);
        Ok(list
            .into_iter()
            .enumerate()
            .map(|(i, v)| {
                let mut source = self.grant.clone();
                source.url = v.url;
                source.audio = v.audio;
                source.quality = v.height;
                source.selection_format = "hls".into();
                source.preferred = source.preferred && i == preferred;
                source.label = if v.height == 0 {
                    format!("{} kbps", (v.bandwidth + 500) / 1000)
                } else {
                    format!("{}P", v.height)
                };
                source
            })
            .collect())
    }
    pub async fn media(
        &self,
        url: &Url,
        live: bool,
        token: &CancellationToken,
    ) -> Result<MediaList> {
        match self.playlist(url, live, token).await? {
            Playlist::Media(media) => Ok(media),
            Playlist::Master(_) => Err(Error::Unavailable),
        }
    }
    pub fn video_url(&self) -> &Url {
        &self.grant.url
    }
    pub fn audio_url(&self) -> Option<&Url> {
        self.grant.audio.as_ref()
    }
    async fn read_once(&self, segment: &Segment, token: &CancellationToken) -> Result<Vec<u8>> {
        let (_, body) = self.bytes(&segment.url, SEGMENT_LIMIT, token).await?;
        let key = if let Some(enc) = &segment.encryption {
            let hit = self
                .keys
                .lock()
                .map_err(|_| Error::Storage)?
                .get(&enc.url)
                .copied();
            let key = if let Some(value) = hit {
                value
            } else {
                let (_, data) = self.bytes(&enc.url, 16, token).await?;
                let value = <[u8; 16]>::try_from(data).map_err(|_| Error::Unavailable)?;
                let mut keys = self.keys.lock().map_err(|_| Error::Storage)?;
                if keys.len() >= 128 {
                    keys.clear();
                }
                keys.insert(enc.url.clone(), value);
                value
            };
            Some((key, enc.iv))
        } else {
            None
        };
        let kind = segment.kind;
        let read = tokio::task::spawn_blocking(move || {
            decode(&body, kind, key.as_ref().map(|(k, v)| (k, v)))
        });
        let decoded = tokio::select! {biased;_=token.cancelled()=>return Err(Error::Cancelled),r=read=>r.map_err(|_|Error::Unavailable)?};
        if decoded.is_err() {
            if let Some(enc) = &segment.encryption {
                self.keys
                    .lock()
                    .map_err(|_| Error::Storage)?
                    .remove(&enc.url);
            }
        }
        decoded
    }
    async fn candidates(&self, segment: &Segment, token: &CancellationToken) -> Result<Vec<u8>> {
        match self.read_once(segment, token).await {
            Ok(data) => return Ok(data),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(Error::Limit) => return Err(Error::Limit),
            Err(_) => (),
        }
        if let Some(names) = segment
            .url
            .host_str()
            .and_then(|h| self.grant.fallback.get(h))
        {
            for name in names {
                let mut next = segment.clone();
                next.url
                    .set_host(Some(name))
                    .map_err(|_| Error::Unavailable)?;
                if let Some(enc) = &mut next.encryption {
                    if enc.url.host_str() == segment.url.host_str() {
                        enc.url
                            .set_host(Some(name))
                            .map_err(|_| Error::Unavailable)?;
                    }
                }
                match self.read_once(&next, token).await {
                    Ok(data) => return Ok(data),
                    Err(Error::Cancelled) => return Err(Error::Cancelled),
                    Err(Error::Limit) => return Err(Error::Limit),
                    Err(_) => (),
                }
            }
        }
        Err(Error::Unavailable)
    }
    pub async fn segment(
        &self,
        playlist: &Url,
        segment: &Segment,
        live: bool,
        token: &CancellationToken,
    ) -> Result<Vec<u8>> {
        match self.candidates(segment, token).await {
            Err(Error::Unavailable) if !live => (),
            res => return res,
        }
        for _ in 0..2 {
            let mut refresh = playlist.clone();
            let counter = self.refresh.fetch_add(1, Ordering::Relaxed) % 3;
            refresh
                .query_pairs_mut()
                .append_pair("__park_refresh", &counter.to_string());
            let list = match self.media(&refresh, false, token).await {
                Ok(v) => v,
                Err(Error::Unavailable) => continue,
                Err(e) => return Err(e),
            };
            let next = if segment.kind == Kind::Init {
                list.initialization.as_ref()
            } else {
                list.segments
                    .iter()
                    .find(|s| s.sequence == segment.sequence)
            };
            let Some(next) = next
                .filter(|s| s.kind == segment.kind && (s.seconds - segment.seconds).abs() < 0.001)
            else {
                continue;
            };
            match self.candidates(next, token).await {
                Err(Error::Unavailable) => continue,
                res => return res,
            }
        }
        Err(Error::Unavailable)
    }
}
