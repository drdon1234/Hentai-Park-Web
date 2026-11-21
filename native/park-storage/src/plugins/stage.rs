use super::*;
use std::fs::{File, OpenOptions};

// 只有持有此锁的安装事务可以访问暂存的登录状态。崩溃后锁由系统释放。
fn lock(root: &Path) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("source_stage.lock"))
        .map_err(|_| Error::IoFailure)?;
    Ok(crate::recovery::try_lock_file(&file, true)?.then_some(file))
}
fn clean(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(info) if info.is_dir() => fs::remove_dir_all(path).map_err(|_| Error::IoFailure),
        Ok(_) => Err(Error::InvalidJournal),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(_) => Err(Error::IoFailure),
    }
}
pub(super) fn clean_abandoned(root: &Path) -> Result<()> {
    let path = root.join("source_stage");
    if !path.try_exists().map_err(|_| Error::IoFailure)? {
        return Ok(());
    }
    if let Some(_lock) = lock(root)? {
        clean(&path)?;
    }
    Ok(())
}
pub struct Stage {
    path: PathBuf,
    _lock: File,
}
impl Stage {
    pub fn try_create(root: &Path) -> Result<Option<Self>> {
        let Some(exclusive) = lock(root)? else {
            return Ok(None);
        };
        let path = root.join("source_stage");
        clean(&path)?;
        let mut builder = fs::DirBuilder::new();
        let dir = builder.recursive(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            dir.mode(0o700);
        }
        dir.create(&path).map_err(|_| Error::IoFailure)?;
        Ok(Some(Self {
            path,
            _lock: exclusive,
        }))
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = clean(&self.path);
    }
}
