use crate::{
    connect::connect,
    policy::{authorize, public_address, redirect},
    Error, Session,
};
use bytes::Bytes;
use http::{header, HeaderMap, Method, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use rustls::{ClientConfig, RootCertStore};
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use url::{Position, Url};

pub fn public_https(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && matches!(url.host(),Some(url::Host::Domain(domain)) if domain.contains('.') && !domain.ends_with(".local") && !domain.ends_with(".localhost"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostScope {
    Declared,
    PluginHttp,
    PublicResource,
}

pub struct Transport {
    pub(crate) tls: Arc<ClientConfig>,
    pub(crate) proxy: Option<Url>,
}
pub struct Request {
    pub url: Url,
    pub method: Method,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
    pub connect_address: Option<IpAddr>,
    pub follow_redirects: bool,
}
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub url: Url,
}

// 只要请求结束或 future 被取消，负责套接字的后台任务就一起退出。
pub(crate) struct ConnectionTask(pub(crate) tokio::task::JoinHandle<()>);
impl Drop for ConnectionTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 保持连接直到读取完成、调用取消或接收方释放句柄。数据面直接消费原始字节。
pub struct WireResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub url: Url,
    body: hyper::body::Incoming,
    _connection: ConnectionTask,
    epoch: CancellationToken,
    cancel: CancellationToken,
    deadline: tokio::time::Instant,
}
impl WireResponse {
    pub(crate) fn idle_timeout(&mut self,timeout:Duration){self.deadline=tokio::time::Instant::now()+timeout}
    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, Error> {
        loop {
            let frame = tokio::select! {biased;_=self.cancel.cancelled()=>return Err(Error::Cancelled),_=self.epoch.cancelled()=>return Err(Error::Cancelled),
            result=tokio::time::timeout_at(self.deadline,self.body.frame())=>result.map_err(|_|Error::Timeout)?,};
            let Some(frame) = frame else { return Ok(None) };
            if let Ok(bytes) = frame.map_err(|_| Error::Network)?.into_data() {
                return Ok(Some(bytes));
            }
        }
    }
}

impl Transport {
    pub fn new(proxy: Option<Url>, roots: RootCertStore) -> Result<Self, Error> {
        let tls = Self::tls_builder()?.with_root_certificates(roots).with_no_client_auth();
        Self::configured(proxy, tls)
    }

    pub fn with_system_trust(proxy: Option<Url>, certs: Vec<Vec<u8>>) -> Result<Self, Error> {
        #[cfg(any(target_vendor = "apple", target_os = "linux"))]
        if certs.is_empty() {
            use rustls_platform_verifier::BuilderVerifierExt;
            let tls = Self::tls_builder()?.with_platform_verifier()
                .map_err(|_| Error::Tls)?.with_no_client_auth();
            return Self::configured(proxy, tls);
        }
        Self::new(proxy, Self::system_roots(certs)?)
    }

    fn tls_builder() -> Result<rustls::ConfigBuilder<ClientConfig,rustls::WantsVerifier>, Error> {
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions().map_err(|_| Error::Tls)
    }

    fn configured(proxy: Option<Url>, tls: ClientConfig) -> Result<Self, Error> {
        if proxy.as_ref().is_some_and(|p| {
            !matches!(p.scheme(), "http" | "https")
                || p.host().is_none()
                || p.path() != "/"
                || p.username() != ""
                || p.password().is_some()
                || p.query().is_some()
                || p.fragment().is_some()
        }) {
            return Err(Error::Blocked);
        }
        Ok(Self {
            proxy,
            tls: Arc::new(tls),
        })
    }
    pub fn public_roots() -> RootCertStore {
        RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned())
    }
    fn system_roots(certs: Vec<Vec<u8>>) -> Result<RootCertStore, Error> {
        let mut roots = RootCertStore::empty();
        let (accepted, _) = roots.add_parsable_certificates(
            certs
                .into_iter()
                .map(rustls::pki_types::CertificateDer::from),
        );
        if accepted == 0 {
            return Err(Error::Tls);
        }
        Ok(roots)
    }
    pub async fn execute(
        &self,
        session: &Session,
        domains: &[String],
        request: Request,
        max_bytes: usize,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Response, Error> {
        let scope = if session.purpose() == crate::Purpose::ExternalImage {
            HostScope::PublicResource
        } else {
            HostScope::Declared
        };
        self.buffered(session, domains, request, max_bytes, timeout, cancel, None, scope).await
    }
    pub async fn execute_plugin(
        &self, session: &Session, domains: &[String], request: Request, max_bytes: usize,
        timeout: Duration, cancel: &CancellationToken, read: Option<&crate::ReadScope>,
    ) -> Result<Response, Error> {
        if session.purpose() != crate::Purpose::Source { return Err(Error::Blocked); }
        self.buffered(session, domains, request, max_bytes, timeout, cancel, read, HostScope::PluginHttp).await
    }
    async fn buffered(
        &self, session: &Session, domains: &[String], request: Request, max_bytes: usize,
        timeout: Duration, cancel: &CancellationToken, read: Option<&crate::ReadScope>, scope: HostScope,
    ) -> Result<Response, Error> {
        let head = request.method == Method::HEAD;
        let mut response = self
            .open_scoped(session, domains, request, timeout, cancel, scope, read)
            .await?;
        let mut data = Vec::new();
        while let Some(chunk) = response.next_chunk().await? {
            if chunk.len() > max_bytes - data.len() {
                return Err(Error::Limit);
            }
            data.extend_from_slice(&chunk);
        }
        if !head && !data.is_empty() {
            if let Some(encoding) = response.headers.get(header::CONTENT_ENCODING) {
                data = decode(
                    data,
                    encoding.to_str().map_err(|_| Error::Network)?,
                    max_bytes,
                )?;
            }
        }
        Ok(Response {
            status: response.status,
            headers: response.headers,
            body: data.into(),
            url: response.url,
        })
    }
    pub async fn open(
        &self,
        session: &Session,
        domains: &[String],
        request: Request,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<WireResponse, Error> {
        self.open_scoped(session, domains, request, timeout, cancel, HostScope::Declared, None)
            .await
    }
    pub async fn open_media(
        &self,
        session: &Session,
        domains: &[String],
        request: Request,
        timeout: Duration,
        cancel: &CancellationToken,
        session_scope: bool,
    ) -> Result<WireResponse, Error> {
        if session.purpose() != crate::Purpose::Media {
            return Err(Error::Blocked);
        }
        let scope = if session_scope { HostScope::PublicResource } else { HostScope::Declared };
        self.open_scoped(session, domains, request, timeout, cancel, scope, None)
            .await
    }
    async fn open_scoped(
        &self,
        session: &Session,
        domains: &[String],
        request: Request,
        timeout: Duration,
        cancel: &CancellationToken,
        scope: HostScope,
        read: Option<&crate::ReadScope>,
    ) -> Result<WireResponse, Error> {
        let epoch = session.token()?;
        let deadline = tokio::time::Instant::now() + timeout;
        tokio::select! {biased;
            _=cancel.cancelled()=>Err(Error::Cancelled),_=epoch.cancelled()=>Err(Error::Cancelled),
            result=tokio::time::timeout_at(deadline,self.transfer(session,domains,request,&epoch,cancel,deadline,scope,read))=>result.map_err(|_|Error::Timeout)?,
        }
    }
    async fn transfer(
        &self,
        session: &Session,
        domains: &[String],
        mut request: Request,
        epoch: &CancellationToken,
        cancel: &CancellationToken,
        deadline: tokio::time::Instant,
        scope: HostScope,
        read: Option<&crate::ReadScope>,
    ) -> Result<WireResponse, Error> {
        let original = request.url.origin();
        if scope == HostScope::PublicResource
            && (!matches!(request.method, Method::GET | Method::HEAD)
                || request.body.is_some()
                || request.connect_address.is_some())
        {
            return Err(Error::Blocked);
        }
        if request.connect_address.is_some_and(|ip| {
            !ip.is_ipv4()
                || !public_address(ip)
                || matches!(ip,IpAddr::V4(v) if matches!(v.octets(),[198,18|19,_,_]))
        }) || request.connect_address.is_some()
            && (!public_https(&request.url) || authorize(&request.url, domains).is_err())
        {
            return Err(Error::Blocked);
        }
        if !matches!(
            request.method,
            Method::GET
                | Method::HEAD
                | Method::POST
                | Method::PUT
                | Method::PATCH
                | Method::DELETE
                | Method::OPTIONS
        ) {
            return Err(Error::Blocked);
        }
        for name in [
            header::HOST,
            header::CONTENT_LENGTH,
            header::TRANSFER_ENCODING,
            header::CONNECTION,
            header::UPGRADE,
            header::PROXY_AUTHORIZATION,
            header::ACCEPT_ENCODING,
            header::TRAILER,
            header::TE,
        ] {
            request.headers.remove(name);
        }
        if !session.purpose.credentials() {
            request.headers.remove(header::COOKIE);
            request.headers.remove(header::AUTHORIZATION);
        }
        for _ in 0..8 {
            if let Some(scope) = read { scope.hop(&request, epoch)?; }
            let url = &request.url;
            if matches!(session.purpose(),crate::Purpose::Media|crate::Purpose::ExternalImage) && !public_https(url) {
                return Err(Error::Blocked);
            }
            let declared = authorize(url, domains).is_ok();
            let external_read = scope == HostScope::PluginHttp && !declared;
            if external_read {
                // 普通插件可读取公开外域资源。站点 Cookie、任意自定义凭据和连接地址不能外借。
                if !public_https(url) || !matches!(request.method, Method::GET | Method::HEAD)
                    || request.body.is_some() || (url.origin() == original && request.connect_address.is_some()) {
                    return Err(Error::Blocked);
                }
                crate::policy::strip_cross_origin(&mut request.headers);
            } else if scope == HostScope::Declared && !declared {
                return Err(Error::Blocked);
            }
            let pinned = if url.origin() == original {
                request.connect_address
            } else {
                None
            };
            let socket = connect(url, pinned, self.proxy.as_ref(), self.tls.clone()).await?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(socket))
                    .await
                    .map_err(|_| Error::Network)?;
            let _task = ConnectionTask(tokio::spawn(async move {
                let _ = connection.await;
            }));
            let mut builder = http::Request::builder()
                .method(&request.method)
                .uri(&url[Position::BeforePath..Position::AfterQuery]);
            let headers = builder.headers_mut().ok_or(Error::Blocked)?;
            headers.clone_from(&request.headers);
            let authority = &url[Position::BeforeHost..Position::AfterPort];
            headers.insert(header::HOST, authority.parse().map_err(|_| Error::Blocked)?);
            if !external_read && !headers.contains_key(header::COOKIE) {
                let value = session.header(url, epoch)?;
                if !value.is_empty() {
                    headers.insert(header::COOKIE, value.parse().map_err(|_| Error::Blocked)?);
                }
            }
            headers.insert(
                header::ACCEPT_ENCODING,
                if !session.purpose.credentials() || request.headers.contains_key(header::RANGE) {
                    header::HeaderValue::from_static("identity")
                } else {
                    header::HeaderValue::from_static("gzip, deflate")
                },
            );
            let outbound = builder
                .body(Full::new(request.body.clone().unwrap_or_default()))
                .map_err(|_| Error::Blocked)?;
            let response = sender
                .send_request(outbound)
                .await
                .map_err(|_| Error::Network)?;
            let status = response.status();
            if !external_read { session.accept(url, response.headers(), epoch)?; }
            if request.follow_redirects && matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                if let Some(location) = response.headers().get(header::LOCATION) {
                    let next = url
                        .join(location.to_str().map_err(|_| Error::Blocked)?)
                        .map_err(|_| Error::Blocked)?;
                    redirect(
                        url,
                        &next,
                        status,
                        &mut request.method,
                        &mut request.headers,
                        &mut request.body,
                    )?;
                    request.url = next;
                    continue;
                }
            }
            let (parts, body) = response.into_parts();
            return Ok(WireResponse {
                status,
                headers: parts.headers,
                url: request.url,
                body,
                _connection: _task,
                epoch: epoch.clone(),
                cancel: cancel.clone(),
                deadline,
            });
        }
        Err(Error::Blocked)
    }
}

fn decode(mut bytes: Vec<u8>, encodings: &str, limit: usize) -> Result<Vec<u8>, Error> {
    use std::io::Read;
    for label in encodings.split(',').rev() {
        let reader: Box<dyn Read> = match label.trim().to_ascii_lowercase().as_str() {
            "identity" => continue,
            "gzip" => Box::new(flate2::read::MultiGzDecoder::new(bytes.as_slice())),
            "deflate" => Box::new(flate2::read::ZlibDecoder::new(bytes.as_slice())),
            _ => return Err(Error::Network),
        };
        let mut decoded = Vec::new();
        reader
            .take((limit as u64).saturating_add(1))
            .read_to_end(&mut decoded)
            .map_err(|_| Error::Network)?;
        if decoded.len() > limit {
            return Err(Error::Limit);
        }
        bytes = decoded;
    }
    Ok(bytes)
}
