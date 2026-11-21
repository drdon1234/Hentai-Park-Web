use crate::{Error, ImageAsset};
use flate2::{write::ZlibEncoder, Compression};
use image::{ImageFormat, ImageReader};
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref, TextStr};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::NamedTempFile;
use zip::{write::SimpleFileOptions, ZipWriter};

pub struct ComicInfo {
    pub title: String,
    pub series: String,
    pub writer: String,
    pub tags: String,
    pub web: String,
}
impl ComicInfo {
    fn xml(&self, count: usize) -> String {
        let mut xml=String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<ComicInfo xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n");
        for (tag, value) in [
            ("Title", &self.title),
            ("Series", &self.series),
            ("Writer", &self.writer),
            ("Tags", &self.tags),
            ("Web", &self.web),
        ] {
            if value.is_empty() {
                continue;
            }
            let value = value
                .chars()
                .filter(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
                .collect::<String>();
            let escaped = value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;");
            xml.push_str(&format!("  <{tag}>{escaped}</{tag}>\n"));
        }
        xml.push_str(&format!("  <PageCount>{count}</PageCount>\n</ComicInfo>\n"));
        xml
    }
}
pub fn filename(value: &str) -> String {
    let clean: String = value
        .chars()
        .map(|c| {
            if c.is_control() || "\\/:*?\"<>|".contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let short: String = clean
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect();
    let name = short.trim_end_matches(['.', ' ']);
    if name.is_empty() {
        return String::from("untitled");
    }
    let base = name.split('.').next().unwrap().to_ascii_uppercase();
    let device = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            base.strip_prefix(prefix)
                .is_some_and(|v| v.chars().count() == 1 && "123456789¹²³".contains(v))
        });
    if device {
        format!("_{name}")
    } else {
        name.to_owned()
    }
}
pub struct Staged {
    file: NamedTempFile,
    name: String,
    extension: &'static str,
}
impl Staged {
    fn new(dir: &Path, name: String, ext: &'static str) -> Result<Self, Error> {
        Ok(Self {
            file: NamedTempFile::new_in(dir).map_err(|_| Error::Storage)?,
            name,
            extension: ext,
        })
    }
}
/// 所有卷写完并刷盘后才公开。保留同名文件，并在失败或取消后清理本次已生成的卷。
pub fn publish(
    dir: &Path,
    files: Vec<Staged>,
    check: &impl Fn() -> Result<(), Error>,
) -> Result<Vec<PathBuf>, Error> {
    let mut published = Vec::new();
    let outcome = (|| {
        for staged in files {
            let mut temp = staged.file;
            temp.as_file().sync_all().map_err(|_| Error::Storage)?;
            for n in 0u64.. {
                check()?;
                let suffix = if n == 0 {
                    String::new()
                } else {
                    format!(" ({})", n + 1)
                };
                let target = dir.join(format!("{}{suffix}.{}", staged.name, staged.extension));
                match temp.persist_noclobber(&target) {
                    Ok(_) => {
                        published.push(target);
                        break;
                    }
                    Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => temp = e.file,
                    Err(_) => return Err(Error::Storage),
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = outcome {
        for path in published.iter().rev() {
            std::fs::remove_file(path).map_err(|_| Error::Storage)?;
        }
        return Err(error);
    }
    Ok(published)
}

pub fn cbz(
    dir: &Path,
    basename: &str,
    info: &ComicInfo,
    pages: &[Arc<ImageAsset>],
    check: &impl Fn() -> Result<(), Error>,
) -> Result<Staged, Error> {
    let mut staged = Staged::new(dir, basename.to_owned(), "cbz")?;
    let mut archive = ZipWriter::new(staged.file.as_file_mut());
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (i, page) in pages.iter().enumerate() {
        check()?;
        let extension = match page.format {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
            ImageFormat::Gif => "gif",
            ImageFormat::WebP => "webp",
            ImageFormat::Bmp => "bmp",
            _ => return Err(Error::InvalidImage),
        };
        archive
            .start_file(format!("{:04}.{extension}", i + 1), options)
            .map_err(|_| Error::Storage)?;
        let mut input = File::open(page.file.path()).map_err(|_| Error::Unavailable)?;
        let mut buffer = [0; 65536];
        loop {
            check()?;
            let n = input.read(&mut buffer).map_err(|_| Error::Unavailable)?;
            if n == 0 {
                break;
            }
            archive
                .write_all(&buffer[..n])
                .map_err(|_| Error::Storage)?;
        }
    }
    archive
        .start_file("ComicInfo.xml", options)
        .map_err(|_| Error::Storage)?;
    archive
        .write_all(info.xml(pages.len()).as_bytes())
        .map_err(|_| Error::Storage)?;
    archive.finish().map_err(|_| Error::Storage)?;
    Ok(staged)
}

fn zlib(data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = ZlibEncoder::new(Vec::new(), Compression::default());
    out.write_all(data)
        .and_then(|_| out.finish())
        .map_err(|_| Error::Storage)
}
// PDF 的 DCTDecode 只能在原件的组件数与声明颜色空间匹配时直接使用。
fn jpeg_components(bytes: &[u8]) -> Option<u8> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut offset = 2;
    while bytes.get(offset) == Some(&0xff) {
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        if matches!(marker, 0xda | 0xd9) {
            return None;
        }
        let length = u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        let block = bytes.get(offset..offset + length)?;
        if matches!(marker,0xc0..=0xc3|0xc5..=0xc7|0xc9..=0xcb|0xcd..=0xcf)
            && block.get(2) == Some(&8)
        {
            return block.get(7).copied();
        }
        offset += length;
    }
    None
}
fn embed(pdf: &mut Pdf, base: i32, asset: &ImageAsset) -> Result<(), Error> {
    use image::ImageDecoder;
    let input = BufReader::new(File::open(asset.file.path()).map_err(|_| Error::Unavailable)?);
    let mut decoder = ImageReader::with_format(input, asset.format)
        .into_decoder()
        .map_err(|_| Error::InvalidImage)?;
    let orientation = decoder.orientation().map_err(|_| Error::InvalidImage)?;
    let original = if asset.format == ImageFormat::Jpeg
        && orientation == image::metadata::Orientation::NoTransforms
    {
        let bytes = std::fs::read(asset.file.path()).map_err(|_| Error::Unavailable)?;
        match jpeg_components(&bytes) {
            Some(n @ (1 | 3)) => Some((bytes, n)),
            _ => None,
        }
    } else {
        None
    };
    let (width, height, pixels, mask, filter, gray) = if let Some((bytes, n)) = original {
        let (w, h) = decoder.dimensions();
        (w, h, bytes, None, Filter::DctDecode, n == 1)
    } else {
        let mut image =
            image::DynamicImage::from_decoder(decoder).map_err(|_| Error::InvalidImage)?;
        image.apply_orientation(orientation);
        let (w, h) = (image.width(), image.height());
        let rgb = zlib(image.to_rgb8().as_raw())?;
        let mask = if image.color().has_alpha() {
            let alpha: Vec<_> = image.into_rgba8().pixels().map(|p| p.0[3]).collect();
            if alpha.iter().all(|v| *v == 255) {
                None
            } else {
                Some(zlib(&alpha)?)
            }
        } else {
            None
        };
        (w, h, rgb, mask, Filter::FlateDecode, false)
    };
    let (page_id, object, mask_id, content_id) = (
        Ref::new(base),
        Ref::new(base + 1),
        Ref::new(base + 2),
        Ref::new(base + 3),
    );
    let mut xobject = pdf.image_xobject(object, &pixels);
    xobject
        .width(width as i32)
        .height(height as i32)
        .bits_per_component(8)
        .filter(filter);
    if gray {
        xobject.color_space().device_gray();
    } else {
        xobject.color_space().device_rgb();
    }
    if mask.is_some() {
        xobject.s_mask(mask_id);
    }
    xobject.finish();
    if let Some(bytes) = mask {
        let mut alpha = pdf.image_xobject(mask_id, &bytes);
        alpha
            .width(width as i32)
            .height(height as i32)
            .bits_per_component(8)
            .filter(Filter::FlateDecode);
        alpha.color_space().device_gray();
        alpha.finish();
    }
    let w = 210.0 * 72.0 / 25.4;
    let h = w * height as f32 / width as f32;
    let mut page = pdf.page(page_id);
    page.parent(Ref::new(2))
        .media_box(Rect::new(0., 0., w, h))
        .contents(content_id);
    let name = Name(b"Page");
    page.resources().x_objects().pair(name, object);
    page.finish();
    let mut content = Content::new();
    content
        .save_state()
        .transform([w, 0., 0., h, 0., 0.])
        .x_object(name)
        .restore_state();
    pdf.stream(content_id, &content.finish());
    Ok(())
}
pub fn pdf_volumes(
    dir: &Path,
    basename: &str,
    title: &str,
    pages: &[Arc<ImageAsset>],
    max_pages: usize,
    part: impl Fn(usize) -> String,
    check: &impl Fn() -> Result<(), Error>,
) -> Result<Vec<Staged>, Error> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut size = 0;
    for (i, page) in pages.iter().enumerate() {
        let bytes = std::fs::metadata(page.file.path())
            .map_err(|_| Error::Unavailable)?
            .len();
        if i > start
            && ((max_pages > 0 && i - start >= max_pages) || size + bytes > 160 * 1024 * 1024)
        {
            ranges.push(start..i);
            start = i;
            size = 0;
        }
        size += bytes;
    }
    ranges.push(start..pages.len());
    let mut files = Vec::new();
    for (volume, range) in ranges.iter().enumerate() {
        let suffix = if ranges.len() > 1 {
            format!(" ({})", part(volume + 1))
        } else {
            String::new()
        };
        let mut pdf = Pdf::new();
        let count = range.len();
        pdf.catalog(Ref::new(1)).pages(Ref::new(2));
        pdf.pages(Ref::new(2))
            .kids((0..count).map(|i| Ref::new(4 + 4 * i as i32)))
            .count(count as i32);
        pdf.document_info(Ref::new(3))
            .title(TextStr(&format!("{title}{suffix}")))
            .creator(TextStr("HentaiPark"));
        for (i, page) in pages[range.clone()].iter().enumerate() {
            check()?;
            embed(&mut pdf, 4 + 4 * i as i32, page)?;
        }
        let mut staged = Staged::new(dir, format!("{basename}{suffix}"), "pdf")?;
        staged
            .file
            .write_all(&pdf.finish())
            .map_err(|_| Error::Storage)?;
        files.push(staged);
    }
    Ok(files)
}
