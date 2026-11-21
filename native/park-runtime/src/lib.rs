mod catalog;
mod engine;
pub use catalog::{installed_manifest, release, Installed, PluginPreview, SourceCatalog};
mod native_host;
mod pool;
mod state;
pub use engine::{Capabilities, Error, Invocation, SourceRuntime};
pub use native_host::NativeHost;
pub use state::SourceState;
pub(crate) fn source_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 100
        && key != "_app"
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
