/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
#[cfg(test)]
use super::model::Commit;
use super::model::SyncError;
use crate::paths::SYNC_DIR;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use uuid::Uuid;

const CACHE_FILE: &str = "google-drive-commit-cache.json";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct CommitCache {
    pub folder_id: String,
    pub entries: BTreeMap<String, CachedCommit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct CachedCommit {
    pub version: String,
    pub json: String,
}

impl CommitCache {
    pub fn new(folder_id: String) -> Self {
        Self {
            folder_id,
            entries: BTreeMap::new(),
        }
    }
}

fn cache_dir(root: &Path, create: bool) -> Result<Option<Dir>, SyncError> {
    let root = Dir::open_ambient_dir(root, ambient_authority())?;
    if create {
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        {
            use cap_std::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match root.create_dir_with(SYNC_DIR, &builder) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    match root.open_dir_nofollow(SYNC_DIR) {
        Ok(dir) => Ok(Some(dir)),
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn load(root: &Path) -> Result<Option<CommitCache>, SyncError> {
    let Some(dir) = cache_dir(root, false)? else {
        return Ok(None);
    };
    match dir.symlink_metadata(CACHE_FILE) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(SyncError::Integrity(
                "Google Drive commit cache is not a regular file".into(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = dir.open_with(CACHE_FILE, &options)?;
    if !file.metadata()?.is_file() {
        return Err(SyncError::Integrity(
            "Google Drive commit cache is not a regular file".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub(super) fn save(root: &Path, cache: &CommitCache) -> Result<(), SyncError> {
    let dir = cache_dir(root, true)?.unwrap();
    let temporary = format!(".google-drive-commit-cache-{}", Uuid::new_v4());
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_fs_ext::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = dir.open_with(&temporary, &options)?;
        file.write_all(&serde_json::to_vec(cache)?)?;
        file.sync_all()?;
        drop(file);
        dir.rename(&temporary, &dir, CACHE_FILE)?;
        Ok::<_, SyncError>(())
    })();
    let _ = dir.remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn commit_cache_round_trips_and_rejects_invalid_json() {
        let root = std::env::temp_dir().join(format!("touchhle-commit-cache-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let commit = Commit {
            id: Uuid::new_v4(),
            device_id: Uuid::new_v4(),
            created_unix_ms: 10,
            parents: Vec::new(),
            entries: Default::default(),
        };
        let cache = CommitCache {
            folder_id: "drive-folder".into(),
            entries: BTreeMap::from([(
                "drive-file".into(),
                CachedCommit {
                    version: "1".into(),
                    json: serde_json::to_string(&commit).unwrap(),
                },
            )]),
        };
        save(&root, &cache).unwrap();
        assert_eq!(load(&root).unwrap(), Some(cache));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root.join(SYNC_DIR).join(CACHE_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        fs::write(root.join(SYNC_DIR).join(CACHE_FILE), b"corrupt").unwrap();
        assert!(load(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
