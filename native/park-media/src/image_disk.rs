use crate::{prepare, Error, ImageAsset, PageFile};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};
use tokio_util::sync::CancellationToken;

static DISK: Mutex<()> = Mutex::new(());
const MAX_IMAGE: u64 = 20 * 1024 * 1024;

#[derive(Clone)]
pub struct ImageCacheEntry {
    root: PathBuf,
    key: String,
    limit: u64,
}
impl ImageCacheEntry {
    pub fn new(
        cache: &Path,
        source: &str,
        address: &str,
        cipher: &serde_json::Value,
        limit: u64,
    ) -> Self {
        let identity = if cipher["type"] == "aes-cbc" {
            let key = serde_json::json!(["aes-cbc", cipher["key"], cipher["iv"]]);
            format!("{address}:decrypt:{:x}", Sha256::digest(key.to_string()))
        } else {
            address.to_owned()
        };
        let key = format!(
            "{:x}",
            Sha256::digest(serde_json::json!([source, identity]).to_string())
        );
        Self {
            root: cache.join("park_cache"),
            key,
            limit,
        }
    }
    fn path(&self, create: bool) -> Option<PathBuf> {
        let image = self.root.join("img");
        let bucket = image.join(&self.key[..2]);
        for dir in [&self.root, &image, &bucket] {
            if create && !dir.exists() {
                match fs::create_dir(dir) {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                    Err(_) => return None,
                }
            }
            if !fs::symlink_metadata(dir).ok()?.is_dir() {
                return None;
            }
        }
        let path = bucket.join(&self.key);
        match fs::symlink_metadata(&path) {
            Ok(m) if !m.is_file() => None,
            Ok(_) => Some(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(path),
            _ => None,
        }
    }
    pub fn read(
        &self,
        cache: &Path,
        token: &CancellationToken,
    ) -> Result<Option<ImageAsset>, Error> {
        let snapshot = {
            let _guard = DISK.lock().map_err(|_| Error::Storage)?;
            let Some(path) = self.path(false) else {
                return Ok(None);
            };
            let Ok(f) = fs::File::open(&path) else {
                return Ok(None);
            };
            if !f
                .metadata()
                .is_ok_and(|m| (1..=MAX_IMAGE).contains(&m.len()))
            {
                let _ = fs::remove_file(path);
                return Ok(None);
            }
            let mut temp = tempfile::NamedTempFile::new_in(cache).map_err(|_| Error::Storage)?;
            std::io::copy(&mut f.take(MAX_IMAGE + 1), &mut temp).map_err(|_| Error::Storage)?;
            temp
        };
        let fingerprint = checksum(snapshot.path())?;
        match prepare(PageFile::Temporary(snapshot), 0, false, cache, token) {
            Ok(asset) => Ok(Some(asset)),
            Err(Error::Cancelled) => Err(Error::Cancelled),
            Err(_) => {
                self.remove_if(&fingerprint);
                Ok(None)
            }
        }
    }
    pub fn remove_if(&self, digest: &[u8]) {
        let Ok(_guard) = DISK.lock() else { return };
        if let Some(path) = self
            .path(false)
            .filter(|p| checksum(p).is_ok_and(|d| d == digest))
        {
            let _ = fs::remove_file(path);
        }
    }
    pub fn store(&self, bytes: &[u8], token: &CancellationToken) {
        if bytes.len() as u64 > self.limit || token.is_cancelled() {
            return;
        }
        let Ok(_guard) = DISK.lock() else { return };
        let Some(path) = self.path(true) else { return };
        let Some(parent) = path.parent() else { return };
        let Ok(mut temp) = tempfile::NamedTempFile::new_in(parent) else {
            return;
        };
        if temp.write_all(bytes).is_err() || token.is_cancelled() {
            return;
        }
        if temp.persist(path).is_ok() {
            self.trim()
        }
    }
    fn trim(&self) {
        let mut images = Vec::new();
        let mut bytes = 0u64;
        let mut media = 0u64;
        for (root, is_image) in [
            (self.root.join("img"), true),
            (self.root.join("subscription_media"), false),
        ] {
            let mut pending = vec![root];
            while let Some(dir) = pending.pop() {
                if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                    continue;
                }
                let Ok(entries) = fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    if kind.is_dir() {
                        pending.push(entry.path());
                        continue;
                    }
                    if !kind.is_file() {
                        continue;
                    }
                    let Ok(m) = entry.metadata() else { continue };
                    if !is_image {
                        media = media.saturating_add(m.len());
                        continue;
                    }
                    let name = entry.file_name();
                    if !name
                        .to_str()
                        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                    {
                        continue;
                    }
                    bytes = bytes.saturating_add(m.len());
                    images.push((
                        m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                        entry.path(),
                        m.len(),
                    ));
                }
            }
        }
        let budget = self.limit.saturating_sub(media);
        if bytes <= budget {
            return;
        }
        images.sort_unstable();
        for (_, file, len) in images {
            if bytes <= budget / 5 * 4 {
                break;
            }
            if fs::remove_file(file).is_ok() {
                bytes = bytes.saturating_sub(len)
            }
        }
    }
}
pub fn checksum(path: &Path) -> Result<Vec<u8>, Error> {
    let mut f = fs::File::open(path)
        .map_err(|_| Error::Storage)?
        .take(MAX_IMAGE + 1);
    let mut digest = Sha256::new();
    std::io::copy(&mut f, &mut digest).map_err(|_| Error::Storage)?;
    Ok(digest.finalize().to_vec())
}
