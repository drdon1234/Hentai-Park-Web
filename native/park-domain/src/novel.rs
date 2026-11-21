use regex_lite::Regex;
use std::{ops::Range, sync::LazyLock};

pub struct TextSegment {
    pub source: Range<usize>,
    pub translate: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub enum NovelMode {
    Original,
    Translated,
    Bilingual,
}

struct Anchor {
    original: Range<usize>,
    display: Range<usize>,
}

/// 数据库和批注均保存原文 UTF-16 坐标。双语段的译文能返回原文，但重开时优先显示原文。
pub struct NovelText {
    pub text: String,
    original_length: usize,
    display_length: usize,
    original_low: Vec<usize>,
    display_low: Vec<usize>,
    spans: Vec<Anchor>,
    originals: Vec<usize>,
}

fn low_surrogate(unit: u16) -> bool {
    (0xdc00..=0xdfff).contains(&unit)
}

fn scalar_offset(offset: usize, low: &[usize]) -> usize {
    offset - usize::from(low.binary_search(&offset).is_ok())
}

fn scale_offset(at: usize, from: &Range<usize>, to: &Range<usize>) -> usize {
    to.start + (((at - from.start) as u128 * to.len() as u128) / from.len() as u128) as usize
}

impl NovelText {
    pub fn new<'a>(
        text: String,
        mode: NovelMode,
        translations: impl IntoIterator<Item = (i64, i64, &'a str)>,
    ) -> Option<Self> {
        let units: Vec<_> = text.encode_utf16().collect();
        let low = units
            .iter()
            .enumerate()
            .filter_map(|(i, &v)| low_surrogate(v).then_some(i))
            .collect();
        let mut view = Self {
            text: String::new(),
            original_length: units.len(),
            display_length: 0,
            original_low: low,
            display_low: Vec::new(),
            spans: Vec::new(),
            originals: Vec::new(),
        };
        let mut cursor = 0;
        if mode != NovelMode::Original {
            for (start, end, translation) in translations {
                let start = usize::try_from(start).ok()?;
                let end = usize::try_from(end).ok()?;
                if start < cursor
                    || end <= start
                    || end > units.len()
                    || translation.is_empty()
                    || [start, end]
                        .iter()
                        .any(|p| units.get(*p).is_some_and(|&v| low_surrogate(v)))
                {
                    return None;
                }
                if cursor < start {
                    view.append(
                        cursor..start,
                        &String::from_utf16(&units[cursor..start]).ok()?,
                        true,
                    );
                }
                if mode == NovelMode::Bilingual {
                    view.append(
                        start..end,
                        &String::from_utf16(&units[start..end]).ok()?,
                        true,
                    );
                    view.append(end..end, "\n", false);
                }
                view.append(start..end, translation, mode == NovelMode::Translated);
                cursor = end;
            }
        }
        if cursor == 0 {
            view.text = text;
            view.display_length = units.len();
        } else if cursor < units.len() {
            view.append(
                cursor..units.len(),
                &String::from_utf16(&units[cursor..]).ok()?,
                true,
            );
        }
        Some(view)
    }

    fn append(&mut self, original: Range<usize>, value: &str, anchor: bool) {
        let start = self.display_length;
        for unit in value.encode_utf16() {
            if low_surrogate(unit) {
                self.display_low.push(self.display_length);
            }
            self.display_length += 1;
        }
        self.text.push_str(value);
        if anchor {
            self.originals.push(self.spans.len());
        }
        self.spans.push(Anchor {
            original,
            display: start..self.display_length,
        });
    }

    pub fn displayed(&self, offset: usize) -> usize {
        let at = scalar_offset(offset.min(self.original_length), &self.original_low);
        if self.spans.is_empty() {
            return at;
        }
        let index = self
            .originals
            .partition_point(|&i| self.spans[i].original.end <= at);
        let Some(&i) = self.originals.get(index) else {
            return self.display_length;
        };
        let span = &self.spans[i];
        scalar_offset(
            scale_offset(at, &span.original, &span.display),
            &self.display_low,
        )
    }

    pub fn valid_offsets(&self) -> bool {
        self.original_length.max(self.display_length) <= i32::MAX as usize
    }

    pub fn source(&self, offset: usize) -> usize {
        if self.spans.is_empty() {
            return scalar_offset(offset.min(self.original_length), &self.original_low);
        }
        let at = scalar_offset(offset.min(self.display_length), &self.display_low);
        let index = self.spans.partition_point(|s| s.display.end <= at);
        let Some(span) = self.spans.get(index) else {
            return self.original_length;
        };
        scalar_offset(
            scale_offset(at, &span.display, &span.original),
            &self.original_low,
        )
    }
}

// 与 v0.15.4 的保存格式相同，长段在 800 个 UTF-16 单元内的后半段寻找断点。
pub fn translation_segments(text: &str) -> Vec<TextSegment> {
    static BREAK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\n[\s\x{00a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]*\n").unwrap()
    });
    let mut result = Vec::new();
    let mut start = 0;
    for boundary in BREAK.find_iter(text) {
        paragraph(text, start..boundary.start(), &mut result);
        result.push(TextSegment {
            source: boundary.range(),
            translate: false,
        });
        start = boundary.end();
    }
    paragraph(text, start..text.len(), &mut result);
    result
}

fn whitespace(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

fn paragraph(text: &str, range: Range<usize>, result: &mut Vec<TextSegment>) {
    let mut start = range.start;
    while start < range.end {
        let rest = &text[start..range.end];
        let mut units = 0;
        let mut limit = 0;
        let mut boundary = None;
        for (offset, c) in rest.char_indices() {
            if units + c.len_utf16() > 800 {
                break;
            }
            units += c.len_utf16();
            limit = offset + c.len_utf8();
            if whitespace(c) || ".!?。！？；;".contains(c) {
                boundary = Some((units, limit));
            }
        }
        if limit < rest.len() {
            if let Some((n, end)) = boundary {
                if n >= units / 2 {
                    limit = end;
                }
            }
        }
        let piece = &rest[..limit];
        let left = piece.len() - piece.trim_start_matches(whitespace).len();
        let right = piece.trim_end_matches(whitespace).len().max(left);
        for (a, b, translate) in [(0, left, false), (left, right, true), (right, limit, false)] {
            if a < b {
                result.push(TextSegment {
                    source: (start + a)..(start + b),
                    translate,
                });
            }
        }
        start += limit;
    }
}
