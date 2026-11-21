use crate::Error;
use cookie_store::CookieStore;
use http::{header::SET_COOKIE, HeaderMap};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub enum Purpose {
    Source,
    Media,
    ExternalImage,
    Repository,
}
impl Purpose {
    pub(crate) fn credentials(self) -> bool {
        matches!(self, Self::Source)
    }
}

struct Partition {
    checkpoint: Option<String>,
    jar: CookieStore,
    generation: CancellationToken,
    directory: Option<PathBuf>,
    reload: bool,
}
impl Partition {
    fn commit(&mut self, jar: CookieStore) -> Result<(), Error> {
        if let Some(directory) = &self.directory {
            self.reload = true;
            crate::jar::save(directory, &jar)?;
        }
        self.jar = jar;
        self.reload = false;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Session {
    part: Arc<Mutex<Partition>>,
    pub(crate) purpose: Purpose,
}
impl Session {
    pub fn purpose(&self) -> Purpose {
        self.purpose
    }
    /// 订阅检查只需比较账号上下文，凭据值始终留在分区内部。
    pub fn subscription_context(
        &self,
        names: &[String],
        version: &serde_json::Value,
        settings: &serde_json::Value,
        account: &serde_json::Value,
        proxy: &str,
    ) -> Result<String, Error> {
        use sha2::{Digest, Sha256};
        if !self.purpose.credentials() {
            return Err(Error::Blocked);
        }
        let state = self.lock()?;
        let mut auth = std::collections::BTreeMap::new();
        for cookie in state
            .jar
            .iter_unexpired()
            .filter(|c| names.iter().any(|name| name == c.name()))
        {
            let domain = match &cookie.domain {
                cookie_store::CookieDomain::HostOnly(d) | cookie_store::CookieDomain::Suffix(d) => {
                    d
                }
                _ => return Err(Error::Storage),
            };
            auth.insert(
                format!(
                    "{}\n{}\n{}",
                    domain.to_lowercase(),
                    cookie.path.as_ref(),
                    cookie.name()
                ),
                cookie.value(),
            );
        }
        let serialized = serde_json::to_vec(&(version, settings, account, auth, proxy))
            .map_err(|_| Error::Storage)?;
        Ok(format!("{:x}", Sha256::digest(serialized)))
    }
    fn lock(&self) -> Result<MutexGuard<'_, Partition>, Error> {
        let mut state = self.part.lock().map_err(|_| Error::Storage)?;
        if state.reload {
            state.jar = crate::jar::load(state.directory.as_ref().ok_or(Error::Storage)?)?;
            state.reload = false;
        }
        Ok(state)
    }
    pub(crate) fn token(&self) -> Result<CancellationToken, Error> {
        Ok(self.lock()?.generation.clone())
    }

    pub fn clear(&self) -> Result<(), Error> {
        let mut state = self.lock()?;
        state.generation.cancel();
        state.generation = CancellationToken::new();
        state.commit(CookieStore::default())
    }
    pub fn invalidate(&self) -> Result<(), Error> {
        let mut state = self.part.lock().map_err(|_| Error::Storage)?;
        state.generation.cancel();
        state.generation = CancellationToken::new();
        state.reload = state.directory.is_some();
        Ok(())
    }
    pub fn restored(&self, checkpoint: Option<String>) -> Result<(), Error> {
        let mut state = self.lock()?;
        if state.checkpoint != checkpoint {
            state.generation.cancel();
            state.generation = CancellationToken::new();
            state.reload = true;
            state.checkpoint = checkpoint;
        }
        Ok(())
    }

    pub fn cookies(
        &self,
        url: &Url,
        domains: &[String],
    ) -> Result<std::collections::BTreeMap<String, String>, Error> {
        crate::policy::authorize(url, domains)?;
        let state = self.lock()?;
        if !self.purpose.credentials() {
            return Err(Error::Blocked);
        }
        Ok(state
            .jar
            .get_request_values(url)
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect())
    }

    pub fn set_cookies(
        &self,
        url: &Url,
        domains: &[String],
        values: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), Error> {
        crate::policy::authorize(url, domains)?;
        if !self.purpose.credentials() {
            return Err(Error::Blocked);
        }
        let host = url.host_str().ok_or(Error::Blocked)?;
        let mut state = self.lock()?;
        let mut next = state.jar.clone();
        for (name, value) in values {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                || value
                    .bytes()
                    .any(|b| b <= 0x20 || b >= 0x7f || matches!(b, b'"' | b',' | b';' | b'\\'))
            {
                return Err(Error::Blocked);
            }
            if next
                .get_request_values(url)
                .any(|(k, v)| k == name && v == value)
            {
                continue;
            }
            if name.starts_with("__Host-")
                || name.starts_with("__Secure-")
                || psl::suffix_str(host) == Some(host)
            {
                return Err(Error::Blocked);
            }
            let raw = cookie::Cookie::build((name.clone(), value.clone()))
                .domain(host.to_owned())
                .path("/")
                .expires(
                    cookie::time::OffsetDateTime::now_utc() + cookie::time::Duration::days(365),
                )
                .build();
            next.insert_raw(&raw, url).map_err(|_| Error::Blocked)?;
        }
        state.commit(next)
    }

    pub fn clear_host(&self, url: &Url, domains: &[String]) -> Result<(), Error> {
        crate::policy::authorize(url, domains)?;
        if !self.purpose.credentials() {
            return Err(Error::Blocked);
        }
        let mut state = self.lock()?;
        state.generation.cancel();
        state.generation = CancellationToken::new();
        let keep = state
            .jar
            .iter_unexpired()
            .filter(|c| !c.domain.matches(url))
            .cloned()
            .map(Ok::<_, Error>);
        let next = CookieStore::from_cookies(keep, false)?;
        state.commit(next)
    }

    pub(crate) fn header(&self, url: &Url, epoch: &CancellationToken) -> Result<String, Error> {
        let state = self.lock()?;
        if epoch.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if !self.purpose.credentials() {
            return Ok(String::new());
        }
        let mut values = state.jar.matches(url);
        values.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.as_ref().len()));
        Ok(values
            .iter()
            .map(|c| format!("{}={}", c.name(), c.value()))
            .collect::<Vec<_>>()
            .join("; "))
    }
    pub(crate) fn accept(
        &self,
        url: &Url,
        headers: &HeaderMap,
        epoch: &CancellationToken,
    ) -> Result<(), Error> {
        let mut state = self.lock()?;
        if epoch.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if !self.purpose.credentials() || !headers.contains_key(SET_COOKIE) {
            return Ok(());
        }
        let mut next = state.jar.clone();
        let mut changed = false;
        for raw in headers.get_all(SET_COOKIE) {
            let Some(cookie) = raw
                .to_str()
                .ok()
                .and_then(|v| cookie::Cookie::parse(v).ok())
            else {
                continue;
            };
            if cookie
                .domain()
                .is_some_and(|d| psl::suffix_str(d) == Some(d))
            {
                continue;
            }
            let secure = url.scheme() == "https" && cookie.secure() == Some(true);
            if (cookie.secure() == Some(true) && !secure)
                || (cookie.name().starts_with("__Secure-") && !secure)
                || (cookie.name().starts_with("__Host-")
                    && (!secure || cookie.path() != Some("/") || cookie.domain().is_some()))
            {
                continue;
            }
            if next.insert_raw(&cookie, url).is_ok() {
                changed = true;
            }
        }
        if changed {
            state.commit(next)?;
        }
        Ok(())
    }
}

pub struct Sessions {
    root: PathBuf,
    entries: Mutex<HashMap<(String, Purpose), Session>>,
}
impl Sessions {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            entries: Mutex::default(),
        }
    }
    pub fn partition(&self, owner: &str, purpose: Purpose) -> Result<Session, Error> {
        if owner.is_empty()
            || owner.len() > 100
            || !owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Err(Error::Blocked);
        }
        let mut entries = self.entries.lock().map_err(|_| Error::Storage)?;
        let key = (owner.to_owned(), purpose);
        if let Some(session) = entries.get(&key) {
            return Ok(session.clone());
        }
        let encoded: String = owner.bytes().map(|b| format!("{b:02x}")).collect();
        let directory = purpose
            .credentials()
            .then(|| self.root.join("cookie_jars").join(encoded));
        let jar = match &directory {
            Some(dir) => crate::jar::load(dir)?,
            None => CookieStore::default(),
        };
        let session = Session {
            part: Arc::new(Mutex::new(Partition {
                checkpoint: None,
                jar,
                generation: CancellationToken::new(),
                directory,
                reload: false,
            })),
            purpose,
        };
        entries.insert(key, session.clone());
        Ok(session)
    }
}
