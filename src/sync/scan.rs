/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::model::{RelativePath, SnapshotEntry, SyncError};
use crate::paths::{APPS_DIR, SANDBOX_DIR};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;
use unicode_casefold::UnicodeCaseFold;

pub fn scan_roots(base: &Path) -> Result<BTreeMap<RelativePath, SnapshotEntry>, SyncError> {
    let mut entries = BTreeMap::new();
    let mut names = BTreeMap::new();
    for root in [APPS_DIR, SANDBOX_DIR] {
        let path = base.join(root);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if metadata.is_dir() {
            scan_directory(&path, root, &mut names, &mut entries)?;
        }
    }
    Ok(entries)
}

fn scan_directory(
    directory: &Path,
    relative: &str,
    names: &mut BTreeMap<String, String>,
    entries: &mut BTreeMap<RelativePath, SnapshotEntry>,
) -> Result<(), SyncError> {
    for item in fs::read_dir(directory)? {
        let item = item?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|name| SyncError::InvalidPath(format!("non-UTF-8 filename: {name:?}")))?;
        let path = item.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() && !metadata.is_file() {
            continue;
        }
        let key = RelativePath::new(&format!("{relative}/{name}"))?;
        // Include directories so two distinct directory spellings cannot merge.
        let folded = key.as_str().case_fold().collect::<String>();
        if let Some(previous) = names.insert(folded, key.as_str().to_owned()) {
            if previous != key.as_str() {
                return Err(SyncError::InvalidPath(format!(
                    "case-insensitive collision: {previous} and {}",
                    key.as_str()
                )));
            }
        }
        if metadata.is_dir() {
            scan_directory(&path, key.as_str(), names, entries)?;
        } else {
            entries.insert(key, hash_file(&path)?);
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<SnapshotEntry, SyncError> {
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = fs::File::open(path)?;

    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(SyncError::Integrity(format!(
            "file changed while scanning: {}",
            path.display()
        )));
    }
    let modified = metadata.modified()?;
    let millis = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i128,
        Err(e) => -(e.duration().as_millis() as i128),
    };
    let modified_unix_ms = i64::try_from(millis).map_err(|_| {
        SyncError::Integrity(format!("invalid modification time: {}", path.display()))
    })?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size += count as u64;
    }
    Ok(SnapshotEntry::File {
        sha256: hasher.finalize().into(),
        size,
        modified_unix_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{APPS_DIR, SANDBOX_DIR};
    use std::fs;
    use uuid::Uuid;

    struct TestTree(std::path::PathBuf);

    impl TestTree {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("touchhle-sync-test-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn missing_and_empty_roots_have_no_entries() {
        let tree = TestTree::new();
        assert!(scan_roots(&tree.0).unwrap().is_empty());
        fs::create_dir(tree.0.join(APPS_DIR)).unwrap();
        fs::create_dir(tree.0.join(SANDBOX_DIR)).unwrap();
        assert!(scan_roots(&tree.0).unwrap().is_empty());
    }

    #[test]
    fn hashes_regular_files_in_both_roots_and_nested_directories() {
        let tree = TestTree::new();
        fs::create_dir_all(tree.0.join(APPS_DIR).join("nested")).unwrap();
        fs::create_dir(tree.0.join(SANDBOX_DIR)).unwrap();
        fs::write(tree.0.join(APPS_DIR).join("nested/app.ipa"), b"abc").unwrap();
        fs::write(tree.0.join(SANDBOX_DIR).join("save"), b"abc").unwrap();

        let first = scan_roots(&tree.0).unwrap();
        let second = scan_roots(&tree.0).unwrap();
        assert_eq!(first, second);
        let expected = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        for path in ["touchHLE_apps/nested/app.ipa", "touchHLE_sandbox/save"] {
            match &first[&RelativePath::new(path).unwrap()] {
                SnapshotEntry::File { sha256, size, .. } => {
                    assert_eq!(*sha256, expected);
                    assert_eq!(*size, 3);
                }
                SnapshotEntry::Tombstone => panic!("scanned a tombstone"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlinked_roots_directories_and_files() {
        use std::os::unix::fs::symlink;

        let tree = TestTree::new();
        let outside = tree.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"do not scan").unwrap();
        symlink(&outside, tree.0.join(APPS_DIR)).unwrap();
        fs::create_dir(tree.0.join(SANDBOX_DIR)).unwrap();
        symlink(&outside, tree.0.join(SANDBOX_DIR).join("linked-dir")).unwrap();
        symlink(
            outside.join("secret"),
            tree.0.join(SANDBOX_DIR).join("linked-file"),
        )
        .unwrap();
        fs::write(tree.0.join(SANDBOX_DIR).join("real"), b"ok").unwrap();

        let entries = scan_roots(&tree.0).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries.contains_key(&RelativePath::new("touchHLE_sandbox/real").unwrap()));
    }

    #[test]
    fn rejects_case_insensitive_collisions() {
        let tree = TestTree::new();
        let root = tree.0.join(APPS_DIR);
        fs::create_dir(&root).unwrap();
        fs::write(root.join("Save"), b"first").unwrap();
        fs::write(root.join("save"), b"second").unwrap();
        if fs::read(root.join("Save")).unwrap() != b"first" {
            return; // The host filesystem already folds these names.
        }
        assert!(matches!(
            scan_roots(&tree.0),
            Err(SyncError::InvalidPath(_))
        ));
    }
}
