use crate::Error;
use cookie_store::{CookieDomain, CookieExpiration, CookieStore};
use park_storage::cookies::{parse, Files};
use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;
use url::Url;

fn regular(path: &Path, directory: bool) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(info)
            if if directory {
                info.is_dir()
            } else {
                info.is_file()
            } =>
        {
            Ok(true)
        }
        Ok(_) => Err(Error::Storage),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(_) => Err(Error::Storage),
    }
}
fn read(dir: &Path) -> Result<Files, Error> {
    let mut files = Files::new();
    if !regular(dir, true)? {
        return Ok(files);
    }
    for entry in fs::read_dir(dir).map_err(|_| Error::Storage)? {
        let entry = entry.map_err(|_| Error::Storage)?;
        if !entry.file_type().map_err(|_| Error::Storage)?.is_file() {
            return Err(Error::Storage);
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::Storage)?;
        files.insert(
            format!("ie0_ps1/{name}"),
            fs::read_to_string(entry.path()).map_err(|_| Error::Storage)?,
        );
    }
    Ok(files)
}

fn atomic_file(path: &Path, staging: &Path, data: &[u8]) -> Result<(), Error> {
    let mut temp = tempfile::NamedTempFile::new_in(staging).map_err(|_| Error::Storage)?;
    temp.write_all(data)
        .and_then(|_| temp.as_file().sync_all())
        .map_err(|_| Error::Storage)?;
    temp.persist(path).map_err(|_| Error::Storage)?;
    Ok(())
}
fn sync_dir(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|_| Error::Storage)?;
    let _ = path;
    Ok(())
}

const JOURNAL: &str = "pending_cookies.json";
fn apply(root: &Path, files: &Files) -> Result<(), Error> {
    // 快照中的全部路径和值必须先通过 v10 校验，随后才能替换任何文件。
    parse(files, true).map_err(|_| Error::Storage)?;
    let dir = root.join("ie0_ps1");
    let old = read(&dir)?;
    parse(&old, false).map_err(|_| Error::Storage)?;
    fs::create_dir_all(&dir).map_err(|_| Error::Storage)?;
    for (name, contents) in files {
        if old.get(name) != Some(contents) {
            atomic_file(&root.join(name), root, contents.as_bytes())?;
        }
    }
    for name in old.keys().filter(|k| !files.contains_key(*k)) {
        fs::remove_file(root.join(name)).map_err(|_| Error::Storage)?;
    }
    sync_dir(&dir)?;
    fs::remove_file(root.join(JOURNAL)).map_err(|_| Error::Storage)?;
    sync_dir(root)
}

pub(crate) fn load(root: &Path) -> Result<CookieStore, Error> {
    let mut store = CookieStore::default();
    if !regular(root, true)? {
        return Ok(store);
    }
    let journal = root.join(JOURNAL);
    if regular(&journal, false)? {
        let saved: Files =
            serde_json::from_reader(fs::File::open(journal).map_err(|_| Error::Storage)?)
                .map_err(|_| Error::Storage)?;
        apply(root, &saved)?;
    }
    let files = read(&root.join("ie0_ps1"))?;
    for ((host, _, _, _), raw) in parse(&files, false).map_err(|_| Error::Storage)? {
        if raw
            .expires_datetime()
            .is_some_and(|expiry| expiry <= cookie::time::OffsetDateTime::now_utc())
        {
            continue;
        }
        let origin = Url::parse(&format!("https://{host}/")).map_err(|_| Error::Storage)?;
        store
            .insert_raw(&raw, &origin)
            .map_err(|_| Error::Storage)?;
    }
    Ok(store)
}

pub(crate) fn save(root: &Path, jar: &CookieStore) -> Result<(), Error> {
    type Paths = BTreeMap<String, BTreeMap<String, String>>;
    let mut shared: BTreeMap<String, Paths> = BTreeMap::new();
    let mut hosts: BTreeMap<String, Paths> = BTreeMap::new();
    for cookie in jar.iter_unexpired() {
        let (domain, collection) = match &cookie.domain {
            CookieDomain::HostOnly(host) => (host, &mut hosts),
            CookieDomain::Suffix(domain) => (domain, &mut shared),
            _ => return Err(Error::Storage),
        };
        let mut raw: cookie::Cookie<'static> = (**cookie).clone();
        let path: &str = cookie.path.as_ref();
        raw.set_path(path.to_owned());
        // 旧客户端用创建秒数解释 Max-Age。根据绝对到期时间还原创建时刻，写盘不会续期。
        let created = match (raw.max_age(), &cookie.expires) {
            (Some(age), CookieExpiration::AtUtc(end)) => {
                end.unix_timestamp().saturating_sub(age.whole_seconds())
            }
            _ => cookie::time::OffsetDateTime::now_utc().unix_timestamp(),
        };
        collection
            .entry(domain.clone())
            .or_default()
            .entry(path.to_owned())
            .or_default()
            .insert(raw.name().to_owned(), format!("{raw};_crt={created}"));
    }
    let mut files = Files::new();
    files.insert(
        "ie0_ps1/.index".into(),
        serde_json::to_string(&hosts.keys().collect::<Vec<_>>()).map_err(|_| Error::Storage)?,
    );
    files.insert(
        "ie0_ps1/.domains".into(),
        serde_json::to_string(&shared).map_err(|_| Error::Storage)?,
    );
    for (name, paths) in hosts {
        files.insert(
            format!("ie0_ps1/{name}"),
            serde_json::to_string(&paths).map_err(|_| Error::Storage)?,
        );
    }
    parse(&files, true).map_err(|_| Error::Storage)?;
    regular(root, true)?;
    fs::create_dir_all(root).map_err(|_| Error::Storage)?;
    let data = serde_json::to_vec(&files).map_err(|_| Error::Storage)?;
    atomic_file(&root.join(JOURNAL), root, &data)?;
    sync_dir(root)?;
    apply(root, &files)
}
