use super::{Error, Result};
use std::borrow::Cow;

// 有些复用器把上一幅图像的尾部放在下一个 PES 时间戳之后。时间戳属于
// 在该 PES 内开始的图像。先沿 PID 还原基本流，再按访问单元交给转封装器。
fn payload(packet: &[u8]) -> Option<&[u8]> {
    if packet[3] & 0x10 == 0 {
        return None;
    }
    let offset = if packet[3] & 0x20 == 0 {
        4
    } else {
        5 + packet[4] as usize
    };
    packet.get(offset..)
}
fn pid(p: &[u8]) -> u16 {
    u16::from_be_bytes([p[1] & 31, p[2]])
}
fn is_video(b: &[u8]) -> bool {
    b.starts_with(&[0, 0, 1]) && b.get(3).is_some_and(|v| v & 0xf0 == 0xe0)
}
fn annex_b(b: &[u8]) -> bool {
    b.starts_with(&[0, 0, 1]) || b.starts_with(&[0, 0, 0, 1])
}
struct Mark<'a> {
    offset: usize,
    header: &'a [u8],
}
fn packetize(out: &mut Vec<u8>, pid: u16, counter: &mut u8, head: &[u8], au: &[u8]) {
    let mut header = head.to_vec();
    // 视频 PES 允许不定长。重组后原长度和原对齐标记已失效。
    header[4..6].fill(0);
    header[6] |= 4;
    let total = header.len() + au.len();
    let mut used = 0;
    while used < total {
        let n = (total - used).min(184);
        let mut p = [0xff; 188];
        p[0] = 0x47;
        p[1] = ((pid >> 8) as u8) | if used == 0 { 0x40 } else { 0 };
        p[2] = pid as u8;
        p[3] = if n == 184 { 0x10 } else { 0x30 } | *counter;
        *counter = (*counter + 1) & 15;
        if n < 184 {
            p[4] = (183 - n) as u8;
            if n < 183 {
                p[5] = 0;
            }
        }
        let start = 188 - n;
        let h = header.len().saturating_sub(used).min(n);
        if h > 0 {
            p[start..start + h].copy_from_slice(&header[used..used + h]);
        }
        let at = (used + h).saturating_sub(header.len());
        p[start + h..].copy_from_slice(&au[at..at + n - h]);
        out.extend_from_slice(&p);
        used += n;
    }
}

pub(super) fn aligned(input: &[u8]) -> Result<Cow<'_, [u8]>> {
    let mut target = None;
    for p in input.chunks_exact(188).filter(|p| p[1] & 0x40 != 0) {
        let Some(b) = payload(p).filter(|b| is_video(b) && b.len() >= 9) else {
            continue;
        };
        if b.get(9 + b[8] as usize..)
            .is_some_and(|es| es.len() >= 4 && !annex_b(es))
        {
            target = Some(pid(p));
            break;
        }
    }
    let Some(target) = target else {
        return Ok(Cow::Borrowed(input));
    };
    let mut pes = Vec::new();
    let mut offsets = Vec::new();
    let mut output = Vec::with_capacity(input.len());
    let mut insert = None;
    let mut cc = None;
    for p in input.chunks_exact(188) {
        if pid(p) != target {
            output.extend_from_slice(p);
            continue;
        }
        let Some(data) = payload(p).filter(|b| !b.is_empty()) else {
            continue;
        };
        let counter = p[3] & 15;
        if let Some(prev) = cc {
            if prev == counter {
                continue;
            }
            if (prev + 1) & 15 != counter && p[1] & 0x40 == 0 {
                return Err(Error::Unavailable);
            }
        }
        cc = Some(counter);
        if p[1] & 0x40 != 0 {
            offsets.push(pes.len());
            insert.get_or_insert(output.len());
        }
        if !offsets.is_empty() {
            pes.extend_from_slice(data);
        }
    }
    offsets.push(pes.len());
    let mut es = Vec::with_capacity(pes.len());
    let mut marks = Vec::new();
    for span in offsets.windows(2) {
        let b = &pes[span[0]..span[1]];
        if b.len() < 9 || !is_video(b) {
            return Err(Error::Unavailable);
        }
        let end = 9 + b[8] as usize;
        let length = u16::from_be_bytes([b[4], b[5]]) as usize;
        let b = b
            .get(..if length == 0 { b.len() } else { 6 + length })
            .ok_or(Error::Unavailable)?;
        let body = b.get(end..).ok_or(Error::Unavailable)?;
        marks.push(Mark {
            offset: es.len(),
            header: &b[..end],
        });
        es.extend_from_slice(body);
    }
    let mut access = Vec::new();
    let mut codec = None;
    let mut zeros = 0;
    for (i, &byte) in es.iter().enumerate() {
        if byte == 1 && zeros >= 2 {
            if let Some(&nal) = es.get(i + 1) {
                let avc = nal == 9;
                let hevc = nal == 0x46 && es.get(i + 2).is_some_and(|b| b & 7 != 0);
                if (avc || hevc) && *codec.get_or_insert(avc) == avc {
                    access.push(i - zeros.min(3));
                }
            }
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    // 无分隔符的非对齐字节没有可信图像边界，禁止交付已截断的画面。
    if access.is_empty() {
        return Err(Error::Unavailable);
    }
    access.push(es.len());
    let mut normalized = Vec::with_capacity(pes.len());
    let mut counter = 0;
    let mut prev_mark = None;
    for frame in access.windows(2) {
        let mark = marks
            .partition_point(|m| m.offset <= frame[0])
            .checked_sub(1)
            .ok_or(Error::Unavailable)?;
        if prev_mark == Some(mark) {
            return Err(Error::Unavailable);
        }
        packetize(
            &mut normalized,
            target,
            &mut counter,
            marks[mark].header,
            &es[frame[0]..frame[1]],
        );
        prev_mark = Some(mark);
    }
    let at = insert.ok_or(Error::Unavailable)?;
    let tail = output.split_off(at);
    output.extend_from_slice(&normalized);
    output.extend_from_slice(&tail);
    Ok(Cow::Owned(output))
}
