use crate::{
    hls::{self, HlsClient, Kind},
    Error, VideoSource,
};
use http::{header, Method};
use park_net::{Request, Session, Transport};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;
type Result<T> = std::result::Result<T, Error>;
fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
pub fn selection(sources: &[VideoSource], index: usize, episode: &str) -> Result<Value> {
    let chosen = sources.get(index).ok_or(Error::Unavailable)?;
    extension(chosen, false)?;
    let matches = sources.iter().enumerate().filter(|(_, s)| {
        s.quality == chosen.quality && s.selection_format == chosen.selection_format && s.label == chosen.label
    }).collect::<Vec<_>>();
    let origin = chosen.url.origin();
    let ordinal = matches.iter().position(|(i, _)| *i == index).ok_or(Error::Unavailable)?;
    Ok(json!({
        "quality": chosen.quality, "format": chosen.selection_format, "label": chosen.label,
        "address": hash(&format!("{}{}", origin.ascii_serialization(), chosen.url.path())),
        "origin": hash(&origin.ascii_serialization()), "episode": episode,
        "ordinal": ordinal, "count": matches.len(),
        "originCount": matches.iter().filter(|(_, s)| s.url.origin() == origin).count()
    }))
}
pub fn select<'a>(
    sources: &'a [VideoSource],
    selection: &Value,
    quality: u32,
    episode: &str,
) -> Result<&'a VideoSource> {
    if selection.is_null() {
        return sources
            .iter()
            .find(|s| quality != 0 && s.quality == quality)
            .or_else(|| sources.iter().find(|s| s.preferred))
            .or(sources.first())
            .ok_or(Error::Unavailable);
    }
    let matches: Vec<_> = sources
        .iter()
        .filter(|s| {
            selection["quality"] == s.quality
                && selection["format"] == s.selection_format
                && selection["label"] == s.label
        })
        .collect();
    let unique = |v: Vec<&'a VideoSource>| if v.len() == 1 { Some(v[0]) } else { None };
    if let Some(s) = unique(
        matches
            .iter()
            .copied()
            .filter(|s| {
                selection["address"]
                    == hash(&format!(
                        "{}{}",
                        s.url.origin().ascii_serialization(),
                        s.url.path()
                    ))
            })
            .collect(),
    ) {
        return Ok(s);
    }
    if selection["originCount"] == 1 {
        if let Some(s) = unique(
            matches
                .iter()
                .copied()
                .filter(|s| selection["origin"] == hash(&s.url.origin().ascii_serialization()))
                .collect(),
        ) {
            return Ok(s);
        }
    }
    if selection["count"].as_u64() == Some(matches.len() as u64) {
        if matches.len() == 1 {
            return Ok(matches[0]);
        }
        if selection["episode"] != episode {
            return selection["ordinal"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| matches.get(i).copied())
                .ok_or(Error::Unavailable);
        }
    }
    Err(Error::Unavailable)
}
fn io<T>(r: std::io::Result<T>) -> Result<T> {
    r.map_err(|_| Error::Storage)
}
fn net(e: park_net::Error) -> Error {
    if e == park_net::Error::Cancelled {
        Error::Cancelled
    } else {
        Error::Unavailable
    }
}
pub fn extension(s: &VideoSource, playlist: bool) -> Result<&str> {
    if s.hls {
        return Ok(if playlist || s.audio.is_some() {
            "m3u8"
        } else {
            "mp4"
        });
    }
    match s.format.as_str() {
        "mp4" | "webm" | "mkv" | "mov" | "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg"
        | "opus" => Ok(s.format.as_str()),
        _ => Err(Error::Unavailable),
    }
}
fn file(dir: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        || name.starts_with('.')
    {
        return Err(Error::Unavailable);
    }
    let p = dir.join(name);
    match fs::symlink_metadata(&p) {
        Ok(m) if m.is_file() => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        _ => return Err(Error::Storage),
    }
    Ok(p)
}
pub(crate) fn atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut t = io(tempfile::NamedTempFile::new_in(dir))?;
    io(t.write_all(bytes))?;
    io(t.as_file().sync_all())?;
    t.persist(file(dir, name)?).map_err(|_| Error::Storage)?;
    Ok(())
}
fn content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (from, to) = range.split_once('-')?;
    Some((
        from.parse().ok()?,
        to.parse::<u64>().ok()?.checked_add(1)?,
        total.parse().ok()?,
    ))
}
pub async fn attachment(
    transport: &Transport,
    session: &Session,
    line: &VideoSource,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let req = Request {
        url: line.url.clone(),
        method: Method::GET,
        headers: line.headers.clone(),
        body: None,
        connect_address: None,
        follow_redirects: true,
    };
    let mut response = transport
        .open_media(
            session,
            &line.domains,
            req,
            Duration::from_secs(60),
            cancel,
            line.session_scope,
        )
        .await
        .map_err(net)?;
    if !response.status.is_success()
        || response
            .headers
            .get(header::CONTENT_ENCODING)
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
    Ok(bytes)
}
/// 游戏渠道下载是全新的独立文件，只接受文件响应，不把错误页面作为成品交付。
pub type GameProgress<'a> = &'a (dyn Fn(u64, Option<u64>) -> Result<()> + Sync);

pub async fn game_file(
    transport: &Transport,
    session: &Session,
    line: &VideoSource,
    dir: &Path,
    cancel: &CancellationToken,
    progress: Option<GameProgress<'_>>,
) -> Result<PathBuf> {
    direct_file(transport, session, line, dir, "file", cancel, true, None, progress).await
}

async fn direct_file(
    transport: &Transport,
    session: &Session,
    line: &VideoSource,
    dir: &Path,
    name: &str,
    cancel: &CancellationToken,
    game: bool,
    selection: Option<&Value>,
    progress: Option<GameProgress<'_>>,
) -> Result<PathBuf> {
    let path = file(dir, &format!("{name}.part"))?;
    let journal = format!("{name}.part.json");
    let info = file(dir, &journal)?;
    // 同一路径的不同线路也可能共用 ETag 或 Last-Modified。只在同一逻辑选择内续传。
    let address = hash(&json!([line.url.origin().ascii_serialization(), line.url.path(), selection]).to_string());
    let old: Value = fs::read(&info)
        .ok()
        .filter(|b| b.len() < 4096)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let validator = old["validator"]
        .as_str()
        .filter(|s| !s.is_empty() && old["address"] == address);
    let previous = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut headers = line.headers.clone();
    let mut offset = 0;
    if let Some(tag) = validator.filter(|_| previous > 0 && Some(previous) < old["length"].as_u64())
    {
        headers.insert(
            header::RANGE,
            format!("bytes={previous}-")
                .parse()
                .map_err(|_| Error::Unavailable)?,
        );
        headers.insert(
            header::IF_RANGE,
            tag.parse().map_err(|_| Error::Unavailable)?,
        );
        offset = previous;
    }
    let request = Request {
        url: line.url.clone(),
        method: Method::GET,
        headers,
        body: None,
        connect_address: None,
        follow_redirects: true,
    };
    let mut response = transport
        .open_media(
            session,
            &line.domains,
            request,
            Duration::from_secs(3600),
            cancel,
            line.session_scope,
        )
        .await
        .map_err(net)?;
    if response
        .headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| v != "identity")
    {
        return Err(Error::Unavailable);
    }
    if game {
        let mime = response.headers.get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
        if response.status.as_u16() != 200 || mime.starts_with("text/") || mime.contains("json") || mime.contains("html") {
            return Err(Error::Unavailable);
        }
    }
    let current = response
        .headers
        .get(header::ETAG)
        .and_then(|s| s.to_str().ok())
        .filter(|s| !s.starts_with("W/"))
        .or_else(|| {
            response
                .headers
                .get(header::LAST_MODIFIED)
                .and_then(|s| s.to_str().ok())
        })
        .unwrap_or("")
        .to_owned();
    let length = match response.status.as_u16() {
        200 => {
            offset = 0;
            response
                .headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
        }
        206 => {
            let (from, end, length) = response
                .headers
                .get(header::CONTENT_RANGE)
                .and_then(|s| content_range(s.to_str().ok()?))
                .ok_or(Error::Unavailable)?;
            if offset != from
                || end != length
                || end <= from
                || offset > 0
                    && (Some(current.as_str()) != validator
                        || old["length"].as_u64() != Some(length))
            {
                return Err(Error::Unavailable);
            }
            Some(length)
        }
        _ => return Err(Error::Unavailable),
    };
    let mut output = io(fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .append(offset != 0)
        .open(&path))?;
    atomic(
        dir,
        &journal,
        json!({"validator":current,"address":address,"length":length})
            .to_string()
            .as_bytes(),
    )?;
    let mut last_report = None;
    while let Some(data) = response.next_chunk().await.map_err(net)? {
        offset = offset.checked_add(data.len() as u64).ok_or(Error::Limit)?;
        if length.is_some_and(|n| offset > n) {
            return Err(Error::Unavailable);
        }
        io(output.write_all(&data))?;
        if let Some(report)=progress {
            if last_report.is_none_or(|at:std::time::Instant|at.elapsed()>=Duration::from_millis(500)){
                report(offset,length)?;last_report=Some(std::time::Instant::now());
            }
        }
    }
    if offset == 0 || length.is_some_and(|n| offset != n) {
        return Err(Error::Unavailable);
    }
    io(output.sync_all())?;
    drop(output);
    let mut prefix = Vec::new();
    io(io(fs::File::open(&path))?.take(64).read_to_end(&mut prefix))?;
    let text = String::from_utf8_lossy(&prefix);
    let text = text.trim_start();
    if text.starts_with('<') || text.starts_with('{') || text.starts_with("#EXTM3U") {
        return Err(Error::Unavailable);
    }
    Ok(path)
}

fn subdirectory(dir: &Path, name: &str) -> Result<PathBuf> {
    let path = dir.join(name);
    if !io(path.try_exists())? {
        io(fs::create_dir(&path))?;
    }
    if !io(fs::symlink_metadata(&path))?.is_dir() || io(path.canonicalize())? != path {
        return Err(Error::Storage);
    }
    Ok(path)
}
fn signature(url: &url::Url, segments: &[&hls::Segment], selection: Option<&Value>) -> String {
    let mut values = vec![json!(url.as_str()), json!(selection)];
    for segment in segments {
        let mut item = vec![json!(segment.url.as_str()), json!(segment.seconds)];
        match segment.kind {
            Kind::Init => item.push(json!("initialization")),
            Kind::Fragment => item.push(json!("fragment")),
            _ => (),
        }
        if let Some(key) = &segment.encryption {
            item.extend([
                json!(key.url.as_str()),
                json!(key.iv),
                json!(segment.sequence.to_string()),
            ]);
        }
        values.push(Value::Array(item));
    }
    hash(&Value::Array(values).to_string())
}
async fn track(
    client: &HlsClient,
    url: &url::Url,
    dir: &Path,
    name: &str,
    keep: bool,
    selection: Option<&Value>,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let list = client.media(url, false, cancel).await?;
    if !list.ended || list.segments.is_empty() {
        return Err(Error::Unavailable);
    }
    let base = format!("{name}.part.hls");
    let pieces = subdirectory(dir, &base)?;
    let segments: Vec<_> = list.initialization.iter().chain(&list.segments).collect();
    let signature = signature(url, &segments, selection);
    let receipt = file(&pieces, "state.json")?;
    let previous: Value = fs::File::open(&receipt)
        .ok()
        .and_then(|f| serde_json::from_reader(f.take(16 * 1024 * 1024)).ok())
        .unwrap_or(Value::Null);
    let mut hashes = if previous["signature"] == signature {
        previous["segments"]
            .as_object()
            .cloned()
            .unwrap_or_default()
    } else {
        Default::default()
    };
    let mut local = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n",
        list.segments
            .iter()
            .map(|s| s.seconds.ceil() as u64)
            .max()
            .unwrap_or(1)
            .max(1)
    );
    let mut movie = if keep {
        None
    } else {
        Some(io(tempfile::NamedTempFile::new_in(dir))?)
    };
    let mut bootstrap = None;
    let mut elapsed = 0.;
    let mut epoch = 0.;
    let mut initialization = None;
    for (i, seg) in segments.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let ext = if seg.kind == Kind::TransportStream {
            "ts"
        } else {
            "mp4"
        };
        let name = format!("{i}.{ext}");
        let path = file(&pieces, &name)?;
        let existing = if let Some(expected) = hashes.get(&i.to_string()).and_then(Value::as_str) {
            fs::metadata(&path)
                .ok()
                .filter(|m| m.len() > 0 && m.len() <= hls::SEGMENT_LIMIT as u64)
                .and_then(|_| fs::read(&path).ok())
                .filter(|b| format!("{:x}", Sha256::digest(b)) == expected)
        } else {
            None
        };
        let bytes = match existing {
            Some(bytes) => bytes,
            None => {
                let bytes = client.segment(url, seg, false, cancel).await?;
                atomic(&pieces, &name, &bytes)?;
                hashes.insert(
                    i.to_string(),
                    json!(format!("{:x}", Sha256::digest(&bytes))),
                );
                atomic(
                    &pieces,
                    "state.json",
                    json!({"signature":signature,"segments":hashes})
                        .to_string()
                        .as_bytes(),
                )?;
                bytes
            }
        };
        if seg.discontinuity {
            local.push_str("#EXT-X-DISCONTINUITY\n");
        }
        if seg.kind == Kind::Init {
            local.push_str(&format!("#EXT-X-MAP:URI=\"{base}/{name}\"\n"));
        } else {
            local.push_str(&format!("#EXTINF:{},\n{base}/{name}\n", seg.seconds));
        }
        if let Some(out) = movie.as_mut() {
            if seg.kind == Kind::TransportStream {
                if bootstrap.is_none() || seg.discontinuity {
                    bootstrap = Some(hls::remux::Bootstrap::new(bytes.clone())?);
                    epoch = elapsed;
                }
                let window = bootstrap
                    .as_ref()
                    .ok_or(Error::Unavailable)?
                    .window(i, &bytes, elapsed, epoch, cancel)?;
                if initialization
                    .as_ref()
                    .is_some_and(|init| init != &window.initialization)
                {
                    return Err(Error::Unavailable);
                }
                if initialization.is_none() {
                    io(out.write_all(&window.initialization))?;
                    initialization = Some(window.initialization);
                }
                io(out.write_all(&window.media))?;
            } else {
                io(out.write_all(&bytes))?;
            }
        }
        if seg.kind != Kind::Init {
            elapsed += seg.seconds;
        }
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let partial = format!("{name}.part");
    if let Some(out) = movie {
        io(out.as_file().sync_all())?;
        out.persist(file(dir, &partial)?)
            .map_err(|_| Error::Storage)?;
    } else {
        local.push_str("#EXT-X-ENDLIST\n");
        atomic(dir, &partial, local.as_bytes())?;
    }
    file(dir, &partial)
}
pub async fn save(
    transport: Arc<Transport>,
    session: Session,
    line: VideoSource,
    dir: &Path,
    name: &str,
    playlist: bool,
    selection: Option<&Value>,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    if !line.hls {
        return direct_file(&transport, &session, &line, dir, name, cancel, false, selection,None).await;
    }
    file(dir, name)?;
    let client = HlsClient::new(transport, session, line)?;
    if let Some(audio) = client.audio_url() {
        let folder = format!("{name}.part.tracks");
        let tracks = subdirectory(dir, &folder)?;
        for (name, url) in [("video.m3u8", client.video_url()), ("audio.m3u8", audio)] {
            let part = track(&client, url, &tracks, name, true, selection, cancel).await?;
            io(fs::rename(part, file(&tracks, name)?))?;
        }
        let master=format!("#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"audio\",DEFAULT=YES,AUTOSELECT=YES,URI=\"{folder}/audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,AUDIO=\"audio\"\n{folder}/video.m3u8\n");
        let output = format!("{name}.part");
        atomic(dir, &output, master.as_bytes())?;
        return file(dir, &output);
    }
    track(&client, client.video_url(), dir, name, playlist, selection, cancel).await
}
