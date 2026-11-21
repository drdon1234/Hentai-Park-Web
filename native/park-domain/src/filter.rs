use regress::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashSet, sync::LazyLock};

#[derive(Deserialize)]
struct Vocabulary {
    presets: Vec<Preset>,
    aliases: Vec<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preset {
    id: String,
    name: String,
    entries: Vec<String>,
    strong_entries: Vec<String>,
}

static VOCABULARY: LazyLock<Vocabulary> = LazyLock::new(|| {
    serde_json::from_str(include_str!(
        "../../../clients/web/packages/domain-model/src/block-vocabulary.json"
    ))
    .expect("invalid packaged block vocabulary")
});

fn strings(value: &Value) -> Option<Vec<String>> {
    Some(
        value
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
}

pub(crate) fn alias_text(s: &str) -> bool {
    let rule = Rule::new(s);
    s.encode_utf16().count() <= 256
        && rule.scope.is_empty()
        && !rule.text.is_empty()
        && !rule.regex
        && !rule.glob
}
struct Rule<'a> {
    scope: String,
    text: &'a str,
    regex: bool,
    glob: bool,
}
impl<'a> Rule<'a> {
    fn new(raw: &'a str) -> Self {
        let raw = raw.trim();
        let (scope, value) = match raw.split_once(':') {
            Some((s, v)) if !s.eq_ignore_ascii_case("regex") => (s.trim().to_lowercase(), v.trim()),
            _ => (String::new(), raw),
        };
        let regex = value
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("regex:"));
        let text = if regex { &value[6..] } else { value };
        Self {
            scope,
            text,
            regex,
            glob: !regex && text.contains(['*', '?']),
        }
    }
    fn compile(&self, title: bool, preset: bool) -> Option<Regex> {
        if self.text.is_empty() || title && self.scope == "tag" {
            return None;
        }
        let literal = regex_lite::escape(self.text);
        let exp = if self.regex
            || title
                && preset
                && self
                    .text
                    .contains(['[', ']', '(', ')', '+', '|', '\\', '^', '$'])
        {
            self.text.to_owned()
        } else if self.glob {
            let mut glob = String::new();
            for ch in self.text.chars() {
                match ch {
                    '*' => glob.push_str(r"[\s\S]*"),
                    '?' => glob.push_str(r"[\s\S]"),
                    _ => glob.push_str(&regex_lite::escape(&ch.to_string())),
                }
            }
            if title {
                glob
            } else {
                format!("^{glob}$")
            }
        } else if title
            && self
                .text
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b' ')
        {
            format!("(?<![A-Za-z0-9]){literal}(?![A-Za-z0-9])")
        } else if title && preset {
            self.text.to_owned()
        } else if title {
            literal.clone()
        } else {
            format!("^{literal}$")
        };
        let compile = |s: &str| Regex::from_unicode(s.encode_utf16().map(u32::from), "i").ok();
        compile(&exp).or_else(|| if self.regex { None } else { compile(&literal) })
    }
}

struct Pattern {
    scope: String,
    label: String,
    pattern: Regex,
}

pub struct BlockFilter {
    titles: Vec<Pattern>,
    rules: Vec<Pattern>,
}
impl BlockFilter {
    pub fn from_settings(settings: &Value) -> Self {
        let mut filter = Self {
            titles: vec![],
            rules: vec![],
        };
        for raw in strings(&settings["blockedTags"]).unwrap_or_default() {
            let rule = Rule::new(&raw);
            let aliases = if !rule.regex && !rule.glob && !rule.text.is_empty() {
                strings(&settings["blockedTagAliases"][&raw]).unwrap_or_else(|| {
                    VOCABULARY
                        .aliases
                        .iter()
                        .find(|group| {
                            group
                                .iter()
                                .any(|s| s.to_lowercase() == rule.text.to_lowercase())
                        })
                        .into_iter()
                        .flatten()
                        .filter(|s| s.to_lowercase() != rule.text.to_lowercase())
                        .cloned()
                        .collect()
                })
            } else {
                vec![]
            };
            filter.add(&raw, &aliases, None);
        }
        for preset in &VOCABULARY.presets {
            let config = &settings["blockPresets"][&preset.id];
            if config["on"] != true {
                continue;
            }
            let strong = strings(&config["strongEntries"]);
            let mut entries = strings(&config["entries"]).unwrap_or_else(|| preset.entries.clone());
            if strong.is_none() {
                entries.retain(|e| !preset.strong_entries.contains(e));
            }
            if config["strong"] == true {
                entries.extend(strong.unwrap_or_else(|| preset.strong_entries.clone()));
            }
            let mut seen = HashSet::new();
            for raw in entries.into_iter().filter(|e| seen.insert(e.clone())) {
                if Rule::new(&raw).scope == "title" && config["title"] != true {
                    continue;
                }
                let aliases = strings(&config["aliases"][&raw]).unwrap_or_default();
                filter.add(
                    &raw,
                    &aliases,
                    Some((&preset.name, config["title"] == true)),
                );
            }
        }
        filter
    }

    fn add(&mut self, raw: &str, aliases: &[String], preset: Option<(&str, bool)>) {
        let scope = Rule::new(raw).scope;
        let expanded = std::iter::once(raw.trim().to_owned()).chain(aliases.iter().map(|alias| {
            if scope.is_empty() {
                alias.clone()
            } else {
                format!("{scope}:{alias}")
            }
        }));
        for entry in expanded {
            let rule = Rule::new(&entry);
            if let Some((name, true)) = preset {
                if let Some(pattern) = rule.compile(true, true) {
                    self.titles.push(Pattern {
                        scope: "title".into(),
                        label: format!("{name}：{entry}"),
                        pattern,
                    });
                }
            }
            if preset.is_some() && rule.scope == "title" {
                continue;
            }
            if let Some(pattern) = rule.compile(rule.scope == "title", false) {
                let label = if rule.regex {
                    entry.clone()
                } else {
                    entry.to_lowercase()
                };
                self.rules.push(Pattern {
                    scope: rule.scope,
                    label,
                    pattern,
                });
            }
        }
    }

    pub fn first_match(&self, title: &str, tags: &[String]) -> Option<String> {
        let title: Vec<_> = title.encode_utf16().collect();
        let tags: Vec<_> = tags
            .iter()
            .map(|tag| {
                let (namespace, text) = tag.trim().split_once(':').unwrap_or(("", tag));
                (
                    namespace.trim().to_lowercase(),
                    text.trim().encode_utf16().collect::<Vec<_>>(),
                )
            })
            .collect();
        for item in self.titles.iter().chain(&self.rules) {
            let matches = |value: &[u16]| item.pattern.find_from_ucs2(value, 0).next().is_some();
            let found = if item.scope == "title" {
                matches(&title)
            } else {
                tags.iter().any(|(namespace, value)| {
                    (item.scope.is_empty() || item.scope == "tag" || item.scope == *namespace)
                        && matches(value)
                })
            };
            if found {
                return Some(item.label.clone());
            }
        }
        None
    }
}
