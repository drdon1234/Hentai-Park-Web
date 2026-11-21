use super::*;
use park_domain::updates::Release;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, io::Write};

const LATEST: &str = "https://api.github.com/repos/drdon1234/Hentai-Park/releases/latest";
#[derive(Object)]
pub struct AppRelease {
    release: Release,
    host: Arc<RouteHost>,
}
#[uniffi::export(callback_interface)]
pub trait UpdateProgress: Send + Sync {
    fn received(&self, bytes: u64, total: u64);
}
fn request(url: Url, redirect: bool) -> park_net::Request {
    let mut req = park_net::Request {
        url,
        method: "GET".parse().unwrap(),
        headers: Default::default(),
        body: None,
        connect_address: None,
        follow_redirects: redirect,
    };
    req.headers
        .insert("user-agent", "HentaiPark".parse().unwrap());
    req
}
fn net(error: park_net::Error) -> SourceFailure {
    if error == park_net::Error::Cancelled {
        SourceFailure::Cancelled
    } else {
        SourceFailure::Unavailable
    }
}
#[uniffi::export]
impl SourceService {
    pub fn check_app_release(
        &self,
        current: String,
        call: Arc<Request>,
    ) -> Result<Option<Arc<AppRelease>>> {
        let session = self
            .routes
            .sessions
            .partition("_updates", Purpose::Repository)
            .map_err(net)?;
        let mut req = request(Url::parse(LATEST).unwrap(), false);
        req.headers
            .insert("accept", "application/vnd.github+json".parse().unwrap());
        let response = self
            .routes
            .runtime
            .block_on(self.routes.transport.execute(
                &session,
                &["api.github.com".into()],
                req,
                4 * 1024 * 1024,
                Duration::from_secs(15),
                &call.cancelled,
            ))
            .map_err(net)?;
        if response.status.as_u16() == 404 {
            return Ok(None);
        }
        if response.status.as_u16() != 200 {
            return Err(SourceFailure::Unavailable);
        }
        let doc: Value =
            serde_json::from_slice(&response.body).map_err(|_| SourceFailure::Unsupported)?;
        let release = Release::parse(&doc, &current).map_err(|_| SourceFailure::Unsupported)?;
        Ok(release.map(|release| {
            Arc::new(AppRelease {
                release,
                host: self.routes.clone(),
            })
        }))
    }
}
#[uniffi::export]
impl AppRelease {
    pub fn description(&self, chinese: bool, abis: Vec<String>) -> String {
        let release = &self.release;
        json!({"version":release.version,"url":release.url,"notes":release.localized_notes(chinese),"downgrade":release.downgrade,
            "size":release.apk(&abis).map(|a|a.size)}).to_string()
    }
    pub fn prepare_apk(
        &self,
        abis: Vec<String>,
        progress: Box<dyn UpdateProgress>,
        call: Arc<Request>,
    ) -> Result<String> {
        let apk = self.release.apk(&abis).ok_or(SourceFailure::Unsupported)?;
        let session = self
            .host
            .sessions
            .partition("_updates", Purpose::Repository)
            .map_err(net)?;
        let directory = self.host.cache.join("app_updates");
        match fs::symlink_metadata(&directory) {
            Ok(m) if m.is_dir() => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&directory).map_err(|_| SourceFailure::Storage)?;
            }
            _ => return Err(SourceFailure::Storage),
        }
        let mut temp =
            tempfile::NamedTempFile::new_in(&directory).map_err(|_| SourceFailure::Storage)?;
        let mut digest = Sha256::new();
        self.host.runtime.block_on(async {
            let domains = [
                "github.com".into(),
                "release-assets.githubusercontent.com".into(),
                "objects.githubusercontent.com".into(),
            ];
            let req = request(
                Url::parse(&apk.url).map_err(|_| SourceFailure::Unsupported)?,
                true,
            );
            let mut response = self
                .host
                .transport
                .open(
                    &session,
                    &domains,
                    req,
                    Duration::from_secs(1800),
                    &call.cancelled,
                )
                .await
                .map_err(net)?;
            if response.status.as_u16() != 200 {
                return Err(SourceFailure::Unavailable);
            }
            if let Some(length) = response.headers.get("content-length") {
                if length.to_str().ok().and_then(|s| s.parse::<u64>().ok()) != Some(apk.size) {
                    return Err(SourceFailure::Unsupported);
                }
            }
            let mut count = 0u64;
            let mut reported = std::time::Instant::now();
            while let Some(chunk) = response.next_chunk().await.map_err(net)? {
                count += chunk.len() as u64;
                if count > apk.size {
                    return Err(SourceFailure::Unsupported);
                }
                temp.write_all(&chunk).map_err(|_| SourceFailure::Storage)?;
                digest.update(&chunk);
                if reported.elapsed() >= Duration::from_millis(180) || count == apk.size {
                    progress.received(count, apk.size);
                    reported = std::time::Instant::now();
                }
            }
            if count != apk.size || format!("{:x}", digest.finalize()) != apk.sha256 {
                return Err(SourceFailure::Unsupported);
            }
            Ok(())
        })?;
        temp.as_file()
            .sync_all()
            .map_err(|_| SourceFailure::Storage)?;
        if call.cancelled.is_cancelled() {
            return Err(SourceFailure::Cancelled);
        }
        let out = directory.join(format!("{}.apk", apk.sha256));
        temp.persist(&out).map_err(|_| SourceFailure::Storage)?;
        out.into_os_string()
            .into_string()
            .map_err(|_| SourceFailure::Storage)
    }
}
