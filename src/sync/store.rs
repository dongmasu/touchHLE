/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::model::{Commit, RelativePath, SyncError};
#[cfg(test)]
use super::reconcile::RemoteChange;
use super::reconcile::{RemoteChangeBatch, RemoteFile};
#[cfg(test)]
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

pub(crate) const MAX_PARALLEL_OBJECT_WRITES: usize = 4;

pub trait RemoteStore {
    fn identity(&mut self) -> Result<(String, String), SyncError> {
        Err(SyncError::Provider("remote identity unavailable".into()))
    }
    fn migration_completed(&mut self) -> Result<bool, SyncError> {
        Err(SyncError::Provider("migration status unavailable".into()))
    }
    fn mark_migration_completed(&mut self) -> Result<(), SyncError> {
        Err(SyncError::Provider("migration marker unavailable".into()))
    }
    fn restore_checkpoint_indexes(
        &mut self,
        _file_paths_by_id: &BTreeMap<String, RelativePath>,
        _folder_paths_by_id: &BTreeMap<String, String>,
    ) -> Result<(), SyncError> {
        Ok(())
    }
    /// Returns a validated ID-to-Drive-path snapshot when the adapter has one.
    fn checkpoint_folder_paths_by_id(&self) -> Result<Option<BTreeMap<String, String>>, SyncError> {
        Ok(None)
    }
    fn start_page_token(&mut self) -> Result<String, SyncError> {
        Err(SyncError::Provider(
            "current-file operations not supported".into(),
        ))
    }
    fn initial_inventory(&mut self) -> Result<Vec<RemoteFile>, SyncError> {
        Err(SyncError::Provider(
            "current-file operations not supported".into(),
        ))
    }
    fn changes_since(&mut self, _token: &str) -> Result<RemoteChangeBatch, SyncError> {
        Err(SyncError::Provider(
            "current-file operations not supported".into(),
        ))
    }
    fn diagnose_changes_since(
        &mut self,
        _token: &str,
        _file_ids: &[String],
    ) -> Result<(), SyncError> {
        Ok(())
    }
    fn file_metadata(&mut self, _file_id: &str) -> Result<Option<RemoteFile>, SyncError> {
        Err(SyncError::Provider(
            "current-file operations not supported".into(),
        ))
    }
    fn read_file(&mut self, _file_id: &str) -> Result<Option<Vec<u8>>, SyncError> {
        Err(SyncError::Provider(
            "current-file operations not supported".into(),
        ))
    }
    fn write_file(
        &mut self,
        _path: &RelativePath,
        _existing_id: Option<&str>,
        _bytes: &[u8],
    ) -> Result<RemoteFile, SyncError> {
        Err(SyncError::Provider(
            "current-file operations not supported".into(),
        ))
    }
    fn delete_file(&mut self, _file_id: &str) -> Result<(), SyncError> {
        Err(SyncError::Provider(
            "current-file operations not supported".into(),
        ))
    }
    fn list_commits(&mut self) -> Result<Vec<Commit>, SyncError>;
    fn read_object(&mut self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError>;
    fn write_object(&mut self, hash: [u8; 32], bytes: &[u8]) -> Result<(), SyncError>;
    fn write_objects_and_verify(
        &mut self,
        objects: &[([u8; 32], Vec<u8>)],
    ) -> Result<(), SyncError> {
        for (hash, bytes) in objects {
            self.write_object(*hash, bytes)?;
            let stored = self.read_object(hash)?.ok_or_else(|| {
                SyncError::Integrity("uploaded object was missing from the remote store".into())
            })?;
            if stored != *bytes {
                return Err(SyncError::Integrity(
                    "uploaded object did not match the local content".into(),
                ));
            }
        }
        Ok(())
    }
    fn write_commit(&mut self, commit: &Commit) -> Result<(), SyncError>;
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum StoreOperation {
    StartPageToken,
    InitialInventory,
    ChangesSince,
    FileMetadata,
    ReadFile,
    WriteFile,
    DeleteFile,
    ListCommits,
    ReadObject,
    WriteObject,
    WriteCommit,
}

#[cfg(test)]
#[derive(Clone, Default)]
pub struct MemoryRemoteStore {
    identity: Option<(String, String)>,
    migration_completed: bool,
    files: BTreeMap<String, (RemoteFile, Vec<u8>)>,
    changes: Vec<RemoteChange>,
    next_file_id: u64,
    change_requests: usize,
    content_transfers: usize,
    objects: BTreeMap<[u8; 32], Vec<u8>>,
    commits: BTreeMap<Uuid, Commit>,
    failures: BTreeMap<StoreOperation, usize>,
    object_reads: BTreeMap<[u8; 32], usize>,
    corrupt_next_write: bool,
    folder_paths_by_id: BTreeMap<String, String>,
    restored_checkpoint_indexes: Option<(BTreeMap<String, RelativePath>, BTreeMap<String, String>)>,
}

#[cfg(test)]
impl MemoryRemoteStore {
    pub fn set_identity(&mut self, account: &str, root: &str) {
        self.identity = Some((account.to_owned(), root.to_owned()));
    }
    pub fn clear_migration_marker(&mut self) {
        self.migration_completed = false;
    }
    pub fn seed_current_file(&mut self, file: RemoteFile, bytes: Vec<u8>) {
        self.files.insert(file.id.clone(), (file, bytes));
    }

    pub fn rename_current_file(&mut self, file_id: &str, path: RelativePath) {
        let file = {
            let (file, _) = self.files.get_mut(file_id).expect("seeded remote file");
            file.path = path;
            file.version = file
                .version
                .parse::<u64>()
                .unwrap_or(0)
                .saturating_add(1)
                .to_string();
            file.clone()
        };
        self.changes.push(RemoteChange {
            file_id: file_id.to_owned(),
            file: Some(file),
        });
    }

    pub fn seed_folder_paths_by_id(&mut self, folder_paths_by_id: BTreeMap<String, String>) {
        self.folder_paths_by_id = folder_paths_by_id;
    }

    pub fn change_request_count(&self) -> usize {
        self.change_requests
    }

    pub fn content_transfer_count(&self) -> usize {
        self.content_transfers
    }
    #[cfg(test)]
    pub fn corrupt_object(&mut self, hash: [u8; 32], bytes: Vec<u8>) {
        self.objects.insert(hash, bytes);
    }
    pub fn corrupt_next_write(&mut self) {
        self.corrupt_next_write = true;
    }
    pub fn fail_next(&mut self, operation: StoreOperation) {
        self.fail_after(operation, 0);
    }

    /// Fail once after `successful_calls` calls of this operation.
    pub fn fail_after(&mut self, operation: StoreOperation, successful_calls: usize) {
        self.failures.insert(operation, successful_calls);
    }

    fn check(&mut self, operation: StoreOperation) -> Result<(), SyncError> {
        if let Some(remaining) = self.failures.get_mut(&operation) {
            if *remaining == 0 {
                self.failures.remove(&operation);
                return Err(SyncError::Provider(format!(
                    "injected {operation:?} failure"
                )));
            }
            *remaining -= 1;
        }
        Ok(())
    }

    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    pub fn object_hashes(&self) -> Vec<[u8; 32]> {
        self.objects.keys().copied().collect()
    }

    pub fn object_read_count(&self, hash: &[u8; 32]) -> usize {
        self.object_reads.get(hash).copied().unwrap_or_default()
    }

    pub fn restored_checkpoint_indexes(
        &self,
    ) -> Option<(&BTreeMap<String, RelativePath>, &BTreeMap<String, String>)> {
        self.restored_checkpoint_indexes
            .as_ref()
            .map(|(files, folders)| (files, folders))
    }

    #[cfg(test)]
    pub fn remove_commit(&mut self, id: &Uuid) {
        self.commits.remove(id);
    }
}

#[cfg(test)]
impl RemoteStore for MemoryRemoteStore {
    fn identity(&mut self) -> Result<(String, String), SyncError> {
        Ok(self
            .identity
            .clone()
            .unwrap_or_else(|| ("test-account".into(), "test-root".into())))
    }

    fn migration_completed(&mut self) -> Result<bool, SyncError> {
        Ok(self.migration_completed)
    }

    fn mark_migration_completed(&mut self) -> Result<(), SyncError> {
        self.migration_completed = true;
        Ok(())
    }
    fn restore_checkpoint_indexes(
        &mut self,
        file_paths_by_id: &BTreeMap<String, RelativePath>,
        folder_paths_by_id: &BTreeMap<String, String>,
    ) -> Result<(), SyncError> {
        self.restored_checkpoint_indexes =
            Some((file_paths_by_id.clone(), folder_paths_by_id.clone()));
        self.folder_paths_by_id = folder_paths_by_id.clone();
        Ok(())
    }

    fn checkpoint_folder_paths_by_id(&self) -> Result<Option<BTreeMap<String, String>>, SyncError> {
        Ok(Some(self.folder_paths_by_id.clone()))
    }

    fn start_page_token(&mut self) -> Result<String, SyncError> {
        self.check(StoreOperation::StartPageToken)?;
        Ok(self.changes.len().to_string())
    }

    fn initial_inventory(&mut self) -> Result<Vec<RemoteFile>, SyncError> {
        self.check(StoreOperation::InitialInventory)?;
        Ok(self.files.values().map(|(file, _)| file.clone()).collect())
    }

    fn changes_since(&mut self, token: &str) -> Result<RemoteChangeBatch, SyncError> {
        self.change_requests += 1;
        self.check(StoreOperation::ChangesSince)?;
        let index = token
            .parse::<usize>()
            .map_err(|_| SyncError::Provider("invalid changes cursor".into()))?;
        if index > self.changes.len() {
            return Err(SyncError::Provider("invalid changes cursor".into()));
        }
        Ok(RemoteChangeBatch {
            changes: self.changes[index..].to_vec(),
            next_page_token: self.changes.len().to_string(),
        })
    }

    fn file_metadata(&mut self, file_id: &str) -> Result<Option<RemoteFile>, SyncError> {
        self.check(StoreOperation::FileMetadata)?;
        Ok(self.files.get(file_id).map(|(file, _)| file.clone()))
    }

    fn read_file(&mut self, file_id: &str) -> Result<Option<Vec<u8>>, SyncError> {
        self.check(StoreOperation::ReadFile)?;
        let bytes = self.files.get(file_id).map(|(_, bytes)| bytes.clone());
        if bytes.is_some() {
            self.content_transfers += 1;
        }
        Ok(bytes)
    }

    fn write_file(
        &mut self,
        path: &RelativePath,
        existing_id: Option<&str>,
        bytes: &[u8],
    ) -> Result<RemoteFile, SyncError> {
        self.check(StoreOperation::WriteFile)?;
        let mut entries: BTreeMap<_, _> = self
            .files
            .values()
            .filter(|(file, _)| Some(file.id.as_str()) != existing_id)
            .map(|(file, _)| (file.path.clone(), file.entry.clone()))
            .collect();
        entries.insert(path.clone(), super::model::SnapshotEntry::Tombstone);
        super::model::validate_entries(&entries).map_err(SyncError::Integrity)?;
        if self
            .files
            .values()
            .any(|(file, _)| file.path == *path && Some(file.id.as_str()) != existing_id)
        {
            return Err(SyncError::RemotePathExists);
        }
        let (id, version) = if let Some(id) = existing_id {
            let (file, _) = self
                .files
                .get(id)
                .ok_or_else(|| SyncError::Provider("remote file missing".into()))?;
            if file.path != *path {
                return Err(SyncError::Integrity("remote file path changed".into()));
            }
            (id.to_owned(), file.version.parse::<u64>().unwrap_or(0) + 1)
        } else {
            self.next_file_id += 1;
            (format!("file-{}", self.next_file_id), 1)
        };
        let file = RemoteFile {
            path: path.clone(),
            id: id.clone(),
            version: version.to_string(),
            entry: super::model::SnapshotEntry::File {
                sha256: Sha256::digest(bytes).into(),
                size: bytes.len() as u64,
                modified_unix_ms: 0,
            },
            parent_id: None,
        };
        self.files
            .insert(id.clone(), (file.clone(), bytes.to_vec()));
        self.changes.push(RemoteChange {
            file_id: id,
            file: Some(file.clone()),
        });
        self.content_transfers += 1;
        Ok(file)
    }

    fn delete_file(&mut self, file_id: &str) -> Result<(), SyncError> {
        self.check(StoreOperation::DeleteFile)?;
        if self.files.remove(file_id).is_some() {
            self.changes.push(RemoteChange {
                file_id: file_id.to_owned(),
                file: None,
            });
        }
        Ok(())
    }

    fn list_commits(&mut self) -> Result<Vec<Commit>, SyncError> {
        self.check(StoreOperation::ListCommits)?;
        Ok(self.commits.values().cloned().collect())
    }

    fn read_object(&mut self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError> {
        *self.object_reads.entry(*hash).or_default() += 1;
        self.check(StoreOperation::ReadObject)?;
        Ok(self.objects.get(hash).cloned())
    }

    fn write_object(&mut self, hash: [u8; 32], bytes: &[u8]) -> Result<(), SyncError> {
        self.check(StoreOperation::WriteObject)?;
        if Sha256::digest(bytes).as_slice() != hash {
            return Err(SyncError::Integrity(
                "object key does not match bytes".into(),
            ));
        }
        match self.objects.get(&hash) {
            Some(existing) if existing != bytes => {
                Err(SyncError::Integrity("immutable object changed".into()))
            }
            Some(_) => Ok(()),
            None => {
                if self.corrupt_next_write {
                    self.corrupt_next_write = false;
                    self.objects.insert(hash, b"corrupted upload".to_vec());
                } else {
                    self.objects.insert(hash, bytes.to_vec());
                }
                Ok(())
            }
        }
    }

    fn write_commit(&mut self, commit: &Commit) -> Result<(), SyncError> {
        self.check(StoreOperation::WriteCommit)?;
        match self.commits.get(&commit.id) {
            Some(existing) if existing != commit => {
                Err(SyncError::Integrity("immutable commit changed".into()))
            }
            Some(_) => Ok(()),
            None => {
                self.commits.insert(commit.id, commit.clone());
                Ok(())
            }
        }
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub struct SharedRemoteStore(pub std::sync::Arc<std::sync::Mutex<MemoryRemoteStore>>);

#[cfg(test)]
impl RemoteStore for SharedRemoteStore {
    fn identity(&mut self) -> Result<(String, String), SyncError> {
        self.0.lock().unwrap().identity()
    }
    fn migration_completed(&mut self) -> Result<bool, SyncError> {
        self.0.lock().unwrap().migration_completed()
    }
    fn mark_migration_completed(&mut self) -> Result<(), SyncError> {
        self.0.lock().unwrap().mark_migration_completed()
    }
    fn restore_checkpoint_indexes(
        &mut self,
        file_paths_by_id: &BTreeMap<String, RelativePath>,
        folder_paths_by_id: &BTreeMap<String, String>,
    ) -> Result<(), SyncError> {
        self.0
            .lock()
            .unwrap()
            .restore_checkpoint_indexes(file_paths_by_id, folder_paths_by_id)
    }

    fn checkpoint_folder_paths_by_id(&self) -> Result<Option<BTreeMap<String, String>>, SyncError> {
        self.0.lock().unwrap().checkpoint_folder_paths_by_id()
    }

    fn start_page_token(&mut self) -> Result<String, SyncError> {
        self.0.lock().unwrap().start_page_token()
    }
    fn initial_inventory(&mut self) -> Result<Vec<RemoteFile>, SyncError> {
        self.0.lock().unwrap().initial_inventory()
    }
    fn changes_since(&mut self, token: &str) -> Result<RemoteChangeBatch, SyncError> {
        self.0.lock().unwrap().changes_since(token)
    }
    fn file_metadata(&mut self, id: &str) -> Result<Option<RemoteFile>, SyncError> {
        self.0.lock().unwrap().file_metadata(id)
    }
    fn read_file(&mut self, id: &str) -> Result<Option<Vec<u8>>, SyncError> {
        self.0.lock().unwrap().read_file(id)
    }
    fn write_file(
        &mut self,
        path: &RelativePath,
        id: Option<&str>,
        bytes: &[u8],
    ) -> Result<RemoteFile, SyncError> {
        self.0.lock().unwrap().write_file(path, id, bytes)
    }
    fn delete_file(&mut self, id: &str) -> Result<(), SyncError> {
        self.0.lock().unwrap().delete_file(id)
    }
    fn list_commits(&mut self) -> Result<Vec<Commit>, SyncError> {
        self.0.lock().unwrap().list_commits()
    }
    fn read_object(&mut self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError> {
        self.0.lock().unwrap().read_object(hash)
    }
    fn write_object(&mut self, hash: [u8; 32], bytes: &[u8]) -> Result<(), SyncError> {
        self.0.lock().unwrap().write_object(hash, bytes)
    }
    fn write_commit(&mut self, commit: &Commit) -> Result<(), SyncError> {
        self.0.lock().unwrap().write_commit(commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::model::RelativePath;

    #[test]
    fn checkpoint_folder_index_round_trips_all_ancestor_ids() {
        let folders = BTreeMap::from([
            ("apps-folder".into(), "touchHLE/touchHLE_apps".into()),
            ("old-folder".into(), "touchHLE/touchHLE_apps/Old".into()),
            ("sub-folder".into(), "touchHLE/touchHLE_apps/Old/Sub".into()),
        ]);
        let mut store = MemoryRemoteStore::default();
        store
            .restore_checkpoint_indexes(&BTreeMap::new(), &folders)
            .unwrap();

        assert_eq!(
            store.checkpoint_folder_paths_by_id().unwrap(),
            Some(folders)
        );
    }

    #[test]
    fn current_files_keep_identity_versions_and_replay_changes() {
        let mut store = MemoryRemoteStore::default();
        let path = RelativePath::new("touchHLE_apps/game.ipa").unwrap();
        let initial = store.start_page_token().unwrap();
        let first = store.write_file(&path, None, b"first").unwrap();
        assert_eq!(first.version, "1");
        assert_eq!(store.read_file(&first.id).unwrap(), Some(b"first".to_vec()));
        let batch = store.changes_since(&initial).unwrap();
        assert_eq!(batch.changes[0].file.as_ref(), Some(&first));
        let second = store.write_file(&path, Some(&first.id), b"second").unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.version, "2");
        assert_eq!(
            store.file_metadata(&first.id).unwrap(),
            Some(second.clone())
        );
        store.delete_file(&first.id).unwrap();
        store.delete_file(&first.id).unwrap();
        assert_eq!(store.read_file(&first.id).unwrap(), None);
        let replay = store.changes_since(&batch.next_page_token).unwrap();
        assert_eq!(replay.changes.last().unwrap().file_id, first.id);
        assert_eq!(replay.changes.last().unwrap().file, None);
        assert_eq!(replay, store.changes_since(&batch.next_page_token).unwrap());
        assert!(store.initial_inventory().unwrap().is_empty());
    }

    #[test]
    fn failed_changes_read_replays_cursor_without_transferring_content() {
        let mut store = MemoryRemoteStore::default();
        let token = store.start_page_token().unwrap();
        let path = RelativePath::new("touchHLE_sandbox/save/data").unwrap();
        store.write_file(&path, None, b"bytes").unwrap();
        let transfers = store.content_transfer_count();
        store.fail_next(StoreOperation::ChangesSince);
        assert!(store.changes_since(&token).is_err());
        let batch = store.changes_since(&token).unwrap();
        assert_eq!(batch.changes.len(), 1);
        let empty = store.changes_since(&batch.next_page_token).unwrap();
        assert!(empty.changes.is_empty());
        assert_eq!(store.change_request_count(), 3);
        assert_eq!(store.content_transfer_count(), transfers);
    }

    #[test]
    fn failed_write_and_case_collision_do_not_create_events() {
        let mut store = MemoryRemoteStore::default();
        let path = RelativePath::new("touchHLE_apps/Save").unwrap();
        store.fail_next(StoreOperation::WriteFile);
        assert!(store.write_file(&path, None, b"one").is_err());
        assert!(store.changes_since("0").unwrap().changes.is_empty());
        store.write_file(&path, None, b"one").unwrap();
        let collision = RelativePath::new("touchHLE_apps/save").unwrap();
        assert!(matches!(
            store.write_file(&collision, None, b"two"),
            Err(SyncError::Integrity(_))
        ));
        assert_eq!(store.changes_since("0").unwrap().changes.len(), 1);
    }

    #[test]
    fn immutable_writes_are_idempotent_and_divergent_tips_survive() {
        let mut store = MemoryRemoteStore::default();
        let hash: [u8; 32] = sha2::Sha256::digest(b"hello").into();
        store.write_object(hash, b"hello").unwrap();
        store.write_object(hash, b"hello").unwrap();
        assert!(store.write_object(hash, b"wrong").is_err());
        assert_eq!(store.read_object(&hash).unwrap(), Some(b"hello".to_vec()));
        let parent = Commit {
            id: Uuid::from_u128(1),
            device_id: Uuid::nil(),
            created_unix_ms: 0,
            parents: vec![],
            entries: Default::default(),
        };
        store.write_commit(&parent).unwrap();
        for id in [2, 3] {
            store
                .write_commit(&Commit {
                    id: Uuid::from_u128(id),
                    parents: vec![parent.id],
                    ..parent.clone()
                })
                .unwrap();
        }
        store.write_commit(&parent).unwrap();
        assert_eq!(store.list_commits().unwrap().len(), 3);
        assert_eq!(
            crate::sync::merge::resolve_remote_tips(&store.list_commits().unwrap())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn batched_object_writes_verify_remote_bytes_before_success() {
        let mut store = MemoryRemoteStore::default();
        let good: [u8; 32] = Sha256::digest(b"good").into();
        store
            .write_objects_and_verify(&[(good, b"good".to_vec())])
            .unwrap();

        let corrupt: [u8; 32] = Sha256::digest(b"expected").into();
        store.corrupt_next_write();
        assert!(matches!(
            store.write_objects_and_verify(&[(corrupt, b"expected".to_vec())]),
            Err(SyncError::Integrity(_))
        ));
    }

    #[test]
    fn injected_failures_leave_previous_values_intact() {
        let mut store = MemoryRemoteStore::default();
        let hash: [u8; 32] = sha2::Sha256::digest(b"hello").into();
        store.fail_next(StoreOperation::WriteObject);
        assert!(store.write_object(hash, b"hello").is_err());
        assert_eq!(store.read_object(&hash).unwrap(), None);
        store.write_object(hash, b"hello").unwrap();
        store.fail_next(StoreOperation::ReadObject);
        assert!(store.read_object(&hash).is_err());
        assert_eq!(store.read_object(&hash).unwrap(), Some(b"hello".to_vec()));
        store.fail_next(StoreOperation::ListCommits);
        assert!(store.list_commits().is_err());
        store.fail_next(StoreOperation::WriteCommit);
        assert!(store
            .write_commit(&Commit {
                id: Uuid::nil(),
                device_id: Uuid::nil(),
                created_unix_ms: 0,
                parents: vec![],
                entries: Default::default()
            })
            .is_err());
        assert!(store.list_commits().unwrap().is_empty());
    }
}
