use super::{sync_directory, Error as StorageError};
use crate::cookies::{parse, Files};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// 浏览器 CookieManager 完成持久写入后才允许清除恢复日志。调用方在后台线程打开库。
pub trait RecoveryBrowser: Send + Sync {
    fn set_cookie(&self, source: &str, url: String, header: String) -> bool;
    fn flush(&self) -> bool;
}

type Result<T> = std::result::Result<T, StorageError>;
#[derive(Deserialize, Serialize)]
pub struct Partition {
    pub before: Files,
    pub after: Files,
    pub domains: Vec<String>,
}

impl From<crate::cookies::InvalidJar> for StorageError {
    fn from(_: crate::cookies::InvalidJar) -> Self {
        Self::InvalidJournal
    }
}

fn directory(parent: &Path, name: &str) -> Result<PathBuf> {
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(meta) if !meta.file_type().is_dir() => return Err(StorageError::InvalidJournal),
        Ok(_) => (),
        Err(e) if e.kind() == ErrorKind::NotFound => (),
        Err(_) => return Err(StorageError::IoFailure),
    }
    Ok(path)
}

impl Partition {
    pub fn validate(&self) -> Result<()> {
        parse(&self.before, true)?;
        parse(&self.after, true)?;
        Ok(())
    }

    pub fn restore(
        &self,
        root: &Path,
        source: &str,
        browser: Option<&dyn RecoveryBrowser>,
        committed: bool,
    ) -> Result<()> {
        let (desired, other) = if committed {
            (&self.after, &self.before)
        } else {
            (&self.before, &self.after)
        };
        let jars = directory(root, "cookie_jars")?;
        let encoded: String = source.bytes().map(|b| format!("{b:02x}")).collect();
        let partition = directory(&jars, &encoded)?;
        let dir = directory(&partition, "ie0_ps1")?;
        let mut current = Files::new();
        match fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|_| StorageError::IoFailure)?;
                    if !entry
                        .file_type()
                        .map_err(|_| StorageError::IoFailure)?
                        .is_file()
                    {
                        return Err(StorageError::InvalidJournal);
                    }
                    let name = entry
                        .file_name()
                        .into_string()
                        .map_err(|_| StorageError::InvalidJournal)?;
                    current.insert(
                        format!("ie0_ps1/{name}"),
                        fs::read_to_string(entry.path()).map_err(|_| StorageError::IoFailure)?,
                    );
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(_) => return Err(StorageError::IoFailure),
        }
        let mut prior = parse(&current, false)?;
        prior.extend(parse(other, true)?);
        let restored = parse(desired, true)?;
        let mut changed = false;
        // 先移除多余身份，再还原仍存在的项，同名 host-only 与 Domain 分别处理。
        for ((host, path, name, shared), cookie) in prior
            .iter()
            .filter(|(id, _)| !restored.contains_key(*id))
            .chain(restored.iter())
        {
            if !self
                .domains
                .iter()
                .any(|d| host == d || host.ends_with(&format!(".{d}")))
            {
                continue;
            }
            let target = browser.ok_or(StorageError::PendingRecovery)?;
            let id = (host.clone(), path.clone(), name.clone(), *shared);
            let mut header = cookie.clone();
            if !restored.contains_key(&id) {
                header.set_value("");
                header.set_max_age(cookie::time::Duration::ZERO);
                if name.starts_with("__Host-") || name.starts_with("__Secure-") {
                    header.set_secure(true);
                }
            }
            if !target.set_cookie(source, format!("https://{host}{path}"), header.to_string()) {
                return Err(StorageError::PendingRecovery);
            }
            changed = true;
        }
        if changed && !browser.ok_or(StorageError::PendingRecovery)?.flush() {
            return Err(StorageError::PendingRecovery);
        }
        let pending = partition.join("pending_cookies.json");
        if desired.is_empty()
            && current.is_empty()
            && !pending.try_exists().map_err(|_| StorageError::IoFailure)?
        {
            return Ok(());
        }
        fs::create_dir_all(&dir).map_err(|_| StorageError::IoFailure)?;
        for (key, contents) in desired {
            // 暂存放在分区外，进程中止不会在 Cookie 文件名空间留下半个 JSON。
            let mut temp =
                tempfile::NamedTempFile::new_in(root).map_err(|_| StorageError::IoFailure)?;
            temp.write_all(contents.as_bytes())
                .and_then(|_| temp.as_file().sync_all())
                .map_err(|_| StorageError::IoFailure)?;
            temp.persist(partition.join(key))
                .map_err(|_| StorageError::IoFailure)?;
        }
        for key in current.keys().filter(|name| !desired.contains_key(*name)) {
            fs::remove_file(partition.join(key)).map_err(|_| StorageError::IoFailure)?;
        }
        for path in [&dir, &partition, &jars, root] {
            sync_directory(path)?;
        }
        match fs::remove_file(pending) {
            Ok(()) => sync_directory(&partition)?,
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(_) => return Err(StorageError::IoFailure),
        }
        Ok(())
    }
}
