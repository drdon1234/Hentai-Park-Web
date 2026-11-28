use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

// 续载数据只随一次查询存活，不交给平台保存或用于其他站点。
#[derive(Object)]
pub struct ReverseQuery {
    catalog: Arc<SourceCatalog>,
    source: String,
    candidates: ReverseCandidates,
    browser: Option<Value>,
}

#[derive(Object)]
pub struct ReverseCandidates {
    rows: Vec<Value>,
    continuation: Option<Value>,
}
fn invalid<T>() -> Result<T> {
    Err(SourceFailure::Unsupported)
}
fn web(value: &str) -> bool {
    value.len() <= 4096
        && Url::parse(value).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.has_host()
                && u.username().is_empty()
                && u.password().is_none()
        })
}
fn image(value: &str) -> bool {
    if web(value) {
        return true;
    }
    let Some((kind, data)) = value.split_once(',') else {
        return false;
    };
    value.len() <= 524288
        && matches!(
            kind.to_ascii_lowercase().as_str(),
            "data:image/png;base64"
                | "data:image/jpeg;base64"
                | "data:image/webp;base64"
                | "data:image/gif;base64"
        )
        && !data.is_empty()
        && STANDARD.decode(data).is_ok()
}
fn bounded_tree(value: &Value, depth: usize, left: &mut usize) -> bool {
    if depth > 12 || *left == 0 {
        return false;
    }
    *left -= 1;
    match value {
        Value::Array(a) => a.iter().all(|v| bounded_tree(v, depth + 1, left)),
        Value::Object(o) => o.values().all(|v| bounded_tree(v, depth + 1, left)),
        _ => true,
    }
}
fn checked_rows(rows: &[Value]) -> Result<String> {
    for r in rows {
        let string =
            |v: &Value| v.is_null() || v.as_str().is_some_and(|s| s.encode_utf16().count() <= 4096);
        let score = |v: &Value, max: f64| {
            v.is_null() || v.as_f64().is_some_and(|n| (0.0..=max).contains(&n))
        };
        if !r.is_object()
            || !["title", "keyword", "description", "url"]
                .iter()
                .all(|k| string(&r[*k]))
            || r["title"].as_str().unwrap_or("").trim().is_empty()
        {
            return invalid();
        }
        let url = r["url"].as_str().unwrap_or("").trim();
        if (!url.is_empty() && !web(url))
            || (url.is_empty() && r["keyword"].as_str().unwrap_or("").trim().is_empty())
            || !score(&r["similarity"], 100.0)
            || !score(&r["from"], 864000.0)
            || !score(&r["to"], 864000.0)
            || !(r["uncertain"].is_null() || r["uncertain"].is_boolean())
            || !(r["episode"].is_null() || r["episode"].is_number() || r["episode"].is_string())
            || !(r["details"].is_null()
                || r["details"].is_object() && bounded_tree(&r["details"], 0, &mut 4096))
        {
            return invalid();
        }
        if !r["images"].is_null() {
            let images = r["images"].as_array().ok_or(SourceFailure::Unsupported)?;
            if images.len() > 50
                || !images.iter().all(|i| {
                    i["url"].as_str().is_some_and(image)
                        && string(&i["label"])
                        && score(&i["similarity"], 100.0)
                })
            {
                return invalid();
            }
        }
    }
    serde_json::to_string(rows).map_err(|_| SourceFailure::Unsupported)
}
fn take_rows(value: Value, max: usize) -> Result<Vec<Value>> {
    match value {
        Value::Array(rows) if rows.len() <= max && rows.iter().all(Value::is_object) => Ok(rows),
        _ => invalid(),
    }
}
impl ReverseCandidates {
    fn from_value(mut v: Value) -> Result<Self> {
        let continuation = if v.is_object() {
            let next = v["continuation"].take();
            if !next.is_object() || next.to_string().len() > 4194304 {
                return invalid();
            }
            let matches = v["matches"].take();
            if !matches
                .as_array()
                .is_some_and(|r| !r.is_empty() && r.len() <= 20)
            {
                return invalid();
            }
            v = matches;
            Some(next)
        } else {
            None
        };
        Ok(Self {
            rows: take_rows(v, 1000)?,
            continuation,
        })
    }
}

#[uniffi::export]
impl ReverseCandidates {
    #[uniffi::constructor]
    pub fn parse(json: String) -> Result<Arc<Self>> {
        let value = serde_json::from_str(&json).map_err(|_| SourceFailure::Unsupported)?;
        Ok(Arc::new(Self::from_value(value)?))
    }
    pub fn continuation_json(&self) -> Option<String> {
        self.continuation.as_ref().map(Value::to_string)
    }
    pub fn cached_rows(&self) -> Result<String> {
        if self.continuation.is_some() {
            return invalid();
        }
        checked_rows(&self.rows)
    }
    pub fn preview(&self) -> Result<String> {
        checked_rows(&self.rows[..self.rows.len().min(20)])
    }
    pub fn has_more(&self) -> bool {
        self.continuation.is_some() || self.rows.len() > 20
    }
}

#[uniffi::export]
impl ReverseQuery {
    pub fn preview(&self) -> Result<String> {
        self.candidates.preview()
    }
    pub fn has_more(&self) -> bool {
        self.candidates.has_more()
    }
    pub fn all(&self, request: Arc<Request>) -> Result<String> {
        if let Some(next) = &self.candidates.continuation {
            let (_, raw) = self.catalog.invoke(
                &self.source,
                "reverseSearchMore",
                std::slice::from_ref(next),
                plugins::budget(&request),
            )?;
            checked_rows(&take_rows(downloads::checked(raw)?, 1000)?)
        } else {
            if request.cancelled.is_cancelled() {
                return Err(SourceFailure::Cancelled);
            }
            self.candidates.cached_rows()
        }
    }

    /// 返回需要可见应用内浏览器上传的初始地址。浏览器反搜不会把图片交给
    /// 原生 HTTP，上传和结果页始终在同一 WebView 会话中完成。
    pub fn browser_url(&self) -> Option<String> {
        self.browser
            .as_ref()
            .and_then(|v| v["url"].as_str().map(str::to_owned))
    }

    pub fn browser_upload_url(&self) -> Option<String> {
        self.browser
            .as_ref()
            .and_then(|v| v["uploadUrl"].as_str().map(str::to_owned))
    }

    pub fn browser_file_field(&self) -> Option<String> {
        self.browser
            .as_ref()
            .and_then(|v| v["fileField"].as_str().map(str::to_owned))
    }

    pub fn browser_result_path(&self) -> Option<String> {
        self.browser
            .as_ref()
            .and_then(|v| v["resultPath"].as_str().map(str::to_owned))
    }

    pub fn browser_result_query(&self) -> Option<String> {
        self.browser
            .as_ref()
            .and_then(|v| v["resultQuery"].as_str().map(str::to_owned))
    }

    /// 将同一浏览器会话读到的结果页交回插件解析，生成普通候选查询。
    pub fn browser_result(
        &self,
        url: String,
        html: String,
        request: Arc<Request>,
    ) -> Result<Arc<Self>> {
        if html.len() > 4 * 1024 * 1024 || url.len() > 4096 {
            return invalid();
        }
        let call = plugins::budget(&request);
        let (_, raw) = self.catalog.invoke(
            &self.source,
            "reverseSearchResult",
            &[json!({"url": url, "html": html})],
            call,
        )?;
        Ok(Arc::new(Self {
            catalog: self.catalog.clone(),
            source: self.source.clone(),
            candidates: ReverseCandidates::from_value(downloads::checked(raw)?)?,
            browser: None,
        }))
    }
}
#[uniffi::export]
impl SourceService {
    pub fn reverse_image(
        &self,
        source: String,
        png: Vec<u8>,
        request: Arc<Request>,
    ) -> Result<Arc<ReverseQuery>> {
        if png.len() > 1_800_000 || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
            return invalid();
        }
        let call = plugins::budget(&request);
        let manifest = self.catalog.manifest(&source, call.clone())?;
        if manifest["capabilities"]["reverseSearch"] != true {
            return invalid();
        }
        let input = json!({"mimeType":"image/png","base64":STANDARD.encode(&png)});
        let (_, raw) = self
            .catalog
            .invoke(&source, "reverseSearch", &[input], call)?;
        let checked = downloads::checked(raw)?;
        if checked.get("browser").is_some() {
            let browser = checked.get("browser").cloned().ok_or(SourceFailure::Unsupported)?;
            let domains = manifest["domains"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>();
            let valid_url = |value: &Value| {
                let Some(raw) = value.as_str() else { return false };
                let Ok(url) = Url::parse(raw) else { return false };
                url.scheme() == "https"
                    && url.port_or_known_default() == Some(443)
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none()
                    && url.host_str().is_some_and(|host| {
                        domains.iter().any(|domain| {
                            let domain = domain.trim_start_matches("*.");
                            host == domain || host.ends_with(&format!(".{domain}"))
                        })
                    })
            };
            let file = browser["fileField"].as_str().unwrap_or("");
            let path = browser["resultPath"].as_str().unwrap_or("");
            let query = browser["resultQuery"].as_str().unwrap_or("");
            if !browser.is_object()
                || !valid_url(&browser["url"])
                || !valid_url(&browser["uploadUrl"])
                || file.is_empty()
                || file.len() > 128
                || path.is_empty()
                || path.len() > 256
                || query.is_empty()
                || query.len() > 128
            {
                return invalid();
            }
            return Ok(Arc::new(ReverseQuery {
                catalog: self.catalog.clone(),
                source,
                candidates: ReverseCandidates { rows: Vec::new(), continuation: None },
                browser: Some(browser),
            }));
        }
        Ok(Arc::new(ReverseQuery {
            catalog: self.catalog.clone(),
            source,
            candidates: ReverseCandidates::from_value(checked)?,
            browser: None,
        }))
    }
}
