use semver::Version;
use serde_json::Value;

const GITHUB: &str = "https://github.com/drdon1234/Hentai-Park/releases";
#[derive(Debug)]
pub struct InvalidRelease;
#[derive(Clone)]
pub struct Apk {
    pub name: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}
pub struct Release {
    pub version: String,
    pub url: String,
    pub notes: String,
    pub downgrade: bool,
    assets: Vec<Apk>,
}
impl Release {
    pub fn parse(payload: &Value, current: &str) -> Result<Option<Self>, InvalidRelease> {
        let mut current = Version::parse(current).map_err(|_| InvalidRelease)?;
        if payload.is_null() || payload.as_object().is_some_and(|o| o.is_empty()) {
            return Ok(None);
        }
        let draft = payload["draft"].as_bool().ok_or(InvalidRelease)?;
        let prerelease = payload["prerelease"].as_bool().ok_or(InvalidRelease)?;
        if draft || prerelease {
            return Ok(None);
        }
        let tag = payload["tag_name"].as_str().ok_or(InvalidRelease)?;
        let text = tag.strip_prefix('v').unwrap_or(tag);
        let mut version = Version::parse(text).map_err(|_| InvalidRelease)?;
        if !version.pre.is_empty() {
            return Ok(None);
        }
        let url = format!("{GITHUB}/tag/{tag}");
        if payload["html_url"] != url {
            return Err(InvalidRelease);
        }
        let notes = match &payload["body"] {
            Value::Null => "",
            Value::String(s) => s,
            _ => return Err(InvalidRelease),
        };
        current.build = semver::BuildMetadata::EMPTY;
        version.build = semver::BuildMetadata::EMPTY;
        if version == current {
            return Ok(None);
        }
        let assets = payload["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| Apk::parse(row, tag, text))
            .collect();
        Ok(Some(Self {
            version: text.into(),
            url,
            notes: notes.into(),
            downgrade: version < current,
            assets,
        }))
    }
    pub fn apk(&self, abis: &[String]) -> Option<&Apk> {
        abis.iter()
            .map(String::as_str)
            .chain(std::iter::once("universal"))
            .filter(|abi| matches!(*abi, "arm64-v8a" | "armeabi-v7a" | "x86_64" | "universal"))
            .find_map(|abi| {
                self.assets
                    .iter()
                    .find(|a| a.name.ends_with(&format!("-{abi}.apk")))
            })
    }
    pub fn localized_notes(&self, chinese: bool) -> String {
        let text = self.notes.replace("\r\n", "\n");
        let section = |name: &str| -> Option<Vec<&str>> {
            let mut lines = text.lines();
            lines.find(|l| {
                l.strip_prefix("##")
                    .is_some_and(|s| s.starts_with(char::is_whitespace) && s.trim() == name)
            })?;
            Some(lines.take_while(|l| !l.starts_with("## ")).collect())
        };
        let first = if chinese { "中文" } else { "English" };
        section(first)
            .or_else(|| section(if chinese { "English" } else { "中文" }))
            .unwrap_or_else(|| text.lines().collect())
            .join("\n")
            .trim()
            .to_owned()
    }
}
impl Apk {
    fn parse(row: &Value, tag: &str, version: &str) -> Option<Self> {
        let name = row["name"].as_str()?;
        let tail = name.strip_prefix(&format!("HentaiPark-{version}-"))?;
        let (build, abi) = tail.split_once("-android-")?;
        if build.is_empty()
            || !build.bytes().all(|b| b.is_ascii_digit())
            || !matches!(
                abi,
                "arm64-v8a.apk" | "armeabi-v7a.apk" | "x86_64.apk" | "universal.apk"
            )
        {
            return None;
        }
        let url = format!("{GITHUB}/download/{tag}/{name}");
        let size = row["size"]
            .as_u64()
            .filter(|v| (1..=2_147_483_648).contains(v))?;
        let sha256 = row["digest"].as_str()?.strip_prefix("sha256:")?;
        if row["browser_download_url"] != url
            || sha256.len() != 64
            || !sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        Some(Self {
            name: name.into(),
            url,
            size,
            sha256: sha256.to_ascii_lowercase(),
        })
    }
}
