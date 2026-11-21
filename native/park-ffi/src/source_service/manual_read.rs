use super::*;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) fn confirmation_page(manifest: &Value, raw: &Value) -> Option<Url> {
    let text = raw
        .as_str()
        .filter(|s| s.len() <= 8192 && !s.chars().any(char::is_control))?;
    let page = Url::parse(text).ok()?;
    let host = page.host_str()?;
    let domains = manifest["domains"].as_array()?;
    (park_net::public_https(&page)
        && domains.iter().filter_map(Value::as_str).any(|domain| {
            host.eq_ignore_ascii_case(domain)
                || host.ends_with(&format!(".{}", domain.to_ascii_lowercase()))
        }))
    .then_some(page)
}

#[derive(Record)]
pub struct BrowseResult {
    pub content: Option<String>,
    pub confirmation: Option<Arc<ManualRead>>,
}

#[derive(Object)]
pub struct ManualRead {
    pub(super) page: Url,
    pub(super) source: String,
    pub(super) digest: String,
    pub(super) method: &'static str,
    pub(super) args: Vec<Value>,
    pub(super) catalog: Arc<SourceCatalog>,
    pub(super) scope: Option<Arc<park_net::ReadScope>>,
    pub(super) consumed: AtomicBool,
}

#[uniffi::export]
impl ManualRead {
    pub fn url(&self) -> String {
        self.page.to_string()
    }

    pub fn can_read_again(&self) -> bool {
        self.scope.as_ref().is_some_and(|scope| scope.active())
            && !self.consumed.load(Ordering::Acquire)
    }

    /// 只对原来真实发过同源 GET/HEAD 的读取提供一次恢复。用户关闭页面不触发调用。
    pub fn read_again(&self, request: Arc<Request>) -> Result<String> {
        let scope = self.scope.as_ref().ok_or(SourceFailure::Unsupported)?;
        if !scope.active() || self.consumed.swap(true, Ordering::AcqRel) {
            return Err(SourceFailure::Cancelled);
        }
        let (_, output) = self.catalog.invoke_installed(
            &self.source,
            self.method,
            &self.args,
            plugins::budget(&request).with_read_scope(scope.clone()),
            Some(&self.digest),
        )?;
        Ok(downloads::checked(output)?.to_string())
    }
}
