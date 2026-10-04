/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::model::{is_ignored_sync_file, RelativePath, SnapshotEntry, SyncError};
use crate::paths::{APPS_DIR, SANDBOX_DIR, SYNC_DIR};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, Metadata, MetadataExt, OpenOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Instant, UNIX_EPOCH};
use unicode_casefold::UnicodeCaseFold;
use uuid::Uuid;

const HASH_CACHE_FILE: &str = "local-scan-hash-cache.json";
const HASH_CACHE_SCHEMA_VERSION: u32 = 1;

pub fn scan_roots(base: &Path) -> Result<BTreeMap<RelativePath, SnapshotEntry>, SyncError> {
    scan_roots_with_progress(base, |_, _| {})
}

pub fn scan_roots_with_progress(
    base: &Path,
    mut progress: impl FnMut(usize, u64),
) -> Result<BTreeMap<RelativePath, SnapshotEntry>, SyncError> {
    let started = Instant::now();
    let old_cache = match load_hash_cache(base) {
        Ok(Some(cache)) => cache,
        Ok(None) => HashCache::default(),
        Err(error) => {
            log!("Google Drive local SHA-256 cache ignored: {error}");
            HashCache::default()
        }
    };
    let result = scan_roots_impl(
        base,
        &old_cache,
        |_| {},
        |files, bytes| {
            progress(files, bytes);
        },
    )?;
    if let Err(error) = save_hash_cache(base, &result.cache) {
        log!("Google Drive local SHA-256 cache could not be saved: {error}");
    }
    log!(
        "Google Drive local scan completed: files={}, bytes={}, bytes_hashed={}, cache_hits={}, elapsed_ms={}",
        result.metrics.files,
        result.metrics.bytes,
        result.metrics.bytes_hashed,
        result.metrics.cache_hits,
        started.elapsed().as_millis()
    );
    Ok(result.entries)
}

fn scan_roots_with_hook(
    base: &Path,
    before_open: impl FnMut(&Path),
) -> Result<BTreeMap<RelativePath, SnapshotEntry>, SyncError> {
    Ok(scan_roots_impl(base, &HashCache::default(), before_open, |_, _| {})?.entries)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HashCache {
    schema_version: u32,
    entries: BTreeMap<RelativePath, CachedHash>,
}

impl Default for HashCache {
    fn default() -> Self {
        Self {
            schema_version: HASH_CACHE_SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedHash {
    fingerprint: FileFingerprint,
    sha256: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileFingerprint {
    size: u64,
    modified_secs: i64,
    modified_nanos: u32,
    changed_secs: i64,
    changed_nanos: u32,
    #[serde(default)]
    identity: Option<FileIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Default)]
struct ScanMetrics {
    files: usize,
    bytes: u64,
    bytes_hashed: u64,
    cache_hits: usize,
}

struct ScanResult {
    entries: BTreeMap<RelativePath, SnapshotEntry>,
    cache: HashCache,
    metrics: ScanMetrics,
}

fn scan_roots_impl(
    base: &Path,
    old_cache: &HashCache,
    mut before_open: impl FnMut(&Path),
    mut progress: impl FnMut(usize, u64),
) -> Result<ScanResult, SyncError> {
    let mut entries = BTreeMap::new();
    let mut cache = HashCache::default();
    let mut metrics = ScanMetrics::default();
    let mut names = BTreeMap::new();
    let base_dir = Dir::open_ambient_dir(base, ambient_authority())?;
    for root in [APPS_DIR, SANDBOX_DIR] {
        let path = base.join(root);
        let metadata = match base_dir.symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if metadata.is_dir() {
            before_open(&path);
            let root_dir = base_dir.open_dir_nofollow(root)?;
            scan_directory(
                &root_dir,
                &path,
                root,
                old_cache,
                &mut cache,
                &mut before_open,
                &mut progress,
                &mut metrics,
                &mut names,
                &mut entries,
            )?;
        }
    }
    Ok(ScanResult {
        entries,
        cache,
        metrics,
    })
}

fn scan_directory(
    directory: &Dir,
    display_path: &Path,
    relative: &str,
    old_cache: &HashCache,
    cache: &mut HashCache,
    before_open: &mut impl FnMut(&Path),
    progress: &mut impl FnMut(usize, u64),
    metrics: &mut ScanMetrics,
    names: &mut BTreeMap<String, String>,
    entries: &mut BTreeMap<RelativePath, SnapshotEntry>,
) -> Result<(), SyncError> {
    for item in directory.read_dir(".")? {
        let item = item?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|name| SyncError::InvalidPath(format!("non-UTF-8 filename: {name:?}")))?;
        let path = display_path.join(&name);
        let metadata = directory.symlink_metadata(&name)?;
        if !metadata.is_dir() && !metadata.is_file() {
            continue;
        }
        let key = RelativePath::new(&format!("{relative}/{name}"))?;
        if metadata.is_file() && is_ignored_sync_file(&key) {
            continue;
        }
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
            before_open(&path);
            let child = directory.open_dir_nofollow(&name)?;
            scan_directory(
                &child,
                &path,
                key.as_str(),
                old_cache,
                cache,
                before_open,
                progress,
                metrics,
                names,
                entries,
            )?;
        } else {
            before_open(&path);
            let previous = old_cache.entries.get(&key);
            let (entry, cached, was_hit, bytes_hashed) =
                hash_file(directory, &name, &path, previous)?;
            if let SnapshotEntry::File { size, .. } = &entry {
                metrics.files += 1;
                metrics.bytes = metrics.bytes.saturating_add(*size);
                metrics.bytes_hashed = metrics.bytes_hashed.saturating_add(bytes_hashed);
                metrics.cache_hits += usize::from(was_hit);
                progress(metrics.files, metrics.bytes);
            }
            cache.entries.insert(key.clone(), cached);
            entries.insert(key, entry);
        }
    }
    Ok(())
}

fn hash_file(
    directory: &Dir,
    name: &str,
    path: &Path,
    cached: Option<&CachedHash>,
) -> Result<(SnapshotEntry, CachedHash, bool, u64), SyncError> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use cap_fs_ext::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT opens the link, not its target.
        options.custom_flags(0x0020_0000);
    }
    let mut file = directory.open_with(name, &options)?;

    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(SyncError::Integrity(format!(
            "file changed while scanning: {}",
            path.display()
        )));
    }
    let fingerprint = file_fingerprint(&metadata, path)?;
    let modified_unix_ms = modified_unix_ms(&metadata, path)?;
    if let Some(cached) = cached.filter(|cached| cached.fingerprint == fingerprint) {
        let after = file.metadata()?;
        if file_fingerprint(&after, path)? != fingerprint {
            return Err(SyncError::Integrity(format!(
                "file changed while scanning: {}",
                path.display()
            )));
        }
        let entry = SnapshotEntry::File {
            sha256: cached.sha256,
            size: fingerprint.size,
            modified_unix_ms,
        };
        return Ok((entry, cached.clone(), true, 0));
    }

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
    let after = file.metadata()?;
    if size != fingerprint.size || file_fingerprint(&after, path)? != fingerprint {
        return Err(SyncError::Integrity(format!(
            "file changed while scanning: {}",
            path.display()
        )));
    }
    let sha256 = hasher.finalize().into();
    Ok((
        SnapshotEntry::File {
            sha256,
            size,
            modified_unix_ms,
        },
        CachedHash {
            fingerprint,
            sha256,
        },
        false,
        size,
    ))
}

fn file_fingerprint(metadata: &Metadata, path: &Path) -> Result<FileFingerprint, SyncError> {
    #[cfg(unix)]
    {
        Ok(FileFingerprint {
            size: metadata.len(),
            modified_secs: metadata.mtime(),
            modified_nanos: u32::try_from(metadata.mtime_nsec()).map_err(|_| {
                SyncError::Integrity(format!("invalid modification time: {}", path.display()))
            })?,
            changed_secs: metadata.ctime(),
            changed_nanos: u32::try_from(metadata.ctime_nsec()).map_err(|_| {
                SyncError::Integrity(format!("invalid change time: {}", path.display()))
            })?,
            identity: Some(FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }),
        })
    }
    #[cfg(not(unix))]
    {
        let modified = system_time_parts(metadata.modified()?.into_std(), path)?;
        let created = metadata.created().unwrap_or(metadata.modified()?);
        let changed = system_time_parts(created.into_std(), path)?;
        Ok(FileFingerprint {
            size: metadata.len(),
            modified_secs: modified.0,
            modified_nanos: modified.1,
            changed_secs: changed.0,
            changed_nanos: changed.1,
            identity: None,
        })
    }
}

fn modified_unix_ms(metadata: &Metadata, path: &Path) -> Result<i64, SyncError> {
    let modified = metadata.modified()?;
    let millis = match modified.into_std().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i128,
        Err(error) => -(error.duration().as_millis() as i128),
    };
    i64::try_from(millis)
        .map_err(|_| SyncError::Integrity(format!("invalid modification time: {}", path.display())))
}

#[cfg(not(unix))]
fn system_time_parts(time: std::time::SystemTime, path: &Path) -> Result<(i64, u32), SyncError> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => Ok((
            i64::try_from(duration.as_secs()).map_err(|_| {
                SyncError::Integrity(format!("invalid file timestamp: {}", path.display()))
            })?,
            duration.subsec_nanos(),
        )),
        Err(error) => {
            let duration = error.duration();
            let secs = i64::try_from(duration.as_secs()).map_err(|_| {
                SyncError::Integrity(format!("invalid file timestamp: {}", path.display()))
            })?;
            if duration.subsec_nanos() == 0 {
                Ok((-secs, 0))
            } else {
                Ok((-secs - 1, 1_000_000_000 - duration.subsec_nanos()))
            }
        }
    }
}

fn hash_cache_dir(base: &Path, create: bool) -> Result<Option<Dir>, SyncError> {
    let base = Dir::open_ambient_dir(base, ambient_authority())?;
    if create {
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        {
            use cap_std::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match base.create_dir_with(SYNC_DIR, &builder) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    match base.open_dir_nofollow(SYNC_DIR) {
        Ok(dir) => Ok(Some(dir)),
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn load_hash_cache(base: &Path) -> Result<Option<HashCache>, SyncError> {
    let Some(dir) = hash_cache_dir(base, false)? else {
        return Ok(None);
    };
    match dir.symlink_metadata(HASH_CACHE_FILE) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(SyncError::Integrity(
                "local SHA-256 cache is not a regular file".into(),
            ))
        }
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
    let mut file = dir.open_with(HASH_CACHE_FILE, &options)?;
    if !file.metadata()?.is_file() {
        return Err(SyncError::Integrity(
            "local SHA-256 cache is not a regular file".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let cache: HashCache = serde_json::from_slice(&bytes)?;
    if cache.schema_version != HASH_CACHE_SCHEMA_VERSION {
        return Err(SyncError::Integrity(
            "unsupported local SHA-256 cache version".into(),
        ));
    }
    Ok(Some(cache))
}

fn save_hash_cache(base: &Path, cache: &HashCache) -> Result<(), SyncError> {
    let dir = hash_cache_dir(base, true)?.unwrap();
    let temporary = format!(".local-scan-hash-cache-{}", Uuid::new_v4());
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
        dir.rename(&temporary, &dir, HASH_CACHE_FILE)?;
        super::apply::sync_directory(&dir)?;
        Ok::<_, SyncError>(())
    })();
    let _ = dir.remove_file(&temporary);
    result
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

    #[test]
    fn unchanged_files_reuse_cached_sha256_without_reading_file_contents() {
        let tree = TestTree::new();
        let apps = tree.0.join(APPS_DIR);
        fs::create_dir_all(&apps).unwrap();
        fs::write(apps.join("large.ipa"), vec![0x5a; 1024 * 1024]).unwrap();

        let first = scan_roots_impl(&tree.0, &HashCache::default(), |_| {}, |_, _| {}).unwrap();
        let second = scan_roots_impl(&tree.0, &first.cache, |_| {}, |_, _| {}).unwrap();

        assert_eq!(second.entries, first.entries);
        assert_eq!(second.metrics.cache_hits, 1);
        assert_eq!(second.metrics.bytes_hashed, 0);
        assert_eq!(second.metrics.bytes, 1024 * 1024);
    }

    #[test]
    fn changed_file_metadata_invalidates_cached_sha256() {
        let tree = TestTree::new();
        let apps = tree.0.join(APPS_DIR);
        fs::create_dir_all(&apps).unwrap();
        let file = apps.join("app.ipa");
        fs::write(&file, b"old").unwrap();
        let first = scan_roots_impl(&tree.0, &HashCache::default(), |_| {}, |_, _| {}).unwrap();
        fs::write(&file, b"new contents").unwrap();

        let second = scan_roots_impl(&tree.0, &first.cache, |_| {}, |_, _| {}).unwrap();

        assert_eq!(second.metrics.cache_hits, 0);
        assert_eq!(second.metrics.bytes_hashed, b"new contents".len() as u64);
        assert_ne!(second.entries, first.entries);
    }

    #[test]
    fn hash_cache_round_trips_under_private_sync_directory() {
        let tree = TestTree::new();
        let apps = tree.0.join(APPS_DIR);
        fs::create_dir_all(&apps).unwrap();
        fs::write(apps.join("save"), b"bytes").unwrap();
        let scan = scan_roots_impl(&tree.0, &HashCache::default(), |_| {}, |_, _| {}).unwrap();

        save_hash_cache(&tree.0, &scan.cache).unwrap();

        assert_eq!(load_hash_cache(&tree.0).unwrap(), Some(scan.cache));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(tree.0.join(SYNC_DIR).join(HASH_CACHE_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn ignores_common_os_metadata_files_in_both_roots() {
        let tree = TestTree::new();
        let apps = tree.0.join(APPS_DIR);
        let sandbox = tree.0.join(SANDBOX_DIR).join("nested");
        fs::create_dir_all(&apps).unwrap();
        fs::create_dir_all(&sandbox).unwrap();
        for name in [".DS_Store", "Thumbs.db", "ehthumbs.db", "desktop.ini"] {
            fs::write(apps.join(name), b"metadata").unwrap();
        }
        fs::write(sandbox.join(".ds_store"), b"metadata").unwrap();
        fs::write(sandbox.join("save.dat"), b"game data").unwrap();

        let entries = scan_roots(&tree.0).unwrap();

        assert_eq!(entries.len(), 1);
        assert!(
            entries.contains_key(&RelativePath::new("touchHLE_sandbox/nested/save.dat").unwrap())
        );
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

    #[cfg(unix)]
    #[test]
    fn directory_replaced_with_symlink_is_never_traversed() {
        use std::os::unix::fs::symlink;

        let tree = TestTree::new();
        let outside = tree.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"outside data").unwrap();
        let root = tree.0.join(APPS_DIR);
        fs::create_dir(&root).unwrap();
        let nested = root.join("nested");
        fs::create_dir(&nested).unwrap();
        let mut swapped = false;
        let result = scan_roots_with_hook(&tree.0, |path| {
            if path == nested && !swapped {
                let parked = root.join("parked");
                fs::rename(&nested, &parked).unwrap();
                symlink(&outside, &nested).unwrap();
                swapped = true;
            }
        });
        assert!(swapped);
        assert!(
            result.is_err()
                || !result
                    .unwrap()
                    .contains_key(&RelativePath::new("touchHLE_apps/nested/secret").unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_replaced_with_symlink_is_never_traversed() {
        use std::os::unix::fs::symlink;

        let tree = TestTree::new();
        let outside = tree.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"outside data").unwrap();
        let root = tree.0.join(APPS_DIR);
        fs::create_dir(&root).unwrap();
        let mut swapped = false;
        let result = scan_roots_with_hook(&tree.0, |path| {
            if path == root && !swapped {
                fs::rename(&root, tree.0.join("parked")).unwrap();
                symlink(&outside, &root).unwrap();
                swapped = true;
            }
        });
        assert!(swapped);
        assert!(
            result.is_err()
                || !result
                    .unwrap()
                    .contains_key(&RelativePath::new("touchHLE_apps/secret").unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn parent_replaced_before_file_open_cannot_redirect_hashing() {
        use std::os::unix::fs::symlink;

        let tree = TestTree::new();
        let outside = tree.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("save"), b"outside").unwrap();
        let root = tree.0.join(APPS_DIR);
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("save"), b"inside").unwrap();
        let mut swapped = false;
        let entries = scan_roots_with_hook(&tree.0, |path| {
            if path == nested.join("save") && !swapped {
                fs::rename(&nested, root.join("parked")).unwrap();
                symlink(&outside, &nested).unwrap();
                swapped = true;
            }
        })
        .unwrap();
        assert!(swapped);
        let key = RelativePath::new("touchHLE_apps/nested/save").unwrap();
        match &entries[&key] {
            SnapshotEntry::File { sha256, .. } => {
                let inside: [u8; 32] = Sha256::digest(b"inside").into();
                let outside: [u8; 32] = Sha256::digest(b"outside").into();
                assert_eq!(*sha256, inside);
                assert_ne!(*sha256, outside);
            }
            SnapshotEntry::Tombstone => panic!("scanned a tombstone"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_replaced_with_symlink_before_open_is_not_hashed() {
        use std::os::unix::fs::symlink;

        let tree = TestTree::new();
        let outside = tree.0.join("outside");
        fs::write(&outside, b"outside").unwrap();
        let root = tree.0.join(APPS_DIR);
        fs::create_dir(&root).unwrap();
        let file = root.join("save");
        fs::write(&file, b"inside").unwrap();
        let mut swapped = false;
        let result = scan_roots_with_hook(&tree.0, |path| {
            if path == file && !swapped {
                fs::rename(&file, root.join("parked")).unwrap();
                symlink(&outside, &file).unwrap();
                swapped = true;
            }
        });
        assert!(swapped);
        assert!(result.is_err());
    }
}
