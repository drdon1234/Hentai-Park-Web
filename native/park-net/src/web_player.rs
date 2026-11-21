use crate::{public_https, Error};
use serde_json::Value;
use url::Url;

/// 网页播放的导航许可独立于插件 HTTP、媒体请求头及源 Cookie。
pub struct WebNavigation {
    pub initial: Url,
    main: Vec<String>,
    frames: Vec<String>,
}
fn safe_url(raw: &str) -> Option<Url> {
    let uri = Url::parse(raw).ok()?;
    let mut checked = uri.clone();
    checked.set_fragment(None);
    let host = checked.host_str()?;
    let suffix = host.rsplit('.').next()?;
    (public_https(&checked)
        && suffix.len() >= 2
        && suffix.bytes().all(|c| c.is_ascii_alphabetic())
        && !matches!(
            suffix,
            "internal" | "test" | "invalid" | "localhost" | "local"
        )
        && host.split('.').all(|s| {
            !s.is_empty()
                && !s.starts_with('-')
                && !s.ends_with('-')
                && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        }))
    .then_some(uri)
}
fn names(value: &Value) -> Result<Vec<String>, Error> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let list = value
        .as_array()
        .filter(|list| list.len() <= 256)
        .ok_or(Error::Blocked)?;
    list.iter()
        .map(|v| {
            let host = v.as_str().ok_or(Error::Blocked)?.to_ascii_lowercase();
            let url = safe_url(&format!("https://{host}")).ok_or(Error::Blocked)?;
            if url.host_str() != Some(&host) || psl::domain_str(&host).is_none() {
                return Err(Error::Blocked);
            }
            Ok(host)
        })
        .collect()
}

/// 游戏导航按作品明确列出的主机匹配；存档身份和播放器页面在解析后固定。
pub struct GameNavigation {
    pub initial: Url,
    pub movie: Url,
    pub player: Option<Url>,
    pub storage_id: String,
    hosts: Vec<String>,
}

impl GameNavigation {
    pub fn from_plugin(value: &Value, work: &str) -> Result<Self, Error> {
        let movie = value["url"]
            .as_str()
            .and_then(safe_url)
            .ok_or(Error::Blocked)?;
        let storage_id = value
            .get("storageId")
            .map_or(Some(work), Value::as_str)
            .ok_or(Error::Blocked)?;
        if storage_id.is_empty()
            || storage_id.encode_utf16().count() > 256
            || storage_id.chars().any(char::is_control)
        {
            return Err(Error::Blocked);
        }
        let mut hosts = names(&value["navigationDomains"])?;
        if hosts.len() > 8 {
            return Err(Error::Blocked);
        }
        hosts.push(movie.host_str().ok_or(Error::Blocked)?.to_string());
        let player = match value["kind"].as_str() {
            Some("html") => None,
            Some("flash") => {
                let url = value["player"]
                    .as_str()
                    .and_then(safe_url)
                    .ok_or(Error::Blocked)?;
                if movie.host_str() != url.host_str()
                    || !movie.path().to_ascii_lowercase().ends_with(".swf")
                {
                    return Err(Error::Blocked);
                }
                Some(url)
            }
            _ => return Err(Error::Blocked),
        };
        let initial = if player.is_some() {
            movie
                .join("/__hentaipark__/ruffle.html")
                .map_err(|_| Error::Blocked)?
        } else {
            movie.clone()
        };
        Ok(Self {
            movie,
            initial,
            player,
            storage_id: storage_id.to_string(),
            hosts,
        })
    }
    pub fn document(&self, raw: &str) -> bool {
        safe_url(raw).is_some_and(|url| {
            self.hosts
                .iter()
                .any(|host| Some(host.as_str()) == url.host_str())
        })
    }
    pub fn resource(&self, raw: &str) -> bool {
        safe_url(raw).is_some()
    }
}
impl WebNavigation {
    pub fn from_plugin(value: &Value) -> Result<Self, Error> {
        let initial =
            safe_url(value["url"].as_str().ok_or(Error::Blocked)?).ok_or(Error::Blocked)?;
        let host = initial.host_str().ok_or(Error::Blocked)?;
        let mut main = names(&value["navigationDomains"])?;
        main.push(psl::domain_str(host).unwrap_or(host).into());
        Ok(Self {
            initial,
            main,
            frames: names(&value["frameDomains"])?,
        })
    }
    pub fn allows(&self, raw: &str, subframe: bool) -> bool {
        if subframe && raw == "about:blank" {
            return true;
        }
        let Some(url) = safe_url(raw) else {
            return false;
        };
        let Some(host) = url.host_str() else {
            return false;
        };
        let matches = |zone: &String| {
            host == zone || host.strip_suffix(zone).is_some_and(|s| s.ends_with('.'))
        };
        self.main.iter().any(matches) || subframe && self.frames.iter().any(matches)
    }
    pub fn allows_resource(&self, raw: &str) -> bool {
        safe_url(raw).is_some()
    }
}
