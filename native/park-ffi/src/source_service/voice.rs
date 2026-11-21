use super::*;
use crate::ProgressStore;
use park_domain::subtitles;

#[derive(uniffi::Record)]
pub struct VoiceCue {
    pub start_ms: i64,
    pub end_ms: Option<i64>,
    pub text: String,
}
fn parse(bytes: &[u8], format: &str) -> Result<Vec<VoiceCue>> {
    let text = std::str::from_utf8(bytes).map_err(|_| SourceFailure::Unsupported)?;
    let cues = subtitles::parse(text, format);
    if cues.is_empty() {
        return Err(SourceFailure::Unsupported);
    }
    Ok(cues
        .into_iter()
        .map(|c| VoiceCue {
            start_ms: c.start_ms,
            end_ms: c.end_ms,
            text: c.text,
        })
        .collect())
}
#[uniffi::export]
impl SourceService {
    pub fn voice_subtitles(
        &self,
        source: String,
        work: String,
        node: String,
        request: Arc<Request>,
    ) -> Result<Vec<VoiceCue>> {
        let (manifest, result) = self.catalog.invoke(
            &source,
            "voiceResource",
            &[work.into(), node.into()],
            plugins::budget(&request),
        )?;
        let value = downloads::checked(result)?;
        let format = value["format"]
            .as_str()
            .filter(|s| matches!(*s, "vtt" | "srt" | "lrc"))
            .ok_or(SourceFailure::Unsupported)?;
        let line = VideoSource::from_plugin(&manifest, &value)?;
        let session = self
            .routes
            .sessions
            .partition(&source, Purpose::Media)
            .map_err(|_| SourceFailure::Storage)?;
        let bytes = self
            .routes
            .runtime
            .block_on(park_media::download::attachment(
                &self.routes.transport,
                &session,
                &line,
                2 * 1024 * 1024,
                &request.cancelled,
            ))?;
        parse(&bytes, format)
    }
}
#[uniffi::export]
impl ProgressStore {
    pub fn offline_subtitles(&self, key: crate::ChapterKey, node: String) -> Result<Vec<VoiceCue>> {
        use std::io::Read;
        let asset = self
            .bundle_file(key, node)
            .map_err(|_| SourceFailure::Storage)?;
        let path = std::path::Path::new(&asset.path);
        let format = path
            .extension()
            .and_then(|s| s.to_str())
            .ok_or(SourceFailure::Unsupported)?;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .and_then(|f| f.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes))
            .map_err(|_| SourceFailure::Storage)?;
        if bytes.len() > 2 * 1024 * 1024 {
            return Err(SourceFailure::Unsupported);
        }
        parse(&bytes, format)
    }
}
