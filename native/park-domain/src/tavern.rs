use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use std::io::Cursor;

#[derive(Debug)]
pub struct InvalidResource;
type Result<T> = std::result::Result<T, InvalidResource>;
fn metadata(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut reader = png::Decoder::new(Cursor::new(bytes))
        .read_info()
        .map_err(|_| InvalidResource)?;
    let info = reader.info();
    if u64::from(info.width) * u64::from(info.height) > 40_000_000 {
        return Err(InvalidResource);
    }
    reader.finish().map_err(|_| InvalidResource)?;
    let info = reader.info();
    for key in ["ccv3", "chara"] {
        let text = if let Some(t) = info
            .uncompressed_latin1_text
            .iter()
            .rev()
            .find(|t| t.keyword == key)
        {
            t.text.clone()
        } else if let Some(t) = info
            .compressed_latin1_text
            .iter()
            .rev()
            .find(|t| t.keyword == key)
        {
            t.get_text().map_err(|_| InvalidResource)?
        } else if let Some(t) = info.utf8_text.iter().rev().find(|t| t.keyword == key) {
            t.get_text().map_err(|_| InvalidResource)?
        } else {
            continue;
        };
        if text.len() > 8 * 1024 * 1024 {
            return Err(InvalidResource);
        }
        return STANDARD.decode(text).map_err(|_| InvalidResource);
    }
    Err(InvalidResource)
}
pub fn valid_content(kind: &str, raw: &Value) -> bool {
    if !raw.is_object() {
        return false;
    }
    let nested = raw.get("data").filter(|v| v.is_object()).unwrap_or(raw);
    match kind {
        "character" => {
            let text = |key: &str| nested[key].as_str().is_some_and(|s| !s.trim().is_empty());
            text("name")
                && (text("description")
                    || text("personality")
                    || text("first_mes")
                    || nested["alternate_greetings"].as_array().is_some_and(|v| {
                        v.iter()
                            .any(|s| s.as_str().is_some_and(|s| !s.trim().is_empty()))
                    }))
        }
        "lorebook" => {
            let entries = raw
                .get("entries")
                .filter(|v| v.is_array() || v.is_object())
                .unwrap_or(&nested["entries"]);
            entries
                .as_array()
                .is_some_and(|v| v.iter().any(Value::is_object))
                || entries
                    .as_object()
                    .is_some_and(|v| v.values().any(Value::is_object))
        }
        _ => false,
    }
}
pub fn parse_resource(kind: &str, format: &str, bytes: &[u8]) -> Result<Value> {
    if bytes.is_empty() || bytes.len() > 12 * 1024 * 1024 {
        return Err(InvalidResource);
    }
    let raw = match (format, kind) {
        ("png", "character") => serde_json::from_slice(&metadata(bytes)?),
        ("json", _) => serde_json::from_slice(bytes),
        _ => return Err(InvalidResource),
    }
    .map_err(|_| InvalidResource)?;
    if !valid_content(kind, &raw) {
        return Err(InvalidResource);
    }
    Ok(raw)
}

/// 原样保留图像、动画与其他扩展块，仅替换卡片元数据。
pub fn encode_resource(kind: &str, image: Option<&[u8]>, card: &Value) -> Result<Vec<u8>> {
    if !valid_content(kind, card) {
        return Err(InvalidResource);
    }
    let json = serde_json::to_vec(card).map_err(|_| InvalidResource)?;
    let Some(image) = image else {
        return (json.len() <= 12 * 1024 * 1024)
            .then_some(json)
            .ok_or(InvalidResource);
    };
    if kind != "character"
        || image.len() > 12 * 1024 * 1024
        || !image.starts_with(b"\x89PNG\r\n\x1a\n")
    {
        return Err(InvalidResource);
    }
    let mut chunk = b"tEXt".to_vec();
    chunk.extend_from_slice(if card["spec"] == "chara_card_v3" {
        b"ccv3\0"
    } else {
        b"chara\0"
    });
    chunk.extend_from_slice(STANDARD.encode(json).as_bytes());
    let mut out = image[..8].to_vec();
    let mut input = &image[8..];
    while input.len() >= 12 {
        let size = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
        let end = size.checked_add(12).ok_or(InvalidResource)?;
        let raw = input.get(..end).ok_or(InvalidResource)?;
        let tag = &raw[4..8];
        let payload = &raw[8..end - 4];
        if tag == b"IEND" {
            out.extend_from_slice(&((chunk.len() - 4) as u32).to_be_bytes());
            out.extend_from_slice(&chunk);
            out.extend_from_slice(&crc32fast::hash(&chunk).to_be_bytes());
            out.extend_from_slice(raw);
            if input.len() != end || parse_resource(kind, "png", &out)? != *card {
                return Err(InvalidResource);
            }
            return Ok(out);
        }
        let replaced = matches!(tag, b"iTXt" | b"zTXt" | b"tEXt")
            && payload
                .iter()
                .position(|&c| c == 0)
                .is_some_and(|i| matches!(&payload[..i], b"chara" | b"ccv3"));
        if !replaced {
            out.extend_from_slice(raw);
        }
        input = &input[end..];
    }
    Err(InvalidResource)
}
