use crate::{ProgressStore, StorageError};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, fs, io, path::{Component, Path, PathBuf}};

#[derive(uniffi::Record)]
pub struct StorageUsage {
    pub home_cache: u64,
    pub media_cache: u64,
    pub database: u64,
    pub backups: u64,
    pub downloads: HashMap<String,u64>,
    pub unavailable: u32,
}

fn file_bytes(root: &Path) -> Result<u64, StorageError> {
    let mut pending = vec![root.to_path_buf()];
    let mut bytes = 0u64;
    while let Some(path) = pending.pop() {
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(StorageError::IoFailure),
        };
        if meta.is_dir() {
            for entry in fs::read_dir(path).map_err(|_| StorageError::IoFailure)? {
                pending.push(entry.map_err(|_| StorageError::IoFailure)?.path());
            }
        } else if meta.is_file() { bytes = bytes.saturating_add(meta.len()); }
    }
    Ok(bytes)
}

fn records(path: &Path) -> Result<Vec<Value>, StorageError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => serde_json::from_reader(fs::File::open(path).map_err(|_| StorageError::IoFailure)?)
            .map_err(|_| StorageError::InvalidDownload),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
        _ => Err(StorageError::IoFailure),
    }
}

fn game_directory(row: &Value) -> Option<PathBuf> {
    let path = Path::new(row["directory"].as_str()?);
    if !path.is_absolute() || path.components().any(|v| v == Component::ParentDir) { return None; }
    let mut suffix = PathBuf::from("games");
    for key in [&row["item"]["source"], &row["item"]["id"], &row["resourceId"], &row["channelId"]] {
        suffix.push(format!("{:x}",Sha256::digest(key.as_str()?.as_bytes())));
    }
    if !path.ends_with(&suffix) { return None; }
    for dir in path.ancestors().take(5) {
        match fs::symlink_metadata(dir) {
            Ok(meta) if meta.is_dir() => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            _ => return None,
        }
    }
    Some(path.to_owned())
}

#[uniffi::export]
impl ProgressStore {
    pub fn storage_usage(&self, cache_directory: String, backup_directory: String) -> Result<StorageUsage, StorageError> {
        let cache = Path::new(&cache_directory);
        let mut result = StorageUsage {
            home_cache: park_media::image_disk::cache_bytes(cache, true, false).map_err(|_| StorageError::IoFailure)?,
            media_cache: park_media::image_disk::cache_bytes(cache, false, false).map_err(|_| StorageError::IoFailure)?,
            database: 0, backups: file_bytes(Path::new(&backup_directory))?, downloads: HashMap::new(), unavailable: 0,
        };
        for name in ["park.db","park.db-wal","park.db-shm"] { result.database += file_bytes(&self.root.join(name))?; }
        let mut dirs = Vec::new();
        {
            let db = self.lock()?;
            let mut query = db.prepare("SELECT source,substr(uid,length(source)+2),dir FROM download_tasks WHERE substr(uid,1,length(source)+1)=source||':'")?;
            let rows = query.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?;
            for row in rows {
                let (source,id,path)=row?;
                let dir = park_storage::downloads::owned_directory(&db,&source,&id,Path::new(&path)).ok();
                dirs.push((source,dir));
            }
        }
        for row in records(&self.root.join("game_downloads.json"))? {
            dirs.push((row["item"]["source"].as_str().unwrap_or_default().to_owned(),game_directory(&row)));
        }
        for row in records(&self.root.join("recordings.json"))? {
            let dir = row["id"].as_str().and_then(|id| park_storage::recordings::directory(&self.root,id).ok());
            dirs.push((row["item"]["source"].as_str().unwrap_or_default().to_owned(),dir));
        }
        for (source,dir) in dirs {
            if let Some(bytes) = dir.and_then(|dir| file_bytes(&dir).ok()) {
                *result.downloads.entry(source).or_default() += bytes;
            } else { result.unavailable += 1; }
        }
        Ok(result)
    }

    pub fn clear_storage_cache(&self, cache_directory: String, home: bool) -> Result<(),StorageError> {
        park_media::image_disk::cache_bytes(Path::new(&cache_directory), home, true)
            .map(|_| ()).map_err(|_| StorageError::IoFailure)
    }

    pub fn delete_read_downloads(&self) -> Result<u64, StorageError> {
        let mut db = self.lock()?;
        Ok(park_storage::downloads::delete_read(&mut db)?)
    }
}
