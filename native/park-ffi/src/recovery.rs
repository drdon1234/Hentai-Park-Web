use crate::StorageError;
use park_storage::recovery;
use std::path::Path;

#[uniffi::export(callback_interface)]
pub trait RecoveryBrowser: Send + Sync {
    fn set_cookie(&self, source: String, url: String, header: String) -> bool;
    fn flush(&self) -> bool;
}
pub(crate) struct Browser<'a>(pub &'a dyn RecoveryBrowser);
impl recovery::RecoveryBrowser for Browser<'_> {
    fn set_cookie(&self, source: &str, url: String, header: String) -> bool {
        self.0.set_cookie(source.to_owned(), url, header)
    }
    fn flush(&self) -> bool {
        self.0.flush()
    }
}
impl From<recovery::Error> for StorageError {
    fn from(e: recovery::Error) -> Self {
        match e {
            recovery::Error::InvalidJournal => Self::InvalidJournal,
            recovery::Error::PendingRecovery => Self::PendingRecovery,
            recovery::Error::IoFailure => Self::IoFailure,
            recovery::Error::Database(e) => e.into(),
            recovery::Error::Backup(e) => e.into(),
        }
    }
}
pub(crate) fn recover(
    root: &Path,
    db: &rusqlite::Connection,
    browser: Option<&dyn RecoveryBrowser>,
) -> Result<(), StorageError> {
    let bridge = browser.map(Browser);
    recovery::recover(
        root,
        db,
        bridge.as_ref().map(|b| b as &dyn recovery::RecoveryBrowser),
    )?;
    park_storage::plugins::recover(
        root,
        bridge.as_ref().map(|b| b as &dyn recovery::RecoveryBrowser),
    )?;
    Ok(())
}
