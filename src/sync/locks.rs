/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::model::SyncError;
use crate::paths::SYNC_DIR;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "android", test))]
use std::sync::{Arc, Condvar, Mutex, OnceLock};

const OPERATION_LOCK: &str = "operation.lock";
const GUEST_LOCK: &str = "guest-session.lock";

#[cfg(any(target_os = "android", test))]
static PROCESS_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<ProcessLockState>>>> = OnceLock::new();

#[cfg(any(target_os = "android", test))]
struct ProcessLockState {
    held: Mutex<HashMap<String, bool>>,
    available: Condvar,
}

#[cfg(any(target_os = "android", test))]
struct ProcessLockGuard {
    state: Arc<ProcessLockState>,
    name: String,
}

#[cfg(any(target_os = "android", test))]
impl Drop for ProcessLockGuard {
    fn drop(&mut self) {
        let mut held = self
            .state
            .held
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        held.insert(self.name.clone(), false);
        self.state.available.notify_one();
    }
}

pub struct SyncLocks {
    root: PathBuf,
}

pub struct LockGuard {
    _file: File,
    #[cfg(target_os = "android")]
    _process_lock: ProcessLockGuard,
}

impl SyncLocks {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    pub fn operation(&self) -> Result<LockGuard, SyncError> {
        self.acquire(OPERATION_LOCK, false)?
            .ok_or_else(|| SyncError::Integrity("sync operation lock unavailable".into()))
    }

    pub fn guest(&self) -> Result<LockGuard, SyncError> {
        self.acquire(GUEST_LOCK, false)?
            .ok_or_else(|| SyncError::Integrity("guest session lock unavailable".into()))
    }

    pub fn try_guest(&self) -> Result<Option<LockGuard>, SyncError> {
        self.acquire(GUEST_LOCK, true)
    }

    fn acquire(&self, name: &str, nonblocking: bool) -> Result<Option<LockGuard>, SyncError> {
        let dir = open_sync_dir(&self.root).map_err(|error| {
            log!("Could not open local sync directory for {name}: {error:?}");
            error
        })?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_fs_ext::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = dir
            .open_with(name, &options)
            .map_err(|error| {
                log!("Could not open local sync lock {name}: {error:?}");
                error
            })?
            .into_std();
        let metadata = file.metadata().map_err(|error| {
            log!("Could not inspect local sync lock {name}: {error:?}");
            error
        })?;
        if !metadata.is_file() {
            return Err(SyncError::Integrity(
                "sync lock is not a regular file".into(),
            ));
        }
        #[cfg(target_os = "android")]
        {
            log_once!(
                "Android uses process-local sync locks because app external storage does not support OS file locks"
            );
            let Some(process_lock) = acquire_process_lock(&self.root, name, nonblocking) else {
                return Ok(None);
            };
            return Ok(Some(LockGuard {
                _file: file,
                _process_lock: process_lock,
            }));
        }
        #[cfg(not(target_os = "android"))]
        if nonblocking {
            match file.try_lock() {
                Ok(()) => Ok(Some(LockGuard { _file: file })),
                Err(std::fs::TryLockError::WouldBlock) => Ok(None),
                Err(std::fs::TryLockError::Error(error)) => {
                    log!("Could not acquire local sync lock {name} without waiting: {error:?}");
                    Err(error.into())
                }
            }
        } else {
            file.lock().map_err(|error| {
                log!("Could not acquire local sync lock {name}: {error:?}");
                error
            })?;
            Ok(Some(LockGuard { _file: file }))
        }
    }
}

#[cfg(any(target_os = "android", test))]
fn acquire_process_lock(root: &Path, name: &str, nonblocking: bool) -> Option<ProcessLockGuard> {
    let state = {
        let mut locks = PROCESS_LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        locks
            .entry(root.to_path_buf())
            .or_insert_with(|| {
                Arc::new(ProcessLockState {
                    held: Mutex::new(HashMap::new()),
                    available: Condvar::new(),
                })
            })
            .clone()
    };

    let mut held = state.held.lock().unwrap_or_else(|error| error.into_inner());
    loop {
        if !held.get(name).copied().unwrap_or(false) {
            held.insert(name.to_owned(), true);
            return Some(ProcessLockGuard {
                state: Arc::clone(&state),
                name: name.to_owned(),
            });
        }
        if nonblocking {
            return None;
        }
        held = state
            .available
            .wait(held)
            .unwrap_or_else(|error| error.into_inner());
    }
}

fn open_sync_dir(root: &Path) -> Result<Dir, SyncError> {
    let root = Dir::open_ambient_dir(root, ambient_authority()).map_err(|error| {
        log!("Could not open touchHLE data root for sync locking: {error:?}");
        error
    })?;
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use cap_std::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match root.create_dir_with(SYNC_DIR, &builder) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            log!("Could not create local sync directory: {error:?}");
            return Err(error.into());
        }
    }
    root.open_dir_nofollow(SYNC_DIR).map_err(|error| {
        log!("Could not open local sync directory without following symlinks: {error:?}");
        error.into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn temp_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("touchhle-locks-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn guest_lock_contention_is_nonblocking_and_drop_releases_lock() {
        let root = temp_root();
        let locks = SyncLocks::new(&root);
        let guard = locks.guest().unwrap();

        assert!(locks.try_guest().unwrap().is_none());
        drop(guard);
        assert!(locks.try_guest().unwrap().is_some());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn operation_lock_serializes_independent_handles() {
        let root = temp_root();
        let first = SyncLocks::new(&root);
        let second = SyncLocks::new(&root);
        let guard = first.operation().unwrap();

        assert!(second.acquire(OPERATION_LOCK, true).unwrap().is_none());
        drop(guard);
        assert!(second.acquire(OPERATION_LOCK, true).unwrap().is_some());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_lock_registry_serializes_independent_handles() {
        let root = temp_root();
        let first = acquire_process_lock(&root, OPERATION_LOCK, false).unwrap();

        assert!(acquire_process_lock(&root, OPERATION_LOCK, true).is_none());
        drop(first);
        assert!(acquire_process_lock(&root, OPERATION_LOCK, true).is_some());

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lock_files_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let sync_dir = root.join(SYNC_DIR);
        fs::create_dir(&sync_dir).unwrap();
        let outside = root.join("outside");
        fs::write(&outside, b"untouched").unwrap();
        symlink(&outside, sync_dir.join(GUEST_LOCK)).unwrap();

        assert!(SyncLocks::new(&root).guest().is_err());
        assert_eq!(fs::read(outside).unwrap(), b"untouched");

        fs::remove_dir_all(root).unwrap();
    }
}
