mod direct;
pub mod download;
mod video_source;
pub use video_source::VideoSource;
pub mod exports;
pub mod hls;
mod images;
pub mod image_disk;
mod loopback;
pub use loopback::PlaybackRoute;
mod remote_image;
mod schedule;
pub use remote_image::{RemoteImages, ResolvedImage};

pub use images::{prepare, ImageAsset, PageFile};
pub use schedule::{ImageKey, ImagePool, ImageTicket, Priority, Route};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Cancelled,
    InvalidImage,
    Unavailable,
    Limit,
    Storage,
}
