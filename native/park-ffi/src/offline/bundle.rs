use super::*;
use park_media::hls::{self, Playlist};
use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use url::Url;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    #[serde(default)]
    kind: String,
    entries: Vec<Entry>,
    #[serde(default)]
    blocks: Vec<BundleBlock>,
    #[serde(default)]
    complete: bool,
    #[serde(default)]
    preview_only: bool,
}
#[derive(Deserialize)]
struct Entry {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    parent: Option<String>,
    file: Option<String>,
    size: Option<u64>,
    #[serde(default, rename = "subtitleIds")]
    subtitles: Vec<String>,
}
#[derive(Deserialize, Debug, uniffi::Record)]
pub struct BundleBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<String>,
    pub asset: Option<String>,
}
#[derive(Debug, uniffi::Record)]
pub struct BundleEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub parent: Option<String>,
    pub subtitles: Vec<String>,
}
#[derive(Debug, uniffi::Record)]
pub struct BundleContents {
    pub kind: String,
    pub complete: bool,
    pub preview_only: bool,
    pub blocks: Vec<BundleBlock>,
    pub entries: Vec<BundleEntry>,
}
#[derive(Debug, uniffi::Record)]
pub struct BundleFile {
    pub path: String,
    pub resources: Vec<String>,
}

fn limited(file: &Path, limit: usize) -> Result<Vec<u8>, StorageError> {
    let mut bytes = Vec::new();
    let file = File::open(file).map_err(|_| StorageError::UnavailableDownload)?;
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| StorageError::UnavailableDownload)?;
    if bytes.len() > limit {
        return Err(StorageError::InvalidDownload);
    }
    Ok(bytes)
}
fn manifest(dir: &Path) -> Result<Manifest, StorageError> {
    let bytes = limited(&asset(dir, "bundle.json")?, 16 * 1024 * 1024)?;
    let m: Manifest = serde_json::from_slice(&bytes).map_err(|_| StorageError::InvalidDownload)?;
    if m.entries.len() > 10000 {
        return Err(StorageError::InvalidDownload);
    }
    let types: HashMap<_, _> = m
        .entries
        .iter()
        .map(|e| (e.id.as_str(), e.kind.as_str()))
        .collect();
    if types.len() != m.entries.len() || types.contains_key("") {
        return Err(StorageError::InvalidDownload);
    }
    for block in &m.blocks {
        if block.kind == "text" && block.text.is_some() {
            continue;
        }
        if !matches!(block.kind.as_str(), "image" | "video" | "audio")
            || block.asset.as_deref().and_then(|id| types.get(id)).copied()
                != Some(block.kind.as_str())
        {
            return Err(StorageError::InvalidDownload);
        }
    }
    Ok(m)
}
fn string(path: PathBuf) -> Result<String, StorageError> {
    path.into_os_string()
        .into_string()
        .map_err(|_| StorageError::InvalidDownload)
}

/// 包内清单只能指向该清单的分段或固定的独立音视频轨道。解析与播放共用同一文件白名单。
pub(crate) fn local_playlist(dir: &Path, name: &str) -> Result<Vec<String>, StorageError> {
    let tracks = format!("{name}.part.tracks/");
    let video = format!("{tracks}video.m3u8");
    let audio = format!("{tracks}audio.m3u8");
    let mut files = BTreeSet::new();
    let mut pending = vec![name.to_owned()];
    while let Some(current) = pending.pop() {
        let base = Url::parse(&format!("https://offline.park.invalid/{current}"))
            .map_err(|_| StorageError::InvalidDownload)?;
        let bytes = limited(&asset(dir, &current)?, hls::MANIFEST_LIMIT)?;
        let parsed = hls::parse(&bytes, &base, false).map_err(|_| StorageError::InvalidDownload)?;
        let urls = match parsed {
            Playlist::Master(items) if current == name && items.len() == 1 => {
                let v = &items[0];
                let a = v.audio.as_ref().ok_or(StorageError::InvalidDownload)?;
                if v.url.path().strip_prefix('/') != Some(video.as_str())
                    || a.path().strip_prefix('/') != Some(audio.as_str())
                {
                    return Err(StorageError::InvalidDownload);
                }
                vec![v.url.clone(), a.clone()]
            }
            Playlist::Media(list) if list.ended && !list.segments.is_empty() => {
                let prefix = format!("{current}.part.hls/");
                let mut urls = Vec::new();
                for seg in list.initialization.into_iter().chain(list.segments) {
                    let file = seg
                        .url
                        .path()
                        .strip_prefix('/')
                        .and_then(|p| p.strip_prefix(&prefix))
                        .ok_or(StorageError::InvalidDownload)?;
                    let valid = file == "init.mp4"
                        || file.split_once('.').is_some_and(|(n, ext)| {
                            !n.is_empty()
                                && n.bytes().all(|c| c.is_ascii_digit())
                                && matches!(ext, "ts" | "mp4")
                        });
                    if !valid || seg.encryption.is_some() {
                        return Err(StorageError::InvalidDownload);
                    }
                    urls.push(seg.url);
                }
                urls
            }
            _ => return Err(StorageError::InvalidDownload),
        };
        for url in urls {
            if url.origin() != base.origin() || url.query().is_some() || url.fragment().is_some() {
                return Err(StorageError::InvalidDownload);
            }
            let rel = url.path().trim_start_matches('/');
            let file = asset(dir, rel)?;
            if file != dir.join(rel)
                || file
                    .metadata()
                    .map_err(|_| StorageError::UnavailableDownload)?
                    .len()
                    == 0
            {
                return Err(StorageError::UnavailableDownload);
            }
            if files.insert(string(file)?) && rel.ends_with(".m3u8") {
                pending.push(rel.to_owned());
            }
            if files.len() > 100000 {
                return Err(StorageError::InvalidDownload);
            }
        }
    }
    Ok(files.into_iter().collect())
}
#[uniffi::export]
impl ProgressStore {
    pub fn offline_bundle(&self, key: ChapterKey) -> Result<BundleContents, StorageError> {
        let dir = self.chapter_directory(&key, "bundle")?;
        let m = manifest(&dir)?;
        Ok(BundleContents {
            kind: m.kind,
            complete: m.complete,
            preview_only: m.preview_only,
            blocks: m.blocks,
            entries: m
                .entries
                .into_iter()
                .map(|e| BundleEntry {
                    id: e.id,
                    name: e.name,
                    kind: e.kind,
                    parent: e.parent,
                    subtitles: e.subtitles,
                })
                .collect(),
        })
    }
    pub fn bundle_file(&self, key: ChapterKey, id: String) -> Result<BundleFile, StorageError> {
        self.checked_bundle_file(&key, &id, false)
    }
}

impl ProgressStore {
    pub(crate) fn checked_bundle_file(
        &self, key: &ChapterKey, id: &str, media_only: bool,
    ) -> Result<BundleFile, StorageError> {
        let dir = self.chapter_directory(key, "bundle")?;
        let entry = manifest(&dir)?
            .entries
            .into_iter()
            .find(|e| e.id == id)
            .ok_or(StorageError::InvalidDownload)?;
        if media_only && !matches!(entry.kind.as_str(), "audio" | "video") {
            return Err(StorageError::InvalidDownload);
        }
        resolve(&dir, entry)
    }
}

pub(crate) fn saved_attachment(
    dir: &Path,
    value: &serde_json::Value,
) -> Result<BundleFile, StorageError> {
    let entry = serde_json::from_value(value.clone()).map_err(|_| StorageError::InvalidDownload)?;
    resolve(dir, entry)
}
fn resolve(dir: &Path, entry: Entry) -> Result<BundleFile, StorageError> {
    let name = entry.file.ok_or(StorageError::UnavailableDownload)?;
    let (stem, ext) = name.split_once('.').ok_or(StorageError::InvalidDownload)?;
    if stem != format!("{:x}", Sha256::digest(entry.id.as_bytes()))
        || ext.is_empty()
        || !ext
            .bytes()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
    {
        return Err(StorageError::InvalidDownload);
    }
    let path = asset(dir, &name)?;
    let size = path
        .metadata()
        .map_err(|_| StorageError::UnavailableDownload)?
        .len();
    if size == 0 || Some(size) != entry.size || path != dir.join(&name) {
        return Err(StorageError::UnavailableDownload);
    }
    let resources = if ext == "m3u8" {
        local_playlist(dir, &name)?
    } else {
        Vec::new()
    };
    Ok(BundleFile {
        path: string(path)?,
        resources,
    })
}
