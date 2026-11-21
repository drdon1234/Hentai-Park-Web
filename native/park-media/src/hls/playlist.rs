use super::{unwrap, Error, Kind, Result, MANIFEST_LIMIT};
use std::collections::{BTreeMap, HashMap};
use url::Url;

#[derive(Clone, Debug, PartialEq)]
pub struct Encryption {
    pub url: Url,
    pub iv: [u8; 16],
}
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub url: Url,
    pub seconds: f64,
    pub sequence: u64,
    pub kind: Kind,
    pub encryption: Option<Encryption>,
    pub discontinuity: bool,
}
#[derive(Clone, Debug)]
pub struct MediaList {
    pub initialization: Option<Segment>,
    pub segments: Vec<Segment>,
    pub ended: bool,
    pub target_seconds: Option<u32>,
}
#[derive(Clone, Debug)]
pub struct Variant {
    pub url: Url,
    pub height: u32,
    pub bandwidth: u64,
    pub audio: Option<Url>,
}
#[derive(Clone, Debug)]
pub enum Playlist {
    Master(Vec<Variant>),
    Media(MediaList),
}

fn attributes(mut text: &str) -> Result<BTreeMap<&str, &str>> {
    let mut result = BTreeMap::new();
    loop {
        let (name, rest) = text.split_once('=').ok_or(Error::Unavailable)?;
        if name.is_empty()
            || name
                .bytes()
                .any(|b| !(b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-'))
        {
            return Err(Error::Unavailable);
        }
        let (value, tail) = if let Some(quoted) = rest.strip_prefix('"') {
            quoted.split_once('"').ok_or(Error::Unavailable)?
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let value = &rest[..end];
            if value.is_empty() || value.bytes().any(|b| b.is_ascii_whitespace() || b == b'"') {
                return Err(Error::Unavailable);
            }
            (value, &rest[end..])
        };
        if result.insert(name, value).is_some() {
            return Err(Error::Unavailable);
        }
        if tail.is_empty() {
            break;
        }
        text = tail.strip_prefix(',').ok_or(Error::Unavailable)?;
    }
    Ok(result)
}
fn resolve(base: &Url, path: &str) -> Result<Url> {
    if path.is_empty() || path.contains("{$") {
        return Err(Error::Unavailable);
    }
    let url = base.join(path).map_err(|_| Error::Unavailable)?;
    if !park_net::public_https(&url) {
        return Err(Error::Unavailable);
    }
    Ok(url)
}
pub fn parse(bytes: &[u8], base: &Url, live: bool) -> Result<Playlist> {
    let unwrapped = unwrap(bytes, MANIFEST_LIMIT)?;
    let text = std::str::from_utf8(&unwrapped).map_err(|_| Error::Unavailable)?;
    let mut lines: Vec<_> = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.first() != Some(&"#EXTM3U") {
        return Err(Error::Unavailable);
    }
    // RFC 8216 §4.1 将以 # 开头且不以 #EXT 开头的整行定义为注释。
    lines.retain(|line| !line.starts_with('#') || line.starts_with("#EXT"));
    if lines.iter().any(|l| l.starts_with("#EXT-X-STREAM-INF:")) {
        Ok(Playlist::Master(master(&lines, base)?))
    } else {
        Ok(Playlist::Media(media(&lines, base, live)?))
    }
}
fn master(lines: &[&str], base: &Url) -> Result<Vec<Variant>> {
    let mut audio: HashMap<&str, (u8, Option<Url>)> = HashMap::new();
    for (i, raw) in lines
        .iter()
        .filter_map(|line| line.strip_prefix("#EXT-X-MEDIA:"))
        .enumerate()
    {
        if i >= 256 {
            return Err(Error::Limit);
        }
        let attr = attributes(raw)?;
        match attr.get("TYPE").copied() {
            Some("AUDIO") => {
                let group = attr
                    .get("GROUP-ID")
                    .filter(|g| !g.is_empty())
                    .ok_or(Error::Unavailable)?;
                let url = attr.get("URI").map(|v| resolve(base, v)).transpose()?;
                let rank = if attr.get("DEFAULT") == Some(&"YES") {
                    0
                } else if attr.get("AUTOSELECT") == Some(&"YES") {
                    1
                } else {
                    2
                };
                if audio.get(group).is_none_or(|v| rank < v.0) {
                    audio.insert(group, (rank, url));
                }
            }
            Some("SUBTITLES" | "CLOSED-CAPTIONS") => (),
            Some("VIDEO") if !attr.contains_key("URI") => (),
            _ => return Err(Error::Unavailable),
        }
    }
    let mut variants = Vec::new();
    let mut pending = None;
    let mut count = 0;
    for line in lines {
        if let Some(raw) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            count += 1;
            if count > 32 || pending.is_some() {
                return Err(Error::Unavailable);
            }
            let attr = attributes(raw)?;
            let bandwidth = attr
                .get("BANDWIDTH")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0u64);
            let height = if let Some(res) = attr.get("RESOLUTION") {
                let (w, h) = res.split_once('x').ok_or(Error::Unavailable)?;
                if w.parse::<u32>().ok().is_none_or(|v| v == 0) {
                    return Err(Error::Unavailable);
                }
                h.parse::<u32>()
                    .ok()
                    .filter(|h| (1..=4320).contains(h))
                    .ok_or(Error::Unavailable)?
            } else {
                if bandwidth == 0 || bandwidth > 10_000_000_000 {
                    return Err(Error::Unavailable);
                }
                0
            };
            let separate = attr
                .get("AUDIO")
                .map(|g| audio.get(g).map(|e| e.1.clone()).ok_or(Error::Unavailable))
                .transpose()?
                .flatten();
            pending = Some((height, bandwidth, separate));
        } else if !line.starts_with('#') {
            let (height, bandwidth, audio) = pending.take().ok_or(Error::Unavailable)?;
            let url = resolve(base, line)?;
            if !variants
                .iter()
                .any(|v: &Variant| v.height == height && (height > 0 || v.bandwidth == bandwidth))
            {
                variants.push(Variant {
                    url,
                    height,
                    bandwidth,
                    audio,
                });
            }
        } else if line.contains("URI=")
            && ![
                "#EXT-X-MEDIA:",
                "#EXT-X-I-FRAME-STREAM-INF:",
                "#EXT-X-SESSION-DATA:",
            ]
            .iter()
            .any(|s| line.starts_with(s))
        {
            return Err(Error::Unavailable);
        }
    }
    if pending.is_some() || variants.is_empty() {
        return Err(Error::Unavailable);
    }
    variants.sort_by_key(|v| std::cmp::Reverse((v.height, v.bandwidth)));
    Ok(variants)
}
fn media(lines: &[&str], base: &Url, live: bool) -> Result<MediaList> {
    let mut list = MediaList {
        initialization: None,
        segments: Vec::new(),
        ended: false,
        target_seconds: None,
    };
    let mut sequence = None;
    let mut duration = None;
    let mut encryption: Option<(Url, Option<[u8; 16]>)> = None;
    let mut discontinuity = false;
    for &line in lines {
        if list.ended {
            if line
                .strip_prefix("#EXT-X-TOTAL_DURATION:")
                .and_then(|s| s.parse::<f64>().ok())
                .is_some_and(|v| v.is_finite() && v >= 0.)
            {
                continue;
            }
            return Err(Error::Unavailable);
        }
        if let Some(raw) = line.strip_prefix("#EXT-X-KEY:") {
            let attr = attributes(raw)?;
            encryption = match attr.get("METHOD").copied() {
                Some("NONE") if attr.len() == 1 => None,
                Some("AES-128")
                    if attr.keys().all(|k| {
                        matches!(
                            *k,
                            "METHOD" | "URI" | "IV" | "KEYFORMAT" | "KEYFORMATVERSIONS"
                        )
                    }) && attr.get("KEYFORMAT").is_none_or(|v| *v == "identity")
                        && attr.get("KEYFORMATVERSIONS").is_none_or(|v| *v == "1") =>
                {
                    let key = resolve(base, attr.get("URI").ok_or(Error::Unavailable)?)?;
                    let iv = attr
                        .get("IV")
                        .map(|v| {
                            let hex = v
                                .strip_prefix("0x")
                                .or_else(|| v.strip_prefix("0X"))
                                .filter(|s| {
                                    !s.is_empty()
                                        && s.len() <= 32
                                        && s.bytes().all(|b| b.is_ascii_hexdigit())
                                })
                                .ok_or(Error::Unavailable)?;
                            Ok(u128::from_str_radix(hex, 16)
                                .map_err(|_| Error::Unavailable)?
                                .to_be_bytes())
                        })
                        .transpose()?;
                    Some((key, iv))
                }
                _ => return Err(Error::Unavailable),
            };
        } else if let Some(raw) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            if sequence.is_some()
                || !list.segments.is_empty()
                || duration.is_some()
                || !raw.bytes().all(|c| c.is_ascii_digit())
            {
                return Err(Error::Unavailable);
            }
            sequence = Some(raw.parse::<u64>().map_err(|_| Error::Unavailable)?);
        } else if let Some(raw) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            let value = raw.parse().map_err(|_| Error::Unavailable)?;
            if list.target_seconds.replace(value).is_some() {
                return Err(Error::Unavailable);
            }
        } else if let Some(raw) = line.strip_prefix("#EXT-X-MAP:") {
            let attr = attributes(raw)?;
            if attr.len() != 1
                || list.initialization.is_some()
                || !list.segments.is_empty()
                || duration.is_some()
            {
                return Err(Error::Unavailable);
            }
            let url = resolve(base, attr.get("URI").ok_or(Error::Unavailable)?)?;
            let encryption = encryption
                .as_ref()
                .map(|(url, iv)| {
                    Ok(Encryption {
                        url: url.clone(),
                        iv: iv.ok_or(Error::Unavailable)?,
                    })
                })
                .transpose()?;
            list.initialization = Some(Segment {
                url,
                seconds: 0.,
                sequence: 0,
                encryption,
                kind: Kind::Init,
                discontinuity: false,
            });
        } else if let Some(raw) = line.strip_prefix("#EXTINF:") {
            let seconds = raw
                .split(',')
                .next()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|n| n.is_finite() && *n > 0.)
                .ok_or(Error::Unavailable)?;
            if duration.replace(seconds).is_some() {
                return Err(Error::Unavailable);
            }
        } else if line == "#EXT-X-ENDLIST" {
            list.ended = true;
        } else if line == "#EXT-X-DISCONTINUITY" {
            discontinuity = true;
        } else if !line.starts_with('#') {
            let url = resolve(base, line)?;
            let seconds = duration.take().ok_or(Error::Unavailable)?;
            let index = sequence
                .unwrap_or(0)
                .checked_add(list.segments.len() as u64)
                .ok_or(Error::Unavailable)?;
            let encryption = encryption.as_ref().map(|(url, iv)| Encryption {
                url: url.clone(),
                iv: iv.unwrap_or((index as u128).to_be_bytes()),
            });
            let kind = if list.initialization.is_some() {
                Kind::Fragment
            } else {
                Kind::TransportStream
            };
            if list.segments.len() >= if live { 1024 } else { 30000 } {
                return Err(Error::Limit);
            }
            list.segments.push(Segment {
                url,
                seconds,
                sequence: index,
                encryption,
                kind,
                discontinuity,
            });
            discontinuity = false;
        } else if [
            "#EXT-X-BYTERANGE:",
            "#EXT-X-GAP",
            "#EXT-X-PART:",
            "#EXT-X-PRELOAD-HINT:",
            "#EXT-X-I-FRAMES-ONLY",
        ]
        .iter()
        .any(|s| line.starts_with(s))
            || (line.starts_with("#EXT") && line.to_ascii_uppercase().contains("URI="))
        {
            return Err(Error::Unavailable);
        }
    }
    if duration.is_some()
        || list.segments.is_empty()
        || (!live && !list.ended)
        || (live
            && (sequence.is_none()
                || !list.target_seconds.is_some_and(|v| (1..=3600).contains(&v))))
    {
        return Err(Error::Unavailable);
    }
    Ok(list)
}
