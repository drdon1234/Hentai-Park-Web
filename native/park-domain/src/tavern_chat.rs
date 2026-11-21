use super::tavern::InvalidResource;
use serde_json::{json, Value};
use std::collections::HashSet;
type Result<T> = std::result::Result<T, InvalidResource>;

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or_default()
}
fn fields(v: &Value) -> &Value {
    v.get("data").filter(|x| x.is_object()).unwrap_or(v)
}
fn opts<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&v["extensions"][k])
}
fn num(v: &Value, k: &str, fallback: i64) -> i64 {
    opts(v, k).as_i64().unwrap_or(fallback)
}
fn strings(v: &Value) -> impl Iterator<Item = &str> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}
fn keys(e: &Value) -> &Value {
    e.get("keys").unwrap_or(&e["key"])
}
fn secondary(e: &Value) -> &Value {
    e.get("secondary_keys")
        .or_else(|| e.get("keysecondary"))
        .unwrap_or(&e["secondaryKeys"])
}
fn unsupported(e: &Value) -> bool {
    let pos = opts(e, "position");
    strings(keys(e))
        .chain(strings(secondary(e)))
        .any(|s| s.starts_with('/') && s[1..].contains('/'))
        || !pos.is_null() && !(pos == "before_char" || pos == "after_char" || pos == 0 || pos == 1)
        || [
            "sticky",
            "cooldown",
            "delay",
            "group",
            "use_regex",
            "delayUntilRecursion",
            "matchPersonaDescription",
            "matchCharacterDescription",
            "matchCharacterPersonality",
            "matchCharacterDepthPrompt",
            "matchScenario",
            "matchCreatorNotes",
        ]
        .iter()
        .any(|k| {
            let v = opts(e, k);
            !v.is_null() && v != false && v != 0 && v != ""
        })
}
fn word(c: Option<char>) -> bool {
    c.is_some_and(|c| c == '_' || c.is_alphanumeric())
}
fn keyword(context: &str, key: &str, whole: bool, case: bool) -> bool {
    let (hay, needle) = if case {
        (context.to_owned(), key.trim().to_owned())
    } else {
        (context.to_lowercase(), key.trim().to_lowercase())
    };
    !needle.is_empty()
        && hay.match_indices(&needle).any(|(i, _)| {
            !whole
                || !word(hay[..i].chars().next_back())
                    && !word(hay[i + needle.len()..].chars().next())
        })
}
fn matches<'a>(e: &'a Value, context: &str, terms: &Value) -> (&'static str, &'a str) {
    if e["enabled"] == false || e["disable"] == true {
        return ("disabled", "");
    }
    if text(e, "content").trim().is_empty() {
        return ("empty", "");
    }
    if e["constant"] == true {
        return ("constant", "");
    }
    let whole = opts(e, "matchWholeWords") == true || opts(e, "match_whole_words") == true;
    let sensitive = opts(e, "caseSensitive") == true || opts(e, "case_sensitive") == true;
    let found = |k: &str| {
        let term = &terms[k.trim().to_lowercase()];
        std::iter::once(k)
            .chain(strings(&term["aliases"]))
            .chain(term["translation"].as_str())
            .any(|s| keyword(context, s, whole, sensitive))
    };
    let Some(primary) = strings(keys(e)).find(|k| found(k)) else {
        return ("miss", "");
    };
    let second = secondary(e);
    if e["selective"] == true && strings(second).next().is_some() {
        let pass = match num(e, "selectiveLogic", 0) {
            1 => !strings(second).all(found),
            2 => !strings(second).any(found),
            3 => strings(second).all(found),
            _ => strings(second).any(found),
        };
        if !pass {
            return ("secondary", primary);
        }
    }
    ("keyword", primary)
}
fn order(e: &Value) -> i64 {
    e["insertion_order"]
        .as_i64()
        .or(e["order"].as_i64())
        .unwrap_or(100)
}
fn cost(s: &str) -> usize {
    s.len() + 8
}
struct Lore {
    before: Vec<String>,
    after: Vec<String>,
    explanations: Vec<Value>,
}
impl Lore {
    fn scan(
        &mut self,
        book: &Value,
        history: &[Value],
        max: usize,
        terms: &Value,
        roll: &mut impl FnMut() -> f64,
    ) {
        let entries: Vec<_> = match &book["entries"] {
            Value::Array(a) => a.iter().collect(),
            Value::Object(o) => o.values().collect(),
            _ => return,
        };
        let limit = num(book, "token_budget", 2048).clamp(0, (max / 2) as i64) as usize;
        let mut used = 0;
        let mut selected = Vec::<usize>::new();
        let mut decided = HashSet::new();
        let mut reasons = vec![("miss", "", 0usize); entries.len()];
        let mut recurse = String::new();
        for round in 1..=if book["recursive_scanning"] == true {
            32
        } else {
            1
        } {
            let mut ready = Vec::new();
            for (i, &entry) in entries.iter().enumerate() {
                if decided.contains(&i) {
                    continue;
                }
                if unsupported(entry) {
                    reasons[i] = ("unsupported", "", round);
                    decided.insert(i);
                    continue;
                }
                if round > 1 && opts(entry, "excludeRecursion") == true {
                    continue;
                }
                let depth = num(
                    entry,
                    "scanDepth",
                    num(entry, "scan_depth", num(book, "scan_depth", 4)),
                )
                .clamp(0, 1000) as usize;
                let mut context = history[history.len().saturating_sub(depth)..]
                    .iter()
                    .map(|m| text(m, "content"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !recurse.is_empty() {
                    context.push('\n');
                    context.push_str(&recurse)
                }
                let (reason, key) = matches(entry, &context, terms);
                reasons[i] = (reason, key, round);
                if matches!(reason, "constant" | "keyword") {
                    ready.push(i)
                }
            }
            if ready.is_empty() {
                break;
            }
            ready.sort_by_key(|i| {
                let e = entries[*i];
                (
                    e["constant"] != true,
                    std::cmp::Reverse(num(e, "priority", 0)),
                    order(e),
                )
            });
            for i in ready {
                let e = entries[i];
                decided.insert(i);
                if opts(e, "useProbability") == true
                    && roll() * 100.0 >= num(e, "probability", 100).clamp(0, 100) as f64
                {
                    reasons[i].0 = "probability";
                    continue;
                }
                let n = cost(text(e, "content"));
                if used + n > limit {
                    reasons[i].0 = "budget";
                    continue;
                }
                used += n;
                selected.push(i);
            }
            recurse = selected
                .iter()
                .map(|i| entries[*i])
                .filter(|e| opts(e, "preventRecursion") != true)
                .map(|e| text(e, "content"))
                .collect::<Vec<_>>()
                .join("\n");
        }
        selected.sort_by_key(|i| order(entries[*i]));
        for i in selected {
            let e = entries[i];
            let pos = opts(e, "position");
            (if pos == "before_char" || pos == 0 {
                &mut self.before
            } else {
                &mut self.after
            })
            .push(text(e, "content").to_owned());
        }
        for (e, (reason, key, round)) in entries.iter().zip(reasons) {
            self.explanations.push(json!({"entry":e["comment"].as_str().or(e["name"].as_str()).or_else(||strings(keys(e)).next()).unwrap_or(""),
                "reason":reason,"key":key,"round":round}));
        }
    }
}
fn speaking(s: &Value) -> &Value {
    s["cast"]
        .get(text(s, "speaker"))
        .filter(|v| v.is_object())
        .unwrap_or(&s["card"])
}

pub fn new_session(id: &str, uid: &str, title: &str, card: Value, greeting: usize) -> Value {
    let data = fields(&card);
    let mut messages = Vec::new();
    if let Some(greeting) = std::iter::once(text(data, "first_mes"))
        .chain(strings(&data["alternate_greetings"]))
        .filter(|x| !x.trim().is_empty())
        .nth(greeting)
    {
        messages.push(json!({"role":"assistant","name":data["name"],"content":greeting.replace("{{char}}",text(data,"name")).replace("{{user}}","User")}));
    }
    json!({"id":id,"resourceUid":uid,"title":title,"card":card,"messages":messages,"books":[],"userName":"User","persona":"",
        "updatedAt":0,"inputDraft":"","alternatives":{}})
}
pub fn branch(session: &Value, id: &str, index: usize) -> Result<Value> {
    let mut copy = session.clone();
    let messages = copy["messages"].as_array_mut().ok_or(InvalidResource)?;
    if index >= messages.len() {
        return Err(InvalidResource);
    }
    messages.truncate(index + 1);
    if let Some(a) = copy["alternatives"].as_object_mut() {
        a.retain(|k, _| k.parse::<usize>().is_ok_and(|i| i <= index));
    }
    copy["parentId"] = session["id"].clone();
    copy["id"] = id.into();
    copy["inputDraft"] = "".into();
    copy.as_object_mut()
        .ok_or(InvalidResource)?
        .remove("pendingReply");
    Ok(copy)
}
pub fn apply(s: &mut Value, op: &Value) -> Result<()> {
    let action = text(op, "action");
    if !s["pendingReply"].is_null() && !matches!(action, "checkpoint" | "finish" | "recover") {
        return Err(InvalidResource);
    }
    let string = |key: &str| op[key].as_str().ok_or(InvalidResource);
    match action {
        "input" => s["inputDraft"] = string("text")?.into(),
        "details" => {
            for key in ["notes", "memory", "tags", "persona", "userName"] {
                if let Some(v) = op.get(key) {
                    s[key] = v.as_str().ok_or(InvalidResource)?.into()
                }
            }
        }
        "link" => {
            let id = string("uid")?;
            let books = s["books"].as_array_mut().ok_or(InvalidResource)?;
            books.retain(|x| x != id);
            if op["enabled"] == true {
                books.push(id.into())
            }
        }
        "speaker" => {
            let id = string("uid")?;
            if !id.is_empty() {
                if !s["cast"].is_object() {
                    s["cast"] = json!({})
                }
                if s["cast"].get(id).is_none() {
                    if !op["card"].is_object() || s["cast"].as_object().unwrap().len() >= 20 {
                        return Err(InvalidResource);
                    }
                    s["cast"][id] = op["card"].clone();
                }
            }
            s["speaker"] = id.into();
        }
        "begin" => {
            let mode = string("mode")?;
            let speaker = text(fields(speaking(s)), "name").to_owned();
            let user = s["userName"].as_str().unwrap_or("User").to_owned();
            let messages = s["messages"].as_array_mut().ok_or(InvalidResource)?;
            let name = match mode {
                "reply" => {
                    let input = string("text")?.trim();
                    if input.is_empty() {
                        return Err(InvalidResource);
                    }
                    messages.push(json!({"role":"user","content":input,"name":user}));
                    s["inputDraft"] = "".into();
                    speaker
                }
                "regenerate" | "continuation" => {
                    let last = messages
                        .last()
                        .filter(|m| m["role"] == "assistant")
                        .ok_or(InvalidResource)?;
                    last["name"].as_str().unwrap_or(&speaker).to_owned()
                }
                _ => return Err(InvalidResource),
            };
            s["pendingReply"] = json!({"mode":mode,"content":"","name":name});
        }
        "checkpoint" => {
            if !s["pendingReply"].is_object() {
                return Err(InvalidResource);
            }
            s["pendingReply"]["content"] = string("text")?.into();
        }
        "finish" | "recover" => {
            let Some(pending) = s
                .as_object_mut()
                .ok_or(InvalidResource)?
                .remove("pendingReply")
            else {
                return Ok(());
            };
            let content = op["text"].as_str().unwrap_or(text(&pending, "content"));
            if content.is_empty() {
                return Ok(());
            }
            let status = if action == "recover" {
                "cancelled"
            } else {
                string("status")?
            };
            let mut response = json!({"role":"assistant","name":pending["name"],"content":content,"status":status});
            let messages = s["messages"].as_array_mut().ok_or(InvalidResource)?;
            match text(&pending, "mode") {
                "regenerate" => {
                    let last = messages
                        .last_mut()
                        .filter(|m| m["role"] == "assistant")
                        .ok_or(InvalidResource)?;
                    let alternate = if status == "complete" {
                        std::mem::replace(last, response)
                    } else {
                        response
                    };
                    let key = (messages.len() - 1).to_string();
                    if !s["alternatives"].is_object() {
                        s["alternatives"] = json!({})
                    }
                    let variants = &mut s["alternatives"][key];
                    if variants.is_null() {
                        *variants = json!([])
                    }
                    variants
                        .as_array_mut()
                        .ok_or(InvalidResource)?
                        .push(alternate);
                }
                "continuation" => {
                    let last = messages
                        .last_mut()
                        .filter(|m| m["role"] == "assistant")
                        .ok_or(InvalidResource)?;
                    response["content"] = format!("{}{content}", text(last, "content")).into();
                    *last = response;
                }
                _ => messages.push(response),
            }
        }
        "alternative" => {
            let idx = op["index"].as_u64().ok_or(InvalidResource)? as usize;
            let original = s["messages"]
                .get(idx)
                .filter(|m| m["role"] == "assistant")
                .ok_or(InvalidResource)?
                .clone();
            let variants = s["alternatives"]
                .get_mut(idx.to_string())
                .and_then(Value::as_array_mut)
                .filter(|a| !a.is_empty())
                .ok_or(InvalidResource)?;
            variants.push(original);
            s["messages"][idx] = variants.remove(0);
        }
        _ => return Err(InvalidResource),
    }
    Ok(())
}
pub fn prompt(
    s: &Value,
    books: &[Value],
    config: &Value,
    terms: &Value,
    mode: &str,
    mut roll: impl FnMut() -> f64,
) -> Result<Value> {
    let all = s["messages"].as_array().ok_or(InvalidResource)?;
    let end = all
        .len()
        .checked_sub(usize::from(mode == "regenerate"))
        .ok_or(InvalidResource)?;
    let history = &all[..end];
    let context = num(config, "contextTokens", 65536).clamp(1, 2_000_000) as usize;
    let budget = context
        .checked_sub(num(config, "maxTokens", 4096).clamp(1, 32768) as usize + 256)
        .ok_or(InvalidResource)?;
    let active = speaking(s);
    let selected_card = if matches!(mode, "regenerate" | "continuation") {
        let name = all.last().and_then(|m| m["name"].as_str());
        std::iter::once(&s["card"])
            .chain(s["cast"].as_object().into_iter().flat_map(|o| o.values()))
            .find(|c| name.is_some() && fields(c)["name"].as_str() == name)
            .unwrap_or(active)
    } else {
        active
    };
    let card = fields(selected_card);
    let user = s["userName"].as_str().unwrap_or("User");
    let replace = |x: &str| {
        x.replace("{{char}}", text(card, "name"))
            .replace("{{user}}", user)
    };
    let mut lore = Lore {
        before: vec![],
        after: vec![],
        explanations: vec![],
    };
    lore.scan(&card["character_book"], history, context, terms, &mut roll);
    for book in books {
        lore.scan(
            if book["entries"].is_null() {
                fields(book)
            } else {
                book
            },
            history,
            context,
            terms,
            &mut roll,
        )
    }
    let default="Write the next reply as the fictional character. Respect the user persona and story context.";
    let mut system = vec![replace(if text(card, "system_prompt").trim().is_empty() {
        default
    } else {
        text(card, "system_prompt")
    })
    .replace("{{original}}", default)];
    system.extend(lore.before.iter().map(|x| replace(x)));
    for (label, value) in [
        ("Character", text(card, "name")),
        ("Description", text(card, "description")),
        ("Personality", text(card, "personality")),
        ("Scenario", text(card, "scenario")),
        ("Example dialogue", text(card, "mes_example")),
        ("User name", user),
        ("User persona", text(s, "persona")),
        ("Story memory", text(s, "memory")),
    ] {
        if !value.is_empty() {
            system.push(format!("{label}: {}", replace(value)))
        }
    }
    if let Some(cast) = s["cast"].as_object().filter(|o| !o.is_empty()) {
        let others = std::iter::once(&s["card"])
            .chain(cast.values())
            .filter(|c| *c != speaking(s))
            .map(|c| {
                let d = fields(c);
                format!("{}: {}", text(d, "name"), text(d, "description"))
            })
            .collect::<Vec<_>>();
        if !others.is_empty() {
            system.push(format!("Other characters: {}", others.join("\n")))
        }
    }
    system.extend(lore.after.iter().map(|x| replace(x)));
    if !text(config, "systemPrompt").is_empty() {
        system.push(text(config, "systemPrompt").to_owned())
    }
    let first = system.join("\n\n");
    let mut used = cost(&first);
    let mut tail = Vec::new();
    let post = replace(text(card, "post_history_instructions")).replace("{{original}}", "");
    if !post.trim().is_empty() {
        used += cost(&post);
        tail.push(json!({"role":"system","content":post}))
    }
    if mode == "continuation" {
        let msg = "Continue the last reply from where it stopped.";
        used += cost(msg);
        tail.push(json!({"role":"user","content":msg}))
    }
    let mut selected = Vec::new();
    for m in history.iter().rev() {
        let mut body = text(m, "content").to_owned();
        if s["cast"].as_object().is_some_and(|o| !o.is_empty()) && !text(m, "name").is_empty() {
            body = format!("{}: {body}", text(m, "name"))
        }
        used += cost(&body);
        if used > budget {
            break;
        }
        selected.push(json!({"role":m["role"],"content":body}));
    }
    if selected.is_empty() || mode != "continuation" && selected[0]["role"] != "user" {
        return Err(InvalidResource);
    }
    let trimmed = selected.len() < history.len();
    selected.reverse();
    let mut messages = vec![json!({"role":"system","content":first})];
    messages.extend(selected);
    messages.extend(tail);
    Ok(json!({"messages":messages,"explanations":lore.explanations,"trimmed":trimmed}))
}
