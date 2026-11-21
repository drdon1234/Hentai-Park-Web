use super::{
    playback::{empty, resource_response, MASTER},
    HlsClient, Kind, MediaList, Result, Segment,
};
use crate::loopback::{Listener, PlaybackRoute};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, Method, Response, StatusCode},
    Router,
};
use std::{collections::HashMap, fmt::Write, sync::Arc};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Default)]
struct Registry {
    next: u64,
    current: HashMap<String, Segment>,
    previous: HashMap<String, Segment>,
}
impl Registry {
    fn refresh(&mut self, list: MediaList, track: &str) -> String {
        self.previous = std::mem::take(&mut self.current);
        let mut previous: HashMap<_, _> = self
            .previous
            .iter()
            .map(|(name, s)| ((s.sequence, s.kind as u8), (name.clone(), s)))
            .collect();
        let mut register = |s: Segment, ext: &str| {
            let slot = (s.sequence, s.kind as u8);
            let name = previous
                .remove(&slot)
                .filter(|(_, old)| **old == s)
                .map(|(name, _)| name)
                .unwrap_or_else(|| {
                    self.next += 1;
                    format!("{}.{ext}", self.next)
                });
            self.current.insert(name.clone(), s);
            name
        };
        let mut out = format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{}\n",
            list.target_seconds.unwrap_or(1),
            list.segments[0].sequence
        );
        if let Some(init) = list.initialization {
            let name = register(init, "mp4");
            writeln!(out, "#EXT-X-MAP:URI=\"{track}/{name}\"").unwrap();
        }
        for s in list.segments {
            let seconds = s.seconds;
            let ext = if s.kind == Kind::Fragment {
                "m4s"
            } else {
                "ts"
            };
            if s.discontinuity {
                out.push_str("#EXT-X-DISCONTINUITY\n")
            }
            writeln!(out, "#EXTINF:{seconds},\n{track}/{}", register(s, ext)).unwrap();
        }
        if list.ended {
            out.push_str("#EXT-X-ENDLIST\n")
        }
        out
    }
}
struct Live {
    client: Arc<HlsClient>,
    tracks: [Mutex<Registry>; 2],
    authority: String,
    prefix: String,
    stop: CancellationToken,
    permits: Semaphore,
}
impl PlaybackRoute {
    pub async fn live(client: Arc<HlsClient>, stop: CancellationToken) -> Result<Self> {
        let listener = Listener::bind().await?;
        let entry = if client.audio_url().is_some() {
            "master.m3u8"
        } else {
            "video.m3u8"
        };
        let state = Live {
            client,
            tracks: Default::default(),
            authority: listener.authority.clone(),
            prefix: listener.prefix.clone(),
            stop: stop.clone(),
            permits: Semaphore::new(4),
        };
        Ok(listener.serve(
            entry,
            Router::new().fallback(serve).with_state(Arc::new(state)),
            stop,
        ))
    }
}
async fn serve(State(s): State<Arc<Live>>, req: Request) -> Response<Body> {
    if s.stop.is_cancelled()
        || req.uri().query().is_some()
        || req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            != Some(&s.authority)
    {
        return empty(StatusCode::NOT_FOUND);
    }
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return empty(StatusCode::METHOD_NOT_ALLOWED);
    }
    let Some(path) = req.uri().path().strip_prefix(&s.prefix) else {
        return empty(StatusCode::NOT_FOUND);
    };
    if path == "master.m3u8" && s.client.audio_url().is_some() {
        return resource_response(&req, "application/vnd.apple.mpegurl", MASTER.into());
    }
    let (name, resource) = if let Some(t) = path.strip_suffix(".m3u8") {
        (t, None)
    } else if let Some((t, r)) = path.split_once('/') {
        (t, Some(r))
    } else {
        return empty(StatusCode::NOT_FOUND);
    };
    let (index, url): (usize, &Url) = match name {
        "video" => (0, s.client.video_url()),
        "audio" if s.client.audio_url().is_some() => (1, s.client.audio_url().unwrap()),
        _ => return empty(StatusCode::NOT_FOUND),
    };
    let _permit = tokio::select! {biased;_=s.stop.cancelled()=>return empty(StatusCode::GONE),p=s.permits.acquire()=>p};
    let mut registry = s.tracks[index].lock().await;
    if let Some(key) = resource {
        let segment = registry
            .current
            .get(key)
            .or_else(|| registry.previous.get(key))
            .cloned();
        drop(registry);
        let Some(segment) = segment else {
            return empty(StatusCode::NOT_FOUND);
        };
        let content = if segment.kind == Kind::TransportStream {
            "video/mp2t"
        } else {
            "video/mp4"
        };
        match s.client.segment(url, &segment, true, &s.stop).await {
            Ok(bytes) => resource_response(&req, content, bytes.into()),
            Err(_) => empty(StatusCode::BAD_GATEWAY),
        }
    } else {
        match s.client.media(url, true, &s.stop).await {
            Ok(list) => resource_response(
                &req,
                "application/vnd.apple.mpegurl",
                registry.refresh(list, name).into(),
            ),
            Err(_) => empty(StatusCode::BAD_GATEWAY),
        }
    }
}
