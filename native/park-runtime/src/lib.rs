mod catalog;
mod crypto;
mod engine;
pub use catalog::{installed_manifest, release, Installed, PluginPreview, SourceCatalog};
mod native_host;
mod pool;
mod state;
pub use engine::{Capabilities, Error, Invocation, SourceRuntime};
pub use native_host::NativeHost;
pub use state::SourceState;
use serde_json::Value;
use std::sync::Arc;

/// 由目标平台提供真实浏览器会话，供声明 `network.mode = "browser"` 的插件使用。
/// 实现必须自行维持平台浏览器的 Cookie 分区，返回的响应仍由宿主校验最终地址和大小。
pub trait BrowserTransport: Send + Sync {
    fn request(
        &self,
        source: &str,
        request: Value,
        call: &Invocation,
    ) -> Result<Value, Error>;
}

pub type SharedBrowserTransport = Arc<dyn BrowserTransport>;
pub(crate) fn source_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 100
        && key != "_app"
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
