use regex_lite::{Captures, Regex};
use serde_json::Value;
use std::{collections::HashMap, sync::LazyLock};

pub const PUBLIC_CONTRACT: &str = include_str!("../../../assets/public-contract.json");

fn dictionary(locale: &str) -> &'static HashMap<String, String> {
    macro_rules! vocabulary {
        ($filename:literal) => {{
            static WORDS: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
                serde_json::from_str(include_str!(concat!(
                    "../../../assets/i18n/",
                    $filename,
                    ".json"
                )))
                .expect("invalid packaged vocabulary")
            });
            &WORDS
        }};
    }
    match locale {
        "zh-Hans" => vocabulary!("zh-Hans"),
        "zh-Hant" => vocabulary!("zh-Hant"),
        "ja" => vocabulary!("ja"),
        "ko" => vocabulary!("ko"),
        "es" => vocabulary!("es"),
        "ru" => vocabulary!("ru"),
        "ar" => vocabulary!("ar"),
        _ => vocabulary!("en"),
    }
}

pub struct LocalizedCopy {
    locale: String,
    plugin: Value,
}
impl LocalizedCopy {
    pub fn locale(&self) -> &str {
        &self.locale
    }
    pub fn new(tag: &str, plugin: Value) -> Self {
        let mut parts = tag.split(['_', '-']);
        let language = parts.next().unwrap_or("").to_ascii_lowercase();
        let rest: Vec<_> = parts.map(str::to_ascii_lowercase).collect();
        let tag = match language.as_str() {
            "zh" if rest.iter().any(|p| p == "hant")
                || (!rest.iter().any(|p| p.len() == 4)
                    && rest
                        .iter()
                        .any(|p| matches!(p.as_str(), "tw" | "hk" | "mo"))) =>
            {
                "zh-Hant"
            }
            "zh" => "zh-Hans",
            "ja" | "ko" | "en" | "es" | "ru" | "ar" => &language,
            _ => "en",
        };
        Self {
            locale: tag.to_owned(),
            plugin,
        }
    }
    pub fn text(&self, key: &str, params: &HashMap<String, String>) -> String {
        let plugin = |locale: &str| self.plugin.get(locale)?.get(key)?.as_str();
        let value = plugin(&self.locale)
            .or_else(|| {
                if self.locale == "zh-Hans" {
                    None
                } else {
                    plugin("en")
                }
            })
            .or_else(|| dictionary(&self.locale).get(key).map(String::as_str))
            .or_else(|| dictionary("en").get(key).map(String::as_str))
            .unwrap_or(key);
        if params.is_empty() {
            return value.to_owned();
        }
        static PLACEHOLDER: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\{([A-Za-z][A-Za-z0-9_]*)\}").unwrap());
        PLACEHOLDER
            .replace_all(value, |capture: &Captures<'_>| {
                params
                    .get(&capture[1])
                    .cloned()
                    .unwrap_or_else(|| capture[0].to_owned())
            })
            .into_owned()
    }
}

pub fn normalized_search_text(text: &str) -> String {
    static CHARACTERS: LazyLock<HashMap<char, &'static str>> = LazyLock::new(|| {
        include_str!("../../../assets/search/TSCharacters.txt")
            .lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| {
                let (key, variants) = line.split_once('\t')?;
                Some((
                    key.chars().next()?,
                    variants.split_ascii_whitespace().next()?,
                ))
            })
            .collect()
    });
    let mut value = String::with_capacity(text.len());
    for c in text.to_lowercase().chars() {
        if let Some(replacement) = CHARACTERS.get(&c) {
            value.push_str(replacement);
        } else {
            value.push(c);
        }
    }
    value
}
