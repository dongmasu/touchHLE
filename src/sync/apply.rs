/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::model::{RelativePath, SnapshotEntry, SyncError};
use crate::paths::SYNC_DIR;
use cap_fs_ext::DirExt;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use unicode_casefold::UnicodeCaseFold;
use uuid::Uuid;

pub struct StagedFile {
    path: RelativePath,
    destination: PathBuf,
    operation: StagedOperation,
}

enum StagedOperation {
    File {
        temporary_path: PathBuf,
        expected_sha256: [u8; 32],
    },
    Tombstone,
}

/// `entries` must be selected from a whole snapshot validated by
/// `SyncPlan::resolved_snapshot`; never pass unresolved plan action maps.
pub fn stage_remote_files(
    root: &Path,
    entries: &BTreeMap<RelativePath, SnapshotEntry>,
    mut read_object: impl FnMut(&[u8; 32]) -> Result<Vec<u8>, SyncError>,
) -> Result<Vec<StagedFile>, SyncError> {
    validate_selected(entries)?;
    let root_dir = Dir::open_ambient_dir(root, ambient_authority())?;
    if entries
        .values()
        .all(|entry| matches!(entry, SnapshotEntry::Tombstone))
    {
        return Ok(entries
            .keys()
            .map(|path| StagedFile {
                path: path.clone(),
                destination: root.join(path.as_str()),
                operation: StagedOperation::Tombstone,
            })
            .collect());
    }
    let sync_dir = open_sync_dir(&root_dir)?;
    let name = format!("stage-{}", Uuid::new_v4());
    let stage_dir = create_private_dir(&sync_dir, &name)?;
    let mut staged = Vec::with_capacity(entries.len());
    let result = (|| {
        for (path, entry) in entries {
            let destination = root.join(path.as_str());
            let operation = match entry {
                SnapshotEntry::File { sha256, size, .. } => {
                    let bytes = read_object(sha256)?;
                    if bytes.len() as u64 != *size || Sha256::digest(&bytes).as_slice() != sha256 {
                        return Err(SyncError::Integrity(format!(
                            "downloaded object mismatch for {}",
                            path.as_str()
                        )));
                    }
                    let filename = format!("{}", staged.len());
                    let mut file = stage_dir
                        .open_with(&filename, OpenOptions::new().write(true).create_new(true))?;
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                    StagedOperation::File {
                        temporary_path: root.join(SYNC_DIR).join(&name).join(filename),
                        expected_sha256: *sha256,
                    }
                }
                SnapshotEntry::Tombstone => StagedOperation::Tombstone,
            };
            staged.push(StagedFile {
                path: path.clone(),
                destination,
                operation,
            });
        }
        Ok(staged)
    })();
    if result.is_err() {
        sync_dir.remove_dir_all(&name)?;
    }
    result
}

/// Each rename is atomic; a multi-file apply is not an atomic transaction.
/// The caller must save the new baseline only after the complete apply succeeds.
pub fn apply_staged_files(root: &Path, staged: Vec<StagedFile>) -> Result<(), SyncError> {
    let root_dir = Dir::open_ambient_dir(root, ambient_authority())?;
    let mut selected = BTreeMap::new();
    for item in &staged {
        if item.destination != root.join(item.path.as_str()) {
            return Err(SyncError::InvalidPath(item.path.as_str().to_owned()));
        }
        let entry = match item.operation {
            StagedOperation::File {
                expected_sha256, ..
            } => SnapshotEntry::File {
                sha256: expected_sha256,
                size: 0,
                modified_unix_ms: 0,
            },
            StagedOperation::Tombstone => SnapshotEntry::Tombstone,
        };
        if selected.insert(item.path.clone(), entry).is_some() {
            return Err(SyncError::InvalidPath(item.path.as_str().to_owned()));
        }
    }
    validate_selected(&selected)?;

    // Verify every staged object before changing any live file.
    let sync_dir = open_sync_dir(&root_dir)?;
    let mut verified = Vec::with_capacity(staged.len());
    for item in &staged {
        let bytes = match &item.operation {
            StagedOperation::File {
                temporary_path,
                expected_sha256,
            } => {
                let relative = temporary_path
                    .strip_prefix(root.join(SYNC_DIR))
                    .map_err(|_| SyncError::InvalidPath(item.path.as_str().to_owned()))?;
                let (stage_name, file_name) = staging_parts(relative)?;
                let stage_dir = sync_dir.open_dir_nofollow(stage_name)?;
                let bytes = read_regular(&stage_dir, file_name)?;
                verify_hash(&bytes, expected_sha256)?;
                Some(bytes)
            }
            StagedOperation::Tombstone => None,
        };
        verified.push(bytes);
    }

    // Keep every reachable displaced blob before any replacement or deletion.
    for item in &staged {
        let Some((parent, name)) = open_parent(&root_dir, &item.path, false)? else {
            continue;
        };
        match parent.symlink_metadata(name) {
            Ok(metadata) if metadata.is_file() => {
                let bytes = read_regular(&parent, name)?;
                let hash: [u8; 32] = Sha256::digest(&bytes).into();
                preserve_local_version(root, &item.path, &hash)?;
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(SyncError::InvalidPath(item.path.as_str().to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }

    // Remove selected tombstones before creating descendants of old files.
    for item in &staged {
        if !matches!(item.operation, StagedOperation::Tombstone) {
            continue;
        }
        if let Some((parent, name)) = open_parent(&root_dir, &item.path, false)? {
            match parent.symlink_metadata(name) {
                Ok(metadata) if metadata.is_file() => parent.remove_file(name)?,
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => return Err(SyncError::InvalidPath(item.path.as_str().to_owned())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    for (item, bytes) in staged.iter().zip(&verified) {
        if let Some(bytes) = bytes {
            let (parent, name) = open_parent(&root_dir, &item.path, true)?.unwrap();
            match parent.symlink_metadata(name) {
                Ok(metadata) if metadata.is_dir() => parent.remove_dir(name)?,
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => return Err(SyncError::InvalidPath(item.path.as_str().to_owned())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            let temporary = format!(".touchHLE-sync-{}", Uuid::new_v4());
            let mut file =
                parent.open_with(&temporary, OpenOptions::new().write(true).create_new(true))?;
            file.write_all(bytes)?;
            file.sync_all()?;
            let result = parent.rename(&temporary, &parent, name);
            if result.is_err() {
                let _ = parent.remove_file(&temporary);
            }
            result?;
        }
    }

    for item in &staged {
        if let StagedOperation::File { temporary_path, .. } = &item.operation {
            let relative = temporary_path.strip_prefix(root.join(SYNC_DIR)).unwrap();
            let (stage_name, file_name) = staging_parts(relative)?;
            let stage_dir = sync_dir.open_dir_nofollow(stage_name)?;
            stage_dir.remove_file(file_name)?;
            let _ = sync_dir.remove_dir(stage_name);
        }
    }
    Ok(())
}

pub fn preserve_local_version(
    root: &Path,
    path: &RelativePath,
    content_hash: &[u8; 32],
) -> Result<(), SyncError> {
    let root_dir = Dir::open_ambient_dir(root, ambient_authority())?;
    let (parent, name) = open_parent(&root_dir, path, false)?
        .ok_or_else(|| SyncError::InvalidPath(path.as_str().to_owned()))?;
    let bytes = read_regular(&parent, name)?;
    verify_hash(&bytes, content_hash)?;
    let sync_dir = open_sync_dir(&root_dir)?;
    let recovery = open_private_dir(&sync_dir, "recovery")?;
    let filename = hex_hash(content_hash);
    if let Ok(existing) = read_regular(&recovery, &filename) {
        return verify_hash(&existing, content_hash);
    }
    let temp = format!(".recovery-{}", Uuid::new_v4());
    let mut file = recovery.open_with(&temp, OpenOptions::new().write(true).create_new(true))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    // A hard link never replaces an existing recovery version.
    let result = recovery.hard_link(&temp, &recovery, &filename);
    recovery.remove_file(&temp)?;
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_hash(&read_regular(&recovery, &filename)?, content_hash)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
fn recovery_path(root: &Path, hash: &[u8; 32]) -> PathBuf {
    root.join(SYNC_DIR).join("recovery").join(hex_hash(hash))
}

fn hex_hash(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn verify_hash(bytes: &[u8], hash: &[u8; 32]) -> Result<(), SyncError> {
    if Sha256::digest(bytes).as_slice() == hash {
        Ok(())
    } else {
        Err(SyncError::Integrity("content hash mismatch".to_owned()))
    }
}

fn staging_parts(path: &Path) -> Result<(&std::ffi::OsStr, &std::ffi::OsStr), SyncError> {
    let mut parts = path.components();
    let first = parts.next();
    let second = parts.next();
    match (first, second, parts.next()) {
        (
            Some(std::path::Component::Normal(dir)),
            Some(std::path::Component::Normal(file)),
            None,
        ) if dir.to_string_lossy().starts_with("stage-") => Ok((dir, file)),
        _ => Err(SyncError::InvalidPath(path.display().to_string())),
    }
}

fn validate_selected(entries: &BTreeMap<RelativePath, SnapshotEntry>) -> Result<(), SyncError> {
    let mut spellings = BTreeMap::<String, String>::new();
    let files: BTreeSet<_> = entries
        .iter()
        .filter_map(|(path, entry)| {
            matches!(entry, SnapshotEntry::File { .. }).then_some(path.as_str())
        })
        .collect();
    for (path, entry) in entries {
        let mut prefix = String::new();
        for component in path.as_str().split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let folded = prefix.as_str().case_fold().collect::<String>();
            if let Some(previous) = spellings.insert(folded, prefix.clone()) {
                if previous != prefix {
                    return Err(SyncError::InvalidPath(path.as_str().to_owned()));
                }
            }
        }
        if matches!(entry, SnapshotEntry::File { .. }) {
            let mut current = path.as_str();
            while let Some((parent, _)) = current.rsplit_once('/') {
                if files.contains(parent) {
                    return Err(SyncError::InvalidPath(path.as_str().to_owned()));
                }
                current = parent;
            }
        }
    }
    Ok(())
}

fn ensure_dir(parent: &Dir, name: &str) -> Result<(), SyncError> {
    match parent.create_dir(name) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn open_sync_dir(root: &Dir) -> Result<Dir, SyncError> {
    open_private_dir(root, SYNC_DIR)
}

fn create_private_dir(parent: &Dir, name: &str) -> Result<Dir, SyncError> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use cap_std::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    parent.create_dir_with(name, &builder)?;
    Ok(parent.open_dir_nofollow(name)?)
}

fn open_private_dir(parent: &Dir, name: &str) -> Result<Dir, SyncError> {
    match create_private_dir(parent, name) {
        Ok(dir) => Ok(dir),
        Err(SyncError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(parent.open_dir_nofollow(name)?)
        }
        Err(e) => Err(e),
    }
}

fn open_parent<'a>(
    root: &Dir,
    path: &'a RelativePath,
    create: bool,
) -> Result<Option<(Dir, &'a str)>, SyncError> {
    let (parent, name) = path.as_str().rsplit_once('/').unwrap();
    let mut dir = root.try_clone()?;
    for component in parent.split('/') {
        if create {
            ensure_dir(&dir, component)?;
        }
        dir = match dir.open_dir_nofollow(component) {
            Ok(dir) => dir,
            Err(e)
                if !create
                    && matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
    }
    Ok(Some((dir, name)))
}

fn read_regular(parent: &Dir, name: impl AsRef<Path>) -> Result<Vec<u8>, SyncError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use cap_fs_ext::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let mut file = parent.open_with(name, &options)?;
    if !file.metadata()?.is_file() {
        return Err(SyncError::Integrity("expected regular file".to_owned()));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::model::{RelativePath, SnapshotEntry, SyncError};
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeMap, fs, path::PathBuf};
    use uuid::Uuid;

    struct TestTree(PathBuf);

    impl TestTree {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("touchhle-apply-test-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            fs::create_dir(path.join("touchHLE_apps")).unwrap();
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join("touchHLE_apps").join(name)
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn file_entry(bytes: &[u8]) -> SnapshotEntry {
        SnapshotEntry::File {
            sha256: Sha256::digest(bytes).into(),
            size: bytes.len() as u64,
            modified_unix_ms: 0,
        }
    }

    fn entries(name: &str, entry: SnapshotEntry) -> BTreeMap<RelativePath, SnapshotEntry> {
        BTreeMap::from([(RelativePath::new(name).unwrap(), entry)])
    }

    #[test]
    fn hash_mismatch_does_not_touch_live_file() {
        let tree = TestTree::new();
        fs::write(tree.file("save"), b"local").unwrap();
        let result = stage_remote_files(
            &tree.0,
            &entries("touchHLE_apps/save", file_entry(b"remote")),
            |_| Ok(b"tampered".to_vec()),
        );
        assert!(matches!(result, Err(SyncError::Integrity(_))));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"local");
    }

    #[test]
    fn malicious_remote_path_is_rejected_before_staging() {
        assert!(RelativePath::new("touchHLE_apps/../../escape").is_err());
        assert!(RelativePath::new("touchHLE_apps\\save").is_err());
        assert!(RelativePath::new("touchHLE_apps/CON").is_err());
    }

    #[test]
    fn installed_file_has_verified_contents_and_preserves_prior_version() {
        let tree = TestTree::new();
        fs::write(tree.file("save"), b"local").unwrap();
        let staged = stage_remote_files(
            &tree.0,
            &entries("touchHLE_apps/save", file_entry(b"remote")),
            |_| Ok(b"remote".to_vec()),
        )
        .unwrap();
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"local");
        apply_staged_files(&tree.0, staged).unwrap();
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"remote");
        let local_hash: [u8; 32] = Sha256::digest(b"local").into();
        assert_eq!(
            fs::read(recovery_path(&tree.0, &local_hash)).unwrap(),
            b"local"
        );
    }

    #[test]
    fn deletion_is_reversible_through_recovery() {
        let tree = TestTree::new();
        fs::write(tree.file("save"), b"local").unwrap();
        let staged = stage_remote_files(
            &tree.0,
            &entries("touchHLE_apps/save", SnapshotEntry::Tombstone),
            |_| panic!("tombstones do not read objects"),
        )
        .unwrap();
        apply_staged_files(&tree.0, staged).unwrap();
        assert!(!tree.file("save").exists());
        let hash: [u8; 32] = Sha256::digest(b"local").into();
        assert_eq!(fs::read(recovery_path(&tree.0, &hash)).unwrap(), b"local");
    }

    #[test]
    fn failed_second_read_leaves_both_live_files_and_baseline_unchanged() {
        let tree = TestTree::new();
        fs::write(tree.file("a"), b"first local").unwrap();
        fs::write(tree.file("b"), b"second local").unwrap();
        let state = tree.0.join(".touchHLE_sync/state.json");
        fs::create_dir(state.parent().unwrap()).unwrap();
        fs::write(&state, b"baseline").unwrap();
        let selected = BTreeMap::from([
            (
                RelativePath::new("touchHLE_apps/a").unwrap(),
                file_entry(b"first remote"),
            ),
            (
                RelativePath::new("touchHLE_apps/b").unwrap(),
                file_entry(b"second remote"),
            ),
        ]);
        let mut reads = 0;
        let result = stage_remote_files(&tree.0, &selected, |_| {
            reads += 1;
            if reads == 1 {
                Ok(b"first remote".to_vec())
            } else {
                Err(SyncError::Provider("injected failure".into()))
            }
        });
        assert!(matches!(result, Err(SyncError::Provider(_))));
        assert_eq!(reads, 2);
        assert_eq!(fs::read(tree.file("a")).unwrap(), b"first local");
        assert_eq!(fs::read(tree.file("b")).unwrap(), b"second local");
        assert_eq!(fs::read(state).unwrap(), b"baseline");
    }

    #[test]
    fn tombstone_below_live_file_does_not_require_a_directory() {
        let tree = TestTree::new();
        fs::write(tree.file("parent"), b"local").unwrap();
        let selected = BTreeMap::from([
            (
                RelativePath::new("touchHLE_apps/parent").unwrap(),
                file_entry(b"remote"),
            ),
            (
                RelativePath::new("touchHLE_apps/parent/old").unwrap(),
                SnapshotEntry::Tombstone,
            ),
        ]);
        let staged = stage_remote_files(&tree.0, &selected, |_| Ok(b"remote".to_vec())).unwrap();
        apply_staged_files(&tree.0, staged).unwrap();
        assert_eq!(fs::read(tree.file("parent")).unwrap(), b"remote");
    }

    #[test]
    fn altered_staging_is_rejected_before_any_live_change() {
        let tree = TestTree::new();
        fs::write(tree.file("save"), b"local").unwrap();
        let staged = stage_remote_files(
            &tree.0,
            &entries("touchHLE_apps/save", file_entry(b"remote")),
            |_| Ok(b"remote".to_vec()),
        )
        .unwrap();
        let StagedOperation::File { temporary_path, .. } = &staged[0].operation else {
            panic!("expected staged file");
        };
        fs::write(temporary_path, b"tampered").unwrap();
        assert!(matches!(
            apply_staged_files(&tree.0, staged),
            Err(SyncError::Integrity(_))
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"local");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_destination_directory_is_not_followed() {
        use std::os::unix::fs::symlink;

        let tree = TestTree::new();
        let outside = tree.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("save"), b"outside").unwrap();
        symlink(&outside, tree.file("linked")).unwrap();
        let staged = stage_remote_files(
            &tree.0,
            &entries("touchHLE_apps/linked/save", file_entry(b"remote")),
            |_| Ok(b"remote".to_vec()),
        )
        .unwrap();
        assert!(apply_staged_files(&tree.0, staged).is_err());
        assert_eq!(fs::read(outside.join("save")).unwrap(), b"outside");
    }

    #[test]
    fn replacing_local_file_with_remote_directory_preserves_the_file() {
        let tree = TestTree::new();
        fs::write(tree.file("parent"), b"old parent").unwrap();
        let selected = BTreeMap::from([
            (
                RelativePath::new("touchHLE_apps/parent").unwrap(),
                SnapshotEntry::Tombstone,
            ),
            (
                RelativePath::new("touchHLE_apps/parent/child").unwrap(),
                file_entry(b"child"),
            ),
        ]);
        let staged = stage_remote_files(&tree.0, &selected, |_| Ok(b"child".to_vec())).unwrap();
        apply_staged_files(&tree.0, staged).unwrap();
        assert_eq!(fs::read(tree.file("parent/child")).unwrap(), b"child");
        let hash: [u8; 32] = Sha256::digest(b"old parent").into();
        assert_eq!(
            fs::read(recovery_path(&tree.0, &hash)).unwrap(),
            b"old parent"
        );
    }

    #[test]
    fn replacing_local_directory_with_remote_file_preserves_its_children() {
        let tree = TestTree::new();
        fs::create_dir(tree.file("parent")).unwrap();
        fs::write(tree.file("parent/old"), b"old child").unwrap();
        let selected = BTreeMap::from([
            (
                RelativePath::new("touchHLE_apps/parent/old").unwrap(),
                SnapshotEntry::Tombstone,
            ),
            (
                RelativePath::new("touchHLE_apps/parent").unwrap(),
                file_entry(b"remote"),
            ),
        ]);
        let staged = stage_remote_files(&tree.0, &selected, |_| Ok(b"remote".to_vec())).unwrap();
        apply_staged_files(&tree.0, staged).unwrap();
        assert_eq!(fs::read(tree.file("parent")).unwrap(), b"remote");
        let hash: [u8; 32] = Sha256::digest(b"old child").into();
        assert_eq!(
            fs::read(recovery_path(&tree.0, &hash)).unwrap(),
            b"old child"
        );
    }

    #[test]
    fn corrupt_recovery_copy_blocks_replacement() {
        let tree = TestTree::new();
        fs::write(tree.file("save"), b"local").unwrap();
        let local_hash: [u8; 32] = Sha256::digest(b"local").into();
        let recovery = recovery_path(&tree.0, &local_hash);
        fs::create_dir_all(recovery.parent().unwrap()).unwrap();
        fs::write(recovery, b"corrupt").unwrap();
        let staged = stage_remote_files(
            &tree.0,
            &entries("touchHLE_apps/save", file_entry(b"remote")),
            |_| Ok(b"remote".to_vec()),
        )
        .unwrap();
        assert!(matches!(
            apply_staged_files(&tree.0, staged),
            Err(SyncError::Integrity(_))
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"local");
    }
}
