/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::apply::{
    apply_staged_transaction, recover_transaction, stage_current_files_with_progress,
    stage_migration_files, sync_directory,
};
use super::merge::{
    plan_sync, plan_sync_with_local_baseline, plan_sync_without_common_ancestor,
    resolve_remote_tips, Conflict, ConflictChoice as LegacyConflictChoice, RemoteTip,
    RemoteVersion, SyncPlan as LegacySyncPlan,
};
use super::model::{
    is_ignored_sync_file, load_sync_state, Commit, CurrentSyncState, LoadedSyncState, RelativePath,
    SnapshotEntry, SyncError, SyncState,
};
use super::progress::SyncProgress;
use super::reconcile::{
    plan_sync as plan_current_sync, rebind_conflict_choices, ConflictChoice, FileBaseline,
    FileConflict, RemoteCandidate, RemoteChangeBatch, RemoteFile, RemoteVersionId, SyncPlan,
};
use super::store::RemoteStore;
use crate::paths::SYNC_DIR;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Reserve an ID before any remote operation without advancing sync history.
    pub fn persisted_device_id(&mut self) -> Result<Uuid, SyncError> {
        self.persisted_device_id_impl(|| {})
    }

    fn persisted_device_id_impl(
        &mut self,
        after_missing: impl FnOnce(),
    ) -> Result<Uuid, SyncError> {
        if let Some(bytes) = read_state_bytes(&self.root, &self.state_path)? {
            return Ok(match load_sync_state(Some(&bytes))? {
                LoadedSyncState::Current(state) => state.device_id,
                LoadedSyncState::Legacy(state) => state.device_id,
                LoadedSyncState::Missing => unreachable!(),
            });
        }
        after_missing();

        let dir = state_dir(&self.root, &self.state_path, true)?.unwrap();
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
        let lock_file = dir.open_with("state.lock", &options)?;
        if !lock_file.metadata()?.is_file() {
            return Err(SyncError::Integrity(
                "state lock is not a regular file".into(),
            ));
        }
        let _lock_file = lock_file.into_std();
        // Android app external storage does not implement OS file locks. All
        // production callers hold the coordinator's process-local operation
        // lock while reserving the device ID.
        #[cfg(not(target_os = "android"))]
        _lock_file.lock()?;

        if let Some(bytes) = read_state_bytes(&self.root, &self.state_path)? {
            Ok(match load_sync_state(Some(&bytes))? {
                LoadedSyncState::Current(state) => state.device_id,
                LoadedSyncState::Legacy(state) => state.device_id,
                LoadedSyncState::Missing => unreachable!(),
            })
        } else {
            let fresh = CurrentSyncState::new(Uuid::new_v4());
            save_current_state(&self.root, &self.state_path, &fresh)?;
            Ok(fresh.device_id)
        }
    }

    #[cfg(test)]
    fn persisted_device_id_with(
        &mut self,
        after_missing: impl FnOnce(),
    ) -> Result<Uuid, SyncError> {
        self.persisted_device_id_impl(after_missing)
    }

    #[cfg(test)]
    pub fn store(&mut self) -> &mut S {
        &mut self.store
    }

    #[cfg(test)]
    pub fn into_store(self) -> S {
        self.store
    }

    /// The caller must keep the guest and every other local-tree writer
    /// quiescent for the entire operation, especially through local apply.
    pub fn synchronize(&mut self) -> Result<SyncOutcome, SyncError> {
        self.synchronize_with_choices(None, true, &mut |_| {})
    }

    pub(crate) fn synchronize_with_progress(
        &mut self,
        progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncOutcome, SyncError> {
        self.synchronize_with_choices(None, true, progress)
    }

    pub fn synchronize_for_background(&mut self) -> Result<SyncOutcome, SyncError> {
        self.synchronize_with_choices(None, false, &mut |_| {})
    }

    /// Applies a complete choice set against the exact current-file plan shown.
    pub fn resolve_conflicts(
        &mut self,
        displayed_plan: &SyncPlan,
        choices: &[ConflictChoice],
    ) -> Result<SyncOutcome, SyncError> {
        self.synchronize_with_choices(Some((displayed_plan, choices)), true, &mut |_| {})
    }

    pub(crate) fn resolve_conflicts_with_progress(
        &mut self,
        displayed_plan: &SyncPlan,
        choices: &[ConflictChoice],
        progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncOutcome, SyncError> {
        self.synchronize_with_choices(Some((displayed_plan, choices)), true, progress)
    }

    #[cfg(any())]
    fn synchronize_with_choices(
        &mut self,
        requested_resolution: Option<(&LegacySyncPlan, &[LegacyConflictChoice])>,
        allow_offline_authentication: bool,
        progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncOutcome, SyncError> {
        let state = report_integrity_stage(
            "load local sync state",
            load_state(&self.root, &self.state_path),
        )?;
        let scan_started = Instant::now();
        progress(SyncProgress::Scanning { files: 0, bytes: 0 });
        let mut scan_totals = (0, 0);
        let local = report_integrity_stage(
            "scan local sync files",
            super::scan::scan_roots_with_progress(&self.root, |files, bytes| {
                scan_totals = (files, bytes);
                progress(SyncProgress::Scanning { files, bytes });
            }),
        )?;
        log!(
            "Google Drive local scan: {} files, {} bytes in {} ms",
            scan_totals.0,
            scan_totals.1,
            scan_started.elapsed().as_millis()
        );
        progress(SyncProgress::CheckingRemote);
        let remote_started = Instant::now();
        let commits_result =
            report_integrity_stage("read remote commit index", self.store.list_commits());
        log!(
            "Google Drive remote index read completed in {} ms",
            remote_started.elapsed().as_millis()
        );
        let commits = match commits_result {
            Err(SyncError::Authentication(_)) if allow_offline_authentication => {
                return Ok(SyncOutcome::Offline);
            }
            commits => commits?,
        };
        progress(SyncProgress::Comparing);
        let planning_started = Instant::now();
        let tips = report_integrity_stage(
            "validate remote commit history",
            resolve_remote_tips(&commits),
        )?;
        let saved_baseline = report_integrity_stage(
            "validate saved sync baseline",
            validate_baseline_present(state.last_applied_commit, &state.baseline, &commits),
        )?;
        // Include the saved commit as an ancestry anchor but compare sibling
        // tips from their shared base so changes on both branches conflict.
        let empty_baseline = BTreeMap::new();
        let local_baseline = if state.last_applied_commit.is_some() {
            &saved_baseline
        } else {
            &empty_baseline
        };
        let planning_base = report_integrity_stage(
            "select sync baseline",
            select_planning_base(state.last_applied_commit, &saved_baseline, &commits, &tips),
        )?;
        let plan = match planning_base {
            PlanningBase::Snapshot(snapshot) => report_integrity_stage(
                "plan local and remote changes",
                plan_sync_with_local_baseline(&snapshot, local_baseline, &local, &tips),
            )?,
            PlanningBase::Unrelated => report_integrity_stage(
                "plan unrelated sync histories",
                plan_sync_without_common_ancestor(local_baseline, &local, &tips),
            )?,
            PlanningBase::Ambiguous => report_integrity_stage(
                "plan ambiguous sync histories",
                conservative_conflict_plan(&local, &tips),
            )?,
        };
        log!(
            "Google Drive sync plan: local_changes={}, remote_changes={}, conflicts={} in {} ms",
            plan.publish_local.len(),
            plan.apply_remote.len(),
            plan.conflicts.len(),
            planning_started.elapsed().as_millis()
        );
        if let Some((displayed_plan, _)) = requested_resolution {
            if displayed_plan != &plan {
                return Err(SyncError::UnresolvedConflicts(
                    "sync state changed while conflicts were being resolved; reopen the resolver"
                        .into(),
                ));
            }
            if plan.conflicts.is_empty() {
                return Err(SyncError::UnresolvedConflicts(
                    "the displayed conflicts no longer exist; synchronize again".into(),
                ));
            }
        }
        if !plan.conflicts.is_empty() {
            if requested_resolution.is_none() {
                return Ok(SyncOutcome::Conflicts(plan));
            }
        }
        let choices = requested_resolution.map_or(&[][..], |(_, choices)| choices);
        let resolved = report_integrity_stage(
            "validate sync conflict choices",
            plan.resolved_snapshot(choices),
        )?;
        let publish = requested_resolution.is_some()
            || !plan.publish_local.is_empty()
            || tips.len() > 1
            || tips.is_empty();
        let apply = !plan.apply_remote.is_empty();

        // For an explicit resolution, verify and stage all chosen cloud bytes
        // before publishing a merge commit that records those choices.
        let mut staged_objects = BTreeMap::<[u8; 32], Vec<u8>>::new();
        let pre_staged = if requested_resolution.is_some() {
            Some(report_integrity_stage(
                "stage selected remote files",
                stage_remote_files_with_progress(
                    &self.root,
                    &plan,
                    choices,
                    |hash| {
                        if let Some(bytes) = staged_objects.get(hash) {
                            return Ok(bytes.clone());
                        }
                        let bytes = self.store.read_object(hash)?.ok_or_else(|| {
                            SyncError::Integrity(format!(
                                "missing remote object {}",
                                hex_hash(hash)
                            ))
                        })?;
                        staged_objects.insert(*hash, bytes.clone());
                        Ok(bytes)
                    },
                    |files_done, files_total, bytes_done, bytes_total| {
                        progress(SyncProgress::Downloading {
                            files_done,
                            files_total,
                            bytes_done,
                            bytes_total,
                        });
                        log!(
                            "Google Drive download progress: {files_done}/{files_total} files, {bytes_done}/{bytes_total} bytes"
                        );
                    },
                ),
            )?)
        } else {
            None
        };

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
            let mut upload_objects = Vec::new();
            // Existing history references are trusted; verify only new uploads here.
            for (path, entry) in &resolved {
                let SnapshotEntry::File { sha256, size, .. } = entry else {
                    continue;
                };
                if referenced_objects.contains(sha256) || !uploaded.insert(*sha256) {
                    continue;
                }
                upload_objects.push((path, *sha256, *size));
            }
            let upload_bytes = upload_objects
                .iter()
                .fold(0u64, |total, (_, _, size)| total.saturating_add(*size));
            let mut upload_bytes_done = 0u64;
            let mut uploads_done = 0;
            for batch in upload_objects.chunks(MAX_PARALLEL_OBJECT_WRITES) {
                let mut remote_objects = Vec::with_capacity(batch.len());
                for (path, sha256, size) in batch {
                    progress(SyncProgress::Uploading {
                        files_done: uploads_done,
                        files_total: upload_objects.len(),
                        bytes_done: upload_bytes_done,
                        bytes_total: upload_bytes,
                    });
                    let local_read_started = Instant::now();
                    let bytes =
                        report_integrity_stage("read local upload", read_local(&self.root, path))?;
                    log!(
                        "Google Drive local upload read completed: {} bytes in {} ms",
                        bytes.len(),
                        local_read_started.elapsed().as_millis()
                    );
                    let local_verify_started = Instant::now();
                    report_integrity_stage(
                        "verify local upload",
                        verify_object(&bytes, sha256, *size),
                    )?;
                    log!(
                        "Google Drive local upload hash verified in {} ms",
                        local_verify_started.elapsed().as_millis()
                    );
                    remote_objects.push((*sha256, bytes));
                }
                let remote_started = Instant::now();
                self.store.write_objects_and_verify(&remote_objects)?;
                for (_, _, size) in batch {
                    uploads_done += 1;
                    upload_bytes_done = upload_bytes_done.saturating_add(*size);
                }
                log!(
                    "Google Drive verified remote object batch: {uploads_done}/{} files, {} bytes in {} ms",
                    upload_objects.len(),
                    remote_objects
                        .iter()
                        .map(|(_, bytes)| bytes.len() as u64)
                        .sum::<u64>(),
                    remote_started.elapsed().as_millis()
                );
                progress(SyncProgress::Uploading {
                    files_done: uploads_done,
                    files_total: upload_objects.len(),
                    bytes_done: upload_bytes_done,
                    bytes_total: upload_bytes,
                });
            }
            progress(SyncProgress::Uploading {
                files_done: uploads_done,
                files_total: upload_objects.len(),
                bytes_done: upload_bytes_done,
                bytes_total: upload_bytes,
            });
        }

        let commit_started = Instant::now();
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
        if publish {
            log!(
                "Google Drive commit publication completed in {} ms",
                commit_started.elapsed().as_millis()
            );
        }

        let staged = match pre_staged {
            Some(staged) => staged,
            None => report_integrity_stage(
                "stage remote files",
                stage_remote_files_with_progress(
                    &self.root,
                    &plan,
                    &[],
                    |hash| {
                        if let Some(bytes) = staged_objects.get(hash) {
                            return Ok(bytes.clone());
                        }
                        let bytes = self.store.read_object(hash)?.ok_or_else(|| {
                            SyncError::Integrity(format!(
                                "missing remote object {}",
                                hex_hash(hash)
                            ))
                        })?;
                        staged_objects.insert(*hash, bytes.clone());
                        Ok(bytes)
                    },
                    |files_done, files_total, bytes_done, bytes_total| {
                        progress(SyncProgress::Downloading {
                            files_done,
                            files_total,
                            bytes_done,
                            bytes_total,
                        });
                        log!(
                            "Google Drive download progress: {files_done}/{files_total} files, {bytes_done}/{bytes_total} bytes"
                        );
                    },
                ),
            )?,
        };
        if !staged.is_empty() {
            progress(SyncProgress::Applying {
                files_total: staged.len(),
            });
        }
        let apply_started = Instant::now();
        report_integrity_stage("apply remote files", apply_staged_files(&self.root, staged))?;
        if !plan.apply_remote.is_empty() {
            log!(
                "Google Drive local apply completed in {} ms",
                apply_started.elapsed().as_millis()
            );
        }
        // With no new commit, the remote tip owns the baseline metadata.
        // Local mtimes may differ even when file contents are unchanged.
        let baseline = if publish {
            resolved
        } else if let Some(id) = commit_id {
            commits
                .iter()
                .find(|commit| commit.id == id)
                .ok_or_else(|| SyncError::Integrity(format!("missing synced commit: {id}")))?
                .entries
                .clone()
        } else {
            resolved
        };
        if state.baseline != baseline
            || state.last_applied_commit != commit_id
            || !self.state_path.exists()
        {
            progress(SyncProgress::Finalizing);
            report_integrity_stage(
                "save local sync state",
                save_state(
                    &self.root,
                    &self.state_path,
                    &SyncState {
                        device_id: state.device_id,
                        last_applied_commit: commit_id,
                        baseline,
                    },
                ),
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

    fn synchronize_with_choices(
        &mut self,
        requested_resolution: Option<(&SyncPlan, &[ConflictChoice])>,
        allow_offline_authentication: bool,
        progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncOutcome, SyncError> {
        let bytes = read_state_bytes(&self.root, &self.state_path)?;
        recover_transaction(&self.root, bytes.as_deref(), |checkpoint| {
            restore_state_bytes(&self.root, &self.state_path, checkpoint)
        })?;
        let recovered = read_state_bytes(&self.root, &self.state_path)?;
        match load_sync_state(recovered.as_deref())? {
            LoadedSyncState::Legacy(state) => {
                let journal = read_migration_journal(&self.root, &self.state_path)?;
                if journal.as_ref().is_some_and(|journal| {
                    journal.previous.as_deref() != recovered.as_deref()
                        || journal.device_id != state.device_id
                }) {
                    return Err(SyncError::Integrity(
                        "migration journal does not match checkpoint".into(),
                    ));
                }
                let completed = match self.store.migration_completed() {
                    Err(SyncError::Authentication(_)) if allow_offline_authentication => {
                        return Ok(SyncOutcome::Offline)
                    }
                    result => result?,
                };
                if completed {
                    let mut baseline = CurrentSyncState::new(state.device_id);
                    baseline.baseline = state
                        .baseline
                        .iter()
                        .map(|(path, entry)| {
                            (
                                path.clone(),
                                FileBaseline {
                                    sha256: match entry {
                                        SnapshotEntry::File { sha256, .. } => Some(*sha256),
                                        SnapshotEntry::Tombstone => None,
                                    },
                                    remote_id: None,
                                    remote_version: None,
                                },
                            )
                        })
                        .collect();
                    let (account_id, root_folder_id) = match self.store.identity() {
                        Err(SyncError::Authentication(_)) if allow_offline_authentication => {
                            return Ok(SyncOutcome::Offline)
                        }
                        result => result?,
                    };
                    baseline.account_id = Some(account_id);
                    baseline.root_folder_id = Some(root_folder_id);
                    let outcome = self.synchronize_current(
                        requested_resolution,
                        allow_offline_authentication,
                        progress,
                        0,
                        Some(baseline),
                        requested_resolution.is_some(),
                    )?;
                    if !matches!(outcome, SyncOutcome::Offline | SyncOutcome::Conflicts(_)) {
                        remove_migration_journal(&self.root, &self.state_path)?;
                    }
                    return Ok(outcome);
                }
                return self.migrate_legacy(
                    &state,
                    recovered.as_deref(),
                    requested_resolution,
                    allow_offline_authentication,
                    progress,
                );
            }
            LoadedSyncState::Current(state)
                if state.changes_cursor.is_none()
                    && state.baseline.is_empty()
                    && state.file_paths_by_id.is_empty()
                    && state.account_id.is_none()
                    && state.root_folder_id.is_none() =>
            {
                let has_legacy_history = match self.detect_legacy_history() {
                    Err(SyncError::Authentication(_)) if allow_offline_authentication => {
                        return Ok(SyncOutcome::Offline);
                    }
                    result => result?,
                };
                if has_legacy_history {
                    return self.migrate_legacy(
                        &SyncState {
                            device_id: state.device_id,
                            last_applied_commit: None,
                            baseline: BTreeMap::new(),
                        },
                        recovered.as_deref(),
                        requested_resolution,
                        allow_offline_authentication,
                        progress,
                    );
                }
            }
            LoadedSyncState::Missing => {
                let has_legacy_history = match self.detect_legacy_history() {
                    Err(SyncError::Authentication(_)) if allow_offline_authentication => {
                        return Ok(SyncOutcome::Offline);
                    }
                    result => result?,
                };
                if has_legacy_history {
                    let journal = read_migration_journal(&self.root, &self.state_path)?;
                    return self.migrate_legacy(
                        &SyncState {
                            device_id: journal.map_or_else(Uuid::new_v4, |j| j.device_id),
                            last_applied_commit: None,
                            baseline: BTreeMap::new(),
                        },
                        None,
                        requested_resolution,
                        allow_offline_authentication,
                        progress,
                    );
                }
            }
            _ => {}
        }
        self.synchronize_current(
            requested_resolution,
            allow_offline_authentication,
            progress,
            0,
            None,
            requested_resolution.is_some(),
        )
    }

    fn synchronize_current(
        &mut self,
        requested_resolution: Option<(&SyncPlan, &[ConflictChoice])>,
        allow_offline_authentication: bool,
        progress: &mut dyn FnMut(SyncProgress),
        stale_retries: usize,
        initial_state: Option<CurrentSyncState>,
        refresh_inventory: bool,
    ) -> Result<SyncOutcome, SyncError> {
        let previous_bytes = read_state_bytes(&self.root, &self.state_path)?;
        recover_transaction(&self.root, previous_bytes.as_deref(), |checkpoint| {
            restore_state_bytes(&self.root, &self.state_path, checkpoint)
        })?;
        let previous_bytes = read_state_bytes(&self.root, &self.state_path)?;
        if matches!(
            load_sync_state(previous_bytes.as_deref())?,
            LoadedSyncState::Current(_)
        ) {
            remove_migration_journal(&self.root, &self.state_path)?;
        }
        let state = match initial_state.as_ref() {
            Some(state) => state.clone(),
            None => match load_sync_state(previous_bytes.as_deref())? {
                LoadedSyncState::Current(state) => state,
                LoadedSyncState::Legacy(_) => return Err(SyncError::MigrationRequired),
                LoadedSyncState::Missing => CurrentSyncState::new(Uuid::new_v4()),
            },
        };
        let (account_id, root_folder_id) = match self.store.identity() {
            Err(SyncError::Authentication(_)) if allow_offline_authentication => {
                return Ok(SyncOutcome::Offline)
            }
            result => result?,
        };
        if account_id.is_empty() || root_folder_id.is_empty() {
            return Err(SyncError::Integrity("empty remote identity".into()));
        }
        let state = if state.account_id.as_deref() == Some(&account_id)
            && state.root_folder_id.as_deref() == Some(&root_folder_id)
        {
            state
        } else {
            CurrentSyncState::new(state.device_id)
        };

        progress(SyncProgress::Scanning { files: 0, bytes: 0 });
        let local = report_integrity_stage(
            "scan local sync files",
            super::scan::scan_roots_with_progress(&self.root, |files, bytes| {
                progress(SyncProgress::Scanning { files, bytes });
            }),
        )?;
        progress(SyncProgress::CheckingRemote);
        let baseline: BTreeMap<_, _> = state
            .baseline
            .iter()
            .filter(|(path, _)| !is_ignored_sync_file(path))
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
        let mut ignored_file_paths_by_id: BTreeMap<_, _> = state
            .file_paths_by_id
            .iter()
            .filter(|(_, path)| is_ignored_sync_file(path))
            .map(|(id, path)| (id.clone(), path.clone()))
            .collect();

        let mut remote = BTreeMap::<RelativePath, RemoteFile>::new();
        let batch = if let Some(cursor) = state.changes_cursor.as_deref() {
            if stale_retries == 0 {
                for (path, baseline) in &baseline {
                    let (Some(id), Some(version), Some(sha256)) = (
                        baseline.remote_id.as_ref(),
                        baseline.remote_version.as_ref(),
                        baseline.sha256,
                    ) else {
                        continue;
                    };
                    remote.insert(
                        path.clone(),
                        RemoteFile {
                            path: path.clone(),
                            id: id.clone(),
                            version: version.clone(),
                            entry: SnapshotEntry::File {
                                sha256,
                                size: 0,
                                modified_unix_ms: 0,
                            },
                            parent_id: None,
                        },
                    );
                }
            }
            self.store
                .restore_checkpoint_indexes(&state.file_paths_by_id, &state.folder_paths_by_id)?;
            if refresh_inventory {
                let inventory = match self.store.initial_inventory() {
                    Err(SyncError::Authentication(_)) if allow_offline_authentication => {
                        return Ok(SyncOutcome::Offline)
                    }
                    result => result?,
                };
                log!(
                    "Google Drive reconciliation refreshed remote inventory: {} files",
                    inventory.len()
                );
                for file in inventory {
                    if is_ignored_sync_file(&file.path) {
                        ignored_file_paths_by_id.insert(file.id.clone(), file.path.clone());
                    } else {
                        remote.insert(file.path.clone(), file);
                    }
                }
            }
            let batch = report_integrity_stage(
                "read Drive changes since checkpoint",
                current_changes(self, cursor, allow_offline_authentication),
            )?;
            match batch {
                Some(batch) => batch,
                None => return Ok(SyncOutcome::Offline),
            }
        } else {
            let start = match self.store.start_page_token() {
                Err(SyncError::Authentication(_)) if allow_offline_authentication => {
                    return Ok(SyncOutcome::Offline)
                }
                result => result?,
            };
            let inventory = match self.store.initial_inventory() {
                Err(SyncError::Authentication(_)) if allow_offline_authentication => {
                    return Ok(SyncOutcome::Offline)
                }
                result => result?,
            };
            for file in inventory {
                if is_ignored_sync_file(&file.path) {
                    ignored_file_paths_by_id.insert(file.id.clone(), file.path.clone());
                } else {
                    remote.insert(file.path.clone(), file);
                }
            }
            let batch = report_integrity_stage(
                "read Drive changes after initial inventory",
                current_changes(self, &start, allow_offline_authentication),
            )?;
            match batch {
                Some(batch) => batch,
                None => return Ok(SyncOutcome::Offline),
            }
        };

        let mut id_paths = state.file_paths_by_id.clone();
        for (path, file) in &remote {
            id_paths.insert(file.id.clone(), path.clone());
        }
        for change in batch.changes {
            if change
                .file
                .as_ref()
                .is_some_and(|file| file.id != change.file_id)
            {
                return Err(SyncError::Integrity(
                    "Changes entry ID does not match file metadata".into(),
                ));
            }
            if let Some(old_path) = id_paths.remove(&change.file_id) {
                remote.remove(&old_path);
            }
            if let Some(file) = change.file {
                id_paths.insert(file.id.clone(), file.path.clone());
                if is_ignored_sync_file(&file.path) {
                    ignored_file_paths_by_id.insert(file.id.clone(), file.path.clone());
                } else {
                    ignored_file_paths_by_id.remove(&file.id);
                    remote.insert(file.path.clone(), file);
                }
            } else {
                ignored_file_paths_by_id.remove(&change.file_id);
            }
        }
        report_integrity_stage(
            "validate remote file inventory",
            validate_remote_paths(&remote),
        )?;
        let plan = report_integrity_stage(
            "plan local and current remote files",
            plan_current_sync(&baseline, &local, &remote),
        )?;

        let rebound_choices;
        let choices = if let Some((displayed_plan, requested_choices)) = requested_resolution {
            if plan.conflicts.is_empty() {
                return self.synchronize_current(
                    None,
                    allow_offline_authentication,
                    progress,
                    stale_retries,
                    initial_state,
                    refresh_inventory,
                );
            }
            if !displayed_plan.has_same_content_as(&plan) {
                return Err(SyncError::UnresolvedConflicts(
                    "sync state changed while conflicts were being resolved; reopen the resolver"
                        .into(),
                ));
            }
            rebound_choices = rebind_conflict_choices(displayed_plan, &plan, requested_choices)?;
            rebound_choices.as_slice()
        } else {
            if !plan.conflicts.is_empty() {
                return Ok(SyncOutcome::Conflicts(plan));
            }
            &[]
        };
        let resolved = plan.resolved_snapshot(choices)?;

        progress(SyncProgress::Comparing);
        let staged = stage_current_files_with_progress(
            &self.root,
            &plan,
            choices,
            |id| self.store.read_file(id),
            |files_done, files_total, bytes_done, bytes_total| {
                progress(SyncProgress::Downloading {
                    files_done,
                    files_total,
                    bytes_done,
                    bytes_total,
                });
            },
        )?;
        if !selected_remote_versions_match(&mut self.store, &plan, choices)? {
            if requested_resolution.is_some() {
                return Err(SyncError::UnresolvedConflicts(
                    "selected remote file changed; reopen the resolver".into(),
                ));
            }
            if stale_retries >= 2 {
                return Err(SyncError::Provider(
                    "selected remote file kept changing".into(),
                ));
            }
            return self.synchronize_current(
                None,
                allow_offline_authentication,
                progress,
                stale_retries + 1,
                initial_state,
                refresh_inventory,
            );
        }

        let mut publish = plan.publish_local.clone();
        for choice in choices {
            if choice.selected == super::model::LocalOrRemote::Local {
                let conflict = plan
                    .conflicts
                    .iter()
                    .find(|conflict| conflict.path == choice.path)
                    .ok_or_else(|| SyncError::UnresolvedConflicts("stale choice".into()))?;
                publish.insert(choice.path.clone(), conflict.local.clone());
            }
        }
        for path in publish.keys() {
            if !preflight_remote_path(&mut self.store, path, &remote, &state)? {
                if requested_resolution.is_some() {
                    return Err(SyncError::UnresolvedConflicts(
                        "remote file changed while conflicts were being resolved; reopen the resolver"
                            .into(),
                    ));
                }
                if stale_retries >= 2 {
                    return Err(SyncError::Provider(
                        "remote file kept changing during synchronization".into(),
                    ));
                }
                return self.synchronize_current(
                    None,
                    allow_offline_authentication,
                    progress,
                    stale_retries + 1,
                    initial_state,
                    refresh_inventory,
                );
            }
        }

        let mut published = false;
        for (index, (path, entry)) in publish.iter().enumerate() {
            if !preflight_remote_path(&mut self.store, path, &remote, &state)? {
                if requested_resolution.is_some() {
                    return Err(SyncError::UnresolvedConflicts(
                        "remote file changed before publication; reopen the resolver".into(),
                    ));
                }
                if stale_retries >= 2 {
                    return Err(SyncError::Provider(
                        "remote file kept changing during publication".into(),
                    ));
                }
                return self.synchronize_current(
                    None,
                    allow_offline_authentication,
                    progress,
                    stale_retries + 1,
                    initial_state,
                    refresh_inventory,
                );
            }
            progress(SyncProgress::Uploading {
                files_done: index,
                files_total: publish.len(),
                bytes_done: 0,
                bytes_total: 0,
            });
            let existing_id = remote.get(path).map(|file| file.id.as_str());
            match entry {
                Some(SnapshotEntry::File { sha256, size, .. }) => {
                    let bytes = read_local(&self.root, path)?;
                    verify_object(&bytes, sha256, *size).map_err(|_| {
                        SyncError::UnresolvedConflicts(
                            "local file changed before migration overwrite".into(),
                        )
                    })?;
                    let returned = match self.store.write_file(path, existing_id, &bytes) {
                        Ok(returned) => returned,
                        Err(SyncError::RemotePathExists) if stale_retries < 2 => {
                            log!(
                                "Google Drive publish found an occupied remote path; refreshing inventory before conflict planning"
                            );
                            return self.synchronize_current(
                                None,
                                allow_offline_authentication,
                                progress,
                                stale_retries + 1,
                                initial_state,
                                true,
                            );
                        }
                        Err(error) => return Err(error),
                    };
                    if returned.path != *path
                        || entry_sha256(&returned.entry) != Some(sha256)
                        || returned.id.is_empty()
                        || returned.version.is_empty()
                    {
                        return Err(SyncError::Integrity(
                            "uploaded file metadata did not match the selected path/content".into(),
                        ));
                    }
                    let verified = read_file_for_verification(&mut self.store, &returned.id, path)?
                        .ok_or_else(|| SyncError::Integrity("uploaded file disappeared".into()))?;
                    if !same_file_content(path, sha256, &verified) {
                        return Err(SyncError::Integrity(
                            "uploaded file content changed before verification".into(),
                        ));
                    }
                    remote.insert(path.clone(), verified);
                    published = true;
                }
                None | Some(SnapshotEntry::Tombstone) => {
                    if let Some(id) = existing_id {
                        self.store.delete_file(id)?;
                        if self.store.file_metadata(id)?.is_some() {
                            return Err(SyncError::Integrity(
                                "deleted Drive file is still present".into(),
                            ));
                        }
                    }
                    remote.remove(path);
                    published = true;
                }
            }
        }
        if !selected_remote_versions_match(&mut self.store, &plan, choices)? {
            if requested_resolution.is_some() {
                return Err(SyncError::UnresolvedConflicts(
                    "selected remote file changed before apply; reopen the resolver".into(),
                ));
            }
            if stale_retries >= 2 {
                return Err(SyncError::Provider(
                    "remote file kept changing before apply".into(),
                ));
            }
            return self.synchronize_current(
                None,
                allow_offline_authentication,
                progress,
                stale_retries + 1,
                initial_state,
                refresh_inventory,
            );
        }
        if !state.migration_marker_confirmed {
            self.store.mark_migration_completed()?;
        }
        let mut next_state = state.clone();
        next_state.migration_marker_confirmed = true;
        next_state.account_id = Some(account_id);
        next_state.root_folder_id = Some(root_folder_id);
        next_state.changes_cursor = Some(batch.next_page_token);
        next_state.baseline.clear();
        next_state.file_paths_by_id.clear();
        next_state.file_paths_by_id.extend(ignored_file_paths_by_id);
        for (path, entry) in &resolved {
            let file = remote.get(path);
            let sha256 = match entry {
                SnapshotEntry::File { sha256, .. } => Some(*sha256),
                SnapshotEntry::Tombstone => None,
            };
            next_state.baseline.insert(
                path.clone(),
                FileBaseline {
                    sha256,
                    remote_id: file.map(|file| file.id.clone()),
                    remote_version: file.map(|file| file.version.clone()),
                },
            );
            if let Some(file) = file {
                next_state
                    .file_paths_by_id
                    .insert(file.id.clone(), path.clone());
            }
        }
        if let Some(folder_paths_by_id) = self.store.checkpoint_folder_paths_by_id()? {
            next_state.folder_paths_by_id = folder_paths_by_id;
        }
        next_state.prune_deleted_paths(&local, &remote);
        let checkpoint_bytes = serde_json::to_vec(&next_state)?;
        progress(SyncProgress::Applying {
            files_total: staged.len(),
        });
        let root = self.root.clone();
        let state_path = self.state_path.clone();
        let state_for_save = next_state.clone();
        apply_staged_transaction(
            &root,
            staged,
            previous_bytes,
            &checkpoint_bytes,
            || save_current_state(&root, &state_path, &state_for_save),
            |checkpoint| restore_state_bytes(&root, &state_path, checkpoint),
        )?;
        progress(SyncProgress::Finalizing);
        Ok(if published {
            SyncOutcome::Published
        } else if !plan.apply_remote.is_empty()
            || choices
                .iter()
                .any(|choice| choice.selected == super::model::LocalOrRemote::Remote)
        {
            SyncOutcome::Applied
        } else {
            SyncOutcome::UpToDate
        })
    }

    fn detect_legacy_history(&mut self) -> Result<bool, SyncError> {
        if self.store.migration_completed()? {
            return Ok(false);
        }
        if read_migration_journal(&self.root, &self.state_path)?.is_some() {
            return Ok(true);
        }
        match self.store.list_commits() {
            Ok(commits) => Ok(!commits.is_empty()),
            Err(error) => Err(error),
        }
    }

    fn migrate_legacy(
        &mut self,
        legacy: &SyncState,
        previous: Option<&[u8]>,
        resolution: Option<(&SyncPlan, &[ConflictChoice])>,
        allow_offline: bool,
        progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncOutcome, SyncError> {
        let journal = read_migration_journal(&self.root, &self.state_path)?;
        let resuming = journal.is_some();
        let mut objects = BTreeMap::<[u8; 32], Vec<u8>>::new();
        let mut journal = if let Some(journal) = journal {
            if journal.previous.as_deref() != previous || journal.device_id != legacy.device_id {
                return Err(SyncError::Integrity(
                    "migration journal does not match checkpoint".into(),
                ));
            }
            journal
        } else {
            progress(SyncProgress::Scanning { files: 0, bytes: 0 });
            let local = super::scan::scan_roots(&self.root)?;
            progress(SyncProgress::CheckingRemote);
            let commits = match self.store.list_commits() {
                Err(SyncError::Authentication(_)) if allow_offline => {
                    return Ok(SyncOutcome::Offline)
                }
                result => result?,
            };
            let tips = resolve_remote_tips(&commits)?;
            let saved =
                validate_baseline_present(legacy.last_applied_commit, &legacy.baseline, &commits)?;
            // Verify every tip, including versions the user will not select.
            let references: BTreeSet<_> = tips
                .iter()
                .flat_map(|tip| tip.snapshot.values())
                .chain(saved.values())
                .filter_map(|entry| match entry {
                    SnapshotEntry::File { sha256, size, .. } => Some((*sha256, *size)),
                    SnapshotEntry::Tombstone => None,
                })
                .collect();
            for (hash, size) in references {
                let bytes = match objects.get(&hash) {
                    Some(bytes) => bytes.clone(),
                    None => self.store.read_object(&hash)?.ok_or_else(|| {
                        SyncError::Integrity(format!("missing legacy object {}", hex_hash(&hash)))
                    })?,
                };
                verify_object(&bytes, &hash, size)?;
                objects.insert(hash, bytes);
            }
            let empty = BTreeMap::new();
            let local_base = if legacy.last_applied_commit.is_some() {
                &saved
            } else {
                &empty
            };
            let legacy_plan =
                match select_planning_base(legacy.last_applied_commit, &saved, &commits, &tips)? {
                    PlanningBase::Snapshot(base) => {
                        plan_sync_with_local_baseline(&base, local_base, &local, &tips)?
                    }
                    PlanningBase::Unrelated => {
                        plan_sync_without_common_ancestor(local_base, &local, &tips)?
                    }
                    PlanningBase::Ambiguous => conservative_conflict_plan(&local, &tips)?,
                };
            let plan = migration_resolver_plan(&legacy_plan);
            if let Some((displayed, _)) = resolution {
                if displayed != &plan || plan.conflicts.is_empty() {
                    return Err(SyncError::UnresolvedConflicts(
                        "legacy history changed while choosing conflicts".into(),
                    ));
                }
            } else if !plan.conflicts.is_empty() {
                return Ok(SyncOutcome::Conflicts(plan));
            }
            let choices = resolution.map_or(&[][..], |(_, choices)| choices);
            plan.resolved_snapshot(choices)?;
            let legacy_choices = choices
                .iter()
                .map(|choice| {
                    let id = match &choice.remote_version_id {
                        Some(RemoteVersionId::LegacyCommit(id)) => Some(*id),
                        None => None,
                        _ => {
                            return Err(SyncError::UnresolvedConflicts(
                                "not a legacy commit choice".into(),
                            ))
                        }
                    };
                    Ok(LegacyConflictChoice {
                        path: choice.path.clone(),
                        selected: choice.selected,
                        remote_commit_id: id,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let selected = legacy_plan.resolved_snapshot(&legacy_choices)?;
            let local_selected: BTreeSet<_> = legacy_plan
                .publish_local
                .keys()
                .cloned()
                .chain(
                    choices
                        .iter()
                        .filter(|choice| choice.selected == super::model::LocalOrRemote::Local)
                        .map(|choice| choice.path.clone()),
                )
                .collect();
            super::model::validate_entries(&selected).map_err(SyncError::Integrity)?;
            let journal = MigrationJournal {
                phase: MigrationPhase::Prepared,
                device_id: legacy.device_id,
                previous: previous.map(<[u8]>::to_vec),
                selected,
                local_selected,
                verified: BTreeMap::new(),
                replace_remote: BTreeMap::new(),
                replace_remote_sha256: BTreeMap::new(),
            };
            write_migration_journal(&self.root, &self.state_path, &journal)?;
            journal
        };

        // A token precedes inventory; a second inventory verifies the whole
        // tree, while Changes catches writes racing either inventory.
        let start = match self.store.start_page_token() {
            Err(SyncError::Authentication(_)) if allow_offline => return Ok(SyncOutcome::Offline),
            result => result?,
        };
        let inventory = self.store.initial_inventory()?;
        let mut by_path = BTreeMap::new();
        for file in inventory {
            if by_path.insert(file.path.clone(), file).is_some() {
                return Err(SyncError::Integrity("duplicate migration file path".into()));
            }
        }
        let selected_file_count = journal
            .selected
            .values()
            .filter(|entry| matches!(entry, SnapshotEntry::File { .. }))
            .count();
        log!(
            "Google Drive migration inventory: selected_files={}, actual_files={}",
            selected_file_count,
            by_path.len()
        );
        validate_remote_paths(&by_path)?;
        let local_before_upload = super::scan::scan_roots(&self.root)?;
        let plan = migration_resume_plan(&journal, &local_before_upload, &by_path);
        for conflict in &plan.conflicts {
            let path = &conflict.path;
            let actual = by_path.get(path);
            let expected_revision = journal
                .verified
                .get(path)
                .map(|(id, version)| format!("{id}@{version}"))
                .unwrap_or_else(|| "none".to_owned());
            let actual_revision = actual
                .map(|file| format!("{}@{}", file.id, file.version))
                .unwrap_or_else(|| "missing".to_owned());
            let expected_entry = journal.selected.get(path);
            let remote_content_matches = actual
                .zip(expected_entry)
                .is_some_and(|(file, entry)| same_entry(entry, &file.entry));
            let local_matches = same_optional_entry(local_before_upload.get(path), expected_entry);
            log!(
                "Google Drive migration conflict path={} verified_revision={expected_revision} current_revision={actual_revision} remote_content_matches_selected={} local_matches_selected={}",
                path.as_str(),
                remote_content_matches,
                local_matches
            );
        }
        if !plan.conflicts.is_empty() {
            let Some((displayed, choices)) = resolution.filter(|_| resuming) else {
                return Ok(SyncOutcome::Conflicts(plan));
            };
            if !displayed.has_same_content_as(&plan) {
                return Err(SyncError::UnresolvedConflicts(
                    "migration changed while choosing conflicts; reopen the resolver".into(),
                ));
            }
            let choices = rebind_conflict_choices(displayed, &plan, choices)?;
            let selected = plan.resolved_snapshot(&choices)?;
            let latest = self.store.initial_inventory()?;
            let latest_by_path: BTreeMap<_, _> = latest
                .into_iter()
                .map(|file| (file.path.clone(), file))
                .collect();
            if !same_inventory_content(&latest_by_path, &by_path) {
                return Err(SyncError::UnresolvedConflicts(
                    "Drive files changed while choosing migration conflicts".into(),
                ));
            }
            by_path = latest_by_path;
            for choice in &choices {
                let path = &choice.path;
                match choice.selected {
                    super::model::LocalOrRemote::Local => {
                        journal.local_selected.insert(path.clone());
                        journal.verified.remove(path);
                        if let Some(file) = by_path.get(path) {
                            journal
                                .replace_remote
                                .insert(path.clone(), (file.id.clone(), file.version.clone()));
                            if let Some(sha256) = entry_sha256(&file.entry) {
                                journal.replace_remote_sha256.insert(path.clone(), *sha256);
                            }
                        } else {
                            journal.replace_remote.remove(path);
                            journal.replace_remote_sha256.remove(path);
                        }
                    }
                    super::model::LocalOrRemote::Remote => {
                        journal.local_selected.remove(path);
                        journal.replace_remote.remove(path);
                        journal.replace_remote_sha256.remove(path);
                        if let Some(file) = by_path.get(path) {
                            journal
                                .verified
                                .insert(path.clone(), (file.id.clone(), file.version.clone()));
                        } else {
                            journal.verified.remove(path);
                        }
                    }
                }
            }
            journal.selected = selected;
            write_migration_journal(&self.root, &self.state_path, &journal)?;
        }
        let mut deleted_ids = BTreeSet::new();
        for (path, entry) in &journal.selected {
            if !matches!(entry, SnapshotEntry::Tombstone) {
                continue;
            }
            if let Some(file) = by_path.get(path) {
                if !migration_remote_matches(&journal, path, file)
                    || !self
                        .store
                        .file_metadata(&file.id)?
                        .is_some_and(|current| same_remote_content(file, &current))
                {
                    return Err(SyncError::UnresolvedConflicts(
                        "Drive file changed before migration deletion".into(),
                    ));
                }
                self.store.delete_file(&file.id)?;
                if self.store.file_metadata(&file.id)?.is_some() {
                    return Err(SyncError::Integrity(
                        "deleted migration file is still present".into(),
                    ));
                }
                deleted_ids.insert(file.id.clone());
                journal.replace_remote.remove(path);
                journal.replace_remote_sha256.remove(path);
                write_migration_journal(&self.root, &self.state_path, &journal)?;
            }
        }
        by_path.retain(|_, file| !deleted_ids.contains(&file.id));
        let files_total = journal
            .selected
            .values()
            .filter(|e| matches!(e, SnapshotEntry::File { .. }))
            .count();
        let files: Vec<_> = journal
            .selected
            .iter()
            .filter(|(_, entry)| matches!(entry, SnapshotEntry::File { .. }))
            .collect();
        for (index, (path, entry)) in files.into_iter().enumerate() {
            let SnapshotEntry::File { sha256, size, .. } = entry else {
                unreachable!("migration file iterator only contains files");
            };
            progress(SyncProgress::Uploading {
                files_done: index,
                files_total,
                bytes_done: 0,
                bytes_total: 0,
            });
            if let Some(file) = by_path.get(path) {
                if journal.replace_remote.contains_key(path) {
                    if !migration_remote_matches(&journal, path, file)
                        || !self
                            .store
                            .file_metadata(&file.id)?
                            .is_some_and(|current| same_remote_content(file, &current))
                    {
                        return Err(SyncError::UnresolvedConflicts(
                            "Drive file changed before migration overwrite".into(),
                        ));
                    }
                    let bytes = read_local(&self.root, path)?;
                    verify_object(&bytes, sha256, *size)?;
                    let updated = self.store.write_file(path, Some(&file.id), &bytes)?;
                    let readback = read_file_for_verification(&mut self.store, &updated.id, path)?;
                    log!(
                        "Google Drive migration local overwrite revision path={} before={}@{} returned={}@{} readback={}",
                        path.as_str(),
                        file.id,
                        file.version,
                        updated.id,
                        updated.version,
                        readback
                            .as_ref()
                            .map(|file| format!("{}@{}", file.id, file.version))
                            .unwrap_or_else(|| "missing".to_owned())
                    );
                    if updated.path != *path || !same_entry(entry, &updated.entry) {
                        return Err(SyncError::Integrity(
                            "migration overwrite verification failed".into(),
                        ));
                    }
                    let Some(verified) = readback.filter(|readback| {
                        readback.path == *path
                            && !readback.id.is_empty()
                            && !readback.version.is_empty()
                            && same_entry(entry, &readback.entry)
                    }) else {
                        if let Err(error) = self
                            .store
                            .diagnose_changes_since(&start, std::slice::from_ref(&updated.id))
                        {
                            log!(
                                "Google Drive diagnostic change lookup failed: {}",
                                super::status::redacted_error(&error)
                            );
                        }
                        return Err(SyncError::UnresolvedConflicts(
                            "Drive file changed after migration overwrite".into(),
                        ));
                    };
                    journal.verified.insert(
                        path.clone(),
                        (verified.id.clone(), verified.version.clone()),
                    );
                    journal.replace_remote.remove(path);
                    journal.replace_remote_sha256.remove(path);
                    by_path.insert(path.clone(), verified);
                    write_migration_journal(&self.root, &self.state_path, &journal)?;
                    continue;
                }
                let metadata = self.store.file_metadata(&file.id)?.ok_or_else(|| {
                    SyncError::UnresolvedConflicts("migration file disappeared".into())
                })?;
                if !same_remote_content(file, &metadata) || !same_entry(entry, &file.entry) {
                    return Err(SyncError::UnresolvedConflicts(
                        "migration file changed before verification".into(),
                    ));
                }
                journal
                    .verified
                    .insert(path.clone(), (file.id.clone(), file.version.clone()));
                write_migration_journal(&self.root, &self.state_path, &journal)?;
                continue;
            }
            let bytes = if journal.local_selected.contains(path) {
                read_local(&self.root, path)?
            } else if let Some(bytes) = objects.get(sha256) {
                bytes.clone()
            } else {
                self.store.read_object(sha256)?.ok_or_else(|| {
                    SyncError::Integrity(format!("missing legacy object {}", hex_hash(sha256)))
                })?
            };
            verify_object(&bytes, sha256, *size)?;
            let file = self.store.write_file(path, None, &bytes)?;
            if file.path != *path
                || file.id.is_empty()
                || file.version.is_empty()
                || !same_entry(entry, &file.entry)
                || !read_file_for_verification(&mut self.store, &file.id, path)?
                    .is_some_and(|current| same_remote_content(&file, &current))
            {
                return Err(SyncError::Integrity(
                    "migration upload verification failed".into(),
                ));
            }
            journal
                .verified
                .insert(path.clone(), (file.id.clone(), file.version.clone()));
            write_migration_journal(&self.root, &self.state_path, &journal)?;
        }
        journal.phase = MigrationPhase::Uploaded;
        write_migration_journal(&self.root, &self.state_path, &journal)?;
        progress(SyncProgress::CheckingRemote);
        let final_inventory = self.store.initial_inventory()?;
        let live_count = journal
            .selected
            .values()
            .filter(|entry| matches!(entry, SnapshotEntry::File { .. }))
            .count();
        if final_inventory.len() != live_count {
            log!(
                "Google Drive migration checkpoint rejected: selected_files={}, final_inventory_files={}",
                live_count,
                final_inventory.len()
            );
            return Err(SyncError::UnresolvedConflicts(
                "migration tree changed before checkpoint".into(),
            ));
        }
        let mut verified_revision_advanced = false;
        for file in &final_inventory {
            let expected_revision = journal.verified.get(&file.path);
            if expected_revision != Some(&(file.id.clone(), file.version.clone())) {
                let selected_content_matches = journal
                    .selected
                    .get(&file.path)
                    .is_some_and(|entry| same_entry(entry, &file.entry));
                if selected_content_matches {
                    log!(
                        "Google Drive migration checkpoint accepted same-content revision advance path={} from={:?} to={}@{}",
                        file.path.as_str(),
                        expected_revision,
                        file.id,
                        file.version
                    );
                    journal
                        .verified
                        .insert(file.path.clone(), (file.id.clone(), file.version.clone()));
                    verified_revision_advanced = true;
                } else {
                    log!(
                        "Google Drive migration checkpoint revision mismatch path={} expected={:?} actual={}@{}",
                        file.path.as_str(),
                        expected_revision,
                        file.id,
                        file.version
                    );
                    if let Err(error) = self
                        .store
                        .diagnose_changes_since(&start, std::slice::from_ref(&file.id))
                    {
                        log!(
                            "Google Drive diagnostic change lookup failed: {}",
                            super::status::redacted_error(&error)
                        );
                    }
                    return Err(SyncError::UnresolvedConflicts(
                        "Drive file revision changed before migration checkpoint".into(),
                    ));
                }
            }
            if !journal
                .selected
                .get(&file.path)
                .is_some_and(|entry| same_entry(entry, &file.entry))
            {
                log!(
                    "Google Drive migration checkpoint content mismatch path={} id={} version={}",
                    file.path.as_str(),
                    file.id,
                    file.version
                );
                return Err(SyncError::UnresolvedConflicts(
                    "Drive file content changed before migration checkpoint".into(),
                ));
            }
            if !read_file_for_verification(&mut self.store, &file.id, &file.path)?
                .is_some_and(|current| same_remote_content(file, &current))
            {
                log!(
                    "Google Drive migration checkpoint metadata changed during verification path={} id={} version={}",
                    file.path.as_str(),
                    file.id,
                    file.version
                );
                return Err(SyncError::UnresolvedConflicts(
                    "Drive file changed during migration checkpoint verification".into(),
                ));
            }
        }
        if verified_revision_advanced {
            write_migration_journal(&self.root, &self.state_path, &journal)?;
        }
        let batch = self.store.changes_since(&start)?;
        // Ignore revision churn when the complete final tree still has the
        // selected content at every managed path.
        for change in &batch.changes {
            let change_matches_final = change.file.as_ref().is_some_and(|changed| {
                changed.id == change.file_id
                    && final_inventory
                        .iter()
                        .any(|file| same_remote_content(changed, file))
            });
            let replaced_id_matches_final =
                change.file.is_none()
                    && journal.verified.iter().chain(&journal.replace_remote).any(
                        |(path, (id, _))| {
                            id == &change.file_id
                                && final_inventory.iter().any(|file| {
                                    file.path == *path
                                        && journal
                                            .selected
                                            .get(path)
                                            .is_some_and(|entry| same_entry(entry, &file.entry))
                                })
                        },
                    );
            if !change_matches_final
                && !(change.file.is_none() && deleted_ids.contains(&change.file_id))
                && !replaced_id_matches_final
            {
                let actual = change
                    .file
                    .as_ref()
                    .map(|file| format!("{}@{}", file.path.as_str(), file.version))
                    .unwrap_or_else(|| "deleted".to_owned());
                log!(
                    "Google Drive migration checkpoint change-feed mismatch id={} actual={actual}",
                    change.file_id
                );
                return Err(SyncError::UnresolvedConflicts(
                    "migration tree changed during inventory".into(),
                ));
            }
        }
        // A new installation must not replay preserved legacy commits once
        // the complete live tree has been verified.
        self.store.mark_migration_completed()?;
        let local = super::scan::scan_roots(&self.root)?;
        let apply: BTreeMap<_, _> = journal
            .selected
            .iter()
            .filter(|(path, entry)| {
                !same_optional_entry(local.get(*path), Some(entry))
                    && !journal.local_selected.contains(*path)
            })
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
        let staged = stage_migration_files(&self.root, &apply, |hash| {
            if let Some(bytes) = objects.get(hash) {
                return Ok(bytes.clone());
            }
            if let Some(file) = final_inventory.iter().find(
                |file| matches!(file.entry, SnapshotEntry::File { sha256, .. } if sha256 == *hash),
            ) {
                return self.store.read_file(&file.id)?.ok_or_else(|| {
                    SyncError::UnresolvedConflicts("selected Drive file disappeared".into())
                });
            }
            self.store.read_object(hash)?.ok_or_else(|| {
                SyncError::Integrity(format!("missing legacy object {}", hex_hash(hash)))
            })
        })?;
        let mut next = CurrentSyncState::new(legacy.device_id);
        next.migration_marker_confirmed = true;
        let (account_id, root_folder_id) = self.store.identity()?;
        next.account_id = Some(account_id);
        next.root_folder_id = Some(root_folder_id);
        next.changes_cursor = Some(batch.next_page_token);
        for (path, entry) in &journal.selected {
            let file = final_inventory.iter().find(|file| &file.path == path);
            next.baseline.insert(
                path.clone(),
                FileBaseline {
                    sha256: match entry {
                        SnapshotEntry::File { sha256, .. } => Some(*sha256),
                        _ => None,
                    },
                    remote_id: file.map(|file| file.id.clone()),
                    remote_version: file.map(|file| file.version.clone()),
                },
            );
            if let Some(file) = file {
                next.file_paths_by_id.insert(file.id.clone(), path.clone());
            }
        }
        if let Some(folders) = self.store.checkpoint_folder_paths_by_id()? {
            next.folder_paths_by_id = folders;
        }
        next.prune_deleted_paths(
            &local,
            &final_inventory
                .iter()
                .map(|file| (file.path.clone(), file.clone()))
                .collect(),
        );
        let checkpoint = serde_json::to_vec(&next)?;
        progress(SyncProgress::Applying {
            files_total: staged.len(),
        });
        let root = self.root.clone();
        let state_path = self.state_path.clone();
        apply_staged_transaction(
            &root,
            staged,
            previous.map(<[u8]>::to_vec),
            &checkpoint,
            || save_current_state(&root, &state_path, &next),
            |bytes| restore_state_bytes(&root, &state_path, bytes),
        )?;
        let saved = read_state_bytes(&self.root, &self.state_path)?;
        if !matches!(load_sync_state(saved.as_deref())?, LoadedSyncState::Current(state) if state == next)
        {
            return Err(SyncError::Integrity(
                "migration checkpoint readback failed".into(),
            ));
        }
        progress(SyncProgress::Finalizing);
        remove_migration_journal(&self.root, &self.state_path)?;
        Ok(SyncOutcome::Published)
    }
}

fn same_entry(left: &SnapshotEntry, right: &SnapshotEntry) -> bool {
    match (left, right) {
        (SnapshotEntry::File { sha256: a, .. }, SnapshotEntry::File { sha256: b, .. }) => a == b,
        (SnapshotEntry::Tombstone, SnapshotEntry::Tombstone) => true,
        _ => false,
    }
}

fn entry_sha256(entry: &SnapshotEntry) -> Option<&[u8; 32]> {
    match entry {
        SnapshotEntry::File { sha256, .. } => Some(sha256),
        SnapshotEntry::Tombstone => None,
    }
}

fn same_file_content(path: &RelativePath, sha256: &[u8; 32], file: &RemoteFile) -> bool {
    file.path == *path && entry_sha256(&file.entry) == Some(sha256)
}

fn same_remote_content(left: &RemoteFile, right: &RemoteFile) -> bool {
    left.path == right.path && same_entry(&left.entry, &right.entry)
}

fn same_inventory_content(
    left: &BTreeMap<RelativePath, RemoteFile>,
    right: &BTreeMap<RelativePath, RemoteFile>,
) -> bool {
    left.len() == right.len()
        && left.iter().all(|(path, file)| {
            right
                .get(path)
                .is_some_and(|other| same_remote_content(file, other))
        })
}

fn migration_remote_matches(
    journal: &MigrationJournal,
    path: &RelativePath,
    file: &RemoteFile,
) -> bool {
    journal
        .replace_remote_sha256
        .get(path)
        .is_some_and(|sha256| entry_sha256(&file.entry) == Some(sha256))
        || (journal.replace_remote_sha256.get(path).is_none()
            && journal.replace_remote.get(path) == Some(&(file.id.clone(), file.version.clone())))
}

fn read_file_for_verification<S: RemoteStore>(
    store: &mut S,
    file_id: &str,
    path: &RelativePath,
) -> Result<Option<RemoteFile>, SyncError> {
    if let Some(file) = store.file_metadata(file_id)? {
        return Ok(Some(file));
    }

    let mut matches = store
        .initial_inventory()?
        .into_iter()
        .filter(|file| file.path == *path);
    let file = matches.next();
    if matches.next().is_some() {
        return Err(SyncError::Integrity(
            "multiple Drive files occupy the uploaded path".into(),
        ));
    }
    Ok(file)
}

fn same_optional_entry(left: Option<&SnapshotEntry>, right: Option<&SnapshotEntry>) -> bool {
    match (left, right) {
        (Some(a), Some(b)) => same_entry(a, b),
        (None, Some(SnapshotEntry::Tombstone)) | (None, None) => true,
        _ => false,
    }
}

fn migration_resolver_plan(plan: &LegacySyncPlan) -> SyncPlan {
    let candidate = |path: &RelativePath, entry: &SnapshotEntry| {
        let id = plan
            .remote_tips
            .iter()
            .find(|tip| {
                tip.snapshot
                    .get(path)
                    .is_some_and(|version| same_entry(version, entry))
            })
            .map(|tip| tip.commit_id)
            .unwrap_or(Uuid::nil());
        RemoteCandidate {
            id: RemoteVersionId::LegacyCommit(id),
            entry: Some(entry.clone()),
            file: None,
        }
    };
    SyncPlan {
        local_snapshot: plan.local_snapshot.clone(),
        publish_local: plan
            .publish_local
            .iter()
            .map(|(path, entry)| {
                (
                    path.clone(),
                    (!matches!(entry, SnapshotEntry::Tombstone)).then_some(entry.clone()),
                )
            })
            .collect(),
        apply_remote: plan
            .apply_remote
            .iter()
            .map(|(path, entry)| (path.clone(), candidate(path, entry)))
            .collect(),
        conflicts: plan
            .conflicts
            .iter()
            .map(|conflict| FileConflict {
                path: conflict.path.clone(),
                local: conflict.local.clone(),
                remote_candidates: conflict
                    .remote_candidates
                    .iter()
                    .flat_map(|version| {
                        version.commit_ids.iter().map(|id| RemoteCandidate {
                            id: RemoteVersionId::LegacyCommit(*id),
                            entry: version.entry.clone(),
                            file: None,
                        })
                    })
                    .collect(),
            })
            .collect(),
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
enum MigrationPhase {
    Prepared,
    Uploaded,
}

#[derive(Deserialize, Serialize)]
struct MigrationJournal {
    phase: MigrationPhase,
    device_id: Uuid,
    previous: Option<Vec<u8>>,
    selected: BTreeMap<RelativePath, SnapshotEntry>,
    local_selected: BTreeSet<RelativePath>,
    verified: BTreeMap<RelativePath, (String, String)>,
    #[serde(default)]
    replace_remote: BTreeMap<RelativePath, (String, String)>,
    #[serde(default)]
    replace_remote_sha256: BTreeMap<RelativePath, [u8; 32]>,
}

fn migration_resume_plan(
    journal: &MigrationJournal,
    local: &BTreeMap<RelativePath, SnapshotEntry>,
    remote: &BTreeMap<RelativePath, RemoteFile>,
) -> SyncPlan {
    let mut plan = SyncPlan {
        local_snapshot: journal.selected.clone(),
        ..SyncPlan::default()
    };
    for path in journal
        .selected
        .keys()
        .chain(journal.verified.keys())
        .chain(remote.keys())
        .chain(journal.local_selected.iter())
        .collect::<BTreeSet<_>>()
    {
        let file = remote.get(path);
        let expected = journal.selected.get(path);
        let remote_changed = match (expected, file) {
            (Some(SnapshotEntry::Tombstone), None) | (None, None) => false,
            (Some(expected), Some(file)) => !same_entry(expected, &file.entry),
            (Some(SnapshotEntry::File { .. }), None) => {
                journal.verified.contains_key(path) || journal.replace_remote.contains_key(path)
            }
            (None, Some(_)) => true,
        };
        let local_changed = journal.local_selected.contains(path)
            && !same_optional_entry(local.get(path), expected);
        let approved = file.is_some_and(|file| {
            journal
                .replace_remote_sha256
                .get(path)
                .is_some_and(|sha256| entry_sha256(&file.entry) == Some(sha256))
                || (journal.replace_remote_sha256.get(path).is_none()
                    && journal.replace_remote.get(path).is_some_and(|identity| {
                        (&file.id, &file.version) == (&identity.0, &identity.1)
                    }))
        });
        if local_changed || (remote_changed && !approved) {
            let candidate = file.map_or(
                RemoteCandidate {
                    id: RemoteVersionId::NoDriveFile,
                    entry: None,
                    file: None,
                },
                |file| RemoteCandidate {
                    id: RemoteVersionId::DriveFile(file.id.clone()),
                    entry: Some(file.entry.clone()),
                    file: Some(file.clone()),
                },
            );
            plan.conflicts.push(FileConflict {
                path: path.clone(),
                local: local.get(path).cloned(),
                remote_candidates: vec![candidate],
            });
        }
    }
    plan
}

fn read_migration_journal(root: &Path, path: &Path) -> Result<Option<MigrationJournal>, SyncError> {
    let Some(dir) = state_dir(root, path, false)? else {
        return Ok(None);
    };
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = match dir.open_with("migration.json", &options) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(SyncError::Integrity(
            "migration journal is not a file".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let journal: MigrationJournal = serde_json::from_slice(&bytes)?;
    super::model::validate_entries(&journal.selected).map_err(SyncError::Integrity)?;
    Ok(Some(journal))
}

fn write_migration_journal(
    root: &Path,
    path: &Path,
    journal: &MigrationJournal,
) -> Result<(), SyncError> {
    let dir = state_dir(root, path, true)?.unwrap();
    let name = format!(".migration-{}", Uuid::new_v4());
    let result = (|| {
        let mut file = dir.open_with(&name, OpenOptions::new().write(true).create_new(true))?;
        file.write_all(&serde_json::to_vec(journal)?)?;
        file.sync_all()?;
        drop(file);
        dir.rename(&name, &dir, "migration.json")?;
        sync_directory(&dir)?;
        Ok::<_, SyncError>(())
    })();
    let _ = dir.remove_file(&name);
    result
}

fn remove_migration_journal(root: &Path, path: &Path) -> Result<(), SyncError> {
    let Some(dir) = state_dir(root, path, false)? else {
        return Ok(());
    };
    match dir.remove_file("migration.json") {
        Ok(()) => sync_directory(&dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn report_integrity_stage<T>(
    stage: &'static str,
    result: Result<T, SyncError>,
) -> Result<T, SyncError> {
    if matches!(
        &result,
        Err(SyncError::Integrity(_) | SyncError::InvalidPath(_) | SyncError::Serialization(_))
    ) {
        log!("Google Drive sync validation failed during {stage}");
    }
    result
}

fn current_changes<S: RemoteStore>(
    engine: &mut SyncEngine<S>,
    cursor: &str,
    allow_offline_authentication: bool,
) -> Result<Option<RemoteChangeBatch>, SyncError> {
    match engine.store.changes_since(cursor) {
        Err(SyncError::Authentication(_)) if allow_offline_authentication => Ok(None),
        result => result.map(Some),
    }
}

fn preflight_remote_path<S: RemoteStore>(
    store: &mut S,
    path: &RelativePath,
    remote: &BTreeMap<RelativePath, RemoteFile>,
    state: &CurrentSyncState,
) -> Result<bool, SyncError> {
    let expected = remote.get(path);
    let id = expected.map(|file| file.id.as_str()).or_else(|| {
        state
            .baseline
            .get(path)
            .and_then(|base| base.remote_id.as_deref())
    });
    let current = match id {
        Some(id) => store.file_metadata(id)?,
        None => None,
    };
    Ok(same_remote_version(expected, current.as_ref()))
}

fn selected_remote_versions_match<S: RemoteStore>(
    store: &mut S,
    plan: &SyncPlan,
    choices: &[ConflictChoice],
) -> Result<bool, SyncError> {
    for candidate in
        plan.apply_remote
            .values()
            .chain(choices.iter().filter_map(|choice| {
                if choice.selected != super::model::LocalOrRemote::Remote {
                    return None;
                }
                plan.conflicts
                    .iter()
                    .find(|conflict| conflict.path == choice.path)
                    .and_then(|conflict| {
                        conflict.remote_candidates.iter().find(|candidate| {
                            Some(&candidate.id) == choice.remote_version_id.as_ref()
                        })
                    })
            }))
    {
        let RemoteVersionId::DriveFile(id) = &candidate.id else {
            return Err(SyncError::Integrity(
                "legacy candidate in normal sync".into(),
            ));
        };
        let current = store.file_metadata(id)?;
        if !same_remote_version(candidate.file.as_ref(), current.as_ref()) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_remote_paths(remote: &BTreeMap<RelativePath, RemoteFile>) -> Result<(), SyncError> {
    let entries = remote
        .iter()
        .map(|(path, file)| {
            if path != &file.path || file.id.is_empty() || file.version.is_empty() {
                return Err(SyncError::Integrity("invalid remote file identity".into()));
            }
            Ok((path.clone(), file.entry.clone()))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    super::model::validate_entries(&entries).map_err(SyncError::Integrity)
}

fn same_remote_version(expected: Option<&RemoteFile>, current: Option<&RemoteFile>) -> bool {
    match (expected, current) {
        (None, None) => true,
        (Some(expected), Some(current)) => {
            expected.path == current.path && same_entry(&expected.entry, &current.entry)
        }
        _ => false,
    }
}

// Until account setup persists a repository identity, a foreign root cannot
// be distinguished from a concurrent genesis root. Known baselines must match
// the content and paths of the exact commit that the local state names.
fn validate_baseline_present(
    last_applied: Option<Uuid>,
    saved_baseline: &BTreeMap<RelativePath, SnapshotEntry>,
    commits: &[Commit],
) -> Result<BTreeMap<RelativePath, SnapshotEntry>, SyncError> {
    let Some(baseline_id) = last_applied else {
        return Ok(saved_baseline.clone());
    };
    let by_id: BTreeMap<_, _> = commits.iter().map(|commit| (commit.id, commit)).collect();
    let Some(commit) = by_id.get(&baseline_id) else {
        log!(
            "Google Drive sync baseline mismatch: referenced commit is absent (remote commits={})",
            commits.len()
        );
        return Err(SyncError::Integrity(format!(
            "last applied commit is missing from remote history: {baseline_id}"
        )));
    };
    if &commit.entries != saved_baseline {
        let mut saved_only = 0;
        let mut metadata_only = 0;
        let mut content_changed = 0;
        for (path, saved_entry) in saved_baseline {
            match commit.entries.get(path) {
                None => saved_only += 1,
                Some(SnapshotEntry::File {
                    sha256: remote_hash,
                    size: remote_size,
                    modified_unix_ms: remote_modified,
                }) => match saved_entry {
                    SnapshotEntry::File {
                        sha256: saved_hash,
                        size: saved_size,
                        modified_unix_ms: saved_modified,
                    } if saved_hash == remote_hash && saved_size == remote_size => {
                        if saved_modified != remote_modified {
                            metadata_only += 1;
                        }
                    }
                    _ => content_changed += 1,
                },
                Some(SnapshotEntry::Tombstone) if saved_entry == &SnapshotEntry::Tombstone => {}
                Some(SnapshotEntry::Tombstone) => content_changed += 1,
            }
        }
        let remote_only = commit
            .entries
            .keys()
            .filter(|path| !saved_baseline.contains_key(*path))
            .count();
        log!(
            "Google Drive sync baseline mismatch: saved-only={saved_only}, remote-only={remote_only}, content-changed={content_changed}, metadata-only={metadata_only}"
        );
        if saved_only != 0 || remote_only != 0 || content_changed != 0 {
            return Err(SyncError::Integrity(format!(
                "saved baseline does not match its commit: {baseline_id}"
            )));
        }
        // Only a timestamp changed: compare against the authoritative commit
        // without persisting anything until the full sync succeeds.
        return Ok(commit.entries.clone());
    }
    Ok(saved_baseline.clone())
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
) -> Result<LegacySyncPlan, SyncError> {
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

fn read_state_bytes(root: &Path, path: &Path) -> Result<Option<Vec<u8>>, SyncError> {
    let Some(dir) = state_dir(root, path, false)? else {
        return Ok(None);
    };
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match dir.open_with("state.json", &options) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(SyncError::Integrity("state is not a regular file".into()));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

fn save_current_state(root: &Path, path: &Path, state: &CurrentSyncState) -> Result<(), SyncError> {
    restore_state_bytes(root, path, Some(&serde_json::to_vec(state)?))
}

fn report_state_io<T>(
    operation: &'static str,
    result: Result<T, SyncError>,
) -> Result<T, SyncError> {
    if let Err(SyncError::Io(error)) = &result {
        log!(
            "Google Drive local I/O failed during {operation} (kind={:?}, os_error={:?})",
            error.kind(),
            error.raw_os_error()
        );
    }
    result
}

fn restore_state_bytes(root: &Path, path: &Path, bytes: Option<&[u8]>) -> Result<(), SyncError> {
    let dir = state_dir(root, path, true)?.unwrap();
    if let Some(bytes) = bytes {
        let temporary = format!(".state-{}", Uuid::new_v4());
        let result = (|| {
            let mut file = report_state_io(
                "create sync checkpoint temporary file",
                dir.open_with(&temporary, OpenOptions::new().write(true).create_new(true))
                    .map_err(SyncError::from),
            )?;
            report_state_io(
                "write sync checkpoint",
                file.write_all(bytes).map_err(SyncError::from),
            )?;
            report_state_io(
                "sync sync checkpoint",
                file.sync_all().map_err(SyncError::from),
            )?;
            drop(file);
            report_state_io(
                "replace sync checkpoint",
                dir.rename(&temporary, &dir, "state.json")
                    .map_err(SyncError::from),
            )?;
            report_state_io("sync sync checkpoint directory", sync_directory(&dir))?;
            Ok::<_, SyncError>(())
        })();
        let _ = dir.remove_file(&temporary);
        result
    } else {
        match dir.remove_file("state.json") {
            Ok(()) => sync_directory(&dir),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
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

fn state_exists(root: &Path, path: &Path) -> Result<bool, SyncError> {
    let Some(dir) = state_dir(root, path, false)? else {
        return Ok(false);
    };
    match dir.symlink_metadata("state.json") {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
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
mod current_file_tests {
    use super::*;
    use crate::sync::model::LocalOrRemote;
    use crate::sync::reconcile::RemoteVersionId;
    use crate::sync::store::{MemoryRemoteStore, SharedRemoteStore, StoreOperation};
    use std::fs;

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Race {
        Inventory,
        Preflight,
        PostWrite,
        PostWriteSameContentVersion,
        PostWriteSameContentVersionAtFinalInventory,
        SameContentVersionAtFinalInventory,
        None,
    }

    struct RaceStore {
        memory: MemoryRemoteStore,
        race: Race,
    }

    #[derive(Default)]
    struct CurrentCalls {
        start_tokens: usize,
        inventories: usize,
        changes: usize,
        writes: usize,
    }

    struct AuthOnceHistoryStore {
        memory: MemoryRemoteStore,
        fail_history_once: bool,
        history_reads: usize,
        current_calls: CurrentCalls,
    }

    impl AuthOnceHistoryStore {
        fn new(memory: MemoryRemoteStore) -> Self {
            Self {
                memory,
                fail_history_once: true,
                history_reads: 0,
                current_calls: CurrentCalls::default(),
            }
        }
    }

    impl RemoteStore for AuthOnceHistoryStore {
        fn identity(&mut self) -> Result<(String, String), SyncError> {
            self.memory.identity()
        }
        fn migration_completed(&mut self) -> Result<bool, SyncError> {
            self.memory.migration_completed()
        }
        fn mark_migration_completed(&mut self) -> Result<(), SyncError> {
            self.memory.mark_migration_completed()
        }
        fn start_page_token(&mut self) -> Result<String, SyncError> {
            self.current_calls.start_tokens += 1;
            self.memory.start_page_token()
        }
        fn initial_inventory(&mut self) -> Result<Vec<RemoteFile>, SyncError> {
            self.current_calls.inventories += 1;
            self.memory.initial_inventory()
        }
        fn changes_since(&mut self, cursor: &str) -> Result<RemoteChangeBatch, SyncError> {
            self.current_calls.changes += 1;
            self.memory.changes_since(cursor)
        }
        fn file_metadata(&mut self, id: &str) -> Result<Option<RemoteFile>, SyncError> {
            self.memory.file_metadata(id)
        }
        fn read_file(&mut self, id: &str) -> Result<Option<Vec<u8>>, SyncError> {
            self.memory.read_file(id)
        }
        fn write_file(
            &mut self,
            path: &RelativePath,
            existing_id: Option<&str>,
            bytes: &[u8],
        ) -> Result<RemoteFile, SyncError> {
            self.current_calls.writes += 1;
            self.memory.write_file(path, existing_id, bytes)
        }
        fn delete_file(&mut self, id: &str) -> Result<(), SyncError> {
            self.memory.delete_file(id)
        }
        fn list_commits(&mut self) -> Result<Vec<Commit>, SyncError> {
            self.history_reads += 1;
            if std::mem::take(&mut self.fail_history_once) {
                return Err(SyncError::Authentication("expired credentials".into()));
            }
            self.memory.list_commits()
        }
        fn read_object(&mut self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError> {
            self.memory.read_object(hash)
        }
        fn write_object(&mut self, hash: [u8; 32], bytes: &[u8]) -> Result<(), SyncError> {
            self.memory.write_object(hash, bytes)
        }
        fn write_commit(&mut self, commit: &Commit) -> Result<(), SyncError> {
            self.memory.write_commit(commit)
        }
    }

    impl RemoteStore for RaceStore {
        fn identity(&mut self) -> Result<(String, String), SyncError> {
            self.memory.identity()
        }
        fn migration_completed(&mut self) -> Result<bool, SyncError> {
            self.memory.migration_completed()
        }
        fn mark_migration_completed(&mut self) -> Result<(), SyncError> {
            self.memory.mark_migration_completed()
        }
        fn start_page_token(&mut self) -> Result<String, SyncError> {
            self.memory.start_page_token()
        }
        fn initial_inventory(&mut self) -> Result<Vec<RemoteFile>, SyncError> {
            if self.race == Race::SameContentVersionAtFinalInventory {
                self.race = Race::None;
                let file = self
                    .memory
                    .initial_inventory()?
                    .into_iter()
                    .find(|file| file.path == Tree::path("save"))
                    .ok_or_else(|| SyncError::Integrity("test file missing".into()))?;
                let bytes = self
                    .memory
                    .read_file(&file.id)?
                    .ok_or_else(|| SyncError::Integrity("test file content missing".into()))?;
                self.memory.write_file(&file.path, Some(&file.id), &bytes)?;
                return self.memory.initial_inventory();
            }
            let inventory = self.memory.initial_inventory()?;
            if self.race == Race::Inventory {
                self.race = Race::None;
                self.memory
                    .write_file(&Tree::path("raced"), None, b"raced")?;
            }
            Ok(inventory)
        }
        fn changes_since(&mut self, cursor: &str) -> Result<RemoteChangeBatch, SyncError> {
            self.memory.changes_since(cursor)
        }
        fn file_metadata(&mut self, id: &str) -> Result<Option<RemoteFile>, SyncError> {
            if self.race == Race::Preflight {
                self.race = Race::None;
                self.memory
                    .write_file(&Tree::path("save"), Some(id), b"raced")?;
            }
            self.memory.file_metadata(id)
        }
        fn read_file(&mut self, id: &str) -> Result<Option<Vec<u8>>, SyncError> {
            self.memory.read_file(id)
        }
        fn write_file(
            &mut self,
            path: &RelativePath,
            existing_id: Option<&str>,
            bytes: &[u8],
        ) -> Result<RemoteFile, SyncError> {
            let result = self.memory.write_file(path, existing_id, bytes)?;
            match self.race {
                Race::PostWrite => {
                    self.race = Race::None;
                    self.memory.write_file(path, Some(&result.id), b"raced")?;
                }
                Race::PostWriteSameContentVersion => {
                    self.race = Race::None;
                    self.memory.write_file(path, Some(&result.id), bytes)?;
                }
                Race::PostWriteSameContentVersionAtFinalInventory => {
                    self.race = Race::SameContentVersionAtFinalInventory;
                }
                _ => {}
            }
            Ok(result)
        }
        fn delete_file(&mut self, id: &str) -> Result<(), SyncError> {
            self.memory.delete_file(id)
        }
        fn list_commits(&mut self) -> Result<Vec<Commit>, SyncError> {
            self.memory.list_commits()
        }
        fn read_object(&mut self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError> {
            self.memory.read_object(hash)
        }
        fn write_object(&mut self, hash: [u8; 32], bytes: &[u8]) -> Result<(), SyncError> {
            self.memory.write_object(hash, bytes)
        }
        fn write_commit(&mut self, commit: &Commit) -> Result<(), SyncError> {
            self.memory.write_commit(commit)
        }
    }

    struct Tree(PathBuf);

    impl Tree {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("touchhle-current-{}", Uuid::new_v4()));
            fs::create_dir_all(path.join("touchHLE_apps")).unwrap();
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join("touchHLE_apps").join(name)
        }

        fn path(name: &str) -> RelativePath {
            RelativePath::new(&format!("touchHLE_apps/{name}")).unwrap()
        }

        fn state_path(&self) -> PathBuf {
            self.0.join(SYNC_DIR).join("state.json")
        }

        fn state(&self) -> CurrentSyncState {
            let bytes = fs::read(self.state_path()).unwrap();
            let LoadedSyncState::Current(state) = load_sync_state(Some(&bytes)).unwrap() else {
                panic!("expected current checkpoint");
            };
            state
        }

        fn engine(&self, store: MemoryRemoteStore) -> SyncEngine<MemoryRemoteStore> {
            SyncEngine::new(store, self.0.clone(), self.state_path())
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn no_change_uses_changes_feed_without_content_or_history_reads() {
        let tree = Tree::new();
        fs::write(tree.file("one"), b"one").unwrap();
        let mut engine = tree.engine(MemoryRemoteStore::default());
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        engine.store().fail_next(StoreOperation::ListCommits);
        engine.store().fail_next(StoreOperation::ReadObject);
        let transfers = engine.store().content_transfer_count();
        let requests = engine.store().change_request_count();
        let cursor = tree.state().changes_cursor.unwrap();
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        assert_eq!(engine.store().content_transfer_count(), transfers);
        assert!(engine.store().change_request_count() > requests);
        assert_ne!(cursor, "");
    }

    #[test]
    fn ignores_os_metadata_files_without_losing_drive_copies_or_future_renames() {
        let tree = Tree::new();
        fs::create_dir_all(tree.0.join("touchHLE_sandbox")).unwrap();
        let cases = [
            ("touchHLE_apps/.DS_Store", "touchHLE_apps/.DS_Store"),
            ("touchHLE_apps/Thumbs.db", "touchHLE_apps/Thumbs.db"),
            (
                "touchHLE_sandbox/ehthumbs.db",
                "touchHLE_sandbox/ehthumbs.db",
            ),
            (
                "touchHLE_sandbox/desktop.ini",
                "touchHLE_sandbox/desktop.ini",
            ),
        ];
        let mut store = MemoryRemoteStore::default();
        let mut drive_ids = Vec::new();
        for (path, local_path) in cases {
            fs::write(tree.0.join(local_path), b"local metadata").unwrap();
            let file = store
                .write_file(&RelativePath::new(path).unwrap(), None, b"Drive metadata")
                .unwrap();
            drive_ids.push(file.id);
        }

        let mut engine = tree.engine(store);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        let inventory = engine.store().initial_inventory().unwrap();
        assert_eq!(inventory.len(), cases.len());
        for file in &inventory {
            assert_eq!(
                engine.store().read_file(&file.id).unwrap().unwrap(),
                b"Drive metadata"
            );
        }
        let checkpoint = tree.state();
        assert!(checkpoint.baseline.is_empty());
        assert_eq!(checkpoint.file_paths_by_id.len(), cases.len());

        let renamed = Tree::path("visible-save.dat");
        engine.store().rename_current_file(&drive_ids[0], renamed);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(
            fs::read(tree.file("visible-save.dat")).unwrap(),
            b"Drive metadata"
        );
    }

    #[test]
    fn ignored_checkpointed_metadata_is_not_deleted_from_drive() {
        let tree = Tree::new();
        fs::write(tree.file(".DS_Store"), b"local metadata").unwrap();
        let path = Tree::path(".DS_Store");
        let remote_bytes = b"Drive metadata";
        let mut state = CurrentSyncState::new(Uuid::new_v4());
        state.account_id = Some("test-account".into());
        state.root_folder_id = Some("test-root".into());
        state.changes_cursor = Some("0".into());
        state.baseline.insert(
            path.clone(),
            FileBaseline {
                sha256: Some(Sha256::digest(remote_bytes).into()),
                remote_id: Some("drive-ds-store".into()),
                remote_version: Some("1".into()),
            },
        );
        state
            .file_paths_by_id
            .insert("drive-ds-store".into(), path.clone());
        save_current_state(&tree.0, &tree.state_path(), &state).unwrap();

        let mut store = MemoryRemoteStore::default();
        store.seed_current_file(
            RemoteFile {
                path: path.clone(),
                id: "drive-ds-store".into(),
                version: "1".into(),
                entry: SnapshotEntry::File {
                    sha256: Sha256::digest(remote_bytes).into(),
                    size: remote_bytes.len() as u64,
                    modified_unix_ms: 0,
                },
                parent_id: None,
            },
            remote_bytes.to_vec(),
        );
        let mut engine = tree.engine(store);

        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        assert_eq!(
            engine.store().read_file("drive-ds-store").unwrap().unwrap(),
            remote_bytes
        );
        assert!(tree.state().baseline.is_empty());
    }

    #[test]
    fn occupied_unindexed_remote_path_is_replanned_as_conflict() {
        let tree = Tree::new();
        let path = Tree::path("save");
        let local_bytes = b"local save";
        let remote_bytes = b"cloud save";
        fs::write(tree.file("save"), local_bytes).unwrap();

        let remote = RemoteFile {
            path: path.clone(),
            id: "drive-save".into(),
            version: "7".into(),
            entry: SnapshotEntry::File {
                sha256: Sha256::digest(remote_bytes).into(),
                size: remote_bytes.len() as u64,
                modified_unix_ms: 0,
            },
            parent_id: None,
        };
        let mut store = MemoryRemoteStore::default();
        store.seed_current_file(remote, remote_bytes.to_vec());

        let mut state = CurrentSyncState::new(Uuid::new_v4());
        state.account_id = Some("test-account".into());
        state.root_folder_id = Some("test-root".into());
        state.changes_cursor = Some("0".into());
        state.migration_marker_confirmed = true;
        save_current_state(&tree.0, &tree.state_path(), &state).unwrap();

        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("an occupied remote path with different content must conflict");
        };

        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].path, path);
        assert!(plan.publish_local.is_empty());
        assert_eq!(fs::read(tree.file("save")).unwrap(), local_bytes);
        assert_eq!(
            engine.store().read_file("drive-save").unwrap(),
            Some(remote_bytes.to_vec())
        );
        assert_eq!(tree.state().changes_cursor.as_deref(), Some("0"));

        let choice = ConflictChoice {
            path,
            selected: LocalOrRemote::Local,
            remote_version_id: None,
        };
        assert!(matches!(
            engine.resolve_conflicts(&plan, &[choice]).unwrap(),
            SyncOutcome::Published
        ));
        let updated = engine.store().file_metadata("drive-save").unwrap().unwrap();
        assert_eq!(updated.version, "8");
        assert_eq!(
            engine.store().read_file("drive-save").unwrap(),
            Some(local_bytes.to_vec())
        );
    }

    #[test]
    fn initial_inventory_catches_changes_after_preinventory_token() {
        let tree = Tree::new();
        let mut engine = SyncEngine::new(
            RaceStore {
                memory: MemoryRemoteStore::default(),
                race: Race::Inventory,
            },
            tree.0.clone(),
            tree.state_path(),
        );
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(fs::read(tree.file("raced")).unwrap(), b"raced");
        assert_eq!(tree.state().changes_cursor.as_deref(), Some("1"));
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
    }

    #[test]
    fn persisted_checkpoint_indexes_are_restored_before_reading_changes() {
        let tree = Tree::new();
        let path = Tree::path("save");
        fs::write(tree.file("save"), b"saved bytes").unwrap();
        let device_id = Uuid::new_v4();
        let mut state = CurrentSyncState::new(device_id);
        state.account_id = Some("test-account".into());
        state.root_folder_id = Some("test-root".into());
        state.changes_cursor = Some("0".into());
        state.baseline.insert(
            path.clone(),
            FileBaseline {
                sha256: Some(Sha256::digest(b"saved bytes").into()),
                remote_id: Some("file-id".into()),
                remote_version: Some("7".into()),
            },
        );
        state
            .file_paths_by_id
            .insert("file-id".into(), path.clone());
        state
            .folder_paths_by_id
            .insert("folder-id".into(), "touchHLE/touchHLE_apps".into());
        save_current_state(&tree.0, &tree.state_path(), &state).unwrap();

        let mut engine = tree.engine(MemoryRemoteStore::default());
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        assert_eq!(
            engine.store().restored_checkpoint_indexes(),
            Some((&state.file_paths_by_id, &state.folder_paths_by_id))
        );
    }

    #[test]
    fn full_nested_folder_index_survives_initial_and_no_change_syncs() {
        let tree = Tree::new();
        let path = RelativePath::new("touchHLE_apps/Old/Sub/save").unwrap();
        let local_path = tree.0.join(path.as_str());
        fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        fs::write(&local_path, b"nested bytes").unwrap();

        let folders = BTreeMap::from([
            ("apps-folder".into(), "touchHLE/touchHLE_apps".into()),
            ("old-folder".into(), "touchHLE/touchHLE_apps/Old".into()),
            ("sub-folder".into(), "touchHLE/touchHLE_apps/Old/Sub".into()),
        ]);
        let mut store = MemoryRemoteStore::default();
        store.seed_current_file(
            RemoteFile {
                path: path.clone(),
                id: "nested-file".into(),
                version: "1".into(),
                entry: SnapshotEntry::File {
                    sha256: Sha256::digest(b"nested bytes").into(),
                    size: 12,
                    modified_unix_ms: 0,
                },
                parent_id: Some("sub-folder".into()),
            },
            b"nested bytes".to_vec(),
        );
        store.seed_folder_paths_by_id(folders.clone());
        let mut engine = tree.engine(store);

        engine.synchronize().unwrap();
        assert_eq!(tree.state().folder_paths_by_id, folders);

        engine.synchronize().unwrap();
        assert_eq!(tree.state().folder_paths_by_id, folders);
        assert_eq!(engine.store().content_transfer_count(), 0);
    }

    #[test]
    fn stale_preflight_replans_as_conflict_without_overwriting_remote() {
        let tree = Tree::new();
        fs::write(tree.file("save"), b"base").unwrap();
        let mut seed = tree.engine(MemoryRemoteStore::default());
        seed.synchronize().unwrap();
        let previous = fs::read(tree.state_path()).unwrap();
        fs::write(tree.file("save"), b"local").unwrap();
        let mut engine = SyncEngine::new(
            RaceStore {
                memory: seed.into_store(),
                race: Race::Preflight,
            },
            tree.0.clone(),
            tree.state_path(),
        );
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("stale remote must replan to a conflict");
        };
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"local");
        assert_eq!(fs::read(tree.state_path()).unwrap(), previous);
        let remote = engine.store().memory.initial_inventory().unwrap();
        assert_eq!(
            engine.store().read_file(&remote[0].id).unwrap().unwrap(),
            b"raced"
        );
    }

    #[test]
    fn postwrite_version_race_keeps_old_cursor_and_local_bytes() {
        let tree = Tree::new();
        fs::write(tree.file("save"), b"base").unwrap();
        let mut seed = tree.engine(MemoryRemoteStore::default());
        seed.synchronize().unwrap();
        let previous = fs::read(tree.state_path()).unwrap();
        fs::write(tree.file("save"), b"local").unwrap();
        let mut engine = SyncEngine::new(
            RaceStore {
                memory: seed.into_store(),
                race: Race::PostWrite,
            },
            tree.0.clone(),
            tree.state_path(),
        );
        assert!(matches!(engine.synchronize(), Err(SyncError::Integrity(_))));
        assert_eq!(fs::read(tree.state_path()).unwrap(), previous);
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"local");
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Conflicts(_)
        ));
    }

    #[test]
    fn postwrite_revision_advance_with_same_hash_is_accepted() {
        let tree = Tree::new();
        fs::write(tree.file("save"), b"base").unwrap();
        let mut seed = tree.engine(MemoryRemoteStore::default());
        seed.synchronize().unwrap();
        fs::write(tree.file("save"), b"local").unwrap();
        let mut engine = SyncEngine::new(
            RaceStore {
                memory: seed.into_store(),
                race: Race::PostWriteSameContentVersion,
            },
            tree.0.clone(),
            tree.state_path(),
        );

        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        let remote = engine.store().initial_inventory().unwrap().remove(0);
        assert_eq!(remote.version, "3");
        assert_eq!(
            engine.store().read_file(&remote.id).unwrap().unwrap(),
            b"local"
        );
        let LoadedSyncState::Current(state) =
            load_sync_state(Some(&fs::read(tree.state_path()).unwrap())).unwrap()
        else {
            panic!("successful sync must save a current checkpoint");
        };
        assert_eq!(
            state.baseline[&Tree::path("save")]
                .remote_version
                .as_deref(),
            Some(remote.version.as_str())
        );
    }

    #[test]
    fn one_sided_changes_transfer_only_selected_paths() {
        let tree = Tree::new();
        fs::write(tree.file("same"), b"same").unwrap();
        let mut engine = tree.engine(MemoryRemoteStore::default());
        engine.synchronize().unwrap();
        let before = engine.store().content_transfer_count();
        fs::write(tree.file("local"), b"local").unwrap();
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        assert_eq!(engine.store().content_transfer_count(), before + 1);

        let before = engine.store().content_transfer_count();
        engine
            .store()
            .write_file(&Tree::path("remote"), None, b"remote")
            .unwrap();
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(engine.store().content_transfer_count(), before + 2);
        assert_eq!(fs::read(tree.file("remote")).unwrap(), b"remote");
        assert_eq!(fs::read(tree.file("same")).unwrap(), b"same");
    }

    #[test]
    fn create_and_delete_on_both_sides_converge() {
        let tree = Tree::new();
        fs::write(tree.file("save"), b"initial").unwrap();
        let mut engine = tree.engine(MemoryRemoteStore::default());
        engine.synchronize().unwrap();
        let original = engine.store().initial_inventory().unwrap()[0].clone();
        fs::remove_file(tree.file("save")).unwrap();
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        assert_eq!(engine.store().file_metadata(&original.id).unwrap(), None);

        let created = engine
            .store()
            .write_file(&Tree::path("new"), None, b"remote")
            .unwrap();
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(fs::read(tree.file("new")).unwrap(), b"remote");
        engine.store().delete_file(&created.id).unwrap();
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert!(!tree.file("new").exists());
    }

    #[test]
    fn divergent_choice_keeps_only_selected_bytes_and_rejects_stale_plan() {
        let tree = Tree::new();
        fs::write(tree.file("save"), b"base").unwrap();
        let mut engine = tree.engine(MemoryRemoteStore::default());
        engine.synchronize().unwrap();
        let remote = engine.store().initial_inventory().unwrap()[0].clone();
        fs::write(tree.file("save"), b"local").unwrap();
        engine
            .store()
            .write_file(&Tree::path("save"), Some(&remote.id), b"remote")
            .unwrap();
        let old_checkpoint = fs::read(tree.state_path()).unwrap();
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("divergent versions must require a choice");
        };
        assert_eq!(plan.conflicts.len(), 1);
        assert!(matches!(
            &plan.conflicts[0].remote_candidates[0].id,
            RemoteVersionId::DriveFile(id) if id == &remote.id
        ));
        assert_eq!(fs::read(tree.state_path()).unwrap(), old_checkpoint);
        let choice = ConflictChoice {
            path: Tree::path("save"),
            selected: LocalOrRemote::Remote,
            remote_version_id: Some(RemoteVersionId::DriveFile(remote.id.clone())),
        };
        engine
            .store()
            .write_file(&Tree::path("save"), Some(&remote.id), b"newer")
            .unwrap();
        assert!(matches!(
            engine.resolve_conflicts(&plan, &[choice.clone()]),
            Err(SyncError::UnresolvedConflicts(_))
        ));
        assert_eq!(fs::read(tree.state_path()).unwrap(), old_checkpoint);
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"local");

        let SyncOutcome::Conflicts(current) = engine.synchronize().unwrap() else {
            panic!("newer remote edit still conflicts");
        };
        assert!(matches!(
            engine.resolve_conflicts(&current, &[choice]).unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"newer");
        assert!(!tree.0.join(SYNC_DIR).join("recovery").exists());
        assert_eq!(
            tree.state().baseline[&Tree::path("save")].sha256,
            Some(Sha256::digest(b"newer").into())
        );
    }

    #[test]
    fn transfer_and_publication_failures_keep_cursor_for_retry() {
        let tree = Tree::new();
        let mut engine = tree.engine(MemoryRemoteStore::default());
        engine.synchronize().unwrap();
        let remote = engine
            .store()
            .write_file(&Tree::path("remote"), None, b"remote")
            .unwrap();
        let old = fs::read(tree.state_path()).unwrap();
        engine.store().fail_next(StoreOperation::ReadFile);
        assert!(engine.synchronize().is_err());
        assert_eq!(fs::read(tree.state_path()).unwrap(), old);
        assert!(!tree.file("remote").exists());
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(fs::read(tree.file("remote")).unwrap(), b"remote");

        fs::write(tree.file("local"), b"local").unwrap();
        let old = fs::read(tree.state_path()).unwrap();
        engine.store().fail_next(StoreOperation::WriteFile);
        assert!(engine.synchronize().is_err());
        assert_eq!(fs::read(tree.state_path()).unwrap(), old);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        assert_eq!(
            engine
                .store()
                .file_metadata(&remote.id)
                .unwrap()
                .unwrap()
                .path,
            Tree::path("remote")
        );
    }

    fn legacy_tree(tree: &Tree, entries: &[(&str, &[u8])]) -> MemoryRemoteStore {
        let mut store = MemoryRemoteStore::default();
        let snapshot = entries
            .iter()
            .map(|(name, bytes)| {
                let hash: [u8; 32] = Sha256::digest(bytes).into();
                store.write_object(hash, bytes).unwrap();
                (
                    Tree::path(name),
                    SnapshotEntry::File {
                        sha256: hash,
                        size: bytes.len() as u64,
                        modified_unix_ms: 0,
                    },
                )
            })
            .collect();
        let commit = Commit {
            id: Uuid::new_v4(),
            device_id: Uuid::new_v4(),
            created_unix_ms: 0,
            parents: vec![],
            entries: snapshot,
        };
        store.write_commit(&commit).unwrap();
        save_state(
            &tree.0,
            &tree.state_path(),
            &SyncState {
                device_id: commit.device_id,
                last_applied_commit: Some(commit.id),
                baseline: commit.entries.clone(),
            },
        )
        .unwrap();
        store
    }

    #[test]
    fn legacy_migration_one_tip_verifies_and_preserves_history() {
        let tree = Tree::new();
        let store = legacy_tree(&tree, &[("one", b"one"), ("two", b"two")]);
        let mut legacy = load_state(&tree.0, &tree.state_path()).unwrap();
        legacy.last_applied_commit = None;
        legacy.baseline.clear();
        save_state(&tree.0, &tree.state_path(), &legacy).unwrap();
        let mut engine = tree.engine(store);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        assert_eq!(fs::read(tree.file("one")).unwrap(), b"one");
        assert_eq!(fs::read(tree.file("two")).unwrap(), b"two");
        assert_eq!(tree.state().baseline.len(), 2);
        assert_eq!(engine.store().list_commits().unwrap().len(), 1);
        assert_eq!(engine.store().object_count(), 2);
        engine.store().fail_next(StoreOperation::ListCommits);
        engine.store().fail_next(StoreOperation::ReadObject);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
    }

    #[test]
    fn legacy_migration_detects_history_after_fresh_device_id_checkpoint() {
        let tree = Tree::new();
        let store = legacy_tree(&tree, &[("save", b"old")]);
        let legacy = fs::read(tree.state_path()).unwrap();
        fs::remove_file(tree.state_path()).unwrap();
        let mut engine = tree.engine(store);
        let id = engine.persisted_device_id().unwrap();
        assert_ne!(legacy, fs::read(tree.state_path()).unwrap());
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied | SyncOutcome::Published
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"old");
        assert_eq!(tree.state().device_id, id);
        engine.store().fail_next(StoreOperation::ListCommits);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
    }

    #[test]
    fn new_device_uses_completed_live_tree_even_after_legacy_files_change() {
        let first = Tree::new();
        let store = legacy_tree(&first, &[("save", b"old")]);
        let mut legacy = load_state(&first.0, &first.state_path()).unwrap();
        legacy.last_applied_commit = None;
        legacy.baseline.clear();
        save_state(&first.0, &first.state_path(), &legacy).unwrap();
        let mut shared = SharedRemoteStore(std::sync::Arc::new(std::sync::Mutex::new(store)));
        let mut first_engine = SyncEngine::new(shared.clone(), first.0.clone(), first.state_path());
        first_engine.synchronize().unwrap();
        let file = shared.initial_inventory().unwrap().remove(0);
        shared
            .write_file(&Tree::path("save"), Some(&file.id), b"new")
            .unwrap();

        let second = Tree::new();
        let mut second_engine =
            SyncEngine::new(shared.clone(), second.0.clone(), second.state_path());
        assert!(matches!(
            second_engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(fs::read(second.file("save")).unwrap(), b"new");
        assert_eq!(
            shared.initial_inventory().unwrap()[0].entry,
            SnapshotEntry::File {
                sha256: Sha256::digest(b"new").into(),
                size: 3,
                modified_unix_ms: 0,
            }
        );
    }

    #[test]
    fn old_device_with_legacy_checkpoint_uses_completed_live_tree() {
        let first = Tree::new();
        let store = legacy_tree(&first, &[("save", b"old")]);
        fs::write(first.file("save"), b"old").unwrap();
        let mut shared = SharedRemoteStore(std::sync::Arc::new(std::sync::Mutex::new(store)));
        let legacy_checkpoint = fs::read(first.state_path()).unwrap();
        let mut first_engine = SyncEngine::new(shared.clone(), first.0.clone(), first.state_path());
        first_engine.synchronize().unwrap();
        let file = shared.initial_inventory().unwrap().remove(0);
        shared
            .write_file(&Tree::path("save"), Some(&file.id), b"new")
            .unwrap();
        let second = Tree::new();
        fs::write(second.file("save"), b"old").unwrap();
        fs::create_dir_all(second.state_path().parent().unwrap()).unwrap();
        fs::write(second.state_path(), legacy_checkpoint).unwrap();
        let mut second_engine = SyncEngine::new(shared, second.0.clone(), second.state_path());
        assert!(matches!(
            second_engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(fs::read(second.file("save")).unwrap(), b"new");
    }

    #[test]
    fn completed_remote_migration_resumes_from_legacy_checkpoint_with_journal() {
        let first = Tree::new();
        let store = legacy_tree(&first, &[("save", b"old")]);
        fs::write(first.file("save"), b"old").unwrap();
        let mut shared = SharedRemoteStore(std::sync::Arc::new(std::sync::Mutex::new(store)));
        let previous = fs::read(first.state_path()).unwrap();
        let mut engine = SyncEngine::new(shared.clone(), first.0.clone(), first.state_path());
        engine.synchronize().unwrap();
        let file = shared.initial_inventory().unwrap().remove(0);
        shared
            .write_file(&Tree::path("save"), Some(&file.id), b"new")
            .unwrap();

        let second = Tree::new();
        fs::write(second.file("save"), b"old").unwrap();
        fs::create_dir_all(second.state_path().parent().unwrap()).unwrap();
        fs::write(second.state_path(), &previous).unwrap();
        let legacy: SyncState = serde_json::from_slice(&previous).unwrap();
        write_migration_journal(
            &second.0,
            &second.state_path(),
            &MigrationJournal {
                phase: MigrationPhase::Uploaded,
                device_id: legacy.device_id,
                previous: Some(previous.clone()),
                selected: legacy.baseline.clone(),
                local_selected: BTreeSet::new(),
                verified: BTreeMap::new(),
                replace_remote: BTreeMap::new(),
                replace_remote_sha256: BTreeMap::new(),
            },
        )
        .unwrap();
        let mut second_engine = SyncEngine::new(shared, second.0.clone(), second.state_path());
        assert!(matches!(
            second_engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(fs::read(second.file("save")).unwrap(), b"new");
        assert!(!second.0.join(SYNC_DIR).join("migration.json").exists());
    }

    #[test]
    fn legacy_migration_resume_accepts_same_content_revision_change() {
        let tree = Tree::new();
        let mut store = legacy_tree(&tree, &[("save", b"base")]);
        fs::write(tree.file("save"), b"base").unwrap();
        let previous = fs::read(tree.state_path()).unwrap();
        let legacy: SyncState = serde_json::from_slice(&previous).unwrap();
        let path = Tree::path("save");
        let verified_file = store.write_file(&path, None, b"base").unwrap();
        write_migration_journal(
            &tree.0,
            &tree.state_path(),
            &MigrationJournal {
                phase: MigrationPhase::Uploaded,
                device_id: legacy.device_id,
                previous: Some(previous.clone()),
                selected: legacy.baseline,
                local_selected: BTreeSet::new(),
                verified: BTreeMap::from([(
                    path.clone(),
                    (verified_file.id.clone(), verified_file.version),
                )]),
                replace_remote: BTreeMap::new(),
                replace_remote_sha256: BTreeMap::new(),
            },
        )
        .unwrap();
        store
            .write_file(&path, Some(&verified_file.id), b"base")
            .unwrap();
        let mut engine = tree.engine(store);

        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        assert!(!tree.0.join(SYNC_DIR).join("migration.json").exists());
        let file = engine.store().initial_inventory().unwrap().remove(0);
        let LoadedSyncState::Current(state) =
            load_sync_state(Some(&fs::read(tree.state_path()).unwrap())).unwrap()
        else {
            panic!("migration must save a current checkpoint");
        };
        assert_eq!(
            state.baseline[&path].remote_version.as_deref(),
            Some(file.version.as_str())
        );
        assert_ne!(fs::read(tree.state_path()).unwrap(), previous);
    }

    fn changed_migration_fixture(
        local_bytes: Option<&[u8]>,
        cloud_bytes: Option<&[u8]>,
    ) -> (Tree, MemoryRemoteStore, RelativePath, Vec<u8>) {
        let tree = Tree::new();
        let mut store = legacy_tree(&tree, &[("save", b"base")]);
        if let Some(bytes) = local_bytes {
            fs::write(tree.file("save"), bytes).unwrap();
        }
        let previous = fs::read(tree.state_path()).unwrap();
        let legacy: SyncState = serde_json::from_slice(&previous).unwrap();
        let path = Tree::path("save");
        let original = store.write_file(&path, None, b"base").unwrap();
        write_migration_journal(
            &tree.0,
            &tree.state_path(),
            &MigrationJournal {
                phase: MigrationPhase::Uploaded,
                device_id: legacy.device_id,
                previous: Some(previous.clone()),
                selected: legacy.baseline,
                local_selected: BTreeSet::new(),
                verified: BTreeMap::from([(path.clone(), (original.id.clone(), original.version))]),
                replace_remote: BTreeMap::new(),
                replace_remote_sha256: BTreeMap::new(),
            },
        )
        .unwrap();
        if let Some(bytes) = cloud_bytes {
            store.write_file(&path, Some(&original.id), bytes).unwrap();
        } else {
            store.delete_file(&original.id).unwrap();
        }
        (tree, store, path, previous)
    }

    #[test]
    fn migration_resume_local_choice_uses_current_disk_file() {
        let (tree, store, path, _) =
            changed_migration_fixture(Some(b"current-local"), Some(b"new-cloud"));
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote edit must conflict");
        };
        assert!(matches!(
            plan.conflicts[0].local,
            Some(SnapshotEntry::File { sha256, .. })
                if sha256.as_slice() == Sha256::digest(b"current-local").as_slice()
        ));
        let choice = ConflictChoice {
            path: path.clone(),
            selected: LocalOrRemote::Local,
            remote_version_id: None,
        };
        assert!(matches!(
            engine.resolve_conflicts(&plan, &[choice]).unwrap(),
            SyncOutcome::Published
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"current-local");
        let file = engine.store().initial_inventory().unwrap().remove(0);
        assert_eq!(
            engine.store().read_file(&file.id).unwrap().unwrap(),
            b"current-local"
        );
        assert!(!tree.0.join(SYNC_DIR).join("migration.json").exists());
    }

    #[test]
    fn migration_resume_cloud_choice_applies_current_drive_content() {
        let (tree, store, path, _) =
            changed_migration_fixture(Some(b"current-local"), Some(b"new-cloud"));
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote edit must conflict");
        };
        let file = plan.conflicts[0].remote_candidates[0]
            .file
            .as_ref()
            .unwrap();
        assert_eq!(file.path, path);
        let choice = ConflictChoice {
            path,
            selected: LocalOrRemote::Remote,
            remote_version_id: Some(RemoteVersionId::DriveFile(file.id.clone())),
        };
        engine.resolve_conflicts(&plan, &[choice]).unwrap();
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"new-cloud");
        assert!(!tree.0.join(SYNC_DIR).join("migration.json").exists());
    }

    #[test]
    fn migration_resume_stale_choice_preserves_journal_and_files() {
        let (tree, store, path, previous) =
            changed_migration_fixture(Some(b"current-local"), Some(b"new-cloud"));
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote edit must conflict");
        };
        let before = fs::read(tree.0.join(SYNC_DIR).join("migration.json")).unwrap();
        let file = engine.store().initial_inventory().unwrap().remove(0);
        engine
            .store()
            .write_file(&path, Some(&file.id), b"even-newer-cloud")
            .unwrap();
        let choice = ConflictChoice {
            path,
            selected: LocalOrRemote::Local,
            remote_version_id: None,
        };
        assert!(matches!(
            engine.resolve_conflicts(&plan, &[choice]),
            Err(SyncError::UnresolvedConflicts(_))
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"current-local");
        assert_eq!(fs::read(tree.state_path()).unwrap(), previous);
        assert_eq!(
            fs::read(tree.0.join(SYNC_DIR).join("migration.json")).unwrap(),
            before
        );
    }

    #[test]
    fn migration_resume_requires_new_choice_if_drive_changes_after_failed_upload() {
        let (tree, store, path, _) =
            changed_migration_fixture(Some(b"current-local"), Some(b"new-cloud"));
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote edit must conflict");
        };
        engine.store().fail_next(StoreOperation::WriteFile);
        let choice = ConflictChoice {
            path: path.clone(),
            selected: LocalOrRemote::Local,
            remote_version_id: None,
        };
        assert!(engine.resolve_conflicts(&plan, &[choice]).is_err());
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"current-local");
        assert!(tree.0.join(SYNC_DIR).join("migration.json").exists());

        let file = engine.store().initial_inventory().unwrap().remove(0);
        engine
            .store()
            .write_file(&path, Some(&file.id), b"newer-cloud")
            .unwrap();
        let SyncOutcome::Conflicts(new_plan) = engine.synchronize().unwrap() else {
            panic!("Drive changes after approval require a new choice");
        };
        assert_eq!(new_plan.conflicts[0].path, path);
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"current-local");
    }

    #[test]
    fn migration_resume_retries_checkpoint_failure_after_local_upload_without_reasking() {
        let (tree, store, path, previous) =
            changed_migration_fixture(Some(b"current-local"), Some(b"new-cloud"));
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote edit must conflict");
        };
        engine.store().fail_next(StoreOperation::ChangesSince);
        let choice = ConflictChoice {
            path: path.clone(),
            selected: LocalOrRemote::Local,
            remote_version_id: None,
        };
        assert!(engine.resolve_conflicts(&plan, &[choice]).is_err());
        assert_eq!(fs::read(tree.state_path()).unwrap(), previous);
        let uploaded_file = engine.store().initial_inventory().unwrap().remove(0);
        assert_eq!(
            engine
                .store()
                .read_file(&uploaded_file.id)
                .unwrap()
                .unwrap(),
            b"current-local"
        );

        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        assert!(!tree.0.join(SYNC_DIR).join("migration.json").exists());
        let file = engine.store().initial_inventory().unwrap().remove(0);
        assert_eq!(
            engine.store().read_file(&file.id).unwrap().unwrap(),
            b"current-local"
        );
        let LoadedSyncState::Current(state) =
            load_sync_state(Some(&fs::read(tree.state_path()).unwrap())).unwrap()
        else {
            panic!("migration must save a current sync checkpoint");
        };
        assert_eq!(
            state.baseline[&path].remote_id.as_deref(),
            Some(file.id.as_str())
        );
        assert_eq!(
            state.baseline[&path].remote_version.as_deref(),
            Some(file.version.as_str())
        );
    }

    #[test]
    fn migration_resume_accepts_same_content_revision_change_after_local_upload() {
        let (tree, store, path, _) =
            changed_migration_fixture(Some(b"current-local"), Some(b"new-cloud"));
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote edit must conflict");
        };
        engine.store().fail_next(StoreOperation::ChangesSince);
        let choice = ConflictChoice {
            path: path.clone(),
            selected: LocalOrRemote::Local,
            remote_version_id: None,
        };
        assert!(engine.resolve_conflicts(&plan, &[choice]).is_err());

        let uploaded = engine.store().initial_inventory().unwrap().remove(0);
        engine
            .store()
            .write_file(&path, Some(&uploaded.id), b"current-local")
            .unwrap();
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        assert!(!tree.0.join(SYNC_DIR).join("migration.json").exists());
        let file = engine.store().initial_inventory().unwrap().remove(0);
        assert_eq!(
            engine.store().read_file(&file.id).unwrap().unwrap(),
            b"current-local"
        );
    }

    #[test]
    fn migration_resume_postwrite_race_returns_conflict_instead_of_integrity_failure() {
        let (tree, store, path, _) =
            changed_migration_fixture(Some(b"current-local"), Some(b"new-cloud"));
        let mut engine = SyncEngine::new(
            RaceStore {
                memory: store,
                race: Race::PostWrite,
            },
            tree.0.clone(),
            tree.state_path(),
        );
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote edit must require a choice");
        };
        let choice = ConflictChoice {
            path: path.clone(),
            selected: LocalOrRemote::Local,
            remote_version_id: None,
        };

        assert!(matches!(
            engine.resolve_conflicts(&plan, &[choice]),
            Err(SyncError::UnresolvedConflicts(_))
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"current-local");
        let SyncOutcome::Conflicts(refreshed) = engine.synchronize().unwrap() else {
            panic!("post-upload remote revision must be offered as a fresh conflict");
        };
        assert_eq!(refreshed.conflicts.len(), 1);
        assert_eq!(refreshed.conflicts[0].path, path);
    }

    #[test]
    fn migration_local_choice_accepts_its_verified_postwrite_version() {
        let (tree, store, path, _) =
            changed_migration_fixture(Some(b"current-local"), Some(b"new-cloud"));
        let mut engine = SyncEngine::new(
            RaceStore {
                memory: store,
                race: Race::PostWriteSameContentVersion,
            },
            tree.0.clone(),
            tree.state_path(),
        );
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote edit must require a choice");
        };
        let choice = ConflictChoice {
            path: path.clone(),
            selected: LocalOrRemote::Local,
            remote_version_id: None,
        };

        assert!(matches!(
            engine.resolve_conflicts(&plan, &[choice]),
            Ok(SyncOutcome::Published)
        ));
        assert!(!tree.0.join(SYNC_DIR).join("migration.json").exists());
        let file = engine.store().initial_inventory().unwrap().remove(0);
        assert_eq!(
            engine.store().read_file(&file.id).unwrap().unwrap(),
            b"current-local"
        );
        let LoadedSyncState::Current(state) =
            load_sync_state(Some(&fs::read(tree.state_path()).unwrap())).unwrap()
        else {
            panic!("successful migration must save a current checkpoint");
        };
        assert_eq!(
            state.baseline[&path].remote_version.as_deref(),
            Some(file.version.as_str())
        );
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        engine
            .store()
            .write_file(&path, Some(&file.id), b"current-local")
            .unwrap();
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
    }

    #[test]
    fn migration_local_choice_accepts_final_inventory_version_advance() {
        let (tree, store, path, _) =
            changed_migration_fixture(Some(b"current-local"), Some(b"new-cloud"));
        let mut engine = SyncEngine::new(
            RaceStore {
                memory: store,
                race: Race::PostWriteSameContentVersionAtFinalInventory,
            },
            tree.0.clone(),
            tree.state_path(),
        );
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote edit must require a choice");
        };
        let choice = ConflictChoice {
            path: path.clone(),
            selected: LocalOrRemote::Local,
            remote_version_id: None,
        };

        assert!(matches!(
            engine.resolve_conflicts(&plan, &[choice]),
            Ok(SyncOutcome::Published)
        ));
        let file = engine.store().initial_inventory().unwrap().remove(0);
        assert_eq!(file.version, "4");
        let LoadedSyncState::Current(state) =
            load_sync_state(Some(&fs::read(tree.state_path()).unwrap())).unwrap()
        else {
            panic!("successful migration must save a current checkpoint");
        };
        assert_eq!(
            state.baseline[&path].remote_version.as_deref(),
            Some(file.version.as_str())
        );
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
    }

    #[test]
    fn migration_resume_retries_approved_local_overwrite_after_failed_upload() {
        let (tree, store, path, _) =
            changed_migration_fixture(Some(b"current-local"), Some(b"new-cloud"));
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote edit must conflict");
        };
        engine.store().fail_next(StoreOperation::WriteFile);
        let choice = ConflictChoice {
            path,
            selected: LocalOrRemote::Local,
            remote_version_id: None,
        };
        assert!(engine.resolve_conflicts(&plan, &[choice]).is_err());
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"current-local");
        let file = engine.store().initial_inventory().unwrap().remove(0);
        assert_eq!(
            engine.store().read_file(&file.id).unwrap().unwrap(),
            b"current-local"
        );
    }

    #[test]
    fn migration_resume_remote_deletion_is_a_choice() {
        let (tree, store, path, _) = changed_migration_fixture(Some(b"base"), None);
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote deletion must conflict");
        };
        assert_eq!(
            plan.conflicts[0].remote_candidates[0].id,
            RemoteVersionId::NoDriveFile
        );
        engine
            .resolve_conflicts(
                &plan,
                &[ConflictChoice {
                    path,
                    selected: LocalOrRemote::Remote,
                    remote_version_id: Some(RemoteVersionId::NoDriveFile),
                }],
            )
            .unwrap();
        assert!(!tree.file("save").exists());
        assert!(engine.store().initial_inventory().unwrap().is_empty());
    }

    #[test]
    fn migration_resume_local_deletion_removes_drive_file() {
        let (tree, store, path, _) = changed_migration_fixture(None, Some(b"new-cloud"));
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("remote edit must conflict");
        };
        engine
            .resolve_conflicts(
                &plan,
                &[ConflictChoice {
                    path,
                    selected: LocalOrRemote::Local,
                    remote_version_id: None,
                }],
            )
            .unwrap();
        assert!(!tree.file("save").exists());
        assert!(engine.store().initial_inventory().unwrap().is_empty());
    }

    #[test]
    fn empty_device_checkpoint_ignores_stale_journal_after_remote_completion() {
        let first = Tree::new();
        let store = legacy_tree(&first, &[("save", b"old")]);
        let mut legacy = load_state(&first.0, &first.state_path()).unwrap();
        legacy.baseline.clear();
        legacy.last_applied_commit = None;
        save_state(&first.0, &first.state_path(), &legacy).unwrap();
        let mut shared = SharedRemoteStore(std::sync::Arc::new(std::sync::Mutex::new(store)));
        let mut first_engine = SyncEngine::new(shared.clone(), first.0.clone(), first.state_path());
        first_engine.synchronize().unwrap();
        let file = shared.initial_inventory().unwrap().remove(0);
        shared
            .write_file(&Tree::path("save"), Some(&file.id), b"new")
            .unwrap();

        let second = Tree::new();
        let mut second_engine = SyncEngine::new(shared, second.0.clone(), second.state_path());
        second_engine.persisted_device_id().unwrap();
        write_migration_journal(
            &second.0,
            &second.state_path(),
            &MigrationJournal {
                phase: MigrationPhase::Prepared,
                device_id: second.state().device_id,
                previous: Some(fs::read(second.state_path()).unwrap()),
                selected: BTreeMap::new(),
                local_selected: BTreeSet::new(),
                verified: BTreeMap::new(),
                replace_remote: BTreeMap::new(),
                replace_remote_sha256: BTreeMap::new(),
            },
        )
        .unwrap();
        assert!(matches!(
            second_engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(fs::read(second.file("save")).unwrap(), b"new");
        assert!(!second.0.join(SYNC_DIR).join("migration.json").exists());
    }

    #[test]
    fn already_migrated_device_marks_live_tree_for_new_devices_on_next_sync() {
        let first = Tree::new();
        let store = legacy_tree(&first, &[("save", b"old")]);
        let mut legacy = load_state(&first.0, &first.state_path()).unwrap();
        legacy.last_applied_commit = None;
        legacy.baseline.clear();
        save_state(&first.0, &first.state_path(), &legacy).unwrap();
        let mut first_engine = first.engine(store);
        first_engine.synchronize().unwrap();
        first_engine.store().clear_migration_marker();
        let file = first_engine.store().initial_inventory().unwrap().remove(0);
        first_engine
            .store()
            .write_file(&Tree::path("save"), Some(&file.id), b"new")
            .unwrap();

        let mut state = first.state();
        state.migration_marker_confirmed = false;
        save_current_state(&first.0, &first.state_path(), &state).unwrap();
        assert!(matches!(
            first_engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert!(first.state().migration_marker_confirmed);
        let second = Tree::new();
        let mut second_engine = SyncEngine::new(
            first_engine.into_store(),
            second.0.clone(),
            second.state_path(),
        );
        assert!(matches!(
            second_engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(fs::read(second.file("save")).unwrap(), b"new");
    }

    #[test]
    fn partial_legacy_tree_is_not_mistaken_for_completed_migration() {
        let first = Tree::new();
        let mut store = legacy_tree(&first, &[("one", b"one"), ("two", b"two")]);
        let mut legacy = load_state(&first.0, &first.state_path()).unwrap();
        legacy.last_applied_commit = None;
        legacy.baseline.clear();
        save_state(&first.0, &first.state_path(), &legacy).unwrap();
        store.fail_after(StoreOperation::WriteFile, 1);
        let mut first_engine = first.engine(store);
        assert!(first_engine.synchronize().is_err());
        let mut shared = SharedRemoteStore(std::sync::Arc::new(std::sync::Mutex::new(
            first_engine.into_store(),
        )));
        let file = shared.initial_inventory().unwrap().remove(0);
        shared
            .write_file(&file.path, Some(&file.id), b"not legacy")
            .unwrap();
        let second = Tree::new();
        let mut second_engine =
            SyncEngine::new(shared.clone(), second.0.clone(), second.state_path());
        let SyncOutcome::Conflicts(plan) = second_engine.synchronize().unwrap() else {
            panic!("changed partial migration file must be resolved explicitly");
        };
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(
            plan.conflicts[0].remote_candidates[0]
                .file
                .as_ref()
                .unwrap()
                .entry,
            shared.initial_inventory().unwrap()[0].entry
        );
        assert_eq!(shared.initial_inventory().unwrap().len(), 1);
        assert!(!second.file("one").exists());
    }

    #[test]
    fn switching_remote_identity_rebuilds_without_reusing_cursor_or_old_baseline() {
        for (account, root) in [
            ("another-account", "test-root"),
            ("test-account", "another-root"),
        ] {
            let tree = Tree::new();
            let mut first = tree.engine(MemoryRemoteStore::default());
            fs::write(tree.file("save"), b"old").unwrap();
            first.synchronize().unwrap();
            let state = tree.state();
            assert!(state.changes_cursor.is_some());
            let mut next = MemoryRemoteStore::default();
            next.set_identity(account, root);
            next.write_file(&Tree::path("save"), None, b"new").unwrap();
            let mut second = tree.engine(next);
            assert!(matches!(
                second.synchronize().unwrap(),
                SyncOutcome::Conflicts(_)
            ));
            assert_eq!(fs::read(tree.file("save")).unwrap(), b"old");
            assert_eq!(second.store().change_request_count(), 1);
            assert_eq!(tree.state(), state);
        }
    }

    #[test]
    fn legacy_history_auth_failure_keeps_missing_checkpoint_for_retry() {
        let tree = Tree::new();
        let memory = legacy_tree(&tree, &[("save", b"legacy")]);
        fs::remove_file(tree.state_path()).unwrap();
        let mut engine = SyncEngine::new(
            AuthOnceHistoryStore::new(memory),
            tree.0.clone(),
            tree.state_path(),
        );

        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Offline
        ));
        assert!(read_state_bytes(&tree.0, &tree.state_path())
            .unwrap()
            .is_none());
        assert!(!tree.0.join(SYNC_DIR).join("migration.json").exists());
        assert_eq!(engine.store().current_calls.start_tokens, 0);
        assert_eq!(engine.store().current_calls.inventories, 0);
        assert_eq!(engine.store().current_calls.changes, 0);
        assert_eq!(engine.store().current_calls.writes, 0);

        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied | SyncOutcome::Published
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"legacy");
        assert_eq!(engine.store().history_reads, 3);
        assert!(engine.store().current_calls.inventories > 0);
        assert!(engine.store().current_calls.writes > 0);
    }

    #[test]
    fn legacy_history_auth_failure_preserves_early_device_checkpoint() {
        let tree = Tree::new();
        let memory = legacy_tree(&tree, &[("save", b"legacy")]);
        fs::remove_file(tree.state_path()).unwrap();
        let mut engine = SyncEngine::new(
            AuthOnceHistoryStore::new(memory),
            tree.0.clone(),
            tree.state_path(),
        );
        let device_id = engine.persisted_device_id().unwrap();
        let previous = fs::read(tree.state_path()).unwrap();

        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Offline
        ));
        assert_eq!(fs::read(tree.state_path()).unwrap(), previous);
        assert_eq!(engine.store().current_calls.start_tokens, 0);
        assert_eq!(engine.store().current_calls.inventories, 0);
        assert_eq!(engine.store().current_calls.changes, 0);
        assert_eq!(engine.store().current_calls.writes, 0);

        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied | SyncOutcome::Published
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"legacy");
        assert_eq!(tree.state().device_id, device_id);
        assert!(engine.store().current_calls.inventories > 0);
        assert!(engine.store().current_calls.writes > 0);
        let history_reads = engine.store().history_reads;
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        assert_eq!(engine.store().history_reads, history_reads);
    }

    #[test]
    fn legacy_migration_divergent_tips_require_typed_choices_before_upload() {
        let tree = Tree::new();
        let mut store = legacy_tree(&tree, &[("save", b"base")]);
        let base = store.list_commits().unwrap().remove(0);
        let mut right = base.clone();
        right.id = Uuid::new_v4();
        right.parents = vec![base.id];
        right.entries.insert(
            Tree::path("save"),
            SnapshotEntry::File {
                sha256: Sha256::digest(b"right").into(),
                size: 5,
                modified_unix_ms: 0,
            },
        );
        store
            .write_object(Sha256::digest(b"right").into(), b"right")
            .unwrap();
        let mut left = right.clone();
        left.id = Uuid::new_v4();
        left.entries.insert(
            Tree::path("save"),
            SnapshotEntry::File {
                sha256: Sha256::digest(b"left").into(),
                size: 4,
                modified_unix_ms: 0,
            },
        );
        store
            .write_object(Sha256::digest(b"left").into(), b"left")
            .unwrap();
        store.write_commit(&right).unwrap();
        store.write_commit(&left).unwrap();
        let old = fs::read(tree.state_path()).unwrap();
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("expected choices");
        };
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].remote_candidates.len(), 2);
        assert!(plan.conflicts[0]
            .remote_candidates
            .iter()
            .all(|candidate| matches!(candidate.id, RemoteVersionId::LegacyCommit(_))));
        assert!(engine.store().initial_inventory().unwrap().is_empty());
        assert_eq!(fs::read(tree.state_path()).unwrap(), old);
        let choice = ConflictChoice {
            path: Tree::path("save"),
            selected: LocalOrRemote::Remote,
            remote_version_id: Some(RemoteVersionId::LegacyCommit(right.id)),
        };
        assert!(matches!(
            engine.resolve_conflicts(&plan, &[choice]).unwrap(),
            SyncOutcome::Published
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"right");
        assert_eq!(engine.store().list_commits().unwrap().len(), 3);
    }

    #[test]
    fn legacy_migration_missing_object_keeps_checkpoint_and_local_bytes() {
        let tree = Tree::new();
        let mut store = legacy_tree(&tree, &[("save", b"base")]);
        store.corrupt_object(Sha256::digest(b"base").into(), b"corrupt".to_vec());
        fs::write(tree.file("save"), b"base").unwrap();
        let old = fs::read(tree.state_path()).unwrap();
        let mut engine = tree.engine(store);
        assert!(matches!(engine.synchronize(), Err(SyncError::Integrity(_))));
        assert_eq!(fs::read(tree.state_path()).unwrap(), old);
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"base");
        assert!(engine.store().initial_inventory().unwrap().is_empty());
    }

    #[test]
    fn legacy_migration_checks_size_of_every_tip_reference() {
        let tree = Tree::new();
        let mut store = legacy_tree(&tree, &[("save", b"base")]);
        fs::write(tree.file("save"), b"base").unwrap();
        let base = store.list_commits().unwrap().remove(0);
        let mut bad = base.clone();
        bad.id = Uuid::from_u128(1);
        bad.parents = vec![base.id];
        bad.entries.insert(
            Tree::path("save"),
            SnapshotEntry::File {
                sha256: Sha256::digest(b"base").into(),
                size: 3,
                modified_unix_ms: 0,
            },
        );
        let mut good = base.clone();
        good.id = Uuid::from_u128(2);
        good.parents = vec![base.id];
        store.write_commit(&bad).unwrap();
        store.write_commit(&good).unwrap();
        let old = fs::read(tree.state_path()).unwrap();
        let mut engine = tree.engine(store);
        assert!(matches!(engine.synchronize(), Err(SyncError::Integrity(_))));
        assert_eq!(fs::read(tree.state_path()).unwrap(), old);
        assert!(engine.store().initial_inventory().unwrap().is_empty());
    }

    #[test]
    fn legacy_migration_resumes_partial_upload_and_keeps_offline_edit() {
        let tree = Tree::new();
        let mut store = legacy_tree(&tree, &[("base", b"base"), ("other", b"other")]);
        fs::write(tree.file("base"), b"offline").unwrap();
        fs::write(tree.file("other"), b"other").unwrap();
        let old = fs::read(tree.state_path()).unwrap();
        store.fail_after(StoreOperation::WriteFile, 1);
        let mut engine = tree.engine(store);
        assert!(engine.synchronize().is_err());
        assert_eq!(fs::read(tree.state_path()).unwrap(), old);
        assert_eq!(fs::read(tree.file("base")).unwrap(), b"offline");
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        assert_eq!(fs::read(tree.file("base")).unwrap(), b"offline");
        assert_eq!(fs::read(tree.file("other")).unwrap(), b"other");
        assert_eq!(engine.store().initial_inventory().unwrap().len(), 2);
    }

    #[test]
    fn legacy_migration_retries_inventory_and_upload_readback_failures() {
        for operation in [
            StoreOperation::InitialInventory,
            StoreOperation::FileMetadata,
        ] {
            let tree = Tree::new();
            let store = legacy_tree(&tree, &[("save", b"base")]);
            fs::write(tree.file("save"), b"base").unwrap();
            let old = fs::read(tree.state_path()).unwrap();
            let mut engine = tree.engine(store);
            engine.store().fail_next(operation);
            assert!(engine.synchronize().is_err());
            assert_eq!(fs::read(tree.state_path()).unwrap(), old);
            assert_eq!(fs::read(tree.file("save")).unwrap(), b"base");
            assert!(matches!(
                engine.synchronize().unwrap(),
                SyncOutcome::Published
            ));
            assert_eq!(engine.store().list_commits().unwrap().len(), 1);
            assert_eq!(engine.store().object_count(), 1);
        }
    }

    #[test]
    fn legacy_migration_checkpoint_with_stale_journal_only_cleans_up() {
        let tree = Tree::new();
        let store = legacy_tree(&tree, &[("save", b"base")]);
        fs::write(tree.file("save"), b"base").unwrap();
        let old = fs::read(tree.state_path()).unwrap();
        let mut engine = tree.engine(store);
        engine.synchronize().unwrap();
        write_migration_journal(
            &tree.0,
            &tree.state_path(),
            &MigrationJournal {
                phase: MigrationPhase::Uploaded,
                device_id: tree.state().device_id,
                previous: Some(old),
                selected: BTreeMap::from([(
                    Tree::path("save"),
                    SnapshotEntry::File {
                        sha256: Sha256::digest(b"base").into(),
                        size: 4,
                        modified_unix_ms: 0,
                    },
                )]),
                local_selected: BTreeSet::new(),
                verified: BTreeMap::new(),
                replace_remote: BTreeMap::new(),
                replace_remote_sha256: BTreeMap::new(),
            },
        )
        .unwrap();
        let file = engine.store().initial_inventory().unwrap().remove(0);
        engine
            .store()
            .write_file(&Tree::path("save"), Some(&file.id), b"new")
            .unwrap();
        engine.store().fail_next(StoreOperation::ListCommits);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"new");
        assert!(!tree.0.join(SYNC_DIR).join("migration.json").exists());
    }

    #[test]
    fn legacy_migration_missing_named_baseline_rejects_empty_history() {
        let tree = Tree::new();
        let legacy = SyncState {
            device_id: Uuid::new_v4(),
            last_applied_commit: Some(Uuid::new_v4()),
            baseline: BTreeMap::new(),
        };
        save_state(&tree.0, &tree.state_path(), &legacy).unwrap();
        let mut engine = tree.engine(MemoryRemoteStore::default());
        assert_eq!(engine.persisted_device_id().unwrap(), legacy.device_id);
        assert!(matches!(engine.synchronize(), Err(SyncError::Integrity(_))));
        assert_eq!(
            fs::read(tree.state_path()).unwrap(),
            serde_json::to_vec(&legacy).unwrap()
        );
    }
}

// Commit-graph runtime tests belong to the migration path (Task 4).
#[cfg(any())]
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
        let content_hash: [u8; 32] = Sha256::digest(b"same").into();
        assert_eq!(engine.store().object_read_count(&content_hash), 1);
        assert_eq!(engine.store().list_commits().unwrap().len(), 1);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        assert_eq!(engine.store().list_commits().unwrap().len(), 1);
    }

    #[test]
    fn corrupted_upload_is_not_published() {
        let tree = Tree::new();
        tree.write("save", b"local data");
        let mut store = MemoryRemoteStore::default();
        store.corrupt_next_write();
        let mut engine = tree.engine(store);

        assert!(matches!(engine.synchronize(), Err(SyncError::Integrity(_))));
        assert!(engine.store().list_commits().unwrap().is_empty());
        assert!(!tree.state().exists());
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"local data");
    }

    #[test]
    fn concurrent_first_device_id_adopts_winner_without_replacing_advanced_state() {
        let tree = Tree::new();
        tree.write("save", b"local");
        let mut winner = tree.engine(MemoryRemoteStore::default());
        let late = tree.engine(MemoryRemoteStore::default());
        let (observed_tx, observed_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();

        let late = std::thread::spawn(move || {
            let mut late = late;
            late.persisted_device_id_with(|| {
                observed_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
            })
            .unwrap()
        });

        observed_rx.recv().unwrap();
        let winner_id = winner.persisted_device_id().unwrap();
        assert!(matches!(
            winner.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        let advanced_state = fs::read(tree.state()).unwrap();
        let saved: SyncState = serde_json::from_slice(&advanced_state).unwrap();
        assert_eq!(saved.device_id, winner_id);
        assert!(saved.last_applied_commit.is_some());
        assert!(tree.0.join(SYNC_DIR).join("state.lock").is_file());
        continue_tx.send(()).unwrap();

        assert_eq!(late.join().unwrap(), winner_id);
        assert_eq!(fs::read(tree.state()).unwrap(), advanced_state);
    }

    #[cfg(unix)]
    #[test]
    fn state_initialization_does_not_follow_lock_file_symlink() {
        let tree = Tree::new();
        let sync_dir = tree.0.join(SYNC_DIR);
        fs::create_dir(&sync_dir).unwrap();
        let outside = tree.0.join("outside");
        fs::write(&outside, b"untouched").unwrap();
        std::os::unix::fs::symlink(&outside, sync_dir.join("state.lock")).unwrap();
        let mut engine = tree.engine(MemoryRemoteStore::default());

        assert!(engine.persisted_device_id().is_err());
        assert!(!tree.state().exists());
        assert_eq!(fs::read(outside).unwrap(), b"untouched");
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
                Err(SyncError::Authentication(
                    crate::sync::auth::AuthError::MissingCredentials.to_string(),
                ))
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
        assert!(matches!(
            engine.synchronize_for_background(),
            Err(SyncError::Authentication(_))
        ));
        assert!(!tree.state().exists());

        let mut coordinator = crate::sync::coordinator::SyncCoordinator::new(
            engine,
            crate::sync::coordinator::SyncMode::Headless,
        );
        assert_eq!(
            coordinator.background_reconcile().unwrap(),
            crate::sync::coordinator::BackgroundSyncResult::NeedsAuthorization
        );
        assert_eq!(
            crate::sync::status::load_live_status(&tree.0)
                .unwrap()
                .background_sync,
            crate::sync::status::BackgroundSyncState::NeedsAuthorization
        );
        assert_eq!(fs::read(tree.file("pending")).unwrap(), b"local");
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
    fn saved_baseline_must_match_its_named_commit_before_planning() {
        let tree = Tree::new();
        tree.write("save", b"cloud content");
        let mut engine = tree.engine(MemoryRemoteStore::default());
        engine.synchronize().unwrap();
        let mut state: SyncState =
            serde_json::from_slice(&fs::read(tree.state()).unwrap()).unwrap();
        state
            .baseline
            .remove(&RelativePath::new("touchHLE_apps/save").unwrap());
        let inconsistent = serde_json::to_vec(&state).unwrap();
        fs::write(tree.state(), &inconsistent).unwrap();
        fs::remove_file(tree.file("save")).unwrap();
        let commits_before = engine.store().list_commits().unwrap().len();

        assert!(matches!(engine.synchronize(), Err(SyncError::Integrity(_))));
        assert!(!tree.file("save").exists());
        assert_eq!(fs::read(tree.state()).unwrap(), inconsistent);
        assert_eq!(engine.store().list_commits().unwrap().len(), commits_before);
    }

    #[test]
    fn metadata_only_baseline_mismatch_recovers_after_successful_sync() {
        let tree = Tree::new();
        tree.write("save", b"cloud content");
        let mut engine = tree.engine(MemoryRemoteStore::default());
        engine.synchronize().unwrap();
        let remote = engine.store().list_commits().unwrap().pop().unwrap();
        let mut state: SyncState =
            serde_json::from_slice(&fs::read(tree.state()).unwrap()).unwrap();
        let SnapshotEntry::File {
            modified_unix_ms, ..
        } = state
            .baseline
            .get_mut(&RelativePath::new("touchHLE_apps/save").unwrap())
            .unwrap()
        else {
            panic!("expected file");
        };
        *modified_unix_ms += 1000;
        fs::write(tree.state(), serde_json::to_vec(&state).unwrap()).unwrap();

        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        let saved: SyncState = serde_json::from_slice(&fs::read(tree.state()).unwrap()).unwrap();
        assert_eq!(saved.baseline, remote.entries);
        assert_eq!(saved.last_applied_commit, Some(remote.id));
        assert_eq!(engine.store().list_commits().unwrap().len(), 1);
    }

    #[test]
    fn failed_sync_does_not_persist_normalized_baseline() {
        let tree = Tree::new();
        tree.write("save", b"cloud content");
        let mut engine = tree.engine(MemoryRemoteStore::default());
        engine.synchronize().unwrap();
        let mut state: SyncState =
            serde_json::from_slice(&fs::read(tree.state()).unwrap()).unwrap();
        let SnapshotEntry::File {
            modified_unix_ms, ..
        } = state
            .baseline
            .get_mut(&RelativePath::new("touchHLE_apps/save").unwrap())
            .unwrap()
        else {
            panic!("expected file");
        };
        *modified_unix_ms += 1000;
        let inconsistent = serde_json::to_vec(&state).unwrap();
        fs::write(tree.state(), &inconsistent).unwrap();
        tree.write("new", b"local change");
        engine.store().fail_next(StoreOperation::WriteObject);

        assert!(engine.synchronize().is_err());
        assert_eq!(fs::read(tree.state()).unwrap(), inconsistent);
        assert_eq!(engine.store().list_commits().unwrap().len(), 1);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::Published
        ));
        let saved: SyncState = serde_json::from_slice(&fs::read(tree.state()).unwrap()).unwrap();
        let last_commit = engine
            .store()
            .list_commits()
            .unwrap()
            .into_iter()
            .find(|commit| Some(commit.id) == saved.last_applied_commit)
            .unwrap();
        assert_eq!(saved.baseline, last_commit.entries);
    }

    #[test]
    fn changed_content_in_saved_baseline_still_blocks_sync() {
        let tree = Tree::new();
        tree.write("save", b"cloud content");
        let mut engine = tree.engine(MemoryRemoteStore::default());
        engine.synchronize().unwrap();
        let mut state: SyncState =
            serde_json::from_slice(&fs::read(tree.state()).unwrap()).unwrap();
        let SnapshotEntry::File { sha256, .. } = state
            .baseline
            .get_mut(&RelativePath::new("touchHLE_apps/save").unwrap())
            .unwrap()
        else {
            panic!("expected file");
        };
        *sha256 = [0; 32];
        let invalid = serde_json::to_vec(&state).unwrap();
        fs::write(tree.state(), &invalid).unwrap();
        let commits_before = engine.store().list_commits().unwrap().len();

        assert!(matches!(engine.synchronize(), Err(SyncError::Integrity(_))));
        assert_eq!(fs::read(tree.state()).unwrap(), invalid);
        assert_eq!(engine.store().list_commits().unwrap().len(), commits_before);
    }

    #[test]
    fn local_mtime_only_change_does_not_poison_remote_baseline() {
        let tree = Tree::new();
        tree.write("save", b"cloud content");
        let mut engine = tree.engine(MemoryRemoteStore::default());
        engine.synchronize().unwrap();
        let remote = engine.store().list_commits().unwrap().pop().unwrap();
        let file = fs::File::open(tree.file("save")).unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        file.set_modified(modified + std::time::Duration::from_secs(3))
            .unwrap();

        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
        let saved: SyncState = serde_json::from_slice(&fs::read(tree.state()).unwrap()).unwrap();
        assert_eq!(saved.baseline, remote.entries);
        assert!(matches!(
            engine.synchronize().unwrap(),
            SyncOutcome::UpToDate
        ));
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
    fn resolving_remote_choice_publishes_merge_and_preserves_local_version() {
        let (tree, store, local_id, remote_id) =
            sibling_branches("save", b"local edit", "save", b"remote edit");
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("expected conflict before resolving");
        };
        let choice = ConflictChoice {
            path: plan.conflicts[0].path.clone(),
            selected: super::super::model::LocalOrRemote::Remote,
            remote_commit_id: Some(remote_id),
        };
        let previous_commit_count = engine.store().list_commits().unwrap().len();

        assert!(matches!(
            engine.resolve_conflicts(&plan, &[choice]),
            Ok(SyncOutcome::Published)
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"remote edit");
        let commits = engine.store().list_commits().unwrap();
        assert_eq!(commits.len(), previous_commit_count + 1);
        let tips = resolve_remote_tips(&commits).unwrap();
        assert_eq!(tips.len(), 1);
        let merge = commits
            .iter()
            .find(|commit| commit.id == tips[0].commit_id)
            .unwrap();
        let mut expected_parents = vec![local_id, remote_id];
        expected_parents.sort();
        assert_eq!(merge.parents, expected_parents);

        let local_hash: [u8; 32] = Sha256::digest(b"local edit").into();
        let remote_hash: [u8; 32] = Sha256::digest(b"remote edit").into();
        assert_eq!(
            engine.store().read_object(&local_hash).unwrap(),
            Some(b"local edit".to_vec())
        );
        assert_eq!(
            engine.store().read_object(&remote_hash).unwrap(),
            Some(b"remote edit".to_vec())
        );
    }

    #[test]
    fn changed_remote_state_rejects_stale_conflict_choices_without_mutation() {
        let (tree, store, _, remote_id) =
            sibling_branches("save", b"local edit", "save", b"remote edit");
        let mut engine = tree.engine(store);
        let saved_state = fs::read(tree.state()).unwrap();
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("expected conflict before resolving");
        };
        let choice = ConflictChoice {
            path: plan.conflicts[0].path.clone(),
            selected: super::super::model::LocalOrRemote::Remote,
            remote_commit_id: Some(remote_id),
        };
        let remote_tip = resolve_remote_tips(&engine.store().list_commits().unwrap())
            .unwrap()
            .into_iter()
            .find(|tip| tip.commit_id == remote_id)
            .unwrap();
        engine
            .store()
            .write_commit(&Commit {
                id: Uuid::new_v4(),
                device_id: Uuid::new_v4(),
                created_unix_ms: 1,
                parents: vec![remote_id],
                entries: remote_tip.snapshot,
            })
            .unwrap();
        let commit_count = engine.store().list_commits().unwrap().len();

        assert!(matches!(
            engine.resolve_conflicts(&plan, &[choice]),
            Err(SyncError::UnresolvedConflicts(_))
        ));
        assert_eq!(engine.store().list_commits().unwrap().len(), commit_count);
        assert_eq!(fs::read(tree.state()).unwrap(), saved_state);
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"local edit");
    }

    #[test]
    fn failed_remote_staging_does_not_publish_conflict_resolution() {
        let (tree, store, _, remote_id) =
            sibling_branches("save", b"local edit", "save", b"remote edit");
        let mut engine = tree.engine(store);
        let SyncOutcome::Conflicts(plan) = engine.synchronize().unwrap() else {
            panic!("expected conflict before resolving");
        };
        let choice = ConflictChoice {
            path: plan.conflicts[0].path.clone(),
            selected: super::super::model::LocalOrRemote::Remote,
            remote_commit_id: Some(remote_id),
        };
        let saved_state = fs::read(tree.state()).unwrap();
        let commit_count = engine.store().list_commits().unwrap().len();
        engine.store().fail_next(StoreOperation::ReadObject);

        assert!(engine.resolve_conflicts(&plan, &[choice]).is_err());
        assert_eq!(engine.store().list_commits().unwrap().len(), commit_count);
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
