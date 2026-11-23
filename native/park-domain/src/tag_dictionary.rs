use crate::normalized_search_text;
use regex_lite::Regex;
use serde_json::{json, Value};
use std::{collections::HashSet, sync::LazyLock};

struct Tag {
    scope: String,
    name: String,
    translation: String,
    search_name: String,
    search_translation: String,
    order: Vec<u16>,
}

pub struct TagDictionary(Vec<Tag>);

impl TagDictionary {
    pub fn parse(bytes: &[u8], source: &str, locale: &str) -> Option<Self> {
        if bytes.len() > 8 * 1024 * 1024 {
            return None;
        }
        let pack: Value = serde_json::from_slice(bytes).ok()?;
        let tags = pack["tags"].as_object()?;
        let version = semver::Version::parse(pack["version"].as_str()?).ok()?;
        if pack["schema"] != 1
            || pack["source"] != source
            || pack["locale"] != locale
            || !version.pre.is_empty()
            || !version.build.is_empty()
            || tags.is_empty()
            || tags.len() > 100_000
            || (!pack["provenance"].is_null()
                && !pack["provenance"]
                    .as_array()
                    .is_some_and(|rows| rows.len() <= 32 && rows.iter().all(Value::is_object)))
        {
            return None;
        }
        static PLACEHOLDER: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?i)^(TODO|TBD|N/A|\?+|-+)$").unwrap());
        let mut seen = HashSet::new();
        let mut entries = Vec::with_capacity(tags.len());
        for (key, value) in tags {
            let name = key.trim();
            let translation = value.as_str()?.trim();
            if name.is_empty()
                || name.encode_utf16().count() > 256
                || translation.is_empty()
                || translation.encode_utf16().count() > 512
                || PLACEHOLDER.is_match(translation)
            {
                return None;
            }
            let key = name.to_lowercase();
            if !seen.insert(key.clone()) {
                return None;
            }
            let (scope, name) = key.split_once(':').unwrap_or(("", &key));
            entries.push(Tag {
                scope: scope.into(),
                name: name.into(),
                translation: translation.into(),
                search_name: normalized_search_text(name),
                search_translation: normalized_search_text(translation),
                order: format!("{scope}:{name}").encode_utf16().collect(),
            });
        }
        Some(Self(entries))
    }

    pub fn translation(&self, name: &str, namespace: &str) -> Option<&str> {
        let name = name.trim().to_lowercase();
        let namespace = namespace.trim().to_lowercase();
        let mut unscoped = None;
        for tag in &self.0 {
            if tag.name == name {
                if tag.scope == namespace { return Some(&tag.translation); }
                if tag.scope.is_empty() { unscoped = Some(tag.translation.as_str()); }
            }
        }
        unscoped
    }

    pub fn suggestions(&self, query: &str) -> Value {
        static LAST: LazyLock<regress::Regex> = LazyLock::new(|| {
            regress::Regex::with_flags(r#"(?:^|\s)-?((?:[a-z]+:)?(?:"[^"]*"?\$?|[^\s"]*))$"#, "i")
                .unwrap()
        });
        let text = query.trim();
        let token = LAST
            .find(text)
            .and_then(|m| m.group(1))
            .map_or(text, |range| &text[range]);
        let needle = normalized_search_text(&token.replace(['"', '$'], ""));
        if needle.is_empty() {
            return json!([]);
        }
        let (scope, term) = needle.split_once(':').unwrap_or(("", &needle));
        let mut ranked = Vec::new();
        for tag in &self.0 {
            if !scope.is_empty() && scope != tag.scope {
                continue;
            }
            let score = if tag.search_name == term || tag.search_translation == term {
                0
            } else if tag.search_name.starts_with(term) || tag.search_translation.starts_with(term)
            {
                1
            } else if tag.search_name.contains(term) || tag.search_translation.contains(term) {
                2
            } else {
                continue;
            };
            ranked.push((score, tag));
        }
        let order =
            |a: &(u8, &Tag), b: &(u8, &Tag)| a.0.cmp(&b.0).then_with(|| a.1.order.cmp(&b.1.order));
        if ranked.len() > 20 {
            ranked.select_nth_unstable_by(20, order);
            ranked.truncate(20);
        }
        ranked.sort_unstable_by(order);
        Value::Array(
            ranked
                .into_iter()
                .map(|(_, tag)| {
                    json!({
                        "namespace": tag.scope, "name": tag.name, "translation": tag.translation
                    })
                })
                .collect(),
        )
    }
}
