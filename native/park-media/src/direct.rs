use crate::loopback::{Listener, PlaybackRoute};
use crate::Error;
use axum::{
    body::{Body, Bytes},
    extract::{Request as Incoming, State},
    http::{header, HeaderMap, Method, Response, StatusCode},
    Router,
};
use park_net::{Request, Session, Transport, WireResponse};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use url::Url;

struct Sparse {
    file: File,
    spans: BTreeMap<u64, u64>,
    bytes: u64,
}
impl Sparse {
    fn cached_end(&self, offset: u64) -> u64 {
        self.spans
            .range(..=offset)
            .next_back()
            .map_or(offset, |(_, end)| offset.max(*end))
    }
    fn gap_end(&self, start: u64, end: u64) -> u64 {
        self.spans
            .range(start..)
            .next()
            .map_or(end, |(s, _)| end.min(*s))
    }
    fn read(&mut self, start: u64, end: u64) -> Result<Bytes, Error> {
        let mut bytes = vec![0; (end - start).min(128 * 1024) as usize];
        self.file
            .seek(SeekFrom::Start(start))
            .and_then(|_| self.file.read_exact(&mut bytes))
            .map_err(|_| Error::Storage)?;
        Ok(bytes.into())
    }
    fn write(&mut self, start: u64, data: &[u8]) {
        if self.bytes.saturating_add(data.len() as u64) > 2 * 1024 * 1024 * 1024 {
            return;
        }
        if self
            .file
            .seek(SeekFrom::Start(start))
            .and_then(|_| self.file.write_all(data))
            .is_err()
        {
            return;
        }
        let mut lo = start;
        let mut hi = start + data.len() as u64;
        if let Some((&a, &b)) = self.spans.range(..=lo).next_back() {
            if b >= lo {
                lo = a;
                hi = hi.max(b);
            }
        }
        let joins: Vec<_> = self.spans.range(lo..=hi).map(|(&a, &b)| (a, b)).collect();
        for (a, b) in joins {
            self.spans.remove(&a);
            self.bytes -= b - a;
            hi = hi.max(b);
        }
        self.spans.insert(lo, hi);
        self.bytes += hi - lo;
    }
}
pub(crate) fn range(input: Option<&str>, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let Some(input) = input else {
        return Some((0, len));
    };
    let (from, to) = input.strip_prefix("bytes=")?.split_once('-')?;
    if from.is_empty() {
        let n = to.parse::<u64>().ok()?;
        return (n > 0).then_some((len.saturating_sub(n), len));
    }
    let from = from.parse::<u64>().ok()?;
    let end = if to.is_empty() {
        len
    } else {
        to.parse::<u64>().ok()?.saturating_add(1).min(len)
    };
    (from < end).then_some((from, end))
}
fn content_range(headers: &HeaderMap) -> Option<(u64, u64, u64)> {
    let (bounds, size) = headers
        .get(header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .strip_prefix("bytes ")?
        .split_once('/')?;
    let (a, b) = bounds.split_once('-')?;
    let (a, b, n) = (
        a.parse::<u64>().ok()?,
        b.parse::<u64>().ok()?,
        size.parse::<u64>().ok()?,
    );
    (a <= b && b < n).then_some((a, b + 1, n))
}
struct Source {
    session_scope: bool,
    transport: Arc<Transport>,
    session: Session,
    domains: Vec<String>,
    url: Url,
    headers: HeaderMap,
}
impl Source {
    async fn get(
        &self,
        start: u64,
        end: u64,
        token: &CancellationToken,
    ) -> Result<WireResponse, Error> {
        let mut headers = self.headers.clone();
        headers.insert(
            header::RANGE,
            format!("bytes={start}-{}", end - 1)
                .parse()
                .map_err(|_| Error::Unavailable)?,
        );
        let req = Request {
            url: self.url.clone(),
            method: Method::GET,
            headers,
            body: None,
            connect_address: None,
            follow_redirects: true,
        };
        let res = self
            .transport
            .open_media(
                &self.session,
                &self.domains,
                req,
                Duration::from_secs(300),
                token,
                self.session_scope,
            )
            .await
            .map_err(net_error)?;
        if res
            .headers
            .get(header::CONTENT_ENCODING)
            .is_some_and(|h| h != "identity")
        {
            return Err(Error::Unavailable);
        }
        Ok(res)
    }
}
fn net_error(e: park_net::Error) -> Error {
    if e == park_net::Error::Cancelled {
        Error::Cancelled
    } else {
        Error::Unavailable
    }
}
struct RouteState {
    source: Source,
    cache: Mutex<Option<Sparse>>,
    length: u64,
    mime: header::HeaderValue,
    validator: Option<(header::HeaderName, header::HeaderValue)>,
    stop: CancellationToken,
    authority: String,
    path: String,
}

impl RouteState {
    fn find(&self, pos: u64, end: u64) -> Result<(Option<Bytes>, u64), Error> {
        let mut cache = self.cache.lock().map_err(|_| Error::Storage)?;
        let Some(cache) = cache.as_mut() else {
            return Ok((None, end));
        };
        let until = cache.cached_end(pos).min(end);
        if until > pos {
            Ok((Some(cache.read(pos, until)?), until))
        } else {
            Ok((None, cache.gap_end(pos, end)))
        }
    }
    fn remember(&self, start: u64, data: &[u8]) -> Result<(), Error> {
        if let Some(cache) = self.cache.lock().map_err(|_| Error::Storage)?.as_mut() {
            cache.write(start, data);
        }
        Ok(())
    }
}
impl PlaybackRoute {
    pub async fn direct(
        transport: Arc<Transport>,
        session: Session,
        grant: crate::VideoSource,
        cache: &Path,
        stop: CancellationToken,
    ) -> Result<Self, Error> {
        let source = Source {
            transport,
            session,
            domains: grant.domains,
            url: grant.url,
            headers: grant.headers,
            session_scope: grant.session_scope,
        };
        let mut first = source.get(0, 1, &stop).await?;
        let mut sparse = tempfile::tempfile_in(cache).ok().map(|file| Sparse {
            file,
            spans: BTreeMap::new(),
            bytes: 0,
        });
        let length = match first.status {
            StatusCode::PARTIAL_CONTENT => {
                let (start, end, n) = content_range(&first.headers).ok_or(Error::Unavailable)?;
                if start != 0 || end != 1 {
                    return Err(Error::Unavailable);
                }
                let data = first
                    .next_chunk()
                    .await
                    .map_err(net_error)?
                    .ok_or(Error::Unavailable)?;
                if data.len() != 1 {
                    return Err(Error::Unavailable);
                }
                if let Some(c) = &mut sparse {
                    c.write(0, &data);
                }
                n
            }
            StatusCode::OK => match first
                .headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
            {
                Some(n) => n,
                None => {
                    let mut size = 0u64;
                    while let Some(chunk) = first.next_chunk().await.map_err(net_error)? {
                        if let Some(c) = &mut sparse {
                            c.write(size, &chunk);
                        }
                        size = size.checked_add(chunk.len() as u64).ok_or(Error::Limit)?;
                    }
                    size
                }
            },
            _ => return Err(Error::Unavailable),
        };
        if length == 0 {
            return Err(Error::Unavailable);
        }
        let mime = first
            .headers
            .get(header::CONTENT_TYPE)
            .cloned()
            .unwrap_or(header::HeaderValue::from_static("application/octet-stream"));
        let validator = [header::ETAG, header::LAST_MODIFIED]
            .into_iter()
            .find_map(|name| first.headers.get(&name).cloned().map(|v| (name, v)));
        drop(first);
        let listener = Listener::bind().await?;
        let path = format!("{}video.mp4", listener.prefix);
        let state = Arc::new(RouteState {
            source,
            cache: Mutex::new(sparse),
            length,
            mime,
            validator,
            stop: stop.clone(),
            authority: listener.authority.clone(),
            path,
        });
        let app = Router::new().fallback(serve).with_state(state);
        Ok(listener.serve("video.mp4", app, stop))
    }
}

async fn serve(State(state): State<Arc<RouteState>>, incoming: Incoming) -> Response<Body> {
    let fail = |status| {
        Response::builder()
            .status(status)
            .body(Body::empty())
            .unwrap()
    };
    if state.stop.is_cancelled()
        || incoming.uri().path() != state.path
        || incoming.uri().query().is_some()
        || incoming
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            != Some(&state.authority)
    {
        return fail(StatusCode::NOT_FOUND);
    }
    if !matches!(*incoming.method(), Method::GET | Method::HEAD) {
        return fail(StatusCode::METHOD_NOT_ALLOWED);
    }
    let raw = incoming.headers().get(header::RANGE);
    let bounds = match raw {
        Some(v) => v.to_str().ok().and_then(|v| range(Some(v), state.length)),
        None => range(None, state.length),
    };
    let Some((start, end)) = bounds else {
        return Response::builder()
            .status(416)
            .header(header::CONTENT_RANGE, format!("bytes */{}", state.length))
            .body(Body::empty())
            .unwrap();
    };
    let mut response = Response::builder()
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, &state.mime)
        .header(header::CONTENT_LENGTH, end - start);
    if raw.is_some() {
        response = response.status(206).header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{}", end - 1, state.length),
        );
    }
    if incoming.method() == Method::HEAD {
        return response.body(Body::empty()).unwrap();
    }
    let body = async_stream::try_stream! {
        let error=||std::io::Error::other("media_unavailable");
        let mut pos=start;
        while pos<end {
            if state.stop.is_cancelled(){Err(error())?;}
            let (hit,gap)=state.find(pos,end).map_err(|_|error())?;
            if let Some(data)=hit{pos+=data.len() as u64;yield data;continue}
            let mut wire=state.source.get(pos,gap,&state.stop).await.map_err(|_|error())?;
            if let Some((name,val))=&state.validator{if wire.headers.get(name)!=Some(val){Err(error())?;}}
            let (mut offset,wire_end)=match wire.status{
                StatusCode::PARTIAL_CONTENT=>{let(a,b,total)=content_range(&wire.headers).ok_or_else(error)?;if total!=state.length || a>pos || b<=pos{Err(error())?;}(a,b)},
                StatusCode::OK=>{
                    if let Some(length)=wire.headers.get(header::CONTENT_LENGTH){if length.to_str().ok().and_then(|v|v.parse::<u64>().ok())!=Some(state.length){Err(error())?;}}
                    (0,state.length)
                },
                _=>Err(error())?,
            };
            let until=gap.min(wire_end);
            while pos<until{
                let chunk=wire.next_chunk().await.map_err(|_|error())?.ok_or_else(error)?;
                let next=offset.checked_add(chunk.len() as u64).ok_or_else(error)?;if next>wire_end{Err(error())?;}
                let from=pos.max(offset);let to=until.min(next);
                if to>from{
                    let data=chunk.slice((from-offset) as usize..(to-offset) as usize);
                    state.remember(from,&data).map_err(|_|error())?;
                    pos=to;yield data;
                }
                offset=next;
            }
        }
    };
    use futures_util::TryStreamExt;
    response
        .body(Body::from_stream(body.map_err(|err: std::io::Error| err)))
        .unwrap()
}
