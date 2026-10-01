/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::model::{Commit, SyncError};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

pub trait RemoteStore {
    fn list_commits(&mut self) -> Result<Vec<Commit>, SyncError>;
    fn read_object(&mut self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError>;
    fn write_object(&mut self, hash: [u8; 32], bytes: &[u8]) -> Result<(), SyncError>;
    fn write_commit(&mut self, commit: &Commit) -> Result<(), SyncError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum StoreOperation {
    ListCommits,
    ReadObject,
    WriteObject,
    WriteCommit,
}

#[derive(Clone, Default)]
pub struct MemoryRemoteStore {
    objects: BTreeMap<[u8; 32], Vec<u8>>,
    commits: BTreeMap<Uuid, Commit>,
    failures: BTreeMap<StoreOperation, usize>,
}

impl MemoryRemoteStore {
    #[cfg(test)]
    pub fn corrupt_object(&mut self, hash: [u8; 32], bytes: Vec<u8>) {
        self.objects.insert(hash, bytes);
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
}

impl RemoteStore for MemoryRemoteStore {
    fn list_commits(&mut self) -> Result<Vec<Commit>, SyncError> {
        self.check(StoreOperation::ListCommits)?;
        Ok(self.commits.values().cloned().collect())
    }

    fn read_object(&mut self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError> {
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
                self.objects.insert(hash, bytes.to_vec());
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
mod tests {
    use super::*;

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
