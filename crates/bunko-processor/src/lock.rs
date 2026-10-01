//! One storage, one processor: an exclusive OS lock on
//! `<storage>/.processing/serve.lock` for the life of `serve`. The spool sweep and the
//! workspace sweep at start are only safe under it. The OS drops the lock when the
//! process dies, however it dies. (0.5.2 had no lock on Windows; this one uses
//! `LockFileEx` there.)

use std::fs::File;
use std::path::Path;

#[derive(Debug)]
pub struct StorageLock {
    _file: File,
}

/// Take the storage's lock. `Ok(None)` when another process holds it.
pub fn lock_storage(storage: &Path) -> std::io::Result<Option<StorageLock>> {
    let dir = storage.join(".processing");
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(dir.join("serve.lock"))?;
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => Ok(Some(StorageLock { _file: file })),
        Err(fs4::TryLockError::WouldBlock) => Ok(None),
        Err(fs4::TryLockError::Error(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_is_refused_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock_storage(dir.path()).unwrap();
        assert!(first.is_some());
        assert!(lock_storage(dir.path()).unwrap().is_none());
        drop(first);
        assert!(lock_storage(dir.path()).unwrap().is_some());
    }
}
