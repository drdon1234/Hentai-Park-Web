use crate::Error;
use image::{GenericImageView, ImageFormat, ImageReader, RgbaImage};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

pub enum PageFile {
    Original(PathBuf),
    Temporary(NamedTempFile),
}
impl PageFile {
    pub fn path(&self) -> &Path {
        match self {
            Self::Original(path) => path,
            Self::Temporary(file) => file.path(),
        }
    }
}
pub struct ImageAsset {
    pub file: PageFile,
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    temporary_bytes: u64,
}
impl ImageAsset {
    pub(crate) fn temporary_bytes(&self) -> u64 {
        self.temporary_bytes
    }
}
fn checked(call: &CancellationToken) -> Result<(), Error> {
    if call.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
fn reader(file: &Path) -> Result<ImageReader<BufReader<File>>, Error> {
    let input = File::open(file).map_err(|_| Error::Unavailable)?;
    ImageReader::new(BufReader::new(input))
        .with_guessed_format()
        .map_err(|_| Error::InvalidImage)
}

pub fn prepare(
    input: PageFile,
    strips: u32,
    crop: bool,
    cache: &Path,
    call: &CancellationToken,
) -> Result<ImageAsset, Error> {
    checked(call)?;
    let stream = reader(input.path())?;
    let format = stream.format().ok_or(Error::InvalidImage)?;
    let (width, height) = stream.into_dimensions().map_err(|_| Error::InvalidImage)?;
    let pixels = u64::from(width) * u64::from(height);
    if pixels == 0 || pixels > 40_000_000 {
        return Err(Error::Limit);
    }
    let mut stream = reader(input.path())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(192 * 1024 * 1024);
    stream.limits(limits);
    let mut decoder = stream.into_decoder().map_err(|_| Error::InvalidImage)?;
    use image::ImageDecoder;
    let animated = match format {
        ImageFormat::Gif => {
            use image::AnimationDecoder;
            let file = BufReader::new(File::open(input.path()).map_err(|_| Error::Unavailable)?);
            let mut frames = image::codecs::gif::GifDecoder::new(file)
                .map_err(|_| Error::InvalidImage)?
                .into_frames();
            frames
                .next()
                .transpose()
                .map_err(|_| Error::InvalidImage)?
                .ok_or(Error::InvalidImage)?;
            frames
                .next()
                .transpose()
                .map_err(|_| Error::InvalidImage)?
                .is_some()
        }
        ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(BufReader::new(
            File::open(input.path()).map_err(|_| Error::Unavailable)?,
        ))
        .map_err(|_| Error::InvalidImage)?
        .has_animation(),
        ImageFormat::Png => image::codecs::png::PngDecoder::new(BufReader::new(
            File::open(input.path()).map_err(|_| Error::Unavailable)?,
        ))
        .map_err(|_| Error::InvalidImage)?
        .is_apng()
        .map_err(|_| Error::InvalidImage)?,
        _ => false,
    };
    let orientation = decoder.orientation().map_err(|_| Error::InvalidImage)?;
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(|_| Error::InvalidImage)?;
    checked(call)?;
    if animated || (strips <= 1 && !crop) {
        let temporary_bytes = if matches!(input, PageFile::Temporary(_)) {
            std::fs::metadata(input.path())
                .map_err(|_| Error::Storage)?
                .len()
        } else {
            0
        };
        return Ok(ImageAsset {
            file: input,
            width,
            height,
            format,
            temporary_bytes,
        });
    }
    image.apply_orientation(orientation);
    let mut rgba = image.into_rgba8();
    let (width, height) = rgba.dimensions();
    if strips > height {
        return Err(Error::InvalidImage);
    }
    if strips > 1 {
        let step = height / strips;
        let rest = height % strips;
        let stride = width as usize * 4;
        let mut output = RgbaImage::new(width, height);
        for i in 0..strips {
            checked(call)?;
            let rows = step + if i == 0 { rest } else { 0 };
            let from = (height - step * (i + 1) - rest) as usize * stride;
            let to = (step * i + if i == 0 { 0 } else { rest }) as usize * stride;
            let size = rows as usize * stride;
            output.as_mut()[to..to + size].copy_from_slice(&rgba.as_raw()[from..from + size]);
        }
        rgba = output;
    }
    if crop {
        if let Some((x, y, w, h)) = content_rect(&rgba, call)? {
            rgba = rgba.view(x, y, w, h).to_image();
        }
    }
    checked(call)?;
    let (out_width, out_height) = rgba.dimensions();
    let mut tmp = NamedTempFile::new_in(cache).map_err(|_| Error::Storage)?;
    {
        let mut stream = BufWriter::new(tmp.as_file_mut());
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut stream, ImageFormat::Png)
            .map_err(|_| Error::Storage)?;
        stream.flush().map_err(|_| Error::Storage)?;
    }
    checked(call)?;
    let temporary_bytes = tmp.as_file().metadata().map_err(|_| Error::Storage)?.len();
    Ok(ImageAsset {
        file: PageFile::Temporary(tmp),
        width: out_width,
        height: out_height,
        format: ImageFormat::Png,
        temporary_bytes,
    })
}

fn content_rect(
    image: &RgbaImage,
    call: &CancellationToken,
) -> Result<Option<(u32, u32, u32, u32)>, Error> {
    let (w, h) = image.dimensions();
    if w < 8 || h < 8 {
        return Ok(None);
    }
    let base = image.get_pixel(0, 0);
    let same = |x, y| {
        image.get_pixel(x, y).0[..3]
            .iter()
            .zip(&base.0[..3])
            .all(|(a, b)| a.abs_diff(*b) <= 24)
    };
    let dx = w.div_ceil(400).clamp(1, 8) as usize;
    let dy = h.div_ceil(400).clamp(1, 8) as usize;
    let row = |y| (0..w).step_by(dx).all(|x| same(x, y));
    let (mut top, mut bottom, mut left, mut right) = (0, h, 0, w);
    while top < h && row(top) {
        if top % 64 == 0 {
            checked(call)?
        }
        top += 1;
    }
    if top == h {
        return Ok(None);
    }
    while bottom > top && row(bottom - 1) {
        if bottom % 64 == 0 {
            checked(call)?
        }
        bottom -= 1;
    }
    let col = |x| (top..bottom).step_by(dy).all(|y| same(x, y));
    while left < w && col(left) {
        left += 1;
    }
    while right > left && col(right - 1) {
        right -= 1;
    }
    let padx = (w + 100) / 200;
    let pady = (h + 100) / 200;
    left = left.saturating_sub(padx);
    top = top.saturating_sub(pady);
    right = (right + padx).min(w);
    bottom = (bottom + pady).min(h);
    if (right - left) * 5 < w * 2
        || (bottom - top) * 5 < h * 2
        || (left * 100 < w && top * 100 < h && (w - right) * 100 < w && (h - bottom) * 100 < h)
    {
        return Ok(None);
    }
    Ok(Some((left, top, right - left, bottom - top)))
}
