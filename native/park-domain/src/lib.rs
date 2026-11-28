mod filter;
mod language;
mod tag_dictionary;
pub use tag_dictionary::TagDictionary;
mod novel;
pub mod subtitles;
pub mod updates;
pub mod settings;
pub mod tavern;
pub mod tavern_chat;
pub mod tavern_proposal;
pub use filter::{BlockFilter, BLOCK_VOCABULARY};
pub use language::{normalized_search_text, LocalizedCopy, PUBLIC_CONTRACT};
pub use novel::translation_segments;
pub use novel::{NovelMode, NovelText};

/// 宿主崩溃诊断只输出固定事件，不写入 panic 载荷、源文件或调用栈。
pub fn install_crash_diagnostics() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        std::panic::set_hook(Box::new(|_| {
            use std::io::Write;
            let _ = writeln!(std::io::stderr().lock(), "event=runtime.crash code=INTERNAL_ERROR");
        }));
    });
}
