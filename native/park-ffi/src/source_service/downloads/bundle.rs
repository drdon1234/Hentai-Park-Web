use super::*;
use park_media::{RemoteImages, ResolvedImage, Route};
use sha2::{Digest, Sha256};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::collections::{HashMap, HashSet};

fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
fn field<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str()
        .filter(|s| !s.is_empty())
        .ok_or(SourceFailure::Unsupported)
}
fn read_local(dir: &Path, name: &str, max: u64) -> Result<Option<Value>> {
    let path = dir.join(name);
    match fs::symlink_metadata(&path) {
        Ok(s) if s.is_file() && s.len() <= max => Ok(Some(
            serde_json::from_reader(
                fs::File::open(path)
                    .map_err(|_| SourceFailure::Storage)?
                    .take(max + 1),
            )
            .map_err(|_| SourceFailure::Storage)?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        _ => Err(SourceFailure::Storage),
    }
}
struct Contents {
    entries: Vec<Value>,
    blocks: Vec<Value>,
    inputs: HashMap<String, Value>,
}
impl Contents {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            blocks: Vec::new(),
            inputs: HashMap::new(),
        }
    }
    fn add(&mut self, entry: Value, raw: Value) -> Result<()> {
        let id = field(&entry, "id")?.to_owned();
        if self.inputs.contains_key(&id) {
            return Ok(());
        }
        if self.entries.len() >= 10000 {
            return Err(SourceFailure::Unsupported);
        }
        self.entries.push(entry);
        self.inputs.insert(id, raw);
        Ok(())
    }
    fn article(&mut self, detail: &Value, article: bool) -> Result<()> {
        if !article {
            if let Some(s) = detail["text"]
                .as_str()
                .or(detail["description"].as_str())
                .filter(|v| !v.is_empty())
            {
                self.blocks.push(json!({"type":"text","text":s}));
            }
        }
        let raw = detail[if article { "blocks" } else { "media" }].as_array();
        if article && raw.is_none() {
            return Err(SourceFailure::Unsupported);
        }
        for (i, raw) in raw.into_iter().flatten().enumerate() {
            let kind = field(raw, "type")?;
            if kind == "text" {
                self.blocks.push(json!({"type":"text","text":raw["text"]}));
                continue;
            }
            if !matches!(kind, "image" | "video" | "audio") {
                return Err(SourceFailure::Unsupported);
            }
            let id = if article {
                format!(
                    "{kind}:{}",
                    raw["id"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| raw["id"].as_i64().map(|v| v.to_string()))
                        .unwrap_or_else(|| i.to_string())
                )
            } else {
                format!("media:{i}")
            };
            let episode = raw["episodeId"].as_str().or_else(|| {
                if article && kind == "video" {
                    raw["id"].as_str()
                } else {
                    None
                }
            });
            let url = raw["url"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(Url::parse)
                .transpose()
                .map_err(|_| SourceFailure::Unsupported)?;
            if url.as_ref().is_some_and(|u| !park_net::public_https(u)) {
                return Err(SourceFailure::Unsupported);
            }
            let origin = if raw["resource"].is_object() {
                json!({"cacheKey":raw["resource"]["cacheKey"],"size":raw["resource"]["size"]})
            } else {
                json!(url.map(|u| format!("{}{}", u.host_str().unwrap_or(""), u.path())))
            };
            let mut entry = json!({"id":id,"name":raw["name"].as_str().map(str::to_owned).unwrap_or_else(||format!("{} {}",detail["title"].as_str().unwrap_or(""),i+1)),
                "type":kind,"identity":digest(&json!([origin,episode]).to_string())});
            if raw["previewOnly"] == true {
                entry["previewOnly"] = true.into();
            }
            let mut input = raw.clone();
            if let Some(ep) = episode {
                input["episodeId"] = json!(ep);
            }
            self.add(entry, input)?;
            self.blocks.push(json!({"type":kind,"asset":id}));
        }
        Ok(())
    }
}
impl ChapterJob<'_> {
    fn bundle_detail(&self, manifest: &Value, item: &Value) -> Result<Value> {
        if manifest["capabilities"]["detail"] == true {
            return self.call("detail", &[self.work.into(), item.clone()]);
        }
        let scope = &item["downloadScope"];
        let mut cursor = Value::Null;
        let mut seen = HashSet::new();
        for page in 1..=1000 {
            let list = if manifest["capabilities"]["album"] == true {
                self.call(
                    "subscription.media",
                    &[scope.clone(), json!({"page":page,"cursor":cursor})],
                )?
            } else {
                self.call(
                    "subscription.posts",
                    &[field(scope, "target")?.into(), json!({"cursor":cursor})],
                )?
            };
            if let Some(item) = list["items"]
                .as_array()
                .ok_or(SourceFailure::Unsupported)?
                .iter()
                .find(|i| i["id"] == self.work)
            {
                return Ok(item.clone());
            }
            if list["hasNext"] != true {
                break;
            }
            cursor = list["cursor"].clone();
            if !cursor.is_null() && !seen.insert(cursor.to_string()) {
                return Err(SourceFailure::Unsupported);
            }
        }
        Err(SourceFailure::Unavailable)
    }
    fn voice_tree(&self, contents: &mut Contents, node: &Value) -> Result<()> {
        let mut pending = vec![(node.clone(), None::<String>, 0)];
        let mut visited = HashSet::new();
        while let Some((node, parent, depth)) = pending.pop() {
            let id = field(&node, "id")?;
            let kind = field(&node, "type")?;
            if depth > 64 || visited.len() >= 10000 || !visited.insert(id.to_owned()) {
                return Err(SourceFailure::Unsupported);
            }
            let mut e = json!({"id":id,"name":field(&node,"name")?,"type":kind,"parent":parent});
            if node["subtitleIds"].is_array() {
                e["subtitleIds"] = node["subtitleIds"].clone();
            }
            contents.add(e, json!({"voice":id}))?;
            if kind != "directory" {
                continue;
            }
            let children = if node["childrenComplete"] != false && node["children"].is_array() {
                node["children"].as_array().unwrap().clone()
            } else {
                let mut out = Vec::new();
                let mut children_seen = HashSet::new();
                for page in 1..=1000 {
                    let dir = self.call(
                        "voiceDirectory",
                        &[self.work.into(), id.into(), json!({"page":page})],
                    )?;
                    let before = out.len();
                    for child in dir["items"].as_array().ok_or(SourceFailure::Unsupported)? {
                        if children_seen.insert(field(child, "id")?.to_owned()) {
                            out.push(child.clone());
                        }
                    }
                    if out.len() + visited.len() > 10000 {
                        return Err(SourceFailure::Unsupported);
                    }
                    if dir["hasNext"] != true {
                        break;
                    }
                    if before == out.len() || page == 1000 {
                        return Err(SourceFailure::Unsupported);
                    }
                }
                out
            };
            for child in children.into_iter().rev() {
                pending.push((child, Some(id.to_owned()), depth + 1));
            }
        }
        for id in node["subtitleIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            contents.add(
                json!({"id":id,"name":id,"type":"subtitle"}),
                json!({"voice":id}),
            )?;
        }
        Ok(())
    }
    fn voice(&self, contents: &mut Contents, detail: &Value, node: &Value) -> Result<()> {
        if let Some(node) = Some(node)
            .filter(|n| n.as_object().is_some_and(|m| !m.is_empty()))
            .or_else(|| detail.get("voiceRoot").filter(|root| root.is_object()))
        {
            return self.voice_tree(contents, node);
        }
        let fallback = vec![
            json!({"id":detail["singleChapterId"].as_str().unwrap_or("_"),"title":detail["title"]}),
        ];
        for track in detail["chapters"]
            .as_array()
            .filter(|a| !a.is_empty())
            .unwrap_or(&fallback)
        {
            let id = field(track, "id")?;
            let kind = detail["voiceTrackKinds"][id].as_str().unwrap_or(
                if detail["voiceMediaForm"] == "videoOnly" {
                    "video"
                } else {
                    "audio"
                },
            );
            let subs = detail["voiceSubtitles"][id]
                .as_array()
                .cloned()
                .unwrap_or_default();
            contents.add(
                json!({"id":id,"name":track["title"],"type":kind,"subtitleIds":subs}),
                json!({"episodeId":id}),
            )?;
            for id in subs.iter().filter_map(Value::as_str) {
                contents.add(
                    json!({"id":id,"name":id,"type":"subtitle"}),
                    json!({"voice":id}),
                )?;
            }
        }
        Ok(())
    }
    fn save_bundle(&self, doc: &Value) -> Result<()> {
        let bytes = serde_json::to_vec(doc).map_err(|_| SourceFailure::Storage)?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(SourceFailure::Unsupported);
        }
        self.write(|dir| atomic(dir, "bundle.json", |f| f.write_all(&bytes)))
    }
    pub(super) fn bundle(&self, plan: &queue::DownloadPlan) -> Result<u64> {
        let manifest = self.service.catalog.manifest(
            self.source,
            Invocation::child(Duration::from_secs(45), &self.request.cancelled),
        )?;
        let detail = self.bundle_detail(&manifest, &plan.item)?;
        if detail.get("id").is_some_and(|id| id != self.work) {
            return Err(SourceFailure::Unsupported);
        }
        let mut contents = Contents::new();
        let voice = plan.item["mediaKind"] == "voice" || manifest["type"] == "voice";
        if voice {
            self.voice(&mut contents, &detail, &plan.bundle_nodes[self.chapter])?;
        } else {
            contents.article(
                &detail,
                manifest["type"] == "article" || detail["blocks"].is_array(),
            )?;
        }
        if contents.entries.is_empty() && contents.blocks.is_empty() {
            return Err(SourceFailure::Unavailable);
        }
        let dir = self.write(|d| Ok(d.to_owned()))?;
        let old = read_local(&dir, "bundle.json", 16 * 1024 * 1024)?;
        let prior: HashMap<_, _> = old
            .as_ref()
            .and_then(|d| d["entries"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|e| Some((e["id"].as_str()?, e)))
            .collect();
        let mut total = 0u64;
        for entry in &mut contents.entries {
            if let Some(old) = prior
                .get(field(entry, "id")?)
                .filter(|v| v["identity"] == entry["identity"] && v["type"] == entry["type"])
            {
                if let Ok(file) = crate::offline::saved_attachment(&dir, old) {
                    total += file_size(&file)?;
                    for k in ["file", "size", "format"] {
                        entry[k] = old[k].clone();
                    }
                }
            }
        }
        let preview =
            detail["locked"] == true || contents.entries.iter().any(|e| e["previewOnly"] == true);
        let mut doc = json!({"kind":if voice{json!("voice")}else{manifest["type"].clone()},"title":detail["title"],"entries":contents.entries,"blocks":contents.blocks,"complete":false,"previewOnly":preview});
        self.save_bundle(&doc)?;
        for i in 0..doc["entries"].as_array().unwrap().len() {
            let e = &doc["entries"][i];
            if e["file"].is_string() || e["type"] == "directory" {
                continue;
            }
            let id = field(e, "id")?;
            let raw = &contents.inputs[id];
            let (name, bytes) = self.attachment(&manifest, e, raw)?;
            let len = fs::metadata(dir.join(&name))
                .map_err(|_| SourceFailure::Storage)?
                .len();
            if len == 0 {
                return Err(SourceFailure::Unavailable);
            }
            let e = &mut doc["entries"][i];
            e["format"] = json!(name.rsplit('.').next());
            e["file"] = json!(name);
            e["size"] = len.into();
            total = total.checked_add(bytes).ok_or(SourceFailure::Storage)?;
            self.save_bundle(&doc)?;
        }
        doc["complete"] = true.into();
        self.save_bundle(&doc)?;
        Ok(total)
    }
}
fn file_size(file: &crate::BundleFile) -> Result<u64> {
    std::iter::once(&file.path)
        .chain(&file.resources)
        .try_fold(0u64, |n, p| {
            n.checked_add(fs::metadata(p).map_err(|_| SourceFailure::Storage)?.len())
                .ok_or(SourceFailure::Storage)
        })
}
impl ChapterJob<'_> {
    fn attachment(&self, manifest: &Value, entry: &Value, raw: &Value) -> Result<(String, u64)> {
        let kind = field(entry, "type")?;
        let stem = digest(field(entry, "id")?);
        if raw["resource"].is_object() {
            let name = format!("{stem}.{}", if kind == "image" { "img" } else { "mp4" });
            let bytes = self.chunks(&name, &raw["resource"], kind == "image")?;
            return Ok((name, bytes));
        }
        if matches!(kind, "audio" | "video") {
            let line = if raw["url"].is_string() && raw["episodeId"].is_null() {
                VideoSource::from_plugin(
                    manifest,
                    &json!({"url":raw["url"],"headers":raw["headers"],"format":raw["format"].as_str().unwrap_or("mp4"),"preferred":true}),
                )?
            } else {
                let id = raw["episodeId"]
                    .as_str()
                    .or(raw["voice"].as_str())
                    .unwrap_or("_");
                let req = Arc::new(Request {
                    cancelled: self.request.cancelled.child_token(),
                });
                let lines = self.service.video_lines(
                    self.source.to_owned(),
                    self.work.to_owned(),
                    id.to_owned(),
                    req,
                )?;
                park_media::download::select(&lines.grant, &Value::Null, 0, id)?.clone()
            };
            let name = format!("{stem}.{}", park_media::download::extension(&line, true)?);
            let (_, bytes) = self.media(&line, &name, true)?;
            return Ok((name, bytes));
        }
        let voice = raw["voice"].as_str();
        let asset = if let Some(id) = voice {
            self.call("voiceResource", &[self.work.into(), id.into()])?
        } else {
            raw.clone()
        };
        if kind == "image" {
            let name = format!("{stem}.img");
            let domains = manifest["domains"]
                .as_array()
                .ok_or(SourceFailure::Unsupported)?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let remote = RemoteImages::new(
                self.store.images.clone(),
                self.service.routes.transport.clone(),
                &self.service.routes.sessions,
                self.source,
                domains,
                self.service.routes.cache.clone(),
                &manifest["imageDecrypt"],
            )?;
            let defaults = manifest["imageHeaders"].clone();
            let host = Url::parse(field(&asset, "url")?)
                .map_err(|_| SourceFailure::Unsupported)?
                .host_str()
                .ok_or(SourceFailure::Unsupported)?
                .to_owned();
            let image = remote
                .load(
                    &stem,
                    0,
                    false,
                    Route {
                        host,
                        browser_session: None,
                    },
                    Priority::Background,
                    move |_, _| ResolvedImage::from_page(&asset, &[&defaults, &asset["headers"]]),
                )
                .wait(|| self.request.cancelled.is_cancelled())?;
            let size = self.write(|dir| {
                let mut input = io(fs::File::open(image.file.path()))?;
                let len = io(input.metadata())?.len();
                atomic(dir, &name, |f| {
                    std::io::copy(&mut input, f)?;
                    Ok(())
                })?;
                Ok(len)
            })?;
            return Ok((name, size));
        }
        let format = asset["format"]
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 10
                    && s.bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            })
            .unwrap_or("bin");
        let name = format!("{stem}.{format}");
        let line =
            VideoSource::from_plugin(manifest, &json!({"url":asset["url"],"format":format}))?;
        let session = self
            .service
            .routes
            .sessions
            .partition(self.source, Purpose::Media)
            .map_err(|_| SourceFailure::Storage)?;
        let limit = if kind == "subtitle" {
            2 * 1024 * 1024
        } else {
            32 * 1024 * 1024
        };
        let bytes = self
            .service
            .routes
            .runtime
            .block_on(park_media::download::attachment(
                &self.service.routes.transport,
                &session,
                &line,
                limit,
                &self.request.cancelled,
            ))?;
        if bytes.is_empty() {
            return Err(SourceFailure::Unavailable);
        }
        if kind == "subtitle"
            && park_domain::subtitles::parse(
                std::str::from_utf8(&bytes).map_err(|_| SourceFailure::Unsupported)?,
                format,
            )
            .is_empty()
        {
            return Err(SourceFailure::Unsupported);
        }
        self.write(|dir| atomic(dir, &name, |f| f.write_all(&bytes)))?;
        Ok((name, bytes.len() as u64))
    }
    fn chunks(&self, name: &str, resource: &Value, image: bool) -> Result<u64> {
        let size = resource["size"]
            .as_u64()
            .filter(|v| (1..=536870912).contains(v))
            .ok_or(SourceFailure::Unsupported)?;
        let key = field(resource, "cacheKey")?;
        if key.len() > 512 {
            return Err(SourceFailure::Unsupported);
        }
        let part = format!("{name}.{}.part", digest(&json!([key, size]).to_string()));
        let (dir, mut offset) = self.write(|dir| {
            let file = dir.join(&part);
            match fs::symlink_metadata(&file) {
                Ok(m)
                    if m.is_file()
                        && m.len() <= size
                        && (m.len() == size || m.len() % 131072 == 0) =>
                {
                    Ok((dir.to_owned(), m.len()))
                }
                Ok(m) if m.is_file() => {
                    io(fs::File::create(file))?;
                    Ok((dir.to_owned(), 0))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((dir.to_owned(), 0)),
                _ => Err(queue::Error::Invalid),
            }
        })?;
        while offset < size {
            let chunk = self.call(
                "subscription.mediaChunk",
                &[resource.clone(), json!(offset)],
            )?;
            let encoded = field(&chunk, "base64")?;
            if encoded.len() > 174764 {
                return Err(SourceFailure::Unsupported);
            }
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| SourceFailure::Unsupported)?;
            if bytes.len() as u64 != (size - offset).min(131072) {
                return Err(SourceFailure::Unsupported);
            }
            self.write(|dir| {
                let mut file = io(fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join(&part)))?;
                if io(file.metadata())?.len() != offset {
                    return Err(queue::Error::Invalid);
                }
                io(file.write_all(&bytes))?;
                io(file.sync_data())
            })?;
            offset += bytes.len() as u64;
        }
        if image {
            park_media::prepare(
                park_media::PageFile::Original(dir.join(&part)),
                0,
                false,
                &self.service.routes.cache,
                &self.request.cancelled,
            )?;
        }
        self.write(|dir| io(fs::rename(dir.join(&part), dir.join(name))))?;
        Ok(size)
    }
}
