use super::{Error, Result};
use axum::body::Bytes;
use broadcast_common::Package;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use transmux::{ir::CodecConfig, CmafMux, Media, ProgressiveMux, TsDemux};

// 转封装器仅接收数据面已验证的 TS 字节，没有文件路径或联网入口。
pub struct Window {
    pub initialization: Bytes,
    pub media: Bytes,
}
#[derive(Clone)]
pub struct Bootstrap {
    bytes: Arc<[u8]>,
    origin: i64,
    counts: Vec<(u32, usize)>,
}
fn demux(input: &[u8]) -> Result<Media> {
    if input.len() < 376 || !input.len().is_multiple_of(188) {
        return Err(Error::Unavailable);
    }
    let aligned = super::access_units::aligned(input)?;
    let mut media = TsDemux::new()
        .demux(&aligned)
        .map_err(|_| Error::Unavailable)?;
    media.tracks.retain(|t| {
        matches!(
            t.spec.config,
            CodecConfig::Avc { .. } | CodecConfig::Hevc { .. } | CodecConfig::Aac { .. }
        ) && !t.samples.is_empty()
    });
    if media.tracks.is_empty() {
        return Err(Error::Unavailable);
    }
    Ok(media)
}
fn timestamp_90k(track: &transmux::Track) -> Result<i64> {
    let dts = track
        .samples
        .first()
        .and_then(|s| s.timing?.dts())
        .ok_or(Error::Unavailable)?;
    i64::try_from(i128::from(dts) * 90000 / i128::from(track.spec.timescale.get()))
        .map_err(|_| Error::Limit)
}
fn origin(media: &Media) -> Result<i64> {
    media
        .tracks
        .iter()
        .map(timestamp_90k)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .min()
        .ok_or(Error::Unavailable)
}
fn check(token: &CancellationToken) -> Result<()> {
    if token.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
impl Bootstrap {
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        let media = demux(&bytes)?;
        Ok(Self {
            origin: origin(&media)?,
            counts: media
                .tracks
                .iter()
                .map(|t| (t.spec.track_id.value(), t.samples.len()))
                .collect(),
            bytes: bytes.into(),
        })
    }
    pub fn first(&self) -> &[u8] {
        &self.bytes
    }
    fn with_prefix(&self, bytes: &[u8]) -> Result<Media> {
        let mut input = Vec::with_capacity(self.bytes.len() + bytes.len());
        input.extend_from_slice(&self.bytes);
        input.extend_from_slice(bytes);
        let mut media = demux(&input)?;
        for track in &mut media.tracks {
            let count = self
                .counts
                .iter()
                .find(|(id, _)| *id == track.spec.track_id.value())
                .map_or(0, |(_, n)| *n);
            if count >= track.samples.len() {
                return Err(Error::Unavailable);
            }
            track.samples.drain(..count);
        }
        Ok(media)
    }
    pub fn window(
        &self,
        index: usize,
        bytes: &[u8],
        nominal_seconds: f64,
        epoch_start_seconds: f64,
        cancel: &CancellationToken,
    ) -> Result<Window> {
        check(cancel)?;
        if !nominal_seconds.is_finite()
            || nominal_seconds < 0.
            || nominal_seconds > i64::MAX as f64 / 90000.
            || !epoch_start_seconds.is_finite()
            || !(0. ..=nominal_seconds).contains(&epoch_start_seconds)
        {
            return Err(Error::Unavailable);
        }
        let mut media = match demux(bytes) {
            Ok(m) if m.tracks.len() >= self.counts.len() => m,
            _ if index > 0 => self.with_prefix(bytes)?,
            other => other?,
        };
        check(cancel)?;
        let start = origin(&media)?;
        let nominal = ((nominal_seconds - epoch_start_seconds) * 90000.).round() as i128;
        let wrap = 1i128 << 33;
        let relative = (i128::from(start) - i128::from(self.origin)).rem_euclid(wrap);
        let aligned = nominal + (relative - nominal + wrap / 2).rem_euclid(wrap) - wrap / 2;
        let base = (epoch_start_seconds * 90000.).round() as i128 + aligned;
        for t in &mut media.tracks {
            let first = timestamp_90k(t)?;
            let ticks = (i128::from(first) - i128::from(start) + base).max(0)
                * i128::from(t.spec.timescale.get())
                / 90000;
            t.start_decode_time = ticks.try_into().map_err(|_| Error::Limit)?;
        }
        let sequence = u32::try_from(index + 1).map_err(|_| Error::Limit)?;
        let output = CmafMux::new(sequence)
            .package(&media)
            .map_err(|_| Error::Unavailable)?;
        check(cancel)?;
        split(output)
    }
}
fn split(output: Vec<u8>) -> Result<Window> {
    let buffer = Bytes::from(output);
    let mut cursor = 0;
    while cursor + 8 <= buffer.len() {
        let size = u32::from_be_bytes(buffer[cursor..cursor + 4].try_into().unwrap()) as usize;
        if size < 8 || size > buffer.len() - cursor {
            return Err(Error::Unavailable);
        }
        if &buffer[cursor + 4..cursor + 8] == b"moof" && cursor > 0 {
            return Ok(Window {
                initialization: buffer.slice(..cursor),
                media: buffer.slice(cursor..),
            });
        }
        cursor += size;
    }
    Err(Error::Unavailable)
}
pub fn complete_mp4(ts: &[u8], cancel: &CancellationToken) -> Result<Vec<u8>> {
    check(cancel)?;
    let media = demux(ts)?;
    check(cancel)?;
    let output = ProgressiveMux::new(true)
        .package(&media)
        .map_err(|_| Error::Unavailable)?;
    check(cancel)?;
    Ok(output)
}
