use crate::recovery::{atomic, gate, sync_directory, valid_key};
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::LazyLock,
};

const LIMIT: u64 = 8 * 1024 * 1024;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid_dictionary")]
    Invalid,
    #[error("dictionary_io")]
    Io,
}
type Result<T> = std::result::Result<T, Error>;
static CONTRACT: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(park_domain::PUBLIC_CONTRACT).unwrap());

fn directory(root: &Path, name: &str, create: bool) -> Result<PathBuf> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() && !redirect(&m) => (),
        Ok(_) => return Err(Error::Invalid),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if create {
                fs::create_dir(&path).map_err(|_| Error::Io)?;
                sync_directory(root).map_err(|_| Error::Io)?;
            }
        }
        Err(_) => return Err(Error::Io),
    }
    Ok(path)
}
fn redirect(m: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    m.file_type().is_symlink()
}
fn file(root: &Path, source: &str, locale: &str, create: bool) -> Result<PathBuf> {
    if !valid_key(source)
        || !CONTRACT["languages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["id"] == locale)
    {
        return Err(Error::Invalid);
    }
    let hex: String = source.bytes().map(|v| format!("{v:02x}")).collect();
    Ok(
        directory(&directory(root, "tag_translations", create)?, &hex, create)?
            .join(format!("{locale}.json")),
    )
}
fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    let m = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error::Io),
    };
    if redirect(&m) || !m.is_file() || m.len() > LIMIT {
        return Err(Error::Invalid);
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(LIMIT + 1).read_to_end(&mut bytes))
        .map_err(|_| Error::Io)?;
    if bytes.len() as u64 > LIMIT {
        return Err(Error::Invalid);
    }
    Ok(Some(bytes))
}
pub fn installed(root: &Path, source: &str, locale: &str) -> Result<Option<Vec<u8>>> {
    let _lock = gate(root, false).map_err(|_| Error::Io)?;
    read(&file(root, source, locale, false)?)
}
pub fn inventory(root: &Path) -> Result<Value> {
    let _lock = gate(root, false).map_err(|_| Error::Io)?;
    let base = directory(root, "tag_translations", false)?;
    let mut installed = Vec::new();
    let entries = match base.read_dir() {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Value::Array(installed)),
        Err(_) => return Err(Error::Io),
    };
    for entry in entries {
        let entry = entry.map_err(|_| Error::Io)?;
        let name = entry.file_name();
        let Some(hex) = name.to_str() else { continue };
        if hex.len() > 200 || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) { continue; }
        let source: String = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i+2],16).unwrap() as char).collect();
        if !valid_key(&source) { continue; }
        let dir = directory(&base,hex,false)?;
        for language in CONTRACT["languages"].as_array().unwrap() {
            let locale = language["id"].as_str().unwrap();
            match fs::symlink_metadata(dir.join(format!("{locale}.json"))) {
                Ok(m) if m.is_file() && !redirect(&m) => installed.push(json!({"source":source,"locale":locale})),
                Ok(_) => return Err(Error::Invalid),
                Err(e) if e.kind()==io::ErrorKind::NotFound => (),
                Err(_) => return Err(Error::Io),
            }
        }
    }
    Ok(Value::Array(installed))
}
fn metadata(bytes: &[u8], source: &str, locale: &str) -> Result<Value> {
    let doc: Value = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
    if doc["source"] != source || doc["locale"] != locale || doc["schema"] != 1 {
        return Err(Error::Invalid);
    }
    Ok(json!({"version":doc["version"],"provenance":doc["provenance"]}))
}
pub fn install(root: &Path, source: &str, locale: &str, bytes: &[u8]) -> Result<()> {
    if park_domain::TagDictionary::parse(bytes, source, locale).is_none() {
        return Err(Error::Invalid);
    }
    let info = metadata(bytes, source, locale)?;
    let _lock = gate(root, true).map_err(|_| Error::Io)?;
    let path = file(root, source, locale, true)?;
    if let Some(old) = read(&path)? {
        let earlier = metadata(&old, source, locale)?;
        fn version(info: &Value) -> Result<Vec<u64>> {
            info["version"]
                .as_str()
                .ok_or(Error::Invalid)?
                .split('.')
                .map(|n| n.parse().map_err(|_| Error::Invalid))
                .collect()
        }
        if version(&earlier)? > version(&info)? {
            return Err(Error::Invalid);
        }
    }
    let parent = path.parent().unwrap();
    atomic(parent, &format!("{locale}.json"), bytes).map_err(|_| Error::Io)?;
    sync_directory(parent).map_err(|_| Error::Io)
}
pub fn remove(root: &Path, source: &str, locale: &str) -> Result<()> {
    let _lock = gate(root, true).map_err(|_| Error::Io)?;
    let path = file(root, source, locale, false)?;
    match fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() && !redirect(&m) => {
            fs::remove_file(&path).map_err(|_| Error::Io)?;
            sync_directory(path.parent().unwrap()).map_err(|_| Error::Io)
        }
        Ok(_) => Err(Error::Invalid),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(Error::Io),
    }
}
