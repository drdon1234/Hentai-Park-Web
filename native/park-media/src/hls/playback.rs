use super::{Error, HlsClient, Kind, MediaList, Result};
use crate::loopback::{Listener, PlaybackRoute};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, Method, Response, StatusCode},
    Router,
};
use std::{fmt::Write, sync::Arc};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use url::Url;

struct Track {
    list: Arc<MediaList>,
    url: Url,
}
struct HlsRoute {
    windows: Option<Arc<super::windows::Windows>>,
    client: Arc<HlsClient>,
    video: Track,
    audio: Option<Track>,
    authority: String,
    prefix: String,
    stop: CancellationToken,
    permits: Semaphore,
}
pub(super) fn playlist(list: &MediaList, kind: &str, windows: bool) -> String {
    let segments = &list.segments;
    let target = list
        .target_seconds
        .unwrap_or_else(|| {
            segments
                .iter()
                .map(|s| s.seconds.ceil() as u32)
                .max()
                .unwrap_or(1)
        })
        .max(1);
    let mut text=format!("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MEDIA-SEQUENCE:{}\n",segments[0].sequence);
    if windows || list.initialization.is_some() {
        writeln!(text, "#EXT-X-MAP:URI=\"{kind}/init.mp4\"").unwrap();
    }
    for (i, s) in segments.iter().enumerate() {
        if s.discontinuity {
            text.push_str("#EXT-X-DISCONTINUITY\n");
            if windows && i > 0 {
                writeln!(text, "#EXT-X-MAP:URI=\"{kind}/{i}.mp4\"").unwrap();
            }
        }
        let ext = if windows || s.kind == Kind::Fragment {
            "m4s"
        } else {
            "ts"
        };
        writeln!(text, "#EXTINF:{},\n{kind}/{i}.{ext}", s.seconds).unwrap();
    }
    text.push_str("#EXT-X-ENDLIST\n");
    text
}
pub(super) const MASTER:&str="#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"audio\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,AUDIO=\"audio\"\nvideo.m3u8\n";
impl PlaybackRoute {
    pub async fn hls(
        client: Arc<HlsClient>,
        stop: CancellationToken,
        windowed_ts: bool,
    ) -> Result<Self> {
        let video = Track {
            url: client.video_url().clone(),
            list: Arc::new(client.media(client.video_url(), false, &stop).await?),
        };
        let audio = if let Some(url) = client.audio_url() {
            Some(Track {
                url: url.clone(),
                list: Arc::new(client.media(url, false, &stop).await?),
            })
        } else {
            None
        };
        let listener = Listener::bind().await?;
        let entry = if audio.is_some() {
            "master.m3u8"
        } else {
            "video.m3u8"
        };
        let state = HlsRoute {
            windows: (windowed_ts
                && audio.is_none()
                && video.list.initialization.is_none()
                && video.list.segments.len() > 120)
                .then(|| {
                    super::windows::Windows::new(
                        client.clone(),
                        video.url.clone(),
                        video.list.clone(),
                        stop.child_token(),
                    )
                }),
            client,
            video,
            audio,
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
pub(super) fn empty(code: StatusCode) -> Response<Body> {
    Response::builder()
        .status(code)
        .body(Body::empty())
        .unwrap()
}
async fn serve(State(s): State<Arc<HlsRoute>>, request: Request) -> Response<Body> {
    if s.stop.is_cancelled()
        || request.uri().query().is_some()
        || request
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            != Some(&s.authority)
    {
        return empty(StatusCode::NOT_FOUND);
    }
    if !matches!(*request.method(), Method::HEAD | Method::GET) {
        return empty(StatusCode::METHOD_NOT_ALLOWED);
    }
    let Some(path) = request.uri().path().strip_prefix(&s.prefix) else {
        return empty(StatusCode::NOT_FOUND);
    };
    let manifest = match path {
        "master.m3u8" if s.audio.is_some() => Some(MASTER.to_owned()),
        "video.m3u8" => Some(playlist(&s.video.list, "video", s.windows.is_some())),
        "audio.m3u8" => s.audio.as_ref().map(|s| playlist(&s.list, "audio", false)),
        _ => None,
    };
    let (content_type, bytes) = if let Some(text) = manifest {
        (
            "application/vnd.apple.mpegurl",
            axum::body::Bytes::from(text),
        )
    } else if let (Some(windows), Some(name)) = (&s.windows, path.strip_prefix("video/")) {
        let (index, init) = if name == "init.mp4" {
            (0, true)
        } else if let Some(i) = name
            .strip_suffix(".mp4")
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|i| {
                s.video
                    .list
                    .segments
                    .get(*i)
                    .is_some_and(|seg| seg.discontinuity)
            })
        {
            (i, true)
        } else if let Some(i) = name
            .strip_suffix(".m4s")
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n < s.video.list.segments.len())
        {
            (i, false)
        } else {
            return empty(StatusCode::NOT_FOUND);
        };
        match windows.resource(index, init).await {
            Ok(bytes) => ("video/mp4", bytes),
            Err(Error::Cancelled) => return empty(StatusCode::GONE),
            Err(_) => return empty(StatusCode::BAD_GATEWAY),
        }
    } else {
        let Some((track, name)) = path.split_once('/') else {
            return empty(StatusCode::NOT_FOUND);
        };
        let track = match track {
            "video" => Some(&s.video),
            "audio" => s.audio.as_ref(),
            _ => None,
        };
        let Some(track) = track else {
            return empty(StatusCode::NOT_FOUND);
        };
        let segment = if name == "init.mp4" {
            track.list.initialization.as_ref()
        } else {
            name.split_once('.').and_then(|(i, ext)| {
                let s = track.list.segments.get(i.parse::<usize>().ok()?)?;
                (ext == if s.kind == Kind::TransportStream {
                    "ts"
                } else {
                    "m4s"
                })
                .then_some(s)
            })
        };
        let Some(segment) = segment else {
            return empty(StatusCode::NOT_FOUND);
        };
        let _permit = tokio::select! {biased;_=s.stop.cancelled()=>return empty(StatusCode::GONE),p=s.permits.acquire()=>p};
        let data = match s.client.segment(&track.url, segment, false, &s.stop).await {
            Ok(v) => v,
            Err(Error::Cancelled) => return empty(StatusCode::GONE),
            Err(_) => return empty(StatusCode::BAD_GATEWAY),
        };
        (
            if segment.kind == Kind::TransportStream {
                "video/mp2t"
            } else {
                "video/mp4"
            },
            axum::body::Bytes::from(data),
        )
    };
    resource_response(&request,content_type,bytes)
}
pub(super) fn resource_response(request:&Request, content_type:&str, bytes:axum::body::Bytes)->Response<Body>{
    let len = bytes.len() as u64;
    let mut response = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCEPT_RANGES, "bytes");
    let full = bytes;
    let body = if let Some(raw) = request.headers().get(header::RANGE) {
        let Some((a, b)) = raw
            .to_str()
            .ok()
            .and_then(|r| crate::direct::range(Some(r), len))
        else {
            return Response::builder()
                .status(416)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap();
        };
        response = response
            .status(206)
            .header(header::CONTENT_RANGE, format!("bytes {a}-{}/{len}", b - 1));
        full.slice(a as usize..b as usize)
    } else {
        full
    };
    response
        .header(header::CONTENT_LENGTH, body.len())
        .body(if request.method() == Method::HEAD {
            Body::empty()
        } else {
            Body::from(body)
        })
        .unwrap()
}
