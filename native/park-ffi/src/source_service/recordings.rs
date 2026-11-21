use super::*;
use crate::{BundleFile, ProgressStore};
use park_media::hls::{self, HlsClient, Playlist};
use park_storage::recordings as storage;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(uniffi::Object)]
pub struct LiveRecording {
    recording: storage::Recording,
    source: String,
    work: String,
    used: AtomicBool,
}
#[uniffi::export]
impl SourceService {
    pub fn create_recording(
        &self,
        store: Arc<ProgressStore>,
        source: String,
        work: String,
        title: String,
        default_directory: String,
        request: Arc<Request>,
    ) -> Result<Arc<LiveRecording>> {
        let manifest = self.catalog.manifest(&source, plugins::budget(&request))?;
        if manifest["category"] != "live"
            || manifest["type"] != "video"
            || manifest["capabilities"]["videos"] != true
        {
            return Err(SourceFailure::Unsupported);
        }
        let cfg = library_updates::settings(&store)?;
        let dir = cfg["downloadDirectory"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(&default_directory);
        let (id, started) = {
            let db = store.lock().map_err(|_| SourceFailure::Storage)?;
            db.query_row("SELECT lower(hex(randomblob(4))||'-'||hex(randomblob(2))||'-4'||substr(hex(randomblob(2)),2)||'-8'||substr(hex(randomblob(2)),2)||'-'||hex(randomblob(6))),strftime('%Y-%m-%dT%H:%M:%fZ','now')",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).map_err(|_|SourceFailure::Storage)?
        };
        if request.cancelled.is_cancelled() {
            return Err(SourceFailure::Cancelled);
        }
        let recording = storage::Recording::create(
            &store.root,
            Path::new(dir),
            &id,
            &source,
            &work,
            &title,
            &started,
        )
        .map_err(|_| SourceFailure::Storage)?;
        Ok(Arc::new(LiveRecording {
            recording,
            source,
            work,
            used: AtomicBool::new(false),
        }))
    }
}
#[uniffi::export]
impl LiveRecording {
    pub fn id(&self) -> String {
        self.recording.id.clone()
    }
    pub fn capture(&self, service: Arc<SourceService>, request: Arc<Request>) -> Result<()> {
        if self.used.swap(true, Ordering::SeqCst) {
            return Err(SourceFailure::Unsupported);
        }
        let mut capture = hls::recording::Capture::default();
        let (mut seconds, mut bytes, mut failures) = (0., 0, 0);
        let result = loop {
            if request.cancelled.is_cancelled() {
                break Ok(());
            }
            let previous = bytes;
            let run = (|| -> Result<()> {
                let lines = service.video_lines(
                    self.source.clone(),
                    self.work.clone(),
                    "_".into(),
                    request.clone(),
                )?;
                let Some(line) = lines
                    .grant
                    .iter()
                    .find(|s| s.hls && s.preferred)
                    .or_else(|| lines.grant.iter().find(|s| s.hls))
                else {
                    return Err(SourceFailure::Unsupported);
                };
                if !lines.live {
                    return Err(SourceFailure::Unsupported);
                }
                let session = service
                    .routes
                    .sessions
                    .partition(&self.source, Purpose::Media)
                    .map_err(|_| SourceFailure::Storage)?;
                let client =
                    HlsClient::new(service.routes.transport.clone(), session, line.clone())?;
                if client.audio_url().is_some(){return Err(SourceFailure::Unsupported)}
                service.routes.runtime.block_on(capture.follow(
                    &client,
                    &self.recording.directory,
                    &request.cancelled,
                    |s, b| {
                        seconds = s;
                        bytes = b;
                        self.recording
                            .update(s, b, "recording")
                            .map_err(|_| park_media::Error::Storage)
                    },
                ))?;
                Ok(())
            })();
            if request.cancelled.is_cancelled() {
                break Ok(());
            }
            if run.is_ok() || !matches!(run, Err(SourceFailure::Unavailable)) {
                break run;
            }
            if bytes > previous {
                failures = 0
            }
            failures += 1;
            if failures > 5 {
                break run;
            }
            service.routes.runtime.block_on(async{tokio::select!{_=request.cancelled.cancelled()=>(),_=tokio::time::sleep(Duration::from_secs(failures*2))=>()}});
        };
        self.recording
            .update(
                seconds,
                bytes,
                if result.is_ok() { "finished" } else { "failed" },
            )
            .map_err(|_| SourceFailure::Storage)?;
        result
    }
}
#[uniffi::export]
impl ProgressStore {
    pub fn delete_recording(&self,id:String)->Result<()>{
        storage::delete(&self.root,&id).map_err(|_|SourceFailure::Storage)
    }
    pub fn live_recordings(&self) -> Result<String> {
        let rows = storage::view(&self.root, |dir, volumes| {
            let mut seconds = 0.;
            let mut bytes = 0;
            for i in volumes {
                let name = format!("{i}.m3u8");
                let invalid = |_| park_storage::backup::Error::Invalid;
                let files = crate::offline::local_playlist(dir, &name).map_err(invalid)?;
                for path in files {
                    bytes += std::fs::metadata(path)?.len()
                }
                let text = std::fs::read(dir.join(name))?;
                let base = Url::parse("https://offline.park.invalid/index.m3u8").unwrap();
                if let Playlist::Media(list) = hls::parse(&text, &base, false)
                    .map_err(|_| park_storage::backup::Error::Invalid)?
                {
                    seconds += list.segments.iter().map(|s| s.seconds).sum::<f64>()
                }
            }
            Ok((seconds, bytes))
        })
        .map_err(|_| SourceFailure::Storage)?;
        serde_json::to_string(&rows).map_err(|_| SourceFailure::Storage)
    }
    pub fn recording_volume(&self, id: String, volume: u32) -> Result<BundleFile> {
        let dir = storage::directory(&self.root, &id).map_err(|_| SourceFailure::Storage)?;
        let name = format!("{volume}.m3u8");
        let resources =
            crate::offline::local_playlist(&dir, &name).map_err(|_| SourceFailure::Storage)?;
        let path = crate::offline::asset(&dir, &name)
            .map_err(|_| SourceFailure::Storage)?
            .into_os_string()
            .into_string()
            .map_err(|_| SourceFailure::Storage)?;
        Ok(BundleFile { path, resources })
    }

    pub fn play_recording_volume(
        &self,
        id: String,
        volume: u32,
        request: Arc<Request>,
    ) -> Result<Arc<PlaybackSession>> {
        request.check().map_err(|_| SourceFailure::Cancelled)?;
        offline_route(self.recording_volume(id, volume)?, &request)
    }
}
