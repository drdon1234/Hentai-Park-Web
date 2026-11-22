use crate::{prepare, Error, ImageKey, ImagePool, ImageTicket, PageFile, Priority, Route};
use http::{header, HeaderMap, Method};
use park_net::{Request, Session, Transport};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

/// URL 和回退地址由原样插件在任务真正取得额度后解析。域权限来自已安装源的声明。
pub struct ResolvedImage {
    pub urls: Vec<Url>,
    pub headers: HeaderMap,
}
impl ResolvedImage {
    pub fn from_page(
        page: &serde_json::Value,
        headers: &[&serde_json::Value],
    ) -> Result<Self, Error> {
        let mut urls = vec![Url::parse(page["url"].as_str().ok_or(Error::Unavailable)?)
            .map_err(|_| Error::Unavailable)?];
        if let Some(fallbacks) = page["fallbacks"].as_array() {
            for value in fallbacks {
                urls.push(
                    Url::parse(value.as_str().ok_or(Error::Unavailable)?)
                        .map_err(|_| Error::Unavailable)?,
                );
            }
        }
        let mut result = HeaderMap::new();
        for map in headers.iter().filter_map(|v| v.as_object()) {
            for (name, value) in map {
                if let (Ok(name), Some(v)) = (header::HeaderName::try_from(name), value.as_str()) {
                    result.insert(name, v.parse().map_err(|_| Error::Unavailable)?);
                }
            }
        }
        Ok(Self {
            urls,
            headers: result,
        })
    }
}
struct ImageCipher {
    key: Vec<u8>,
    iv: Vec<u8>,
}
impl ImageCipher {
    fn parse(value: &serde_json::Value) -> Result<Option<Self>, Error> {
        if value.is_null() || value.as_object().is_some_and(|v| v.is_empty()) {
            return Ok(None);
        }
        let key = value["key"].as_str().ok_or(Error::Unavailable)?.as_bytes();
        let iv = value["iv"].as_str().ok_or(Error::Unavailable)?.as_bytes();
        if value["type"] != "aes-cbc" || !matches!(key.len(), 16 | 24 | 32) || iv.len() != 16 {
            return Err(Error::Unavailable);
        }
        Ok(Some(Self {
            key: key.to_vec(),
            iv: iv.to_vec(),
        }))
    }
    fn decode<'a>(&self, bytes: &'a mut [u8]) -> Result<&'a [u8], Error> {
        use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
        if image::guess_format(bytes).is_ok() {
            return Ok(bytes);
        }
        macro_rules! decrypt {
            ($aes:ty) => {
                cbc::Decryptor::<$aes>::new_from_slices(&self.key, &self.iv)
                    .map_err(|_| Error::InvalidImage)?
                    .decrypt_padded_mut::<Pkcs7>(bytes)
                    .map_err(|_| Error::InvalidImage)
            };
        }
        match self.key.len() {
            16 => decrypt!(aes::Aes128),
            24 => decrypt!(aes::Aes192),
            32 => decrypt!(aes::Aes256),
            _ => Err(Error::InvalidImage),
        }
    }
}
pub struct RemoteImages {
    pool: Arc<ImagePool>,
    transport: Arc<Transport>,
    session: Session,
    domains: Vec<String>,
    cache: PathBuf,
    source: String,
    generation: u64,
    cipher: Option<ImageCipher>,
}
impl RemoteImages {
    pub fn new(
        pool: Arc<ImagePool>,
        transport: Arc<Transport>,
        sessions: &park_net::Sessions,
        source: &str,
        domains: Vec<String>,
        cache: PathBuf,
        decryption: &serde_json::Value,
    ) -> Result<Arc<Self>, Error> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let session = sessions
            .partition(source, park_net::Purpose::ExternalImage)
            .map_err(|_| Error::Storage)?;
        Ok(Arc::new(Self {
            pool,
            transport,
            session,
            domains,
            cache,
            source: source.into(),
            generation: NEXT.fetch_add(1, Ordering::Relaxed),
            cipher: ImageCipher::parse(decryption)?,
        }))
    }
    pub fn load(
        self: &Arc<Self>,
        identity: &str,
        strips: u32,
        crop: bool,
        route: Route,
        priority: Priority,
        resolve: impl Fn(u8, &CancellationToken) -> Result<ResolvedImage, Error> + Send + 'static,
    ) -> ImageTicket {
        self.schedule(identity, strips, crop, route, priority, None, resolve)
    }
    pub fn load_url(
        self: &Arc<Self>,
        address: &str,
        headers: serde_json::Value,
        entry: crate::image_disk::ImageCacheEntry,
    ) -> Result<ImageTicket, Error> {
        let url = Url::parse(address).map_err(|_| Error::Unavailable)?;
        if !park_net::public_https(&url) {
            return Err(Error::Unavailable);
        }
        let host = url.host_str().ok_or(Error::Unavailable)?.to_owned();
        let asset = serde_json::json!({"url":address});
        Ok(self.schedule(
            address,
            0,
            false,
            Route {
                host,
                browser_session: None,
            },
            Priority::Visible,
            Some(entry),
            move |_, _| ResolvedImage::from_page(&asset, &[&headers]),
        ))
    }
    fn schedule(
        self: &Arc<Self>,
        identity: &str,
        strips: u32,
        crop: bool,
        route: Route,
        priority: Priority,
        disk: Option<crate::image_disk::ImageCacheEntry>,
        resolve: impl Fn(u8, &CancellationToken) -> Result<ResolvedImage, Error> + Send + 'static,
    ) -> ImageTicket {
        let loader = self.clone();
        let key = ImageKey {
            source: self.source.clone(),
            identity: format!("{}:{identity}", self.generation),
            strips,
            crop,
        };
        self.pool.request(key, route, priority, move |token| {
            if let Some(ref disk) = disk {
                if let Some(asset) = disk.read(&loader.cache, token)? {
                    return Ok(asset);
                }
            }
            let io = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| Error::Unavailable)?;
            let mut last = Error::Unavailable;
            for retry in 0..3 {
                if token.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let resolved = match resolve(retry, token) {
                    Ok(v) => v,
                    Err(Error::Cancelled) => return Err(Error::Cancelled),
                    Err(e) => {
                        last = e;
                        continue;
                    }
                };
                let mut headers = HeaderMap::new();
                for name in [
                    header::ACCEPT,
                    header::ACCEPT_LANGUAGE,
                    header::USER_AGENT,
                    header::REFERER,
                    header::ORIGIN,
                ] {
                    if let Some(value) = resolved.headers.get(&name) {
                        if name == header::REFERER || name == header::ORIGIN {
                            let url = value
                                .to_str()
                                .ok()
                                .and_then(|v| Url::parse(v).ok())
                                .ok_or(Error::Unavailable)?;
                            if url.scheme() != "https"
                                || url.port_or_known_default() != Some(443)
                                || !url.username().is_empty()
                                || url.password().is_some()
                                || url.fragment().is_some()
                                || !loader.domains.iter().any(|d| {
                                    url.host_str()
                                        .is_some_and(|h| h == d || h.ends_with(&format!(".{d}")))
                                })
                            {
                                return Err(Error::Unavailable);
                            }
                        }
                        headers.insert(name, value.clone());
                    }
                }
                for url in resolved.urls {
                    if !park_net::public_https(&url) {
                        last=Error::Unavailable;
                        continue;
                    }
                    let req = Request {
                        url,
                        method: Method::GET,
                        headers: headers.clone(),
                        body: None,
                        connect_address: None,
                        follow_redirects: true,
                    };
                    let result = io.block_on(loader.transport.execute(
                        &loader.session,
                        &loader.domains,
                        req,
                        20 * 1024 * 1024,
                        Duration::from_secs(30),
                        token,
                    ));
                    let response = match result {
                        Ok(r) if r.status.is_success() => r,
                        Err(park_net::Error::Cancelled) => return Err(Error::Cancelled),
                        _ => {
                            last = Error::Unavailable;
                            continue;
                        }
                    };
                    let mut file = tempfile::NamedTempFile::new_in(&loader.cache)
                        .map_err(|_| Error::Storage)?;
                    let mut decrypted;
                    let data = if let Some(cipher) = &loader.cipher {
                        decrypted = response.body.to_vec();
                        match cipher.decode(&mut decrypted) {
                            Ok(b) => b,
                            Err(e) => {
                                last = e;
                                continue;
                            }
                        }
                    } else {
                        response.body.as_ref()
                    };
                    file.write_all(data).map_err(|_| Error::Storage)?;
                    match prepare(
                        PageFile::Temporary(file),
                        strips,
                        crop,
                        &loader.cache,
                        token,
                    ) {
                        Ok(asset) => {
                            if let Some(ref disk) = disk {
                                disk.store(data, token)
                            }
                            return Ok(asset);
                        }
                        Err(Error::Cancelled) => return Err(Error::Cancelled),
                        Err(Error::Storage) => return Err(Error::Storage),
                        Err(e) => last = e,
                    }
                }
            }
            Err(last)
        })
    }
}
