use super::*;
use crate::PageImage;
use park_media::{ImagePool, RemoteImages};
use serde_json::{json, Value};

#[derive(uniffi::Enum)]
pub enum SitePreference {
    Like { enabled: bool },
    Favorite { enabled: bool },
    Rating { stars: f64 },
}
fn valid_stars(n: f64) -> bool {
    n.is_finite() && (0.5..=5.0).contains(&n) && (2.0 * n).fract() == 0.0
}

#[derive(uniffi::Enum)]
pub enum BrowseMethod {
    Feed,
    Search,
    Detail,
    Related,
    Ranking,
    RankingOptions,
    MetadataSearch,
    MetadataDetail,
    FilterOptions,
    SearchSuggestions,
    VoiceDirectory,
}
impl BrowseMethod {
    fn protocol(&self) -> (&'static str, usize, usize) {
        match self {
            Self::Feed => ("feed", 2, 2),
            Self::Search => ("search", 1, 1),
            Self::Detail => ("detail", 1, 2),
            Self::Related => ("related", 2, 2),
            Self::Ranking => ("ranking", 2, 2),
            Self::RankingOptions => ("rankingOptions", 1, 1),
            Self::MetadataSearch => ("metadataSearch", 1, 2),
            Self::MetadataDetail => ("metadataDetail", 1, 2),
            Self::FilterOptions => ("filterOptions", 1, 1),
            Self::SearchSuggestions => ("searchSuggestions", 1, 2),
            Self::VoiceDirectory => ("voiceDirectory", 3, 3),
        }
    }
}

#[derive(Object)]
pub struct SourceImages {
    loader: Arc<RemoteImages>,
    pool: Arc<ImagePool>,
    headers: Value,
    cache: PathBuf,
    source: String,
    cipher: Value,
    limit: u64,
}
#[uniffi::export]
impl SourceImages {
    pub fn load(&self, address: String, request: Arc<Request>) -> Result<Arc<PageImage>> {
        let disk = park_media::image_disk::ImageCacheEntry::new(
            &self.cache,
            &self.source,
            &address,
            &self.cipher,
            self.limit,
        );
        let ticket = self
            .loader
            .load_url(&address, self.headers.clone(), disk.clone())?;
        Ok(Arc::new(PageImage {
            asset: ticket.wait(|| request.cancelled.is_cancelled())?,
            disk: Some(disk),
            pool: self.pool.clone(),
            key: ticket.key().clone(),
        }))
    }
}

#[uniffi::export]
impl SourceService {
    pub fn site_interactions(
        &self,
        source: String,
        work: String,
        change: Option<SitePreference>,
        request: Arc<Request>,
    ) -> Result<String> {
        let call = plugins::budget(&request);
        if self.catalog.manifest(&source, call.clone())?["capabilities"]["interaction"] != true {
            return Err(SourceFailure::Unsupported);
        }
        let change = match change {
            Some(SitePreference::Like { enabled }) => json!({"type":"like","value":enabled}),
            Some(SitePreference::Favorite { enabled }) => {
                json!({"type":"favorite","value":enabled})
            }
            Some(SitePreference::Rating { stars }) if valid_stars(stars) => {
                json!({"type":"rating","value":stars})
            }
            Some(_) => return Err(SourceFailure::Unsupported),
            None => json!({}),
        };
        let (_, raw) =
            self.catalog
                .invoke(&source, "interaction", &[work.into(), change.clone()], call)?;
        let data = downloads::checked(raw)?;
        let rows = data
            .as_array()
            .filter(|r| !r.is_empty() && r.len() <= 3)
            .ok_or(SourceFailure::Unsupported)?;
        let mut seen = std::collections::HashSet::new();
        let mut confirmed = change["type"].is_null();
        for row in rows {
            let kind = row["type"].as_str().ok_or(SourceFailure::Unsupported)?;
            let value = &row["value"];
            let valid = match kind {
                "like" | "favorite" => {
                    value.is_boolean() && row.get("canUnset").is_none_or(Value::is_boolean)
                }
                "rating" => {
                    row.get("value").is_some()
                        && (value.is_null() || value.as_f64().is_some_and(valid_stars))
                }
                _ => false,
            };
            if !valid || !seen.insert(kind) {
                return Err(SourceFailure::Unsupported);
            }
            if row["type"] == change["type"] {
                confirmed = if kind == "rating" {
                    value.as_f64() == change["value"].as_f64()
                } else {
                    *value == change["value"]
                };
            }
        }
        if !confirmed {
            return Err(SourceFailure::Unavailable);
        }
        Ok(data.to_string())
    }
    pub fn discovery_sources(&self, request: Arc<Request>) -> Result<String> {
        let mut result = Vec::new();
        for entry in self.catalog.installed()? {
            let call = plugins::budget(&request);
            call.check()?;
            match self.catalog.manifest(&entry.key, call) {
                Ok(manifest) => result.push(json!({
                    "key":entry.key, "name":manifest["name"], "category":manifest["category"],
                    "feed":manifest["feeds"].as_array().is_some_and(|rows| !rows.is_empty()),
                    "entryFeed":manifest["feeds"].as_array().and_then(|feeds|
                        feeds.iter().find(|feed| feed["key"] == manifest["defaultFeed"]).or_else(|| feeds.first())),
                    "reverseOnly":manifest["reverseSearchOnly"],
                    "search":manifest["capabilities"]["search"],
                    "detail":manifest["capabilities"]["detail"],
                    "nativeVideo":manifest["capabilities"]["videos"],
                    "metadataSearch":manifest["capabilities"]["metadataSearch"],
                    "metadataDetail":manifest["capabilities"]["metadataDetail"],
                    "searchSuggestions":manifest["capabilities"]["searchSuggestions"],
                    "rankings":manifest["capabilities"]["rankings"],
                    "rankingOnly":manifest["rankingOnly"],
                    "metadata":manifest["metadataOnly"], "reverse":manifest["capabilities"]["reverseSearch"],
                    "reverseCategory":manifest["reverseSearchCategory"]
                })),
                Err(park_runtime::Error::Cancelled) => return Err(SourceFailure::Cancelled),
                Err(_) => result.push(json!({"key":entry.key,"unavailable":true})),
            }
        }
        Ok(Value::Array(result).to_string())
    }
    pub fn source_manifest(&self, source: String, request: Arc<Request>) -> Result<String> {
        Ok(self
            .catalog
            .manifest(&source, plugins::budget(&request))?
            .to_string())
    }
    pub fn browse(
        &self,
        source: String,
        method: BrowseMethod,
        arguments: String,
        request: Arc<Request>,
    ) -> Result<BrowseResult> {
        let (method, minimum, maximum) = method.protocol();
        if arguments.len() > 2 * 1024 * 1024 {
            return Err(SourceFailure::Unsupported);
        }
        let args: Vec<Value> =
            serde_json::from_str(&arguments).map_err(|_| SourceFailure::Unsupported)?;
        if !(minimum..=maximum).contains(&args.len()) {
            return Err(SourceFailure::Unsupported);
        }
        let trace = Arc::new(park_net::ReadScope::observe());
        let (manifest, response, digest) = self.catalog.invoke_identified(&source, method, &args,
            plugins::budget(&request).with_read_scope(trace.clone()), None)?;
        if response["ok"] == false && matches!(response["error"]["code"].as_str(),
            Some("CONFIRM_REQUIRED" | "VERIFICATION_REQUIRED")) {
            if let Some(page) = manual_read::confirmation_page(&manifest, &response["error"]["url"]) {
                let scope = trace.replay(&page).map(Arc::new);
                return Ok(BrowseResult { content: None, confirmation: Some(Arc::new(ManualRead {
                    page, source, digest, method, args, catalog: self.catalog.clone(), scope,
                    consumed: std::sync::atomic::AtomicBool::new(false),
                })) });
            }
        }
        Ok(BrowseResult { content: Some(downloads::checked(response)?.to_string()), confirmation: None })
    }
    pub fn source_images(
        &self,
        source: String,
        store: Arc<crate::ProgressStore>,
        request: Arc<Request>,
    ) -> Result<Arc<SourceImages>> {
        let manifest = self.catalog.manifest(&source, plugins::budget(&request))?;
        let domains = manifest["domains"]
            .as_array()
            .ok_or(SourceFailure::Unsupported)?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let loader = RemoteImages::new(
            store.images.clone(),
            self.routes.transport.clone(),
            &self.routes.sessions,
            &source,
            domains,
            self.routes.cache.clone(),
            &manifest["imageDecrypt"],
        )?;
        Ok(Arc::new(SourceImages {
            loader,
            pool: store.images.clone(),
            headers: manifest["imageHeaders"].clone(),
            source,
            cache: self.routes.cache.clone(),
            cipher: manifest["imageDecrypt"].clone(),
            limit: store.image_cache_limit,
        }))
    }
}
