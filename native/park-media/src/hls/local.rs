use super::{playback, Kind, MediaList, Playlist};
use crate::{direct::range, loopback::Listener, Error, PlaybackRoute};
use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, Method, Response, StatusCode},
    Router,
};
use std::{
    collections::HashMap,
    io::{Read, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::sync::CancellationToken;
use url::Url;

enum Resource {
    Manifest(String),
    File(PathBuf, &'static str),
}
struct Files {
    resources: HashMap<String, Resource>,
    authority: String,
    prefix: String,
    stop: CancellationToken,
}
type Allowed = HashMap<Url, PathBuf>;

fn allowed<'a>(entries: &'a Allowed, url: &Url) -> Result<&'a Path, Error> {
    entries
        .get(url)
        .map(PathBuf::as_path)
        .ok_or(Error::Unavailable)
}

fn parse_file(entries: &Allowed, url: &Url) -> Result<Playlist, Error> {
    let path = allowed(entries, url)?;
    if path.canonicalize().ok().as_deref() != Some(path) {
        return Err(Error::Storage);
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| {
            file.take(super::MANIFEST_LIMIT as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|_| Error::Storage)?;
    super::parse(&bytes, url, false)
}

fn track(
    resources: &mut HashMap<String, Resource>,
    entries: &Allowed,
    name: &str,
    list: MediaList,
) -> Result<(), Error> {
    if !list.ended || list.segments.is_empty() {
        return Err(Error::Unavailable);
    }
    let manifest = playback::playlist(&list, name, false);
    if let Some(init) = list.initialization {
        if init.encryption.is_some() {
            return Err(Error::Unavailable);
        }
        resources.insert(
            format!("{name}/init.mp4"),
            Resource::File(allowed(entries, &init.url)?.into(), "video/mp4"),
        );
    }
    for (index, segment) in list.segments.into_iter().enumerate() {
        if segment.encryption.is_some() {
            return Err(Error::Unavailable);
        }
        let (ext, mime) = if segment.kind == Kind::Fragment {
            ("m4s", "video/mp4")
        } else {
            ("ts", "video/mp2t")
        };
        resources.insert(
            format!("{name}/{index}.{ext}"),
            Resource::File(allowed(entries, &segment.url)?.into(), mime),
        );
    }
    resources.insert(format!("{name}.m3u8"), Resource::Manifest(manifest));
    Ok(())
}

fn media_type(bytes: &[u8; 12], extension: Option<&str>) -> Option<(&'static str, &'static str)> {
    match &bytes[..4] {
        b"fLaC" => return Some(("audio.flac", "audio/flac")),
        b"OggS" => return Some(("audio.ogg", "audio/ogg")),
        b"RIFF" if &bytes[8..12] == b"WAVE" => return Some(("audio.wav", "audio/wav")),
        _ => {}
    }
    if bytes[0] == 0xff && bytes[1] & 0xf6 == 0xf0 {
        return Some(("audio.aac", "audio/aac"));
    }
    if bytes.starts_with(b"ID3") || bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0 && bytes[1] & 6 != 0 {
        return Some(("audio.mp3", "audio/mpeg"));
    }
    if &bytes[4..8] == b"ftyp" && extension == Some("m4a") {
        return Some(("audio.m4a", "audio/mp4"));
    }
    if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some(("video.webm", "video/webm"));
    }
    if bytes[0] == 0x47 {
        return Some(("video.ts", "video/mp2t"));
    }
    match &bytes[4..8] {
        b"ftyp" | b"styp" | b"moof" | b"moov" | b"mdat" | b"sidx" | b"free" | b"wide" | b"emsg"
        | b"prft" => Some(("video.mp4", "video/mp4")),
        _ => None,
    }
}

impl PlaybackRoute {
    /// 播放已由书库核对归属的本地文件。交给播放器的所有清单均从解析结果重新生成。
    pub async fn offline(
        path: PathBuf,
        related: Vec<String>,
        stop: CancellationToken,
    ) -> Result<Self, Error> {
        let mut resources = HashMap::new();
        let entry = if path.extension().is_some_and(|e| e == "m3u8") {
            let dir = path.parent().ok_or(Error::Storage)?;
            let base = Url::parse("https://offline.park.invalid/").unwrap();
            let to_url = |file: &Path| {
                let relative = file
                    .strip_prefix(dir)
                    .ok()
                    .and_then(Path::to_str)
                    .ok_or(Error::Storage)?;
                base.join(&relative.replace('\\', "/"))
                    .map_err(|_| Error::Storage)
            };
            let root = to_url(&path)?;
            let mut entries = Allowed::new();
            for file in related
                .into_iter()
                .map(PathBuf::from)
                .chain(std::iter::once(path.clone()))
            {
                entries.insert(to_url(&file)?, file);
            }
            match parse_file(&entries, &root)? {
                Playlist::Media(list) => {
                    track(&mut resources, &entries, "video", list)?;
                    "video.m3u8"
                }
                Playlist::Master(variants) if variants.len() == 1 => {
                    let v = &variants[0];
                    for (name, url) in [
                        ("audio", v.audio.as_ref().ok_or(Error::Unavailable)?),
                        ("video", &v.url),
                    ] {
                        let Playlist::Media(list) = parse_file(&entries, url)? else {
                            return Err(Error::Unavailable);
                        };
                        track(&mut resources, &entries, name, list)?;
                    }
                    resources.insert(
                        "master.m3u8".into(),
                        Resource::Manifest(playback::MASTER.into()),
                    );
                    "master.m3u8"
                }
                _ => return Err(Error::Unavailable),
            }
        } else {
            let mut magic = [0u8; 12];
            std::fs::File::open(&path)
                .and_then(|mut f| f.read_exact(&mut magic))
                .map_err(|_| Error::Storage)?;
            let (name, mime) = media_type(&magic, path.extension().and_then(|s| s.to_str())).ok_or(Error::Unavailable)?;
            resources.insert(name.into(), Resource::File(path, mime));
            name
        };
        let listener = Listener::bind().await?;
        let state = Arc::new(Files {
            resources,
            authority: listener.authority.clone(),
            prefix: listener.prefix.clone(),
            stop: stop.clone(),
        });
        Ok(listener.serve(entry, Router::new().fallback(serve).with_state(state), stop))
    }
}

async fn serve(
    State(state): State<Arc<Files>>,
    request: Request,
) -> Result<Response<Body>, StatusCode> {
    if state.stop.is_cancelled()
        || request.uri().query().is_some()
        || request
            .headers()
            .get(header::HOST)
            .and_then(|s| s.to_str().ok())
            != Some(&state.authority)
    {
        return Err(StatusCode::NOT_FOUND);
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }
    let key = request
        .uri()
        .path()
        .strip_prefix(&state.prefix)
        .ok_or(StatusCode::NOT_FOUND)?;
    let resource = state.resources.get(key).ok_or(StatusCode::NOT_FOUND)?;
    let head = *request.method() == Method::HEAD;
    let response = Response::builder()
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    let (path, mime) = match resource {
        Resource::Manifest(text) => {
            return response
                .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
                .header(header::CONTENT_LENGTH, text.len())
                .body(if head {
                    Body::empty()
                } else {
                    Body::from(text.clone())
                })
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        }
        Resource::File(path, mime) => (path, *mime),
    };
    if tokio::fs::canonicalize(path).await.ok().as_ref() != Some(path) {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let meta = file.metadata().await.map_err(|_| StatusCode::NOT_FOUND)?;
    let mut magic = [0u8; 12];
    file.read_exact(&mut magic)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if !meta.is_file() || media_type(&magic, path.extension().and_then(|s| s.to_str())).map(|(_, ty)| ty) != Some(mime) {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let len = meta.len();
    let raw = request
        .headers()
        .get(header::RANGE)
        .map(|h| h.to_str())
        .transpose()
        .map_err(|_| StatusCode::RANGE_NOT_SATISFIABLE)?;
    let Some((start, end)) = range(raw, len) else {
        return response
            .status(416)
            .header(header::CONTENT_RANGE, format!("bytes */{len}"))
            .body(Body::empty())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
    };
    let mut response = response
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, end - start);
    if raw.is_some() {
        response = response.status(206).header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{len}", end - 1),
        );
    }
    let body = if head {
        Body::empty()
    } else {
        file.seek(SeekFrom::Start(start))
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
        let reader = file.take(end - start);
        let stream = futures_util::stream::try_unfold(
            (reader, state.stop.clone()),
            |(mut reader, stop)| async move {
                if reader.limit() == 0 {
                    return Ok::<_, std::io::Error>(None);
                }
                let mut bytes = vec![0u8; reader.limit().min(64 * 1024) as usize];
                let n = tokio::select! {
                    _=stop.cancelled()=>return Ok(None),
                    res=reader.read(&mut bytes)=>res?,
                };
                if n == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                bytes.truncate(n);
                Ok(Some((Bytes::from(bytes), (reader, stop))))
            },
        );
        Body::from_stream(stream)
    };
    response
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
