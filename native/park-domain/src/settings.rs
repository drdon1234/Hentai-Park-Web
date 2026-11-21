use serde_json::{json, Map, Value};
use std::{collections::HashSet, sync::LazyLock};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

static RULES: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("settings-rules.json")).expect("settings rules")
});
static CONTRACT: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(crate::PUBLIC_CONTRACT).expect("public contract"));

pub fn discovery_domains(order: &[String], dimension: &str, media: &str) -> Vec<String> {
    let catalog = &CONTRACT["domains"];
    let defaults: Vec<&str> = catalog["order"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    let mut ordered = Vec::new();
    for id in order.iter().map(String::as_str).filter(|id| defaults.contains(id)) {
        if ordered.contains(&id) { continue; }
        ordered.push(id);
        if id == "gallery" && !order.iter().any(|d| d == "illustration") { ordered.push("illustration"); }
    }
    for (index, id) in defaults.iter().enumerate() {
        if ordered.contains(id) { continue; }
        let previous = defaults[..index].iter().rev().find_map(|p| ordered.iter().position(|d| d == p));
        ordered.insert(previous.map_or(0, |i| i+1), id);
    }
    let group = |field: &str, id: &str| catalog[field].as_object().unwrap().iter()
        .find(|(_,members)| members.as_array().unwrap().iter().any(|v| v.as_str() == Some(id)))
        .map(|(key,_)| key.as_str());
    ordered.into_iter().filter(|id| {
        (!matches!(dimension, "twoD" | "threeD") || group("dimension", id).is_none_or(|v| v == dimension))
        && (!matches!(media, "image" | "video" | "voice" | "other") || group("media", id).unwrap_or("other") == media)
    }).map(str::to_owned).collect()
}
fn strings(v: &Value) -> bool {
    v.as_array().is_some_and(|v| v.iter().all(Value::is_string))
}
fn entries(v: &Value, f: impl Fn(&str, &Value) -> bool) -> bool {
    v.as_object()
        .is_some_and(|o| o.iter().all(|(k, v)| f(k, v)))
}
fn aliases(v: &Value) -> bool {
    entries(v, |_, v| {
        v.as_array().is_some_and(|a| {
            a.iter()
                .all(|e| e.as_str().is_some_and(crate::filter::alias_text))
        })
    })
}
fn release(v: &Value) -> bool {
    let Some(s) = v.as_str() else {
        return false;
    };
    s.split('.').count() == 3
        && s.split('.').all(|p| {
            !p.is_empty()
                && (p == "0" || !p.starts_with('0'))
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<i64>().is_ok()
        })
}
fn date(v: &Value) -> Option<OffsetDateTime> {
    let text = v.as_str()?;
    if let Ok(t) = OffsetDateTime::parse(text, &Rfc3339) {
        return Some(t);
    }
    let stamp = if text.len() == 10 {
        format!("{text}T00:00:00Z")
    } else {
        format!("{}Z", text.replace(' ', "T"))
    };
    OffsetDateTime::parse(&stamp, &Rfc3339).ok()
}
pub fn valid(k: &str, v: &Value) -> bool {
    if let Some(rule) = RULES.get(k) {
        if rule == "bool" {
            return v.is_boolean();
        }
        if rule == "strings" {
            return strings(v);
        }
        if let Some(a) = rule["choices"].as_array() {
            return a.contains(v);
        }
        let bounds = &rule["range"];
        return v
            .as_f64()
            .is_some_and(|n| n >= bounds[0].as_f64().unwrap() && n <= bounds[1].as_f64().unwrap())
            && (rule["integer"] != true || v.is_i64());
    }
    let simple = |v: &Value| v.is_string() || v.is_number() || v.is_boolean();
    match k {
        "ignoredAppUpdate" => v == "" || release(v),
        "ignoredSourceUpdates" => entries(v, |k, v| !k.is_empty() && release(v)),
        "proxy" | "downloadDirectory" => v.is_string(),
        "domainOrder" => v.is_array(),
        "translateTargets" => v.as_array().is_some_and(|a| {
            a.iter().all(|s| {
                CONTRACT["languages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|l| l["translationId"] == *s)
            })
        }),
        "rememberedFilters" => entries(v, |_, v| {
            if !v["filters"].is_object() || !v["common"].is_object() {
                return false;
            }
            (v["start"].is_null() && v["end"].is_null())
                || date(&v["start"])
                    .zip(date(&v["end"]))
                    .is_some_and(|(a, b)| a.year() >= 1900 && b.year() <= 2100 && a <= b)
        }),
        "searchHistories" => entries(v, |_, v| strings(v)),
        "homeSources" | "blockedWorks" => entries(v, |_, v| v.is_string()),
        "blockedTagAliases" => aliases(v),
        "blockedAuthors" => entries(v, |_, v| v["name"].is_string() && strings(&v["works"])),
        "rankingSelection" => entries(v, |k, v| match k {
            "category" | "selected" | "section" | "kind" => v.is_string(),
            "detailSources" | "searchSources" | "navigation" => entries(v, |_, v| v.is_string()),
            "options" => entries(v, |_, v| {
                entries(v, |_, v| {
                    v.is_null() || simple(v) || v.as_array().is_some_and(|a| a.iter().all(simple))
                })
            }),
            _ => false,
        }),
        "blockPresets" => entries(v, |_, v| {
            v.is_object()
                && ["on", "title", "strong"]
                    .iter()
                    .all(|k| v[*k].is_null() || v[*k].is_boolean())
                && ["strongEntries", "entries"]
                    .iter()
                    .all(|k| v[*k].is_null() || strings(&v[*k]))
                && (v["aliases"].is_null() || aliases(&v["aliases"]))
        }),
        _ => true,
    }
}
pub fn normalize(settings: &mut Map<String, Value>) {
    if let Some(Value::Array(old)) = settings.remove("searchHistory") {
        let buckets = settings
            .entry("searchHistories")
            .or_insert_with(|| json!({}));
        if !buckets.is_object() {
            *buckets = json!({});
        }
        let existing = buckets["[\"all\"]"].as_array().cloned().unwrap_or_default();
        let mut seen = HashSet::new();
        buckets["[\"all\"]"] = existing
            .into_iter()
            .chain(old)
            .filter_map(|v| {
                v.as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .filter(|s| seen.insert(s.clone()))
            .take(30)
            .collect::<Vec<_>>()
            .into();
    }
    let get = |k: &str| {
        settings
            .get(k)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let order = get("domainOrder");
    let interests = get("homeCategories");
    let gallery = json!("gallery");
    let illustration = json!("illustration");
    if order.contains(&illustration) || (!order.contains(&gallery) && !interests.contains(&gallery))
    {
        return;
    }
    let defaults = CONTRACT["sourceProtocol"]["categories"].as_array().unwrap();
    let expand = |list: Vec<Value>| {
        let mut out = Vec::new();
        for v in list {
            if !out.contains(&v) {
                out.push(v.clone());
            }
            if v == gallery && !out.contains(&illustration) {
                out.push(illustration.clone());
            }
        }
        out
    };
    settings.insert(
        "domainOrder".into(),
        expand(
            order
                .into_iter()
                .filter(|v| defaults.contains(v))
                .chain(defaults.iter().cloned())
                .collect(),
        )
        .into(),
    );
    if interests.contains(&gallery) {
        settings.insert("homeCategories".into(), expand(interests).into());
    }
}
