use super::*;
use serde_json::Value;

/// 桌面插件宿主已完成一次 videos 调用。授权来自同一运行实例的清单。
#[uniffi::export]
pub fn open_host_video(
    source: String,
    manifest: String,
    entries: String,
    cache_directory: String,
    proxy: String,
    system_roots: Vec<Vec<u8>>,
    request: Arc<Request>,
) -> Result<Arc<PlaybackSession>> {
    if manifest.len() > 2 * 1024 * 1024 || entries.len() > 2 * 1024 * 1024 {
        return Err(SourceFailure::Unsupported);
    }
    let manifest: Value =
        serde_json::from_str(&manifest).map_err(|_| SourceFailure::Unsupported)?;
    let entries: Value = serde_json::from_str(&entries).map_err(|_| SourceFailure::Unsupported)?;
    if manifest["key"].as_str() != Some(&source) {
        return Err(SourceFailure::Unsupported);
    }
    let sessions = Arc::new(Sessions::new(std::path::Path::new(&cache_directory)));
    let host = RouteHost::open(cache_directory, sessions, proxy, system_roots)?;
    let live = manifest["type"] == "video" && manifest["category"] == "live";
    let lines = host.lines(source, &manifest, &entries, live, None, &request)?;
    let selected = lines
        .grant
        .iter()
        .position(|line| line.preferred)
        .or_else(|| {
            lines
                .grant
                .iter()
                .enumerate()
                .max_by_key(|(i, line)| (line.quality, std::cmp::Reverse(*i)))
                .map(|(i, _)| i)
        })
        .ok_or(SourceFailure::Unsupported)?;
    lines.play(
        i32::try_from(selected).map_err(|_| SourceFailure::Unsupported)?,
        request,
    )
}
