use crate::{
    connect::{connect, Socket},
    Error, Purpose, Request, Session, Transport, WireResponse,
};
use futures_util::{SinkExt, StreamExt};
use http::{header, HeaderMap, HeaderName, HeaderValue, Method};
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};
use tokio::time::Instant;
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig, Message},
    WebSocketStream,
};
use tokio_util::sync::CancellationToken;
use url::Url;

fn endpoint(raw: &Value, domains: &Value, scheme: &str) -> Result<Url, Error> {
    let u = Url::parse(raw.as_str().ok_or(Error::Blocked)?).map_err(|_| Error::Blocked)?;
    if u.scheme() != scheme
        || !u.username().is_empty()
        || u.password().is_some()
        || u.fragment().is_some()
    {
        return Err(Error::Blocked);
    }
    let mut https = u.clone();
    https.set_scheme("https").map_err(|_| Error::Blocked)?;
    https.set_port(None).map_err(|_| Error::Blocked)?;
    if !crate::public_https(&https) {
        return Err(Error::Blocked);
    }
    let host = u.host_str().ok_or(Error::Blocked)?;
    let port = u.port_or_known_default().ok_or(Error::Blocked)?;
    if port != 443 && port < 1024 {
        return Err(Error::Blocked);
    }
    let allowed = domains
        .as_array()
        .ok_or(Error::Blocked)?
        .iter()
        .filter_map(Value::as_str)
        .any(|rule| {
            let (name, p) = rule
                .rsplit_once(':')
                .map_or((rule, Some(443)), |(h, v)| (h, v.parse::<u16>().ok()));
            if p != Some(port) {
                return false;
            }
            if let Some(suffix) = name.strip_prefix("*.") {
                host.strip_suffix(&suffix.to_ascii_lowercase())
                    .and_then(|s| s.strip_suffix('.'))
                    .is_some_and(|s| !s.is_empty() && !s.contains('.'))
            } else {
                host.eq_ignore_ascii_case(name)
            }
        });
    if !allowed {
        return Err(Error::Blocked);
    }
    Ok(u)
}
fn headers(raw: &Value) -> Result<HeaderMap, Error> {
    let mut out = HeaderMap::new();
    if raw.is_null() {
        return Ok(out);
    }
    for (k, v) in raw.as_object().ok_or(Error::Blocked)? {
        let key = HeaderName::from_bytes(k.as_bytes()).map_err(|_| Error::Blocked)?;
        if !matches!(
            key.as_str(),
            "accept" | "content-type" | "origin" | "referer"
        ) {
            return Err(Error::Blocked);
        }
        let text = v
            .as_str()
            .filter(|v| v.len() <= 2048)
            .ok_or(Error::Blocked)?;
        out.insert(
            key,
            HeaderValue::from_str(text).map_err(|_| Error::Blocked)?,
        );
    }
    Ok(out)
}
impl Transport {
    pub async fn chat_bootstrap(
        &self,
        session: &Session,
        domains: &Value,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value, Error> {
        if session.purpose() != Purpose::Source {
            return Err(Error::Blocked);
        }
        let mut url = endpoint(&args["url"], domains, "https")?;
        let mut method = match args["method"]
            .as_str()
            .unwrap_or("GET")
            .to_ascii_uppercase()
            .as_str()
        {
            "GET" => Method::GET,
            "POST" => Method::POST,
            _ => return Err(Error::Blocked),
        };
        let mut body = args["body"]
            .as_str()
            .map(|s| bytes::Bytes::copy_from_slice(s.as_bytes()));
        if body.as_ref().is_some_and(|b| b.len() > 262144) {
            return Err(Error::Limit);
        }
        let mut headers = headers(&args["headers"])?;
        let deadline = Instant::now() + Duration::from_secs(20);
        for _ in 0..8 {
            let grant = [url.host_str().ok_or(Error::Blocked)?.into()];
            let res = self
                .execute(
                    session,
                    &grant,
                    Request {
                        url: url.clone(),
                        method: method.clone(),
                        headers: headers.clone(),
                        body: body.clone(),
                        connect_address: None,
                        follow_redirects: false,
                    },
                    262144,
                    deadline.saturating_duration_since(Instant::now()),
                    cancel,
                )
                .await?;
            if matches!(res.status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                let next = url
                    .join(
                        res.headers
                            .get(header::LOCATION)
                            .and_then(|v| v.to_str().ok())
                            .ok_or(Error::Network)?,
                    )
                    .map_err(|_| Error::Network)?;
                if next.origin() != url.origin() {
                    return Err(Error::Blocked);
                }
                crate::policy::redirect(
                    &url,
                    &next,
                    res.status,
                    &mut method,
                    &mut headers,
                    &mut body,
                )?;
                url = endpoint(&json!(next.as_str()), domains, "https")?;
            } else {
                return Ok(
                    json!({"status":res.status.as_u16(),"body":std::str::from_utf8(&res.body).map_err(|_|Error::Network)?}),
                );
            }
        }
        Err(Error::Limit)
    }
}
enum Stream {
    Sse {
        wire: WireResponse,
        buffer: Vec<u8>,
        pending: VecDeque<Value>,
        event: String,
        data: Vec<String>,
        bytes: usize,
    },
    Ws {
        wire: WebSocketStream<Socket>,
        binary: bool,
    },
}
pub struct Chat {
    stream: Stream,
    epoch: CancellationToken,
}
impl Chat {
    pub async fn open(
        net: &Transport,
        session: &Session,
        domains: &Value,
        desc: &Value,
        cancel: &CancellationToken,
    ) -> Result<Self, Error> {
        if session.purpose() != Purpose::Source {
            return Err(Error::Blocked);
        }
        let epoch = session.token()?;
        let make = async {
            let ws = match desc["transport"].as_str() {
                Some("websocket") => true,
                Some("sse") => false,
                _ => return Err(Error::Blocked),
            };
            let url = endpoint(&desc["url"], domains, if ws { "wss" } else { "https" })?;
            let mut head = headers(&desc["headers"])?;
            let stream = if ws {
                let mut https = url.clone();
                https.set_scheme("https").map_err(|_| Error::Blocked)?;
                let cookie = session.header(&https, &epoch)?;
                let socket = connect(&https, None, net.proxy.as_ref(), net.tls.clone()).await?;
                let mut req = url
                    .as_str()
                    .into_client_request()
                    .map_err(|_| Error::Blocked)?;
                req.headers_mut().extend(head);
                if !cookie.is_empty() {
                    req.headers_mut().insert(
                        header::COOKIE,
                        HeaderValue::from_str(&cookie).map_err(|_| Error::Blocked)?,
                    );
                }
                if !desc["protocols"].is_null() {
                    let a = desc["protocols"]
                        .as_array()
                        .filter(|v| v.len() <= 8)
                        .ok_or(Error::Blocked)?;
                    let mut seen = HashSet::new();
                    let mut list = Vec::new();
                    for v in a {
                        let p = v
                            .as_str()
                            .filter(|s| {
                                !s.is_empty()
                                    && s.len() <= 128
                                    && s.bytes().all(|b| {
                                        b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
                                    })
                            })
                            .ok_or(Error::Blocked)?;
                        if !seen.insert(p) {
                            return Err(Error::Blocked);
                        }
                        list.push(p)
                    }
                    if !list.is_empty() {
                        req.headers_mut().insert(
                            header::SEC_WEBSOCKET_PROTOCOL,
                            HeaderValue::from_str(&list.join(", ")).map_err(|_| Error::Blocked)?,
                        );
                    }
                }
                let config = WebSocketConfig::default()
                    .max_frame_size(Some(65536))
                    .max_message_size(Some(65536));
                let (wire, _) =
                    tokio_tungstenite::client_async_with_config(req, socket, Some(config))
                        .await
                        .map_err(|_| Error::Network)?;
                Stream::Ws {
                    wire,
                    binary: desc["binary"].as_bool().unwrap_or(false),
                }
            } else {
                head.insert(
                    header::ACCEPT,
                    HeaderValue::from_static("text/event-stream"),
                );
                let domains = [url.host_str().ok_or(Error::Blocked)?.to_owned()];
                let wire = net
                    .open(
                        session,
                        &domains,
                        Request {
                            url,
                            method: Method::GET,
                            headers: head,
                            body: None,
                            connect_address: None,
                            follow_redirects: false,
                        },
                        Duration::from_secs(20),
                        cancel,
                    )
                    .await?;
                if !wire.status.is_success()
                    || !wire
                        .headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| {
                            v.split(';')
                                .next()
                                .unwrap_or("")
                                .trim()
                                .eq_ignore_ascii_case("text/event-stream")
                        })
                {
                    return Err(Error::Network);
                }
                Stream::Sse {
                    wire,
                    buffer: vec![],
                    pending: VecDeque::new(),
                    event: String::new(),
                    data: vec![],
                    bytes: 0,
                }
            };
            Ok(stream)
        };
        let stream = tokio::select! {biased;_=cancel.cancelled()=>return Err(Error::Cancelled),_=epoch.cancelled()=>return Err(Error::Cancelled),v=tokio::time::timeout(Duration::from_secs(20),make)=>v.map_err(|_|Error::Timeout)??};
        Ok(Self { stream, epoch })
    }
    pub async fn next(&mut self, cancel: &CancellationToken) -> Result<Value, Error> {
        let read = async {
            match &mut self.stream {
                Stream::Ws { wire, binary } => loop {
                    match wire
                        .next()
                        .await
                        .ok_or(Error::Network)?
                        .map_err(|_| Error::Network)?
                    {
                        Message::Text(t) if !*binary => {
                            return Ok(json!({"type":"message","data":t.as_str()}))
                        }
                        Message::Binary(b) if *binary => {
                            return Ok(
                                json!({"type":"message","data":std::str::from_utf8(&b).map_err(|_|Error::Network)?}),
                            )
                        }
                        Message::Ping(_) => {
                            wire.flush().await.map_err(|_| Error::Network)?;
                            return Ok(json!({"type":"heartbeat"}));
                        }
                        Message::Pong(_) => return Ok(json!({"type":"pong"})),
                        _ => return Err(Error::Network),
                    }
                },
                Stream::Sse {
                    wire,
                    buffer,
                    pending,
                    event,
                    data,
                    bytes,
                } => loop {
                    if let Some(v) = pending.pop_front() {
                        return Ok(v);
                    }
                    wire.idle_timeout(Duration::from_secs(90));
                    let chunk = wire.next_chunk().await?.ok_or(Error::Network)?;
                    buffer.extend_from_slice(&chunk);
                    let mut offset = 0;
                    while let Some(i) = buffer[offset..]
                        .iter()
                        .position(|b| matches!(b, b'\n' | b'\r'))
                    {
                        let end = offset + i;
                        let cr = buffer[end] == b'\r';
                        if cr && end + 1 == buffer.len() {
                            break;
                        }
                        *bytes += i + 1;
                        if *bytes > 65536 || pending.len() >= 32 {
                            return Err(Error::Limit);
                        }
                        let line = std::str::from_utf8(&buffer[offset..end])
                            .map_err(|_| Error::Network)?;
                        offset = end + 1 + usize::from(cr && buffer.get(end + 1) == Some(&b'\n'));
                        if line.is_empty() {
                            if !data.is_empty() {
                                pending.push_back(json!({"type":"message","event":if event.is_empty(){"message"}else{event.as_str()},"data":data.join("\n")}));
                            }
                            *bytes = 0;
                            data.clear();
                            event.clear()
                        } else if let Some(comment) = line.strip_prefix(':') {
                            let _ = comment;
                            pending.push_back(json!({"type":"heartbeat"}))
                        } else {
                            let (k, v) = line.split_once(':').unwrap_or((line, ""));
                            let v = v.strip_prefix(' ').unwrap_or(v);
                            match k {
                                "event" => *event = v.into(),
                                "data" => data.push(v.into()),
                                _ => (),
                            }
                        }
                    }
                    buffer.drain(..offset);
                    if *bytes + buffer.len() > 65536 {
                        return Err(Error::Limit);
                    }
                },
            }
        };
        tokio::select! {biased;_=cancel.cancelled()=>Err(Error::Cancelled),_=self.epoch.cancelled()=>Err(Error::Cancelled),res=read=>res}
    }
    pub async fn send(
        &mut self,
        text: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let Stream::Ws { wire, binary } = &mut self.stream else {
            return Err(Error::Blocked);
        };
        let frame = match text {
            None => Message::Ping(Default::default()),
            Some(s) if s.len() <= 65536 => {
                if *binary {
                    Message::binary(s.as_bytes().to_vec())
                } else {
                    Message::text(s.to_owned())
                }
            }
            _ => return Err(Error::Limit),
        };
        tokio::select! {biased;_=cancel.cancelled()=>Err(Error::Cancelled),_=self.epoch.cancelled()=>Err(Error::Cancelled),v=tokio::time::timeout(Duration::from_secs(20),wire.send(frame))=>v.map_err(|_|Error::Timeout)?.map_err(|_|Error::Network)}
    }
}
