use crate::{ProgressStore, Request, StorageError};
use park_storage::backup::{self, Scope};
use std::{fs, path::Path, sync::Arc};

#[derive(uniffi::Enum)]
pub enum BackupScope {
    Manual,
    WithCredentials,
    Automatic,
}
impl From<backup::Error> for StorageError {
    fn from(value: backup::Error) -> Self {
        match value {
            backup::Error::Cancelled => Self::Cancelled,
            backup::Error::Database(err) => err.into(),
            backup::Error::Io(_) => Self::IoFailure,
            backup::Error::Json(err)
                if err.io_error_kind().is_some_and(|kind| {
                    !matches!(
                        kind,
                        std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
                    )
                }) =>
            {
                Self::IoFailure
            }
            _ => Self::InvalidBackup,
        }
    }
}
#[uniffi::export]
impl ProgressStore {
    pub fn restore_backup(
        &self,
        file: String,
        polyfills: String,
        browser: Box<dyn crate::RecoveryBrowser>,
        request: Arc<Request>,
    ) -> Result<(), StorageError> {
        use park_storage::recovery;
        let _gate = recovery::gate(&self.root, true)?;
        let mut db = self.lock()?;
        let bridge = crate::recovery::Browser(browser.as_ref());
        recovery::recover(&self.root, &db, Some(&bridge))?;
        park_storage::plugins::recover(&self.root, Some(&bridge))?;
        request.check()?;
        let src =
            std::io::BufReader::new(fs::File::open(file).map_err(|_| StorageError::IoFailure)?);
        let plan = backup::Import::read(&self.root, src, |source, digest| {
            let call = park_runtime::Invocation::child(
                std::time::Duration::from_secs(10),
                &request.cancelled,
            );
            let manifest =
                park_runtime::installed_manifest(&self.root, source, digest, &polyfills, call)
                    .map_err(|_| backup::Error::Invalid)?;
            Ok(park_storage::cookies::declared_domains(&manifest))
        })?;
        self.apply_backup(&mut db, plan, Some(&bridge), &request)
    }
    pub fn restore_desktop_backup(
        &self,
        file: String,
        registry: String,
        request: Arc<Request>,
    ) -> Result<(), StorageError> {
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        let mut db = self.lock()?;
        park_storage::recovery::recover(&self.root, &db, None)?;
        request.check()?;
        let input = fs::File::open(file).map_err(|_| StorageError::IoFailure)?;
        let installed = serde_json::from_str(&registry).map_err(|_| StorageError::InvalidBackup)?;
        let plan = backup::import_desktop(&self.root, std::io::BufReader::new(input), &installed)?;
        self.apply_backup(&mut db, plan, None, &request)
    }
    pub fn create_desktop_backup(
        &self,
        directory: String,
        timestamp: String,
        scope: BackupScope,
        library: bool,
        settings: bool,
        sources: bool,
        registry: String,
        request: Arc<Request>,
    ) -> Result<String, StorageError> {
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        let db = self.lock()?;
        let installed = serde_json::from_str(&registry).map_err(|_| StorageError::InvalidBackup)?;
        Ok(backup::save_desktop(
            &db,
            &self.root,
            Path::new(&directory),
            &timestamp,
            scope.into(),
            library,
            settings,
            sources,
            &installed,
            || request.cancelled.is_cancelled(),
        )?)
    }
    pub fn create_backup(
        &self,
        directory: String,
        timestamp: String,
        scope: BackupScope,
        library: bool,
        settings: bool,
        sources: bool,
        request: Arc<Request>,
    ) -> Result<String, StorageError> {
        let scope = scope.into();
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        let db = self.lock()?;
        Ok(backup::save(
            &db,
            &self.root,
            Path::new(&directory),
            &timestamp,
            scope,
            library,
            settings,
            sources,
            || request.cancelled.is_cancelled(),
        )?)
    }

    pub fn automatic_backup_if_due(
        &self,
        directory: String,
        timestamp: String,
        request: Arc<Request>,
    ) -> Result<Option<String>, StorageError> {
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        let db = self.lock()?;
        Ok(backup::automatic_if_due(
            &db,
            &self.root,
            Path::new(&directory),
            &timestamp,
            || request.cancelled.is_cancelled(),
        )?)
    }

    pub fn save_preference(&self, name: String, json: String) -> Result<(), StorageError> {
        let value = serde_json::from_str(&json).map_err(|_| StorageError::InvalidBackup)?;
        if !park_domain::settings::valid(&name, &value) {
            return Err(StorageError::InvalidBackup);
        }
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        backup::set_preferences(&self.root, [(name, value)])?;
        Ok(())
    }

    pub fn save_discovery_scope(&self, dimension: String, media: String) -> Result<(), StorageError> {
        let values = [("homeDimension".to_owned(), dimension.into()), ("homeMedia".to_owned(), media.into())];
        if !values.iter().all(|(name, value)| park_domain::settings::valid(name, value)) {
            return Err(StorageError::InvalidBackup);
        }
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        backup::set_preferences(&self.root, values)?;
        Ok(())
    }
}

impl ProgressStore {
    fn apply_backup(
        &self,
        db: &mut rusqlite::Connection,
        mut plan: backup::Import,
        browser: Option<&dyn park_storage::recovery::RecoveryBrowser>,
        request: &Request,
    ) -> Result<(), StorageError> {
        let files = std::mem::take(&mut plan.files);
        let cookies = std::mem::take(&mut plan.cookies);
        park_storage::recovery::apply(&self.root, db, files, cookies, browser, |tx| {
            if plan.merge(tx, || request.cancelled.is_cancelled())? {
                tx.execute(
                    "INSERT INTO park_progress_events(payload) VALUES('null')",
                    [],
                )?;
                tx.execute(
                    "DELETE FROM park_progress_events WHERE revision<?",
                    [tx.last_insert_rowid()],
                )?;
            }
            Ok(())
        })?;
        Ok(())
    }
}
impl From<BackupScope> for Scope {
    fn from(value: BackupScope) -> Self {
        match value {
            BackupScope::Manual => Self::Manual,
            BackupScope::WithCredentials => Self::WithCredentials,
            BackupScope::Automatic => Self::Automatic,
        }
    }
}
