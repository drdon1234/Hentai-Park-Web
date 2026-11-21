use crate::Error;
use http::{HeaderMap, HeaderName, HeaderValue};
use park_net::public_https;
use serde_json::Value;
use url::Url;

#[derive(Clone)]
pub struct VideoSource {
    pub(crate) url: Url,
    pub(crate) headers: HeaderMap,
    pub(crate) domains: Vec<String>,
    pub(crate) session_scope: bool,
    pub label: String,
    pub preferred: bool,
    pub hls: bool,
    pub quality: u32,
    pub format: String,
    pub(crate) selection_format: String,
    pub(crate) header_origin: url::Origin,
    pub(crate) audio: Option<Url>,
    pub(crate) fallback: std::collections::BTreeMap<String, Vec<String>>,
}
fn domains(value: &Value) -> Result<Vec<String>, Error> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    value
        .as_array()
        .ok_or(Error::Unavailable)?
        .iter()
        .map(|v| {
            let d = v.as_str().ok_or(Error::Unavailable)?.to_ascii_lowercase();
            if !d.contains('.')
                || d.split('.').any(|s| s.is_empty())
                || d.bytes()
                    .any(|b| !b.is_ascii_alphanumeric() && !matches!(b, b'-' | b'_' | b'.'))
            {
                return Err(Error::Unavailable);
            }
            Ok(d)
        })
        .collect()
}
fn allowed(url: &Url, domains: &[String]) -> bool {
    url.host_str().is_some_and(|h| {
        domains
            .iter()
            .any(|d| h == d || h.ends_with(&format!(".{d}")))
    })
}
impl VideoSource {
    pub fn from_plugin(manifest: &Value, value: &Value) -> Result<Self, Error> {
        let grant = &manifest["media"];
        if grant["mode"] != "native" {
            return Err(Error::Unavailable);
        }
        let origin = Url::parse(grant["origin"].as_str().ok_or(Error::Unavailable)?)
            .map_err(|_| Error::Unavailable)?;
        let document = domains(&manifest["domains"])?;
        if !public_https(&origin) || origin.query().is_some() || !allowed(&origin, &document) {
            return Err(Error::Unavailable);
        }
        let domains_list = domains(&grant["domains"])?;
        let session_scope = grant["scope"] == "session";
        if !grant["scope"].is_null() && !session_scope {
            return Err(Error::Unavailable);
        }
        let url = Url::parse(value["url"].as_str().ok_or(Error::Unavailable)?)
            .map_err(|_| Error::Unavailable)?;
        if !public_https(&url) || (!session_scope && !allowed(&url, &domains_list)) {
            return Err(Error::Unavailable);
        }
        let hls = value["format"] == "hls" || url.path().to_ascii_lowercase().ends_with(".m3u8");
        let mut fallback = std::collections::BTreeMap::new();
        if let Some(map) = value["fallbackHosts"].as_object() {
            if map.len() > 64 {
                return Err(Error::Limit);
            }
            for (from, hosts) in map {
                let names = domains(hosts)?;
                if names.len() > 16 {
                    return Err(Error::Limit);
                }
                for name in std::iter::once(from).chain(names.iter()) {
                    let candidate =
                        Url::parse(&format!("https://{name}")).map_err(|_| Error::Unavailable)?;
                    if candidate.host_str() != Some(name.as_str())
                        || !public_https(&candidate)
                        || (!session_scope && !allowed(&candidate, &domains_list))
                    {
                        return Err(Error::Unavailable);
                    }
                }
                fallback.insert(from.clone(), names);
            }
        }
        let mut refs = document.clone();
        refs.extend(domains(&grant["referrerDomains"])?);
        let mut headers = HeaderMap::new();
        if let Some(raw) = value["headers"].as_object() {
            for (key, value) in raw {
                let name =
                    HeaderName::from_bytes(key.as_bytes()).map_err(|_| Error::Unavailable)?;
                let val = value.as_str().ok_or(Error::Unavailable)?;
                if val.len() > 4096 {
                    return Err(Error::Unavailable);
                }
                match name.as_str() {
                    "referer" | "origin" => {
                        let uri = Url::parse(val).map_err(|_| Error::Unavailable)?;
                        if !public_https(&uri)
                            || !allowed(&uri, if name == "origin" { &document } else { &refs })
                            || (name == "origin" && val != uri.origin().ascii_serialization())
                        {
                            return Err(Error::Unavailable);
                        }
                    }
                    "accept" | "accept-language" => (),
                    custom
                        if session_scope
                            && custom.starts_with("x-")
                            && custom.len() <= 62
                            && grant["requestHeaders"][custom]["scope"] == "playback-origin" =>
                    {
                        ()
                    }
                    _ => return Err(Error::Unavailable),
                }
                headers.insert(
                    name,
                    HeaderValue::from_str(val).map_err(|_| Error::Unavailable)?,
                );
            }
        }
        let quality = match &value["quality"] {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|n| n as i64)),
            Value::String(s) => s.parse::<i64>().ok(),
            _ => None,
        };
        let label = value["label"]
            .as_str()
            .filter(|label| !label.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| quality.map_or_else(|| "P".into(), |n| format!("{n}P")));
        // 下载身份保留插件声明的格式，传输使用推断后的规范格式。
        let selection_format = value["format"].as_str().unwrap_or_default().to_owned();
        let format = if selection_format.is_empty() {
            if hls { "hls".into() } else { "mp4".into() }
        } else {
            selection_format.to_ascii_lowercase()
        };
        Ok(Self {
            header_origin: url.origin(),
            hls,
            audio: None,
            fallback,
            quality: quality.and_then(|n| n.try_into().ok()).unwrap_or(0),
            format,
            selection_format,
            url,
            headers,
            domains: domains_list,
            session_scope,
            label,
            preferred: value["preferred"] == true,
        })
    }
}
