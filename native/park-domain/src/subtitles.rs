use regex_lite::Regex;
use std::sync::LazyLock;

#[derive(Clone, Debug)]
pub struct Cue {
    pub start_ms: i64,
    pub end_ms: Option<i64>,
    pub text: String,
}
fn clean(text: &str) -> String {
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());
    TAG.replace_all(text, "")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .trim()
        .to_owned()
}
fn scale(fraction: &str) -> i64 {
    fraction.parse::<i64>().unwrap_or(0) * 10i64.pow(3 - fraction.len() as u32)
}
fn milliseconds(s: &str) -> Option<i64> {
    static TIME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:([0-9]{1,3}):)?([0-9]{1,2}):([0-5][0-9])[.,]([0-9]{1,3})$").unwrap()
    });
    let cap = TIME.captures(s)?;
    let hours = cap.get(1).map_or("0", |v| v.as_str()).parse::<i64>().ok()?;
    let minutes = cap[2].parse::<i64>().ok()?;
    if minutes >= 60 {
        return None;
    }
    Some((hours * 3600 + minutes * 60 + cap[3].parse::<i64>().ok()?) * 1000 + scale(&cap[4]))
}
fn blocks(text: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let label = line.trim();
        if matches!(label, "NOTE" | "STYLE" | "REGION") || label.starts_with("NOTE ") {
            while lines.next_if(|l| !l.trim().is_empty()).is_some() {}
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(start) = parts.next() else { continue };
        if parts.next() != Some("-->") {
            continue;
        }
        let Some(end) = parts.next() else { continue };
        let mut body = Vec::new();
        while let Some(l) = lines.next_if(|l| !l.trim().is_empty()) {
            body.push(l);
        }
        if let (Some(start_ms), Some(end_ms)) = (milliseconds(start), milliseconds(end)) {
            let text = clean(&body.join("\n"));
            if end_ms > start_ms && !text.is_empty() {
                cues.push(Cue {
                    start_ms,
                    end_ms: Some(end_ms),
                    text,
                });
            }
        }
    }
    cues
}
fn lyrics(text: &str) -> Vec<Cue> {
    static TIME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\[([0-9]{1,3}):([0-5][0-9])(?:[.:]([0-9]{1,3}))?\]").unwrap()
    });
    let mut values = Vec::new();
    let mut offset = 0;
    for line in text.lines() {
        let mut rest = line.trim();
        if let Some(n) = rest
            .to_ascii_lowercase()
            .strip_prefix("[offset:")
            .and_then(|s| s.strip_suffix(']'))
        {
            offset = n.parse::<i64>().unwrap_or(0);
            continue;
        }
        let mut times = Vec::new();
        while let Some(cap) = TIME.captures(rest) {
            times.push(
                (cap[1].parse::<i64>().unwrap() * 60 + cap[2].parse::<i64>().unwrap()) * 1000
                    + scale(cap.get(3).map_or("", |s| s.as_str())),
            );
            rest = &rest[cap.get(0).unwrap().end()..];
        }
        let label = clean(rest);
        if !label.is_empty() {
            for start_ms in times {
                values.push(Cue {
                    start_ms,
                    end_ms: None,
                    text: label.clone(),
                });
            }
        }
    }
    values.sort_by_key(|c| c.start_ms);
    let adjusted = |ms: i64| ms.saturating_add(offset).clamp(0, i64::from(i32::MAX));
    let mut next = None;
    let mut end = None;
    for cue in values.iter_mut().rev() {
        if next.is_some_and(|n| n > cue.start_ms) {
            end = next.map(adjusted);
        }
        next = Some(cue.start_ms);
        cue.end_ms = end;
        cue.start_ms = adjusted(cue.start_ms);
    }
    values
}
pub fn parse(text: &str, format: &str) -> Vec<Cue> {
    let normalized = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut cues = match format.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "lrc" => lyrics(&normalized),
        "srt" | "vtt" => blocks(&normalized),
        _ => Vec::new(),
    };
    cues.sort_by_key(|v| v.start_ms);
    cues
}
