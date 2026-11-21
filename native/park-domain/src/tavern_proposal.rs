use crate::tavern::InvalidResource;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

fn merged(previous: &Value, incoming: &Value) -> Value {
    let mut result = previous.as_object().cloned().unwrap_or_default();
    for (k, v) in incoming.as_object().into_iter().flatten() {
        let value = if k == "extensions" && v.is_object() {
            merged(result.get(k).unwrap_or(&Value::Null), v)
        } else {
            v.clone()
        };
        result.insert(k.clone(), value);
    }
    Value::Object(result)
}
struct Review<'a> {
    omitted: &'a [String],
    choices: Vec<Value>,
}
impl Review<'_> {
    fn accept(&mut self, id: String, label: &str, value: &Value) -> bool {
        let selected = !self.omitted.contains(&id);
        self.choices
            .push(json!({"id":id,"label":label,"value":value}));
        selected
    }
    fn fields(
        &mut self,
        target: &mut Value,
        source: &Map<String, Value>,
        prefix: &str,
        skip: &str,
    ) {
        for (k, v) in source {
            if k == skip {
                continue;
            }
            let value = if k == "extensions" && v.is_object() {
                merged(&target[k], v)
            } else {
                v.clone()
            };
            if value != target[k] && self.accept(format!("{prefix}:{k}"), k, &value) {
                target[k] = value
            }
        }
    }
    fn entries(&mut self, book: &mut Value, input: &Value) -> Result<(), InvalidResource> {
        let incoming: Vec<&Value> = match input {
            Value::Array(a) => a.iter().collect(),
            Value::Object(o) => o.values().collect(),
            Value::Null => return Ok(()),
            _ => return Err(InvalidResource),
        };
        if book["entries"].is_null() {
            book["entries"] = json!([])
        }
        let original = book["entries"].clone();
        let previous: Vec<(String, &Value)> = match &original {
            Value::Array(a) => a
                .iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), v))
                .collect(),
            Value::Object(o) => o.iter().map(|(k, v)| (k.clone(), v)).collect(),
            _ => return Err(InvalidResource),
        };
        let mut used = HashSet::new();
        for (i, e) in incoming.into_iter().enumerate() {
            if !e.is_object() || !e["content"].is_string() {
                return Err(InvalidResource);
            }
            let prior = ["uid", "id", "comment", "name"].iter().find_map(|k| {
                let val = &e[*k];
                if val.is_null() || val == "" {
                    return None;
                }
                previous
                    .iter()
                    .find(|(key, p)| !used.contains(key) && &p[*k] == val)
            });
            let old = prior.map_or(&Value::Null, |(_, p)| *p);
            let value = merged(old, e);
            if let Some((id, _)) = prior {
                used.insert(id.clone());
            }
            let label = e["comment"]
                .as_str()
                .or(e["name"].as_str())
                .or(e["content"].as_str())
                .unwrap_or("");
            if &value == old || !self.accept(format!("entry:{i}"), label, &value) {
                continue;
            }
            match &mut book["entries"] {
                Value::Array(a) => {
                    if let Some((k, _)) = prior {
                        a[k.parse::<usize>().map_err(|_| InvalidResource)?] = value
                    } else {
                        a.push(value)
                    }
                }
                Value::Object(o) => {
                    let key = prior.map(|(k, _)| k.clone()).unwrap_or_else(|| {
                        (0..)
                            .map(|j| j.to_string())
                            .find(|k| !o.contains_key(k))
                            .unwrap()
                    });
                    o.insert(key, value);
                }
                _ => return Err(InvalidResource),
            }
        }
        Ok(())
    }
}
pub fn review(
    kind: &str,
    mut base: Value,
    incoming: &Value,
    omitted: &[String],
) -> Result<Value, InvalidResource> {
    let character = kind == "character";
    if !base.is_object() || !matches!(kind, "character" | "lorebook") {
        return Err(InvalidResource);
    }
    let data = if incoming["data"].is_object() {
        &incoming["data"]
    } else {
        incoming
    };
    let target = if base["data"].is_object() {
        &mut base["data"]
    } else {
        &mut base
    };
    let mut review = Review {
        omitted,
        choices: Vec::new(),
    };
    review.fields(
        target,
        data.as_object().ok_or(InvalidResource)?,
        "field",
        if character {
            "character_book"
        } else {
            "entries"
        },
    );
    if character {
        if let Some(book) = data["character_book"].as_object() {
            if target["character_book"].is_null() {
                target["character_book"] = json!({})
            }
            let dest = &mut target["character_book"];
            if !dest.is_object() {
                return Err(InvalidResource);
            }
            review.fields(dest, book, "book", "entries");
            review.entries(dest, &data["character_book"]["entries"])?;
        }
    } else {
        review.entries(target, &data["entries"])?
    }
    Ok(json!({"card":base.to_string(),"changes":review.choices}))
}
