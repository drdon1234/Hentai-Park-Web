use cookie::Cookie;
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct InvalidJar;
type Result<T> = std::result::Result<T, InvalidJar>;
pub type Files = BTreeMap<String, String>;
pub type Identity = (String, String, String, bool);
pub type Jar = BTreeMap<Identity, Cookie<'static>>;

pub fn declared_domains(manifest: &serde_json::Value) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    if let Some(domains) = manifest["domains"].as_array().filter(|a| !a.is_empty()) {
        for domain in domains.iter().filter_map(|v| v.as_str()) {
            let d = domain.trim().to_ascii_lowercase();
            if public_host(&d) && psl::domain_str(&d).is_some() {
                out.insert(d);
            }
        }
    } else {
        for v in [
            &manifest["homepage"],
            &manifest["network"]["origin"],
            &manifest["account"]["cookieUrl"],
            &manifest["account"]["loginUrl"],
        ] {
            if let Some(u) = v.as_str().and_then(|v| url::Url::parse(v).ok()) {
                if let Some(domain) = u.host_str().and_then(psl::domain_str) {
                    out.insert(domain.to_owned());
                }
            }
        }
    }
    out.into_iter().collect()
}

pub fn validate_domains(files: &Files, domains: &[String]) -> Result<()> {
    let allowed = |key: &str| {
        let host = key.trim_start_matches('.');
        public_host(host)
            && domains
                .iter()
                .any(|d| host == d || host.strip_suffix(d).is_some_and(|s| s.ends_with('.')))
    };
    parse(files, true)?;
    for (name, raw) in files {
        let hosts = match name.strip_prefix("ie0_ps1/").ok_or(InvalidJar)? {
            ".index" => decode::<Vec<String>>(raw)?,
            ".domains" => decode::<BTreeMap<String, serde_json::Value>>(raw)?
                .into_keys()
                .collect(),
            host => vec![host.to_owned()],
        };
        if hosts.iter().any(|h| !allowed(h)) {
            return Err(InvalidJar);
        }
    }
    Ok(())
}

fn public_host(host: &str) -> bool {
    let mut labels = host.split('.');
    let suffix = labels.next_back().unwrap_or("");
    labels.clone().next().is_some()
        && suffix.len() >= 2
        && suffix.bytes().all(|b| b.is_ascii_lowercase())
        && suffix != "local"
        && suffix != "localhost"
        && host
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && labels.all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_')
                })
        })
}

fn parse_paths(
    jar: &mut Jar,
    host: &str,
    paths: BTreeMap<String, BTreeMap<String, String>>,
    shared: bool,
    snapshot: bool,
) -> Result<()> {
    let host = host.strip_prefix('.').unwrap_or(host);
    if !public_host(host) {
        return Err(InvalidJar);
    }
    for (path, cookies) in paths {
        if !path.starts_with('/') || path.contains([';', '\r', '\n']) {
            return Err(InvalidJar);
        }
        for (name, saved) in cookies {
            let (header, date) = saved.rsplit_once(";_crt=").ok_or(InvalidJar)?;
            let created: i64 = date.parse().map_err(|_| InvalidJar)?;
            if created < 0 || header.chars().any(char::is_control) {
                return Err(InvalidJar);
            }
            let mut cookie = Cookie::parse(header.to_owned()).map_err(|_| InvalidJar)?;
            if cookie.name() != name
                || cookie.path().is_some_and(|p| p != path)
                || (snapshot && cookie.path().is_none() && path != "/")
                || cookie.domain() != shared.then_some(host)
            {
                return Err(InvalidJar);
            }
            if cookie.path().is_none() {
                cookie.set_path(path.clone());
            }
            if let Some(age) = cookie.max_age() {
                let timestamp = created.saturating_add(age.whole_seconds());
                let expiry = cookie::time::OffsetDateTime::from_unix_timestamp(timestamp)
                    .map_err(|_| InvalidJar)?;
                cookie.set_max_age(None);
                cookie.set_expires(expiry);
            }
            jar.insert((host.to_string(), path.clone(), name, shared), cookie);
        }
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &str) -> Result<T> {
    serde_json::from_str(bytes).map_err(|_| InvalidJar)
}

pub fn parse(files: &Files, check_index: bool) -> Result<Jar> {
    let mut result = Jar::new();
    for (key, data) in files {
        let name = key.strip_prefix("ie0_ps1/").ok_or(InvalidJar)?;
        match name {
            ".index" => {
                let hosts: Vec<String> = decode(data)?;
                for host in hosts {
                    if !public_host(&host)
                        || (check_index && !files.contains_key(&format!("ie0_ps1/{host}")))
                    {
                        return Err(InvalidJar);
                    }
                }
            }
            ".domains" => {
                for (domain, paths) in decode::<BTreeMap<String, _>>(data)? {
                    parse_paths(&mut result, &domain, paths, true, check_index)?;
                }
            }
            _ => {
                if !public_host(name) {
                    return Err(InvalidJar);
                }
                parse_paths(&mut result, name, decode(data)?, false, check_index)?;
            }
        }
    }
    Ok(result)
}
