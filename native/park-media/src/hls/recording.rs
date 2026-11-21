use super::{Error, HlsClient, Kind, Result, Segment};
use crate::download::atomic;
use std::{
    fmt::Write,
    fs,
    path::Path,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct Capture {
    last: Option<u64>,
    volume: usize,
    entries: Vec<(f64, &'static str)>,
    initialization: Option<Segment>,
    seconds: f64,
    bytes: u64,
    boundary: bool,
}
impl Capture {
    /// 每次提交都生成以 ENDLIST 收尾的本地卷，网络凭据及源地址不进入清单。
    pub async fn follow(
        &mut self,
        client: &HlsClient,
        dir: &Path,
        token: &CancellationToken,
        mut committed: impl FnMut(f64, u64) -> Result<()>,
    ) -> Result<()> {
        if client.audio_url().is_some() {
            return Err(Error::Unavailable);
        }
        let url = client.video_url();
        let mut resumed = self.last.is_some();
        self.boundary = resumed;
        let mut last_advance = Instant::now();
        loop {
            let list = client.media(url, true, token).await?;
            let tail = list.segments.last().ok_or(Error::Unavailable)?.sequence;
            if self.last.is_none() || resumed && self.last.is_some_and(|last| tail < last) {
                self.last = Some(tail);
            } else {
                for seg in list.segments {
                    if self.last.is_some_and(|n| seg.sequence <= n) {
                        continue;
                    }
                    let data = client.segment(url, &seg, true, token).await?;
                    if token.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    let changed = self
                        .initialization
                        .as_ref()
                        .map(|i| (&i.url, &i.encryption))
                        != list
                            .initialization
                            .as_ref()
                            .map(|i| (&i.url, &i.encryption));
                    if !self.entries.is_empty()
                        && (self.entries.len() >= 20000
                            || self.boundary
                            || changed
                            || seg.discontinuity
                            || self.last.and_then(|i| i.checked_add(1)) != Some(seg.sequence))
                    {
                        self.volume += 1;
                        self.entries.clear();
                    }
                    let name = format!("{}.m3u8", self.volume);
                    let folder_name = format!("{name}.part.hls");
                    let folder = dir.join(&folder_name);
                    if self.entries.is_empty() {
                        match fs::create_dir(&folder) {
                            Ok(()) => (),
                            Err(e)
                                if e.kind() == std::io::ErrorKind::AlreadyExists
                                    && fs::symlink_metadata(&folder).is_ok_and(|m| m.is_dir()) =>
                            {
                                ()
                            }
                            _ => return Err(Error::Storage),
                        }
                        if let Some(init) = &list.initialization {
                            let header = client.segment(url, init, true, token).await?;
                            if token.is_cancelled() {
                                return Err(Error::Cancelled);
                            }
                            atomic(&folder, "init.mp4", &header)?;
                            self.bytes += header.len() as u64;
                        }
                        self.initialization = list.initialization.clone();
                    }
                    let ext = if seg.kind == Kind::TransportStream {
                        "ts"
                    } else {
                        "m4s"
                    };
                    atomic(&folder, &format!("{}.{ext}", self.entries.len()), &data)?;
                    self.entries.push((seg.seconds, ext));
                    let target = self
                        .entries
                        .iter()
                        .map(|(seconds, _)| seconds.ceil() as u64)
                        .max()
                        .unwrap_or(1)
                        .max(1);
                    let version = if self.initialization.is_some() { 6 } else { 3 };
                    let mut text=format!("#EXTM3U\n#EXT-X-VERSION:{version}\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n");
                    if self.initialization.is_some() {
                        writeln!(text, "#EXT-X-MAP:URI=\"{folder_name}/init.mp4\"").unwrap()
                    }
                    for (index, (seconds, ext)) in self.entries.iter().enumerate() {
                        writeln!(text, "#EXTINF:{seconds},\n{folder_name}/{index}.{ext}").unwrap()
                    }
                    text.push_str("#EXT-X-ENDLIST\n");
                    atomic(dir, &name, text.as_bytes())?;
                    self.last = Some(seg.sequence);
                    self.bytes += data.len() as u64;
                    self.seconds += seg.seconds;
                    self.boundary = false;
                    last_advance = Instant::now();
                    committed(self.seconds, self.bytes)?;
                }
            }
            resumed = false;
            if list.ended {
                return Ok(());
            }
            if last_advance.elapsed() > Duration::from_secs(60) {
                return Err(Error::Unavailable);
            }
            let ms = u64::from(list.target_seconds.unwrap_or(1))
                .saturating_mul(500)
                .clamp(500, 10000);
            tokio::select! {biased;_=token.cancelled()=>return Err(Error::Cancelled),_=tokio::time::sleep(Duration::from_millis(ms))=>()}
        }
    }
}
