/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::apply::{apply_staged_files, stage_remote_files};
use super::merge::{
    plan_sync, plan_sync_with_local_baseline, plan_sync_without_common_ancestor,
    resolve_remote_tips, Conflict, RemoteTip, RemoteVersion, SyncPlan,
};
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
        validate_baseline_present(state.last_applied_commit, &commits)?;
        // Include the saved commit as an ancestry anchor but compare sibling
        // tips from their shared base so changes on both branches conflict.
        let empty_baseline = BTreeMap::new();
        let local_baseline = if state.last_applied_commit.is_some() {
            &state.baseline
        } else {
            &empty_baseline
        };
        let plan = match select_planning_base(
            state.last_applied_commit,
            &state.baseline,
            &commits,
            &tips,
        )? {
            PlanningBase::Snapshot(snapshot) => {
                plan_sync_with_local_baseline(&snapshot, local_baseline, &local, &tips)?
            }
            PlanningBase::Unrelated => {
                plan_sync_without_common_ancestor(local_baseline, &local, &tips)?
            }
            PlanningBase::Ambiguous => conservative_conflict_plan(&local, &tips)?,
        };
        if !plan.conflicts.is_empty() {
            return Ok(SyncOutcome::Conflicts(plan));
        }
        let resolved = plan.resolved_snapshot(&[])?;
        let publish = !plan.publish_local.is_empty() || tips.len() > 1 || tips.is_empty();
        let apply = !plan.apply_remote.is_empty();

        if publish {
            let referenced_objects: std::collections::BTreeSet<_> = commits
                .iter()
                .flat_map(|commit| commit.entries.values())
                .filter_map(|entry| match entry {
                    SnapshotEntry::File { sha256, .. } => Some(*sha256),
                    SnapshotEntry::Tombstone => None,
                })
                .collect();
            let mut uploaded = std::collections::BTreeSet::new();
            // Existing history references are trusted; verify only new uploads here.
            for (path, entry) in &resolved {
                let SnapshotEntry::File { sha256, size, .. } = entry else {
                    continue;
                };
                if referenced_objects.contains(sha256) || !uploaded.insert(*sha256) {
                    continue;
                }
                let bytes = read_local(&self.root, path)?;
                verify_object(&bytes, sha256, *size)?;
                self.store.write_object(*sha256, &bytes)?;
                let written = self.store.read_object(sha256)?.ok_or_else(|| {
                    SyncError::Integrity(format!("object absent after write: {}", hex_hash(sha256)))
                })?;
                verify_object(&written, sha256, *size)?;
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

        // Staging verifies selected remote bytes and caches each content hash.
        // The staging layer cleans up its own scratch files on failure.
        let mut staged_objects = BTreeMap::<[u8; 32], Vec<u8>>::new();
        let staged = stage_remote_files(&self.root, &plan, &[], |hash| {
            if let Some(bytes) = staged_objects.get(hash) {
                return Ok(bytes.clone());
            }
            let bytes = self.store.read_object(hash)?.ok_or_else(|| {
                SyncError::Integrity(format!("missing remote object {}", hex_hash(hash)))
            })?;
            staged_objects.insert(*hash, bytes.clone());
            Ok(bytes)
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

// Until account setup persists a repository identity, a foreign root cannot
// be distinguished from a concurrent genesis root; only missing known IDs fail.
fn validate_baseline_present(
    last_applied: Option<Uuid>,
    commits: &[Commit],
) -> Result<(), SyncError> {
    let Some(baseline_id) = last_applied else {
        return Ok(());
    };
    let by_id: BTreeMap<_, _> = commits.iter().map(|commit| (commit.id, commit)).collect();
    if !by_id.contains_key(&baseline_id) {
        return Err(SyncError::Integrity(format!(
            "last applied commit is missing from remote history: {baseline_id}"
        )));
    }
    Ok(())
}

enum PlanningBase {
    Snapshot(BTreeMap<RelativePath, SnapshotEntry>),
    Unrelated,
    Ambiguous,
}

fn select_planning_base(
    last_applied: Option<Uuid>,
    saved_baseline: &BTreeMap<RelativePath, SnapshotEntry>,
    commits: &[Commit],
    tips: &[RemoteTip],
) -> Result<PlanningBase, SyncError> {
    let by_id: BTreeMap<_, _> = commits.iter().map(|commit| (commit.id, commit)).collect();
    // With no saved cloud revision, a single tip is a first download, not
    // its own comparison base. For multiple tips, use their shared history
    // when one exists; unrelated initial roots compare from an empty base.
    if last_applied.is_none() && tips.len() < 2 {
        return Ok(PlanningBase::Snapshot(BTreeMap::new()));
    }
    let mut anchors: Vec<_> = last_applied.into_iter().collect();
    anchors.extend(tips.iter().map(|tip| tip.commit_id));
    anchors.sort();
    anchors.dedup();
    if anchors.is_empty() {
        return Ok(PlanningBase::Snapshot(BTreeMap::new()));
    }

    let mut common: Option<std::collections::BTreeSet<Uuid>> = None;
    for anchor in anchors {
        let mut ancestors = std::collections::BTreeSet::new();
        let mut pending = vec![anchor];
        while let Some(id) = pending.pop() {
            if ancestors.insert(id) {
                let commit = by_id.get(&id).ok_or_else(|| {
                    SyncError::Integrity(format!("missing commit in ancestry: {id}"))
                })?;
                pending.extend(commit.parents.iter().copied());
            }
        }
        common = Some(match common {
            None => ancestors,
            Some(previous) => previous.intersection(&ancestors).copied().collect(),
        });
    }
    let common = common.unwrap_or_default();
    if common.is_empty() {
        return Ok(PlanningBase::Unrelated);
    }

    let mut non_maximal = std::collections::BTreeSet::new();
    for commit in commits {
        if common.contains(&commit.id) {
            non_maximal.extend(
                commit
                    .parents
                    .iter()
                    .filter(|parent| common.contains(parent))
                    .copied(),
            );
        }
    }
    let maximal: Vec<_> = common.difference(&non_maximal).copied().collect();
    if maximal.len() != 1 {
        return Ok(PlanningBase::Ambiguous);
    }
    let base_id = maximal[0];
    if Some(base_id) == last_applied {
        return Ok(PlanningBase::Snapshot(saved_baseline.clone()));
    }
    let base = by_id
        .get(&base_id)
        .ok_or_else(|| SyncError::Integrity(format!("missing merge base: {base_id}")))?;
    Ok(PlanningBase::Snapshot(base.entries.clone()))
}

fn conservative_conflict_plan(
    local: &BTreeMap<RelativePath, SnapshotEntry>,
    tips: &[RemoteTip],
) -> Result<SyncPlan, SyncError> {
    let mut plan = plan_sync(&BTreeMap::new(), local, tips)?;
    let mut paths: std::collections::BTreeSet<_> = local.keys().cloned().collect();
    paths.extend(tips.iter().flat_map(|tip| tip.snapshot.keys().cloned()));
    plan.conflicts = paths
        .into_iter()
        .map(|path| Conflict {
            local: local.get(&path).cloned(),
            remote_candidates: plan
                .remote_tips
                .iter()
                .map(|tip| RemoteVersion {
                    commit_ids: vec![tip.commit_id],
                    entry: tip.snapshot.get(&path).cloned(),
                })
                .collect(),
            path,
        })
        .collect();
    plan.apply_remote.clear();
    plan.publish_local.clear();
    Ok(plan)
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
    fn empty_first_connection_publishes_an_empty_root_commit() {
        let tree = Tree::new();
        let mut engine = tree.engine(MemoryRemoteStore::default());
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        let commits = engine.store().list_commits().unwrap();
        assert_eq!(commits.len(), 1);
        assert!(commits[0].entries.is_empty());
        assert!(commits[0].parents.is_empty());
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        assert_eq!(engine.store().list_commits().unwrap().len(), 1);
    }

    #[test]
    fn unchanged_and_local_only_syncs_do_not_download_history_objects() {
        let tree = Tree::new();
        tree.write("original", b"old object");
        let mut engine = tree.engine(MemoryRemoteStore::default());
        engine.synchronize().unwrap();
        let old_hash: [u8; 32] = Sha256::digest(b"old object").into();
        assert_eq!(engine.store().object_read_count(&old_hash), 1);

        engine.synchronize().unwrap();
        assert_eq!(engine.store().object_read_count(&old_hash), 1);

        tree.write("new", b"new object");
        engine.synchronize().unwrap();
        assert_eq!(engine.store().object_read_count(&old_hash), 1);
        let new_hash: [u8; 32] = Sha256::digest(b"new object").into();
        assert_eq!(engine.store().object_read_count(&new_hash), 1);
    }

    #[test]
    fn repeated_remote_hash_is_fetched_once_for_staging() {
        let tree = Tree::new();
        let mut store = MemoryRemoteStore::default();
        let bytes = b"shared object";
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        store.write_object(hash, bytes).unwrap();
        let mut entries = BTreeMap::new();
        for name in ["one", "two"] {
            entries.insert(
                RelativePath::new(&format!("touchHLE_apps/{name}")).unwrap(),
                SnapshotEntry::File {
                    sha256: hash,
                    size: bytes.len() as u64,
                    modified_unix_ms: 0,
                },
            );
        }
        store
            .write_commit(&Commit {
                id: Uuid::from_u128(17),
                device_id: Uuid::nil(),
                created_unix_ms: 0,
                parents: vec![],
                entries,
            })
            .unwrap();
        let mut engine = tree.engine(store);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(engine.store().object_read_count(&hash), 1);
        assert_eq!(fs::read(tree.file("one")).unwrap(), bytes);
        assert_eq!(fs::read(tree.file("two")).unwrap(), bytes);
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
        store.fail_next(StoreOperation::ReadObject);
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
        store.fail_next(StoreOperation::ReadObject);
        let mut engine = tree.engine(store);
        let first_attempt = engine.synchronize();
        let pending_hash: [u8; 32] = Sha256::digest(b"pending").into();
        assert!(
            first_attempt.is_err(),
            "result: {first_attempt:?}, reads: {}",
            engine.store().object_read_count(&pending_hash)
        );
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
        // The newly uploaded local object is read back before staging fetches remote data.
        store.fail_after(StoreOperation::ReadObject, 1);
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
    fn unrelated_disjoint_genesis_roots_auto_merge() {
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
    fn missing_last_applied_history_is_rejected_without_publication_or_state_change() {
        let tree = Tree::new();
        tree.write("saved", b"baseline");
        let mut engine = tree.engine(MemoryRemoteStore::default());
        engine.synchronize().unwrap();
        tree.write("pending", b"local change");
        let saved_state = fs::read(tree.state()).unwrap();
        let last_applied = serde_json::from_slice::<SyncState>(&saved_state)
            .unwrap()
            .last_applied_commit
            .unwrap();
        engine.store().remove_commit(&last_applied);

        assert!(matches!(engine.synchronize(), Err(SyncError::Integrity(_))));
        assert!(engine.store().list_commits().unwrap().is_empty());
        assert_eq!(fs::read(tree.state()).unwrap(), saved_state);
        assert_eq!(fs::read(tree.file("pending")).unwrap(), b"local change");
    }

    #[test]
    fn concurrent_sibling_tip_with_conflicting_edit_returns_both_candidates() {
        let (tree, store, local_id, remote_id) =
            sibling_branches("save", b"local edit", "save", b"remote edit");
        let saved_state = fs::read(tree.state()).unwrap();
        let mut engine = tree.engine(store);

        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("sibling history must reach conflict resolution");
        };
        assert_eq!(plan.remote_tips.len(), 2);
        assert!(plan.remote_tips.iter().any(|tip| tip.commit_id == local_id));
        assert!(plan
            .remote_tips
            .iter()
            .any(|tip| tip.commit_id == remote_id));
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].remote_candidates.len(), 2);
        assert!(matches!(
            plan.resolved_snapshot(&[]),
            Err(SyncError::UnresolvedConflicts(_))
        ));
        assert_eq!(fs::read(tree.state()).unwrap(), saved_state);
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"local edit");
    }

    #[test]
    fn concurrent_sibling_disjoint_edits_merge_with_both_parents() {
        let (tree, store, local_id, remote_id) =
            sibling_branches("device", b"local edit", "cloud", b"remote edit");
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
        let mut expected_parents = vec![local_id, remote_id];
        expected_parents.sort();
        assert_eq!(merge.parents, expected_parents);
        assert_eq!(fs::read(tree.file("device")).unwrap(), b"local edit");
        assert_eq!(fs::read(tree.file("cloud")).unwrap(), b"remote edit");
    }

    #[test]
    fn fresh_empty_device_downloads_unchanged_file_from_shared_base() {
        let (_, store, _, _) = sibling_branches("left", b"left edit", "right", b"right edit");
        let fresh = Tree::new();
        let mut engine = fresh.engine(store);

        engine.synchronize().unwrap();
        assert_eq!(
            fs::read(fresh.file("base")).unwrap(),
            b"common ancestor",
            "empty first-connection state must not turn shared cloud content into a tombstone"
        );
        let tips = resolve_remote_tips(&engine.store().list_commits().unwrap()).unwrap();
        assert!(tips.iter().all(|tip| {
            !matches!(
                tip.snapshot
                    .get(&RelativePath::new("touchHLE_apps/base").unwrap()),
                Some(SnapshotEntry::Tombstone)
            )
        }));
        assert!(fresh.state().exists());
        assert!(matches!(
            tips[0]
                .snapshot
                .get(&RelativePath::new("touchHLE_apps/base").unwrap()),
            Some(SnapshotEntry::File { .. })
        ));
    }

    #[test]
    fn offline_deletion_against_sibling_tip_is_not_resurrected() {
        let origin = Tree::new();
        let mut seed = origin.engine(MemoryRemoteStore::default());
        seed.synchronize().unwrap();
        let shared = seed.into_store();

        let local = Tree::new();
        let mut local_engine = local.engine(shared.clone());
        local_engine.synchronize().unwrap();
        local.write("x", b"branch A");
        local_engine.synchronize().unwrap();
        let saved_state = fs::read(local.state()).unwrap();
        let mut local_store = local_engine.into_store();

        let remote = Tree::new();
        let mut remote_engine = remote.engine(shared);
        remote_engine.synchronize().unwrap();
        remote.write("other", b"branch B");
        remote_engine.synchronize().unwrap();
        let mut combined = remote_engine.into_store();
        for hash in local_store.object_hashes() {
            if combined.read_object(&hash).unwrap().is_none() {
                combined
                    .write_object(hash, &local_store.read_object(&hash).unwrap().unwrap())
                    .unwrap();
            }
        }
        for commit in local_store.list_commits().unwrap() {
            combined.write_commit(&commit).unwrap();
        }
        fs::remove_file(local.file("x")).unwrap();
        let mut engine = local.engine(combined);

        match engine.synchronize().unwrap() {
            SyncOutcome::Conflicts(_) => {
                assert!(!local.file("x").exists());
                assert_eq!(fs::read(local.state()).unwrap(), saved_state);
            }
            SyncOutcome::Published => {
                assert!(!local.file("x").exists());
                let tips = resolve_remote_tips(&engine.store().list_commits().unwrap()).unwrap();
                assert!(matches!(
                    tips[0]
                        .snapshot
                        .get(&RelativePath::new("touchHLE_apps/x").unwrap()),
                    Some(SnapshotEntry::Tombstone)
                ));
            }
            other => panic!("offline deletion must not be auto-overwritten: {other:?}"),
        }
    }

    #[test]
    fn unrelated_same_path_different_files_require_explicit_choice() {
        let first = Tree::new();
        let second = Tree::new();
        first.write("save", b"first root");
        second.write("save", b"second root");
        let mut first_engine = first.engine(MemoryRemoteStore::default());
        let mut second_engine = second.engine(MemoryRemoteStore::default());
        first_engine.synchronize().unwrap();
        second_engine.synchronize().unwrap();
        let mut first_store = first_engine.into_store();
        let mut second_store = second_engine.into_store();
        let first_id = first_store.list_commits().unwrap()[0].id;
        let second_commits = second_store.list_commits().unwrap();
        let second_id = second_commits[0].id;

        let mut combined = first_store;
        for hash in second_store.object_hashes() {
            let bytes = second_store.read_object(&hash).unwrap().unwrap();
            combined.write_object(hash, &bytes).unwrap();
        }
        for commit in second_commits {
            combined.write_commit(&commit).unwrap();
        }
        let saved_state = fs::read(first.state()).unwrap();
        let mut engine = first.engine(combined);

        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("concurrent roots must reach conflict resolution");
        };
        assert_eq!(plan.remote_tips.len(), 2);
        assert!(plan.remote_tips.iter().any(|tip| tip.commit_id == first_id));
        assert!(plan
            .remote_tips
            .iter()
            .any(|tip| tip.commit_id == second_id));
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].remote_candidates.len(), 2);
        let candidate_ids: std::collections::BTreeSet<_> = plan.conflicts[0]
            .remote_candidates
            .iter()
            .flat_map(|candidate| candidate.commit_ids.iter().copied())
            .collect();
        assert_eq!(candidate_ids, [first_id, second_id].into_iter().collect());
        let candidate_hashes: std::collections::BTreeSet<_> = plan.conflicts[0]
            .remote_candidates
            .iter()
            .filter_map(|candidate| match candidate.entry.as_ref() {
                Some(SnapshotEntry::File { sha256, .. }) => Some(*sha256),
                _ => None,
            })
            .collect();
        assert_eq!(
            candidate_hashes,
            [
                Sha256::digest(b"first root").into(),
                Sha256::digest(b"second root").into()
            ]
            .into_iter()
            .collect()
        );
        assert!(matches!(
            plan.resolved_snapshot(&[]),
            Err(SyncError::UnresolvedConflicts(_))
        ));
        assert_eq!(engine.store().list_commits().unwrap().len(), 2);
        assert_eq!(fs::read(first.state()).unwrap(), saved_state);
        assert_eq!(fs::read(first.file("save")).unwrap(), b"first root");
    }

    #[test]
    fn unrelated_file_and_tombstone_roots_require_an_explicit_choice() {
        let tree = Tree::new();
        let path = RelativePath::new("touchHLE_apps/x").unwrap();
        let bytes = b"file root";
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        let file_id = Uuid::from_u128(901);
        let tombstone_id = Uuid::from_u128(902);
        let mut store = MemoryRemoteStore::default();
        store.write_object(hash, bytes).unwrap();
        store
            .write_commit(&Commit {
                id: file_id,
                device_id: Uuid::from_u128(911),
                created_unix_ms: 0,
                parents: vec![],
                entries: BTreeMap::from([(path.clone(), file_entry_for_test(bytes))]),
            })
            .unwrap();
        store
            .write_commit(&Commit {
                id: tombstone_id,
                device_id: Uuid::from_u128(912),
                created_unix_ms: 0,
                parents: vec![],
                entries: BTreeMap::from([(path.clone(), SnapshotEntry::Tombstone)]),
            })
            .unwrap();
        let mut engine = tree.engine(store);

        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("file-versus-tombstone genesis disagreement must not auto-resolve");
        };
        let conflict = plan
            .conflicts
            .iter()
            .find(|conflict| conflict.path == path)
            .unwrap();
        assert_eq!(conflict.remote_candidates.len(), 2);
        assert!(conflict.remote_candidates.iter().any(|candidate| {
            candidate.commit_ids == vec![file_id]
                && candidate.entry.as_ref() == Some(&file_entry_for_test(bytes))
        }));
        assert!(conflict.remote_candidates.iter().any(|candidate| {
            candidate.commit_ids == vec![tombstone_id]
                && candidate.entry == Some(SnapshotEntry::Tombstone)
        }));
        let candidate_ids: std::collections::BTreeSet<_> = conflict
            .remote_candidates
            .iter()
            .flat_map(|candidate| candidate.commit_ids.iter().copied())
            .collect();
        assert_eq!(candidate_ids, [file_id, tombstone_id].into_iter().collect());
        assert!(matches!(
            plan.resolved_snapshot(&[]),
            Err(SyncError::UnresolvedConflicts(_))
        ));
        assert_eq!(engine.store().list_commits().unwrap().len(), 2);
        assert!(!tree.file("x").exists());
        assert!(!tree.state().exists());
    }

    #[test]
    fn incomparable_merge_bases_require_conservative_choices() {
        let tree = Tree::new();
        let mut store = MemoryRemoteStore::default();
        let root = Commit {
            id: Uuid::from_u128(201),
            device_id: Uuid::nil(),
            created_unix_ms: 0,
            parents: vec![],
            entries: BTreeMap::new(),
        };
        let base_one = Commit {
            id: Uuid::from_u128(202),
            parents: vec![root.id],
            ..root.clone()
        };
        let base_two = Commit {
            id: Uuid::from_u128(203),
            parents: vec![root.id],
            ..root.clone()
        };
        for (id, path, content) in [
            (204, "one", b"one".as_slice()),
            (205, "two", b"two".as_slice()),
        ] {
            let hash: [u8; 32] = Sha256::digest(content).into();
            store.write_object(hash, content).unwrap();
            store
                .write_commit(&Commit {
                    id: Uuid::from_u128(id),
                    parents: vec![base_one.id, base_two.id],
                    entries: BTreeMap::from([(
                        RelativePath::new(&format!("touchHLE_apps/{path}")).unwrap(),
                        file_entry_for_test(content),
                    )]),
                    ..root.clone()
                })
                .unwrap();
        }
        store.write_commit(&root).unwrap();
        store.write_commit(&base_one).unwrap();
        store.write_commit(&base_two).unwrap();
        let mut engine = tree.engine(store);

        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("incomparable merge bases must require conservative choices");
        };
        assert!(plan.conflicts.len() >= 2);
        assert_eq!(plan.remote_tips.len(), 2);
        assert!(!tree.state().exists());
        assert!(!tree.file("one").exists());
        assert!(!tree.file("two").exists());
    }

    fn sibling_branches(
        local_path: &str,
        local_bytes: &[u8],
        remote_path: &str,
        remote_bytes: &[u8],
    ) -> (Tree, MemoryRemoteStore, Uuid, Uuid) {
        let origin = Tree::new();
        origin.write("base", b"common ancestor");
        let mut seed = origin.engine(MemoryRemoteStore::default());
        seed.synchronize().unwrap();
        let shared = seed.into_store();

        let local = Tree::new();
        let mut local_engine = local.engine(shared.clone());
        local_engine.synchronize().unwrap();
        local.write(local_path, local_bytes);
        local_engine.synchronize().unwrap();
        let mut local_store = local_engine.into_store();
        let local_id =
            resolve_remote_tips(&local_store.list_commits().unwrap()).unwrap()[0].commit_id;

        let remote = Tree::new();
        let mut remote_engine = remote.engine(shared);
        remote_engine.synchronize().unwrap();
        remote.write(remote_path, remote_bytes);
        remote_engine.synchronize().unwrap();
        let mut combined = remote_engine.into_store();
        for hash in local_store.object_hashes() {
            if combined.read_object(&hash).unwrap().is_none() {
                let bytes = local_store.read_object(&hash).unwrap().unwrap();
                combined.write_object(hash, &bytes).unwrap();
            }
        }
        for commit in local_store.list_commits().unwrap() {
            combined.write_commit(&commit).unwrap();
        }
        let remote_id = resolve_remote_tips(&combined.list_commits().unwrap())
            .unwrap()
            .into_iter()
            .find(|tip| tip.commit_id != local_id)
            .unwrap()
            .commit_id;
        (local, combined, local_id, remote_id)
    }

    #[test]
    fn retry_after_own_publish_then_stage_failure_accepts_descendant_tip() {
        let origin = Tree::new();
        origin.write("base", b"common");
        let mut seed = origin.engine(MemoryRemoteStore::default());
        seed.synchronize().unwrap();
        let shared_store = seed.into_store();

        let local = Tree::new();
        let mut local_engine = local.engine(shared_store.clone());
        local_engine.synchronize().unwrap();
        let baseline_id = serde_json::from_slice::<SyncState>(&fs::read(local.state()).unwrap())
            .unwrap()
            .last_applied_commit
            .unwrap();

        let remote = Tree::new();
        let mut remote_engine = remote.engine(shared_store);
        remote_engine.synchronize().unwrap();
        remote.write("cloud", b"remote change");
        assert!(matches!(
            remote_engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        let remote_store = remote_engine.into_store();
        local.write("device", b"local change");
        let mut store = remote_store;
        // Local upload read-back succeeds; fail the following staged download.
        store.fail_after(StoreOperation::ReadObject, 1);
        let mut engine = local.engine(store);
        assert!(engine.synchronize().is_err());
        let state_after_failure =
            serde_json::from_slice::<SyncState>(&fs::read(local.state()).unwrap()).unwrap();
        assert_eq!(state_after_failure.last_applied_commit, Some(baseline_id));
        assert!(!local.file("cloud").exists());
        assert_eq!(fs::read(local.file("device")).unwrap(), b"local change");

        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(engine.store().list_commits().unwrap().len(), 3);
        assert_eq!(fs::read(local.file("cloud")).unwrap(), b"remote change");
        let final_state =
            serde_json::from_slice::<SyncState>(&fs::read(local.state()).unwrap()).unwrap();
        assert_ne!(final_state.last_applied_commit, Some(baseline_id));
    }

    fn file_entry_for_test(bytes: &[u8]) -> SnapshotEntry {
        SnapshotEntry::File {
            sha256: Sha256::digest(bytes).into(),
            size: bytes.len() as u64,
            modified_unix_ms: 0,
        }
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
