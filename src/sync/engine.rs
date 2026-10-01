/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::apply::{apply_staged_files, stage_remote_files};
use super::merge::{plan_sync, resolve_remote_tips, SyncPlan};
use super::model::{Commit, RelativePath, SnapshotEntry, SyncError, SyncState};
use super::scan::scan_roots;
use super::store::RemoteStore;
use crate::paths::SYNC_DIR;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Debug)]
pub enum SyncOutcome {
    Offline,
    UpToDate,
    Applied,
    Published,
    Conflicts(SyncPlan),
}

pub struct SyncEngine<S: RemoteStore> {
    store: S,
    root: PathBuf,
    state_path: PathBuf,
}

impl<S: RemoteStore> SyncEngine<S> {
    pub fn new(store: S, root: PathBuf, state_path: PathBuf) -> Self {
        Self {
            store,
            root,
            state_path,
        }
    }

    pub fn store(&mut self) -> &mut S {
        &mut self.store
    }

    pub fn into_store(self) -> S {
        self.store
    }

    /// The caller must keep the guest and every other local-tree writer
    /// quiescent for the entire operation, especially through local apply.
    pub fn synchronize(&mut self) -> Result<SyncOutcome, SyncError> {
        let state = load_state(&self.root, &self.state_path)?;
        let local = scan_roots(&self.root)?;
        let commits = match self.store.list_commits() {
            Err(SyncError::Authentication(_)) => return Ok(SyncOutcome::Offline),
            other => other?,
        };
        let tips = resolve_remote_tips(&commits)?;
        let plan = plan_sync(&state.baseline, &local, &tips)?;
        if !plan.conflicts.is_empty() {
            return Ok(SyncOutcome::Conflicts(plan));
        }
        let resolved = plan.resolved_snapshot(&[])?;
        let publish = !plan.publish_local.is_empty()
            || tips.len() > 1
            || (tips.is_empty() && !resolved.is_empty());
        let apply = !plan.apply_remote.is_empty();

        if publish {
            // A full snapshot commit must never reference an unverified object.
            for (path, entry) in &resolved {
                let SnapshotEntry::File { sha256, size, .. } = entry else {
                    continue;
                };
                let remote = self.store.read_object(sha256)?;
                if let Some(bytes) = remote {
                    verify_object(&bytes, sha256, *size)?;
                } else {
                    let bytes = read_local(&self.root, path)?;
                    verify_object(&bytes, sha256, *size)?;
                    self.store.write_object(*sha256, &bytes)?;
                    let written = self.store.read_object(sha256)?.ok_or_else(|| {
                        SyncError::Integrity(format!(
                            "object absent after write: {}",
                            hex_hash(sha256)
                        ))
                    })?;
                    verify_object(&written, sha256, *size)?;
                }
            }
        } else {
            // Even a converged local copy cannot justify advancing state if
            // its referenced cloud object has gone missing or been corrupted.
            for entry in resolved.values() {
                if let SnapshotEntry::File { sha256, size, .. } = entry {
                    let bytes = self.store.read_object(sha256)?.ok_or_else(|| {
                        SyncError::Integrity(format!("missing remote object {}", hex_hash(sha256)))
                    })?;
                    verify_object(&bytes, sha256, *size)?;
                }
            }
        }

        let commit_id = if publish {
            let millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| SyncError::Integrity(e.to_string()))?
                .as_millis();
            let commit = Commit {
                id: Uuid::new_v4(),
                device_id: state.device_id,
                created_unix_ms: i64::try_from(millis)
                    .map_err(|_| SyncError::Integrity("invalid current time".into()))?,
                parents: plan.merge_parents.clone(),
                entries: resolved.clone(),
            };
            self.store.write_commit(&commit)?;
            Some(commit.id)
        } else {
            tips.first().map(|tip| tip.commit_id)
        };

        // Stage only after publication has succeeded; the staging layer
        // cleans up its own scratch files if any download fails.
        let staged = stage_remote_files(&self.root, &plan, &[], |hash| {
            self.store.read_object(hash)?.ok_or_else(|| {
                SyncError::Integrity(format!("missing remote object {}", hex_hash(hash)))
            })
        })?;
        apply_staged_files(&self.root, staged)?;
        if state.baseline != resolved
            || state.last_applied_commit != commit_id
            || !self.state_path.exists()
        {
            save_state(
                &self.root,
                &self.state_path,
                &SyncState {
                    device_id: state.device_id,
                    last_applied_commit: commit_id,
                    baseline: resolved,
                },
            )?;
        }
        Ok(if publish {
            SyncOutcome::Published
        } else if apply {
            SyncOutcome::Applied
        } else {
            SyncOutcome::UpToDate
        })
    }
}

fn state_dir(root: &Path, path: &Path, create: bool) -> Result<Option<Dir>, SyncError> {
    if path != root.join(SYNC_DIR).join("state.json") {
        return Err(SyncError::InvalidPath(path.display().to_string()));
    }
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
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    match root.open_dir_nofollow(SYNC_DIR) {
        Ok(dir) => Ok(Some(dir)),
        Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn load_state(root: &Path, path: &Path) -> Result<SyncState, SyncError> {
    let Some(dir) = state_dir(root, path, false)? else {
        return Ok(fresh_state());
    };
    match dir.symlink_metadata("state.json") {
        Ok(metadata) if !metadata.is_file() => {
            return Err(SyncError::Integrity("state is not a regular file".into()));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(fresh_state()),
        Err(e) => return Err(e.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match dir.open_with("state.json", &options) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(fresh_state()),
        Err(e) => return Err(e.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(SyncError::Integrity("state is not a regular file".into()));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn fresh_state() -> SyncState {
    SyncState {
        device_id: Uuid::new_v4(),
        last_applied_commit: None,
        baseline: BTreeMap::new(),
    }
}

fn save_state(root: &Path, path: &Path, state: &SyncState) -> Result<(), SyncError> {
    let dir = state_dir(root, path, true)?.unwrap();
    let temp = format!(".state-{}", Uuid::new_v4());
    let result = (|| {
        let mut file = dir.open_with(&temp, OpenOptions::new().write(true).create_new(true))?;
        file.write_all(&serde_json::to_vec(state)?)?;
        file.sync_all()?;
        drop(file);
        dir.rename(&temp, &dir, "state.json")?;
        Ok::<_, SyncError>(())
    })();
    let _ = dir.remove_file(&temp);
    result
}

fn read_local(root: &Path, path: &RelativePath) -> Result<Vec<u8>, SyncError> {
    let mut dir = Dir::open_ambient_dir(root, ambient_authority())?;
    let mut parts = path.as_str().split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            #[cfg(unix)]
            {
                use cap_fs_ext::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let mut file = dir.open_with(part, &options)?;
            if !file.metadata()?.is_file() {
                return Err(SyncError::Integrity(format!(
                    "not a regular file: {}",
                    path.as_str()
                )));
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
        dir = dir.open_dir_nofollow(part)?;
    }
    Err(SyncError::InvalidPath(path.as_str().to_string()))
}

fn verify_object(bytes: &[u8], hash: &[u8; 32], size: u64) -> Result<(), SyncError> {
    if bytes.len() as u64 != size || Sha256::digest(bytes).as_slice() != hash {
        return Err(SyncError::Integrity(
            "remote object size or hash mismatch".into(),
        ));
    }
    Ok(())
}

fn hex_hash(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::store::{MemoryRemoteStore, StoreOperation};
    use std::fs;

    struct Tree(std::path::PathBuf);
    impl Tree {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("touchhle-engine-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, name: &str) -> std::path::PathBuf {
            self.0.join("touchHLE_apps").join(name)
        }
        fn write(&self, name: &str, bytes: &[u8]) {
            fs::create_dir_all(self.file(name).parent().unwrap()).unwrap();
            fs::write(self.file(name), bytes).unwrap();
        }
        fn state(&self) -> std::path::PathBuf {
            self.0.join(".touchHLE_sync/state.json")
        }
        fn engine(&self, store: MemoryRemoteStore) -> SyncEngine<MemoryRemoteStore> {
            SyncEngine::new(store, self.0.clone(), self.state())
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn first_publish_is_deduplicated_and_retry_is_up_to_date() {
        let tree = Tree::new();
        tree.write("a", b"same");
        tree.write("b", b"same");
        let mut engine = tree.engine(MemoryRemoteStore::default());
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        assert_eq!(engine.store().object_count(), 1);
        assert_eq!(engine.store().list_commits().unwrap().len(), 1);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        assert_eq!(engine.store().list_commits().unwrap().len(), 1);
    }

    #[test]
    fn empty_local_download_and_first_connection_conflict() {
        let source = Tree::new();
        source.write("same", b"same");
        source.write("different", b"cloud");
        let mut producer = source.engine(MemoryRemoteStore::default());
        producer.synchronize().unwrap();
        let store = producer.into_store();
        let target = Tree::new();
        target.write("same", b"same");
        target.write("different", b"local");
        let mut consumer = target.engine(store.clone());
        let previous = fs::read(target.file("different")).unwrap();
        let SyncOutcome::Conflicts(plan) = consumer.synchronize().unwrap() else {
            panic!("conflict expected");
        };
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.remote_tips.len(), 1);
        assert_eq!(fs::read(target.file("different")).unwrap(), previous);
        assert!(!target.state().exists());
        assert!(!target.0.join(".touchHLE_sync").exists());
        let empty = Tree::new();
        let mut consumer = empty.engine(store);
        assert!(matches!(
            consumer.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(fs::read(empty.file("different")).unwrap(), b"cloud");
        assert!(empty.state().exists());
    }

    #[test]
    fn concurrent_commits_remain_as_tips_and_plan_keeps_sources() {
        let base = Tree::new();
        base.write("save", b"base");
        let mut initial = base.engine(MemoryRemoteStore::default());
        initial.synchronize().unwrap();
        let original = initial.into_store();
        let left = Tree::new();
        let right = Tree::new();
        let mut a = left.engine(original.clone());
        let mut b = right.engine(original);
        a.synchronize().unwrap();
        b.synchronize().unwrap();
        let observer = Tree::new();
        let mut watcher = observer.engine(a.store().clone());
        watcher.synchronize().unwrap();
        left.write("save", b"left");
        right.write("save", b"right");
        a.synchronize().unwrap();
        b.synchronize().unwrap();
        let mut combined = a.into_store();
        for commit in b.store().list_commits().unwrap() {
            if !combined
                .list_commits()
                .unwrap()
                .iter()
                .any(|c| c.id == commit.id)
            {
                combined.write_commit(&commit).unwrap();
            }
        }
        for hash in b.store().object_hashes() {
            combined
                .write_object(hash, &b.store().read_object(&hash).unwrap().unwrap())
                .unwrap();
        }
        let mut checking = observer.engine(combined);
        let SyncOutcome::Conflicts(plan) = checking.synchronize().unwrap() else {
            panic!("expected conflict");
        };
        assert_eq!(plan.remote_tips.len(), 2);
        assert_eq!(plan.merge_parents.len(), 2);
        assert_eq!(plan.conflicts[0].remote_candidates.len(), 2);
        assert_eq!(fs::read(observer.file("save")).unwrap(), b"base");
        assert_eq!(checking.store().list_commits().unwrap().len(), 3);
    }

    #[test]
    fn second_download_failure_and_corruption_leave_destinations_and_state_intact() {
        let source = Tree::new();
        source.write("a", b"cloud a");
        source.write("b", b"cloud b");
        let mut producer = source.engine(MemoryRemoteStore::default());
        producer.synchronize().unwrap();
        let target = Tree::new();
        let mut store = producer.into_store();
        store.fail_after(StoreOperation::ReadObject, 1);
        let mut engine = target.engine(store);
        assert!(engine.synchronize().is_err());
        assert!(!target.file("a").exists());
        assert!(!target.file("b").exists());
        assert!(!target.state().exists());
        engine
            .store()
            .corrupt_object(sha2::Sha256::digest(b"cloud b").into(), b"bad".to_vec());
        assert!(matches!(engine.synchronize(), Err(SyncError::Integrity(_))));
        assert!(!target.state().exists());
        assert!(!target.file("a").exists());
    }

    #[test]
    fn failure_after_object_write_reuses_immutable_bytes_on_retry() {
        let tree = Tree::new();
        tree.write("local", b"pending");
        let mut store = MemoryRemoteStore::default();
        store.fail_after(StoreOperation::ReadObject, 1);
        let mut engine = tree.engine(store);
        assert!(engine.synchronize().is_err());
        assert_eq!(engine.store().object_count(), 1);
        assert!(engine.store().list_commits().unwrap().is_empty());
        assert!(!tree.state().exists());
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        assert_eq!(engine.store().object_count(), 1);
        assert_eq!(engine.store().list_commits().unwrap().len(), 1);
    }

    #[test]
    fn download_failure_after_publication_retries_without_another_commit() {
        let remote = Tree::new();
        remote.write("remote", b"cloud");
        let mut producer = remote.engine(MemoryRemoteStore::default());
        producer.synchronize().unwrap();
        let local = Tree::new();
        local.write("local", b"pending");
        let mut store = producer.into_store();
        // The remote file is read for commit verification first, then staging.
        store.fail_after(StoreOperation::ReadObject, 3);
        let mut engine = local.engine(store);
        assert!(engine.synchronize().is_err());
        assert_eq!(fs::read(local.file("local")).unwrap(), b"pending");
        assert!(!local.file("remote").exists());
        assert!(!local.state().exists());
        assert_eq!(engine.store().list_commits().unwrap().len(), 2);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(engine.store().list_commits().unwrap().len(), 2);
        assert_eq!(fs::read(local.file("remote")).unwrap(), b"cloud");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_state_symlink_is_not_replaced() {
        use std::os::unix::fs::symlink;
        let tree = Tree::new();
        fs::create_dir(tree.state().parent().unwrap()).unwrap();
        symlink("missing-target", tree.state()).unwrap();
        let mut engine = tree.engine(MemoryRemoteStore::default());
        assert!(engine.synchronize().is_err());
        assert_eq!(
            fs::read_link(tree.state()).unwrap(),
            std::path::Path::new("missing-target")
        );
    }

    #[test]
    fn conflict_free_merge_publishes_all_tip_parents() {
        let tree = Tree::new();
        let mut store = MemoryRemoteStore::default();
        let first = b"first";
        let second = b"second";
        let first_hash: [u8; 32] = Sha256::digest(first).into();
        let second_hash: [u8; 32] = Sha256::digest(second).into();
        store.write_object(first_hash, first).unwrap();
        store.write_object(second_hash, second).unwrap();
        for (id, name, hash) in [(1, "a", first_hash), (2, "b", second_hash)] {
            store
                .write_commit(&Commit {
                    id: Uuid::from_u128(id),
                    device_id: Uuid::nil(),
                    created_unix_ms: 0,
                    parents: vec![],
                    entries: BTreeMap::from([(
                        RelativePath::new(&format!("touchHLE_apps/{name}")).unwrap(),
                        SnapshotEntry::File {
                            sha256: hash,
                            size: if id == 1 { first.len() } else { second.len() } as u64,
                            modified_unix_ms: 0,
                        },
                    )]),
                })
                .unwrap();
        }
        let mut engine = tree.engine(store);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        let commits = engine.store().list_commits().unwrap();
        let tips = resolve_remote_tips(&commits).unwrap();
        assert_eq!(tips.len(), 1);
        let merge = commits
            .iter()
            .find(|commit| commit.id == tips[0].commit_id)
            .unwrap();
        assert_eq!(merge.parents, vec![Uuid::from_u128(1), Uuid::from_u128(2)]);
        assert_eq!(fs::read(tree.file("a")).unwrap(), first);
        assert_eq!(fs::read(tree.file("b")).unwrap(), second);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        assert_eq!(engine.store().list_commits().unwrap().len(), 3);
    }

    #[test]
    fn offline_authentication_does_not_persist_state() {
        struct Offline;
        impl RemoteStore for Offline {
            fn list_commits(&mut self) -> Result<Vec<Commit>, SyncError> {
                Err(SyncError::Authentication("unavailable".into()))
            }
            fn read_object(&mut self, _: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError> {
                unreachable!()
            }
            fn write_object(&mut self, _: [u8; 32], _: &[u8]) -> Result<(), SyncError> {
                unreachable!()
            }
            fn write_commit(&mut self, _: &Commit) -> Result<(), SyncError> {
                unreachable!()
            }
        }
        let tree = Tree::new();
        tree.write("pending", b"local");
        let mut engine = SyncEngine::new(Offline, tree.0.clone(), tree.state());
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Offline
        ));
        assert!(!tree.state().exists());
    }

    #[test]
    fn failures_do_not_advance_baseline_and_can_retry() {
        for operation in [
            StoreOperation::ListCommits,
            StoreOperation::WriteObject,
            StoreOperation::WriteCommit,
            StoreOperation::ReadObject,
        ] {
            let tree = Tree::new();
            tree.write("local", b"local");
            let mut store = MemoryRemoteStore::default();
            if operation == StoreOperation::ReadObject {
                let remote = Tree::new();
                remote.write("remote", b"remote");
                let mut producer = remote.engine(store);
                producer.synchronize().unwrap();
                store = producer.into_store();
            }
            store.fail_next(operation);
            let mut engine = tree.engine(store);
            assert!(engine.synchronize().is_err(), "{operation:?}");
            assert!(!tree.state().exists(), "{operation:?}");
            assert_eq!(fs::read(tree.file("local")).unwrap(), b"local");
            assert!(engine.synchronize().is_ok(), "{operation:?}");
        }
    }
}
