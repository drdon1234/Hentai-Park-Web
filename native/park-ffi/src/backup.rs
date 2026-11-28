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
        if name == "tagDownloadReminders" && value == true {
            backup::set_preferences(&self.root, [(name, value), ("tagDownloadIgnoredSources".into(), serde_json::json!([]))])?;
        } else {
            backup::set_preferences(&self.root, [(name, value)])?;
        }
        Ok(())
    }

    pub fn complete_onboarding(&self, categories: Vec<String>, secure_screen: bool, blur_covers: bool) -> Result<(), StorageError> {
        let interests = serde_json::json!(categories);
        if !park_domain::settings::valid("homeCategories", &interests) {
            return Err(StorageError::InvalidBackup);
        }
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        backup::set_preferences(&self.root, [
            ("homeCategories".into(), interests), ("secureScreen".into(), secure_screen.into()),
            ("blurCovers".into(), blur_covers.into()), ("onboarded".into(), true.into()),
        ])?;
        Ok(())
    }

    pub fn save_reader_preferences(&self, json: String) -> Result<(), StorageError> {
        let values: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&json).map_err(|_| StorageError::InvalidBackup)?;
        if !values.iter().all(|(name, value)| {
            matches!(name.as_str(), "readerDim" | "readerWarm" | "readerGrayscale" | "readerInvert" |
                "readerBackground" | "readerShowPageNumber" | "keepScreenOn" |
                "readerGap" | "readerAnimate" | "readerDual" | "readerCoverSingle" |
                "readerSplitWide" | "readerCrop" | "tapToPage" | "readerTapLayout" |
                "readerTapInvertH" | "readerTapInvertV" | "readerOrientation")
                && park_domain::settings::valid(name, value)
        }) {
            return Err(StorageError::InvalidBackup);
        }
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        backup::set_preferences(&self.root, values)?;
        Ok(())
    }

    pub fn save_shelf_preferences(&self, json: String) -> Result<(), StorageError> {
        let values: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&json).map_err(|_| StorageError::InvalidBackup)?;
        if !values.iter().all(|(name, value)| {
            matches!(name.as_str(), "librarySort" | "librarySortAsc" | "libraryFilterUnread"
                | "libraryFilterDownloaded" | "libraryFilterKind" | "libraryFilterSources"
                | "gridColumns" | "badgeSource" | "badgeUnread" | "badgeDownloaded")
                && park_domain::settings::valid(name, value)
        }) {
            return Err(StorageError::InvalidBackup);
        }
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        backup::set_preferences(&self.root, values)?;
        Ok(())
    }

    pub fn save_library_sort(&self, sort: String, ascending: bool) -> Result<(), StorageError> {
        let value = serde_json::Value::String(sort);
        if !park_domain::settings::valid("librarySort",&value) { return Err(StorageError::InvalidBackup); }
        let _gate = park_storage::recovery::gate(&self.root,true)?;
        backup::set_preferences(&self.root,[("librarySort".into(),value),("librarySortAsc".into(),ascending.into())])?;
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

    pub fn save_block_rules(&self, patch: String) -> Result<(), StorageError> {
        let values: serde_json::Map<String,serde_json::Value> =
            serde_json::from_str(&patch).map_err(|_|StorageError::InvalidBackup)?;
        if !values.iter().all(|(k,v)| matches!(k.as_str(),"blockedTags"|"blockedTagAliases"|"blockPresets"|"blockedWorks"|"blockedAuthors")
            && park_domain::settings::valid(k,v)) {return Err(StorageError::InvalidBackup);}
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        backup::set_preferences(&self.root,values)?;
        Ok(())
    }

    pub fn remember_game_reply(&self, source: String) -> Result<(), StorageError> {
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        Ok(backup::remember_game_reply(&self.root, source)?)
    }

    pub fn set_home_membership(&self, name: String, entry: String, enabled: bool) -> Result<Vec<String>, StorageError> {
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        Ok(backup::set_home_membership(&self.root, &name, entry, enabled)?)
    }

    pub fn ignore_dictionary_reminder(&self, source: String) -> Result<(), StorageError> {
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        Ok(backup::ignore_dictionary_reminder(&self.root, source)?)
    }

    pub fn block_library_item(&self, source: String, work: String, title: String, author: Option<String>) -> Result<(), StorageError> {
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        Ok(backup::block_library_item(&self.root, &source, &work, &title, author.as_deref())?)
    }

    pub fn add_blocked_tag(&self, tag: String) -> Result<Vec<String>, StorageError> {
        let _gate = park_storage::recovery::gate(&self.root, true)?;
        Ok(backup::add_blocked_tag(&self.root, tag)?)
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
