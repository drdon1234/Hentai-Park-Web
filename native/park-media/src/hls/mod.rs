mod access_units;
mod fetch;
mod payload;
pub use fetch::HlsClient;
mod playback;
mod local;
mod live;
pub mod recording;
mod playlist;
pub mod remux;
mod windows;
use crate::Error;
pub use payload::{decode, unwrap, Kind};
pub use playlist::{parse, Encryption, MediaList, Playlist, Segment, Variant};

pub const MANIFEST_LIMIT: usize = 2 * 1024 * 1024;
pub const SEGMENT_LIMIT: usize = 32 * 1024 * 1024;
type Result<T> = std::result::Result<T, Error>;
