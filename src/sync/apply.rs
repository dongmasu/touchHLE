/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::merge::{ConflictChoice as LegacyConflictChoice, SyncPlan as LegacySyncPlan};
use super::model::{LocalOrRemote, RelativePath, SnapshotEntry, SyncError};
use super::reconcile::{ConflictChoice, RemoteCandidate, RemoteVersionId, SyncPlan};
use crate::paths::SYNC_DIR;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use unicode_casefold::UnicodeCaseFold;
use uuid::Uuid;

pub struct StagedFile {
    root: PathBuf,
    path: RelativePath,
    destination: PathBuf,
    operation: StagedOperation,
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        cleanup_staging(&self.root, std::slice::from_ref(self));
    }
}

enum StagedOperation {
    File {
        temporary_path: PathBuf,
        expected_sha256: [u8; 32],
    },
    Tombstone,
}

/// Resolves every conflict before reading any remote object or staging an action.
/// Only automatic remote actions and explicitly chosen remote versions are applied.
pub fn stage_remote_files(
    root: &Path,
    plan: &LegacySyncPlan,
    choices: &[LegacyConflictChoice],
    read_object: impl FnMut(&[u8; 32]) -> Result<Vec<u8>, SyncError>,
) -> Result<Vec<StagedFile>, SyncError> {
    stage_remote_files_with_progress(root, plan, choices, read_object, |_, _, _, _| {})
}

pub fn stage_remote_files_with_progress(
    root: &Path,
    plan: &LegacySyncPlan,
    choices: &[LegacyConflictChoice],
    read_object: impl FnMut(&[u8; 32]) -> Result<Vec<u8>, SyncError>,
    progress: impl FnMut(usize, usize, u64, u64),
) -> Result<Vec<StagedFile>, SyncError> {
    let resolved = plan.resolved_snapshot(choices)?;
    let mut entries = BTreeMap::new();
    for (path, entry) in &plan.apply_remote {
        if resolved.contains_key(path) {
            entries.insert(path.clone(), entry.clone());
        }
    }
    for choice in choices {
        if choice.selected == LocalOrRemote::Remote {
            if let Some(entry) = resolved.get(&choice.path) {
                entries.insert(choice.path.clone(), entry.clone());
            }
        }
    }
    stage_selected_files(root, &entries, read_object, progress)
}

fn stage_selected_files(
    root: &Path,
    entries: &BTreeMap<RelativePath, SnapshotEntry>,
    mut read_object: impl FnMut(&[u8; 32]) -> Result<Vec<u8>, SyncError>,
    mut progress: impl FnMut(usize, usize, u64, u64),
) -> Result<Vec<StagedFile>, SyncError> {
    stage_selected_files_by_path(root, entries, |_, hash| read_object(hash), progress)
}

pub(crate) fn stage_migration_files(
    root: &Path,
    entries: &BTreeMap<RelativePath, SnapshotEntry>,
    read_object: impl FnMut(&[u8; 32]) -> Result<Vec<u8>, SyncError>,
) -> Result<Vec<StagedFile>, SyncError> {
    stage_selected_files(root, entries, read_object, |_, _, _, _| {})
}

/// Stages only automatic remote actions and explicitly selected current Drive
/// candidates. The complete choice set and resulting tree are validated first.
pub fn stage_current_files_with_progress(
    root: &Path,
    plan: &SyncPlan,
    choices: &[ConflictChoice],
    mut read_file: impl FnMut(&str) -> Result<Option<Vec<u8>>, SyncError>,
    progress: impl FnMut(usize, usize, u64, u64),
) -> Result<Vec<StagedFile>, SyncError> {
    plan.resolved_snapshot(choices)?;
    let mut selected = BTreeMap::<RelativePath, (SnapshotEntry, Option<String>)>::new();
    for (path, candidate) in &plan.apply_remote {
        selected.insert(
            path.clone(),
            (
                candidate.entry.clone().unwrap_or(SnapshotEntry::Tombstone),
                drive_file_id(path, candidate)?,
            ),
        );
    }
    for choice in choices {
        if choice.selected != LocalOrRemote::Remote {
            continue;
        }
        let conflict = plan
            .conflicts
            .iter()
            .find(|conflict| conflict.path == choice.path)
            .ok_or_else(|| SyncError::UnresolvedConflicts("choice no longer exists".into()))?;
        let candidate = conflict
            .remote_candidates
            .iter()
            .find(|candidate| Some(&candidate.id) == choice.remote_version_id.as_ref())
            .ok_or_else(|| SyncError::UnresolvedConflicts("remote choice is stale".into()))?;
        selected.insert(
            choice.path.clone(),
            (
                candidate.entry.clone().unwrap_or(SnapshotEntry::Tombstone),
                drive_file_id(&choice.path, candidate)?,
            ),
        );
    }
    stage_selected_files_by_path(
        root,
        &selected
            .iter()
            .map(|(path, (entry, _))| (path.clone(), entry.clone()))
            .collect(),
        |path, hash| {
            let Some((SnapshotEntry::File { .. }, Some(id))) = selected.get(path) else {
                return Err(SyncError::Integrity(format!(
                    "missing Drive file identity for {}",
                    path.as_str()
                )));
            };
            let bytes = read_file(id)?.ok_or_else(|| {
                SyncError::Provider(format!(
                    "selected Drive file disappeared: {}",
                    path.as_str()
                ))
            })?;
            if Sha256::digest(&bytes).as_slice() != hash {
                return Err(SyncError::Integrity(format!(
                    "downloaded file hash mismatch for {}",
                    path.as_str()
                )));
            }
            Ok(bytes)
        },
        progress,
    )
}

fn drive_file_id(
    path: &RelativePath,
    candidate: &RemoteCandidate,
) -> Result<Option<String>, SyncError> {
    match &candidate.id {
        RemoteVersionId::DriveFile(id)
            if match (&candidate.entry, &candidate.file) {
                (None, None) => true,
                (Some(entry), Some(file)) => {
                    file.id == *id && file.path == *path && file.entry == *entry
                }
                _ => false,
            } =>
        {
            Ok(Some(id.clone()))
        }
        RemoteVersionId::DriveFile(_) => Err(SyncError::Integrity(
            "selected Drive file does not match its path or content".into(),
        )),
        RemoteVersionId::LegacyCommit(_) => Err(SyncError::Integrity(
            "legacy commit candidates cannot be applied by normal sync".into(),
        )),
        RemoteVersionId::NoDriveFile => Err(SyncError::Integrity(
            "migration deletion candidates cannot be applied by normal sync".into(),
        )),
    }
}

fn stage_selected_files_by_path(
    root: &Path,
    entries: &BTreeMap<RelativePath, SnapshotEntry>,
    mut read_content: impl FnMut(&RelativePath, &[u8; 32]) -> Result<Vec<u8>, SyncError>,
    mut progress: impl FnMut(usize, usize, u64, u64),
) -> Result<Vec<StagedFile>, SyncError> {
    validate_selected(entries)?;
    let files_total = entries
        .values()
        .filter(|entry| matches!(entry, SnapshotEntry::File { .. }))
        .count();
    let bytes_total = entries.values().fold(0u64, |total, entry| match entry {
        SnapshotEntry::File { size, .. } => total.saturating_add(*size),
        SnapshotEntry::Tombstone => total,
    });
    let root_dir = Dir::open_ambient_dir(root, ambient_authority())?;
    if entries
        .values()
        .all(|entry| matches!(entry, SnapshotEntry::Tombstone))
    {
        return Ok(entries
            .keys()
            .map(|path| StagedFile {
                root: root.to_path_buf(),
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
    let mut files_done = 0;
    let mut bytes_done = 0u64;
    progress(0, files_total, 0, bytes_total);
    let result = (|| {
        for (path, entry) in entries {
            let destination = root.join(path.as_str());
            let operation = match entry {
                SnapshotEntry::File { sha256, size, .. } => {
                    let bytes = read_content(path, sha256)?;
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
                    files_done += 1;
                    bytes_done = bytes_done.saturating_add(*size);
                    progress(files_done, files_total, bytes_done, bytes_total);
                    StagedOperation::File {
                        temporary_path: root.join(SYNC_DIR).join(&name).join(filename),
                        expected_sha256: *sha256,
                    }
                }
                SnapshotEntry::Tombstone => StagedOperation::Tombstone,
            };
            staged.push(StagedFile {
                root: root.to_path_buf(),
                path: path.clone(),
                destination,
                operation,
            });
        }
        Ok(staged)
    })();
    if result.is_err() {
        let _ = sync_dir.remove_dir_all(&name);
    }
    result
}

/// Each rename is atomic; multi-file apply can fail after earlier paths changed.
/// The caller must prevent concurrent writes (including guest writes) until apply
/// finishes and save the baseline only after the complete apply succeeds.
pub fn apply_staged_files(root: &Path, staged: Vec<StagedFile>) -> Result<(), SyncError> {
    apply_staged_files_with_hook(root, staged, || {})
}

fn apply_staged_files_with_hook(
    root: &Path,
    staged: Vec<StagedFile>,
    after_backup: impl FnOnce(),
) -> Result<(), SyncError> {
    let result = apply_verified(root, &staged, after_backup, true);
    // Staging is scratch space. Cleanup must never turn a successful apply
    // into a reported failure after live files were already changed.
    cleanup_staging(root, &staged);
    result
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum TransactionPhase {
    Prepared,
    Applying,
    Applied,
    Checkpointed,
    RolledBack,
}

#[derive(Deserialize, Serialize)]
struct TransactionJournal {
    phase: TransactionPhase,
    directory: String,
    expected_checkpoint_sha256: [u8; 32],
    previous_checkpoint: Option<Vec<u8>>,
    entries: Vec<TransactionEntry>,
}

#[derive(Deserialize, Serialize)]
struct TransactionEntry {
    path: RelativePath,
    original: TransactionTarget,
}

#[derive(Deserialize, Serialize)]
enum TransactionTarget {
    Missing,
    Directory,
    File { backup: String, sha256: [u8; 32] },
}

/// Applies one local batch and saves its checkpoint as a single recoverable
/// transaction. The journal remains until the checkpoint replacement succeeds.
pub fn apply_staged_transaction(
    root: &Path,
    staged: Vec<StagedFile>,
    previous_checkpoint: Option<Vec<u8>>,
    checkpoint_bytes: &[u8],
    save_checkpoint: impl FnOnce() -> Result<(), SyncError>,
    restore_checkpoint: impl FnOnce(Option<&[u8]>) -> Result<(), SyncError>,
) -> Result<(), SyncError> {
    apply_staged_transaction_with_hook(
        root,
        staged,
        previous_checkpoint,
        checkpoint_bytes,
        save_checkpoint,
        restore_checkpoint,
        |_| Ok(()),
    )
}

fn apply_staged_transaction_with_hook(
    root: &Path,
    staged: Vec<StagedFile>,
    previous_checkpoint: Option<Vec<u8>>,
    checkpoint_bytes: &[u8],
    save_checkpoint: impl FnOnce() -> Result<(), SyncError>,
    restore_checkpoint: impl FnOnce(Option<&[u8]>) -> Result<(), SyncError>,
    mut hook: impl FnMut(TransactionPhase) -> Result<(), SyncError>,
) -> Result<(), SyncError> {
    if staged.is_empty() {
        return save_checkpoint();
    }
    let root_dir = Dir::open_ambient_dir(root, ambient_authority())?;
    let sync_dir = open_sync_dir(&root_dir)?;
    let directory = format!("transaction-{}", Uuid::new_v4());
    let transaction_dir = create_private_dir(&sync_dir, &directory)?;
    let mut entries = Vec::with_capacity(staged.len());
    for (index, item) in staged.iter().enumerate() {
        let (state, bytes) = target_contents(&root_dir, &item.path)?;
        let original = match (state, bytes) {
            (TargetState::Missing, _) => TransactionTarget::Missing,
            (TargetState::Directory, _) => TransactionTarget::Directory,
            (TargetState::File(hash), Some(bytes)) => {
                let backup = index.to_string();
                let mut file = transaction_dir
                    .open_with(&backup, OpenOptions::new().write(true).create_new(true))?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                TransactionTarget::File {
                    backup,
                    sha256: hash,
                }
            }
            _ => return Err(SyncError::Integrity("invalid transaction target".into())),
        };
        entries.push(TransactionEntry {
            path: item.path.clone(),
            original,
        });
    }
    let mut journal = TransactionJournal {
        phase: TransactionPhase::Prepared,
        directory,
        expected_checkpoint_sha256: Sha256::digest(checkpoint_bytes).into(),
        previous_checkpoint,
        entries,
    };
    write_journal(&sync_dir, &journal)?;
    if let Err(error) = hook(TransactionPhase::Prepared) {
        rollback_transaction(&root_dir, &sync_dir, &journal)?;
        restore_checkpoint(journal.previous_checkpoint.as_deref())?;
        finish_rollback(&sync_dir, &mut journal)?;
        return Err(error);
    }

    journal.phase = TransactionPhase::Applying;
    if let Err(error) = write_journal(&sync_dir, &journal) {
        rollback_transaction(&root_dir, &sync_dir, &journal)?;
        restore_checkpoint(journal.previous_checkpoint.as_deref())?;
        finish_rollback(&sync_dir, &mut journal)?;
        return Err(error);
    }
    if let Err(error) = hook(TransactionPhase::Applying) {
        rollback_transaction(&root_dir, &sync_dir, &journal)?;
        restore_checkpoint(journal.previous_checkpoint.as_deref())?;
        finish_rollback(&sync_dir, &mut journal)?;
        return Err(error);
    }
    let result = report_local_io(
        "apply staged remote files",
        apply_verified(root, &staged, || {}, false),
    );
    cleanup_staging(root, &staged);
    if let Err(error) = result {
        report_local_io(
            "roll back staged remote files",
            rollback_transaction(&root_dir, &sync_dir, &journal),
        )?;
        restore_checkpoint(journal.previous_checkpoint.as_deref())?;
        finish_rollback(&sync_dir, &mut journal)?;
        return Err(error);
    }

    journal.phase = TransactionPhase::Applied;
    if let Err(error) = write_journal(&sync_dir, &journal) {
        rollback_transaction(&root_dir, &sync_dir, &journal)?;
        restore_checkpoint(journal.previous_checkpoint.as_deref())?;
        finish_rollback(&sync_dir, &mut journal)?;
        return Err(error);
    }
    if let Err(error) = hook(TransactionPhase::Applied) {
        rollback_transaction(&root_dir, &sync_dir, &journal)?;
        restore_checkpoint(journal.previous_checkpoint.as_deref())?;
        finish_rollback(&sync_dir, &mut journal)?;
        return Err(error);
    }
    if let Err(error) = report_local_io("save sync checkpoint", save_checkpoint()) {
        restore_checkpoint(journal.previous_checkpoint.as_deref())?;
        report_local_io(
            "roll back files after checkpoint failure",
            rollback_transaction(&root_dir, &sync_dir, &journal),
        )?;
        finish_rollback(&sync_dir, &mut journal)?;
        return Err(error);
    }
    journal.phase = TransactionPhase::Checkpointed;
    if let Err(error) =
        write_journal(&sync_dir, &journal).and_then(|()| hook(TransactionPhase::Checkpointed))
    {
        restore_checkpoint(journal.previous_checkpoint.as_deref())?;
        rollback_transaction(&root_dir, &sync_dir, &journal)?;
        finish_rollback(&sync_dir, &mut journal)?;
        return Err(error);
    }
    // Cleanup is recoverable: after the checkpoint commits, a leftover journal
    // is finalized on the next sync rather than reported as a failed operation.
    let _ = cleanup_transaction(&sync_dir, &journal);
    Ok(())
}

/// Recovers an interrupted local transaction before planning another batch.
/// A matching checkpoint means the prior operation committed; otherwise all
/// displaced bytes and the prior checkpoint are restored.
pub fn recover_transaction(
    root: &Path,
    checkpoint_bytes: Option<&[u8]>,
    restore_checkpoint: impl FnOnce(Option<&[u8]>) -> Result<(), SyncError>,
) -> Result<(), SyncError> {
    let root_dir = Dir::open_ambient_dir(root, ambient_authority())?;
    let sync_dir = open_sync_dir(&root_dir)?;
    let mut file = match open_regular(&sync_dir, "transaction.json") {
        Ok(file) => file,
        Err(SyncError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return cleanup_orphan_staging(&sync_dir)
        }
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let journal: TransactionJournal = serde_json::from_slice(&bytes)?;
    validate_journal(&journal)?;
    if journal.phase == TransactionPhase::RolledBack {
        cleanup_transaction(&sync_dir, &journal)?;
        return cleanup_orphan_staging(&sync_dir);
    }
    if checkpoint_bytes.map(|bytes| <[u8; 32]>::from(Sha256::digest(bytes)))
        == Some(journal.expected_checkpoint_sha256)
    {
        cleanup_transaction(&sync_dir, &journal)?;
        return cleanup_orphan_staging(&sync_dir);
    }
    report_local_io(
        "roll back interrupted file transaction",
        rollback_transaction(&root_dir, &sync_dir, &journal),
    )?;
    report_local_io(
        "restore interrupted sync checkpoint",
        restore_checkpoint(journal.previous_checkpoint.as_deref()),
    )?;
    let mut journal = journal;
    report_local_io(
        "finish interrupted transaction rollback",
        finish_rollback(&sync_dir, &mut journal),
    )?;
    report_local_io(
        "clean up interrupted sync staging",
        cleanup_orphan_staging(&sync_dir),
    )
}

fn validate_journal(journal: &TransactionJournal) -> Result<(), SyncError> {
    if !journal.directory.starts_with("transaction-")
        || journal.directory.contains(['/', '\\'])
        || journal.directory == "transaction-"
        || journal.entries.iter().enumerate().any(|(index, entry)| {
            matches!(
                &entry.original,
                TransactionTarget::File { backup, .. } if *backup != index.to_string()
            )
        })
    {
        return Err(SyncError::Integrity(
            "invalid transaction journal paths".into(),
        ));
    }
    Ok(())
}

fn cleanup_orphan_staging(sync_dir: &Dir) -> Result<(), SyncError> {
    for item in sync_dir.read_dir(".")? {
        let name = item?
            .file_name()
            .into_string()
            .map_err(|_| SyncError::Integrity("invalid sync scratch name".into()))?;
        if !name.starts_with("stage-") && !name.starts_with("transaction-") {
            continue;
        }
        if !sync_dir.symlink_metadata(&name)?.is_dir() {
            return Err(SyncError::Integrity(
                "sync scratch directory is not a directory".into(),
            ));
        }
        sync_dir.remove_dir_all(&name)?;
    }
    Ok(())
}

fn finish_rollback(sync_dir: &Dir, journal: &mut TransactionJournal) -> Result<(), SyncError> {
    journal.phase = TransactionPhase::RolledBack;
    write_journal(sync_dir, journal)?;
    cleanup_transaction(sync_dir, journal)
}

fn write_journal(sync_dir: &Dir, journal: &TransactionJournal) -> Result<(), SyncError> {
    let temporary = format!(".transaction-{}", Uuid::new_v4());
    let result = (|| {
        let mut file = report_local_io(
            "create transaction journal temporary file",
            sync_dir
                .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
                .map_err(SyncError::from),
        )?;
        report_local_io(
            "write transaction journal",
            file.write_all(&serde_json::to_vec(journal)?)
                .map_err(SyncError::from),
        )?;
        report_local_io(
            "sync transaction journal",
            file.sync_all().map_err(SyncError::from),
        )?;
        drop(file);
        report_local_io(
            "replace transaction journal",
            sync_dir
                .rename(&temporary, sync_dir, "transaction.json")
                .map_err(SyncError::from),
        )?;
        report_local_io(
            "sync transaction journal directory",
            sync_directory(sync_dir),
        )?;
        Ok::<_, SyncError>(())
    })();
    let _ = sync_dir.remove_file(&temporary);
    result
}

fn report_local_io<T>(
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

fn rollback_transaction(
    root: &Dir,
    sync_dir: &Dir,
    journal: &TransactionJournal,
) -> Result<(), SyncError> {
    let transaction_dir = sync_dir.open_dir_nofollow(&journal.directory)?;
    let mut paths: Vec<_> = journal.entries.iter().collect();
    paths.sort_by_key(|entry| std::cmp::Reverse(entry.path.as_str().matches('/').count()));
    for entry in &paths {
        remove_transaction_target(root, &entry.path)?;
    }
    paths.sort_by_key(|entry| entry.path.as_str().matches('/').count());
    for entry in paths {
        match &entry.original {
            TransactionTarget::Missing => {}
            TransactionTarget::Directory => {
                let (parent, name) = open_parent(root, &entry.path, true)?.unwrap();
                match parent.create_dir(name) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        if !parent.symlink_metadata(name)?.is_dir() {
                            return Err(SyncError::InvalidPath(entry.path.as_str().into()));
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            TransactionTarget::File { backup, sha256 } => {
                let bytes = read_regular(&transaction_dir, backup)?;
                verify_hash(&bytes, sha256)?;
                restore_transaction_file(root, &entry.path, &bytes)?;
            }
        }
    }
    Ok(())
}

fn remove_transaction_target(root: &Dir, path: &RelativePath) -> Result<(), SyncError> {
    let Some((parent, name)) = open_parent(root, path, false)? else {
        return Ok(());
    };
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.is_file() => parent.remove_file(name)?,
        Ok(metadata) if metadata.is_dir() => parent.remove_dir(name)?,
        Ok(_) => return Err(SyncError::InvalidPath(path.as_str().into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn restore_transaction_file(
    root: &Dir,
    path: &RelativePath,
    bytes: &[u8],
) -> Result<(), SyncError> {
    let (parent, name) = open_parent(root, path, true)?.unwrap();
    let temporary = format!(".touchHLE-restore-{}", Uuid::new_v4());
    let result = (|| {
        let mut file =
            parent.open_with(&temporary, OpenOptions::new().write(true).create_new(true))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        parent.rename(&temporary, &parent, name)?;
        Ok::<_, SyncError>(())
    })();
    let _ = parent.remove_file(&temporary);
    result
}

fn cleanup_transaction(sync_dir: &Dir, journal: &TransactionJournal) -> Result<(), SyncError> {
    match sync_dir.remove_dir_all(&journal.directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    sync_dir.remove_file("transaction.json")?;
    sync_directory(sync_dir)
}

pub(super) fn sync_directory(dir: &Dir) -> Result<(), SyncError> {
    #[cfg(target_os = "android")]
    {
        // Android's emulated external storage returns EBADF for directory fsync.
        let _ = dir;
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        dir.try_clone()?.into_std_file().sync_all()?;
        Ok(())
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TargetState {
    Missing,
    Directory,
    File([u8; 32]),
}

fn apply_verified(
    root: &Path,
    staged: &[StagedFile],
    after_backup: impl FnOnce(),
    preserve_recovery: bool,
) -> Result<(), SyncError> {
    let root_dir = Dir::open_ambient_dir(root, ambient_authority())?;
    let mut selected = BTreeMap::new();
    for item in staged {
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
    for item in staged {
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
    let mut initial = Vec::with_capacity(staged.len());
    for item in staged {
        let (state, bytes) = target_contents(&root_dir, &item.path)?;
        if preserve_recovery {
            if let (TargetState::File(hash), Some(bytes)) = (state, bytes) {
                preserve_bytes(&root_dir, &bytes, &hash)?;
            }
        }
        initial.push(state);
    }
    after_backup();

    // Remove selected tombstones before creating descendants of old files.
    for (item, expected) in staged.iter().zip(&initial) {
        if !matches!(item.operation, StagedOperation::Tombstone) {
            continue;
        }
        let current = check_target(&root_dir, &item.path, *expected)?;
        if !matches!(current, TargetState::File(_)) {
            continue;
        }
        if let Some((parent, name)) = open_parent(&root_dir, &item.path, false)? {
            parent.remove_file(name)?;
        }
    }

    for ((item, bytes), expected) in staged.iter().zip(&verified).zip(&initial) {
        if let Some(bytes) = bytes {
            if *expected == TargetState::Directory {
                check_target(&root_dir, &item.path, TargetState::Directory)?;
                let (parent, name) = open_parent(&root_dir, &item.path, false)?.unwrap();
                parent.remove_dir(name)?;
            }
            let (parent, name) = open_parent(&root_dir, &item.path, true)?.unwrap();
            let temporary = format!(".touchHLE-sync-{}", Uuid::new_v4());
            let mut created = false;
            let result = (|| {
                let mut file = report_local_io(
                    "create local replacement file",
                    parent
                        .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
                        .map_err(SyncError::from),
                )?;
                created = true;
                report_local_io(
                    "write local replacement file",
                    file.write_all(bytes).map_err(SyncError::from),
                )?;
                report_local_io(
                    "sync local replacement file",
                    file.sync_all().map_err(SyncError::from),
                )?;
                drop(file);
                let expected = if *expected == TargetState::Directory {
                    TargetState::Missing
                } else {
                    *expected
                };
                check_target(&root_dir, &item.path, expected)?;
                report_local_io(
                    "replace local file",
                    parent
                        .rename(&temporary, &parent, name)
                        .map_err(SyncError::from),
                )?;
                Ok::<_, SyncError>(())
            })();
            if created {
                let _ = parent.remove_file(&temporary);
            }
            result?;
        }
    }
    Ok(())
}

fn cleanup_staging(root: &Path, staged: &[StagedFile]) {
    let Ok(root_dir) = Dir::open_ambient_dir(root, ambient_authority()) else {
        return;
    };
    let Ok(sync_dir) = root_dir.open_dir_nofollow(SYNC_DIR) else {
        return;
    };
    for item in staged {
        if let StagedOperation::File { temporary_path, .. } = &item.operation {
            if let Ok(relative) = temporary_path.strip_prefix(root.join(SYNC_DIR)) {
                if let Ok((stage_name, file_name)) = staging_parts(relative) {
                    if let Ok(stage_dir) = sync_dir.open_dir_nofollow(stage_name) {
                        let _ = stage_dir.remove_file(file_name);
                        let _ = sync_dir.remove_dir(stage_name);
                    }
                }
            }
        }
    }
}

fn target_contents(
    root: &Dir,
    path: &RelativePath,
) -> Result<(TargetState, Option<Vec<u8>>), SyncError> {
    target_contents_with_hook(root, path, || {})
}

fn target_contents_with_hook(
    root: &Dir,
    path: &RelativePath,
    before_leaf_open: impl FnOnce(),
) -> Result<(TargetState, Option<Vec<u8>>), SyncError> {
    let Some((parent, name)) = open_parent(root, path, false)? else {
        return Ok((TargetState::Missing, None));
    };
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.is_file() => {
            before_leaf_open();
            let bytes = read_regular(&parent, name)?;
            Ok((
                TargetState::File(Sha256::digest(&bytes).into()),
                Some(bytes),
            ))
        }
        Ok(metadata) if metadata.is_dir() => Ok((TargetState::Directory, None)),
        Ok(_) => Err(SyncError::InvalidPath(path.as_str().to_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((TargetState::Missing, None)),
        Err(e) => Err(e.into()),
    }
}

fn check_target(
    root: &Dir,
    path: &RelativePath,
    expected: TargetState,
) -> Result<TargetState, SyncError> {
    let (current, bytes) = target_contents(root, path)?;
    if current != expected {
        if let (TargetState::File(hash), Some(bytes)) = (current, bytes) {
            preserve_bytes(root, &bytes, &hash)?;
        }
        return Err(SyncError::Integrity(format!(
            "local file changed during apply: {}",
            path.as_str()
        )));
    }
    Ok(current)
}

#[cfg(test)]
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
    preserve_bytes(&root_dir, &bytes, content_hash)
}

fn preserve_bytes(root: &Dir, bytes: &[u8], content_hash: &[u8; 32]) -> Result<(), SyncError> {
    verify_hash(bytes, content_hash)?;
    let sync_dir = open_sync_dir(root)?;
    let recovery = open_private_dir(&sync_dir, "recovery")?;
    let filename = hex_hash(content_hash);
    match recovery.symlink_metadata(&filename) {
        Ok(_) => return verify_recovery(&recovery, &filename, content_hash),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let temp = format!(".recovery-{}", Uuid::new_v4());
    let mut created = false;
    let result = (|| {
        let mut file =
            recovery.open_with(&temp, OpenOptions::new().write(true).create_new(true))?;
        created = true;
        file.write_all(bytes)?;
        file.sync_all()?;
        make_readonly(&file)?;
        file.sync_all()?;
        drop(file);
        // A hard link never replaces an existing recovery version.
        Ok::<_, SyncError>(recovery.hard_link(&temp, &recovery, &filename))
    })();
    if created {
        let _ = recovery.remove_file(&temp);
    }
    match result? {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_recovery(&recovery, &filename, content_hash)
        }
        Err(e) => Err(e.into()),
    }
}

fn verify_recovery(recovery: &Dir, name: &str, hash: &[u8; 32]) -> Result<(), SyncError> {
    let mut file = open_regular(recovery, name)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    verify_hash(&bytes, hash)?;
    make_readonly(&file)?;
    Ok(())
}

fn make_readonly(file: &cap_std::fs::File) -> Result<(), SyncError> {
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)?;
    Ok(())
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
    let mut file = open_regular(parent, name)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn open_regular(parent: &Dir, name: impl AsRef<Path>) -> Result<cap_std::fs::File, SyncError> {
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
        options.custom_flags(0x0020_0000);
    }
    let file = parent.open_with(name, &options)?;
    if !file.metadata()?.is_file() {
        return Err(SyncError::Integrity("expected regular file".to_owned()));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::merge::{plan_sync, ConflictChoice, RemoteTip, SyncPlan};
    use crate::sync::model::{RelativePath, SnapshotEntry, SyncError};
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeMap, fs, path::PathBuf};
    use uuid::Uuid;

    fn stage_remote_files(
        root: &Path,
        entries: &BTreeMap<RelativePath, SnapshotEntry>,
        reader: impl FnMut(&[u8; 32]) -> Result<Vec<u8>, SyncError>,
    ) -> Result<Vec<StagedFile>, SyncError> {
        let plan = SyncPlan {
            apply_remote: entries.clone(),
            publish_local: BTreeMap::new(),
            conflicts: Vec::new(),
            merge_parents: Vec::new(),
            local_snapshot: BTreeMap::new(),
            remote_tips: Vec::new(),
        };
        super::stage_remote_files(root, &plan, &[], reader)
    }

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
    fn current_staging_reads_chosen_drive_id_and_checks_hash_and_size() {
        let tree = TestTree::new();
        let path = RelativePath::new("touchHLE_apps/save").unwrap();
        let entry = file_entry(b"remote");
        let file = crate::sync::reconcile::RemoteFile {
            path: path.clone(),
            id: "selected-file".into(),
            version: "3".into(),
            entry: entry.clone(),
            parent_id: None,
        };
        let plan = crate::sync::reconcile::SyncPlan {
            apply_remote: BTreeMap::from([(
                path.clone(),
                crate::sync::reconcile::RemoteCandidate {
                    id: crate::sync::reconcile::RemoteVersionId::DriveFile(file.id.clone()),
                    entry: Some(entry),
                    file: Some(file),
                },
            )]),
            ..Default::default()
        };
        let mut requested = Vec::new();
        let staged = stage_current_files_with_progress(
            &tree.0,
            &plan,
            &[],
            |id| {
                requested.push(id.to_owned());
                Ok(Some(b"remote".to_vec()))
            },
            |_, _, _, _| {},
        )
        .unwrap();
        assert_eq!(requested, ["selected-file"]);
        assert_eq!(staged.len(), 1);
        assert!(!tree.file("save").exists());
        drop(staged);

        assert!(matches!(
            stage_current_files_with_progress(
                &tree.0,
                &plan,
                &[],
                |_| Ok(Some(b"wrong".to_vec())),
                |_, _, _, _| {}
            ),
            Err(SyncError::Integrity(_))
        ));
    }

    #[test]
    fn injected_transaction_failures_restore_original_file_and_checkpoint() {
        for phase in [
            TransactionPhase::Prepared,
            TransactionPhase::Applying,
            TransactionPhase::Applied,
            TransactionPhase::Checkpointed,
        ] {
            let tree = TestTree::new();
            fs::write(tree.file("save"), b"original").unwrap();
            let checkpoint = tree.0.join(SYNC_DIR).join("state.json");
            fs::create_dir(checkpoint.parent().unwrap()).unwrap();
            fs::write(&checkpoint, b"old checkpoint").unwrap();
            let staged = stage_remote_files(
                &tree.0,
                &entries("touchHLE_apps/save", file_entry(b"chosen")),
                |_| Ok(b"chosen".to_vec()),
            )
            .unwrap();
            let result = apply_staged_transaction_with_hook(
                &tree.0,
                staged,
                Some(b"old checkpoint".to_vec()),
                b"new checkpoint",
                || fs::write(&checkpoint, b"new checkpoint").map_err(Into::into),
                |bytes| fs::write(&checkpoint, bytes.unwrap()).map_err(Into::into),
                |at| {
                    if at == phase {
                        Err(SyncError::Provider("injected transaction failure".into()))
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(matches!(result, Err(SyncError::Provider(_))), "{phase:?}");
            assert_eq!(
                fs::read(tree.file("save")).unwrap(),
                b"original",
                "{phase:?}"
            );
            assert_eq!(
                fs::read(checkpoint).unwrap(),
                b"old checkpoint",
                "{phase:?}"
            );
            assert!(!tree.0.join(SYNC_DIR).join("transaction.json").exists());
            assert!(!tree.0.join(SYNC_DIR).join("recovery").exists());
        }
    }

    #[test]
    fn checkpoint_save_failure_rolls_back_and_restart_replays_old_cursor() {
        let tree = TestTree::new();
        fs::write(tree.file("save"), b"original").unwrap();
        let checkpoint = tree.0.join(SYNC_DIR).join("state.json");
        fs::create_dir(checkpoint.parent().unwrap()).unwrap();
        fs::write(&checkpoint, b"old cursor").unwrap();
        let staged = stage_remote_files(
            &tree.0,
            &entries("touchHLE_apps/save", file_entry(b"chosen")),
            |_| Ok(b"chosen".to_vec()),
        )
        .unwrap();
        assert!(apply_staged_transaction(
            &tree.0,
            staged,
            Some(b"old cursor".to_vec()),
            b"new cursor",
            || Err(SyncError::Io(std::io::Error::other(
                "injected checkpoint error"
            ))),
            |bytes| fs::write(&checkpoint, bytes.unwrap()).map_err(Into::into),
        )
        .is_err());
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"original");
        assert_eq!(fs::read(&checkpoint).unwrap(), b"old cursor");
        recover_transaction(&tree.0, Some(b"old cursor"), |_| Ok(())).unwrap();
        assert!(!tree.0.join(SYNC_DIR).join("recovery").exists());
    }

    #[test]
    fn failed_checkpoint_restores_complete_multifile_apply_without_recovery_copies() {
        let tree = TestTree::new();
        fs::write(tree.file("a"), b"old a").unwrap();
        fs::write(tree.file("b"), b"old b").unwrap();
        let selected = BTreeMap::from([
            (
                RelativePath::new("touchHLE_apps/a").unwrap(),
                file_entry(b"new a"),
            ),
            (
                RelativePath::new("touchHLE_apps/b").unwrap(),
                file_entry(b"new b"),
            ),
        ]);
        let first_hash: [u8; 32] = Sha256::digest(b"new a").into();
        let staged = stage_remote_files(&tree.0, &selected, |hash| {
            if *hash == first_hash {
                Ok(b"new a".to_vec())
            } else {
                Ok(b"new b".to_vec())
            }
        })
        .unwrap();
        assert!(apply_staged_transaction(
            &tree.0,
            staged,
            None,
            b"new checkpoint",
            || Err(SyncError::Provider("injected save failure".into())),
            |_| Ok(()),
        )
        .is_err());
        assert_eq!(fs::read(tree.file("a")).unwrap(), b"old a");
        assert_eq!(fs::read(tree.file("b")).unwrap(), b"old b");
        assert!(!tree.0.join(SYNC_DIR).join("recovery").exists());
        assert!(!tree.0.join(SYNC_DIR).join("transaction.json").exists());
    }

    #[test]
    fn recovery_cleans_unjournaled_private_staging() {
        let tree = TestTree::new();
        let root = Dir::open_ambient_dir(&tree.0, ambient_authority()).unwrap();
        let sync = open_sync_dir(&root).unwrap();
        let orphan = create_private_dir(&sync, "stage-orphan").unwrap();
        orphan
            .open_with("0", OpenOptions::new().write(true).create_new(true))
            .unwrap();
        recover_transaction(&tree.0, None, |_| panic!("no checkpoint to restore")).unwrap();
        assert!(!tree.0.join(SYNC_DIR).join("stage-orphan").exists());
    }

    #[test]
    fn interrupted_apply_recovers_only_when_checkpoint_is_not_committed() {
        for committed in [false, true] {
            let tree = TestTree::new();
            fs::write(tree.file("save"), b"selected").unwrap();
            let root = Dir::open_ambient_dir(&tree.0, ambient_authority()).unwrap();
            let sync = open_sync_dir(&root).unwrap();
            let txn = create_private_dir(&sync, "transaction-test").unwrap();
            let mut backup = txn
                .open_with("0", OpenOptions::new().write(true).create_new(true))
                .unwrap();
            backup.write_all(b"original").unwrap();
            backup.sync_all().unwrap();
            let journal = TransactionJournal {
                phase: TransactionPhase::Applying,
                directory: "transaction-test".into(),
                expected_checkpoint_sha256: Sha256::digest(b"new checkpoint").into(),
                previous_checkpoint: Some(b"old checkpoint".to_vec()),
                entries: vec![TransactionEntry {
                    path: RelativePath::new("touchHLE_apps/save").unwrap(),
                    original: TransactionTarget::File {
                        backup: "0".into(),
                        sha256: Sha256::digest(b"original").into(),
                    },
                }],
            };
            write_journal(&sync, &journal).unwrap();
            let checkpoint = if committed {
                b"new checkpoint".as_slice()
            } else {
                b"old checkpoint".as_slice()
            };
            let mut restored = false;
            recover_transaction(&tree.0, Some(checkpoint), |before| {
                assert_eq!(before, Some(b"old checkpoint".as_slice()));
                restored = true;
                Ok(())
            })
            .unwrap();
            assert_eq!(restored, !committed);
            assert_eq!(
                fs::read(tree.file("save")).unwrap(),
                if committed {
                    b"selected".as_slice()
                } else {
                    b"original".as_slice()
                }
            );
            assert!(!tree.0.join(SYNC_DIR).join("transaction.json").exists());
        }
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
        let temporary_path = temporary_path.clone();
        fs::write(&temporary_path, b"tampered").unwrap();
        assert!(matches!(
            apply_staged_files(&tree.0, staged),
            Err(SyncError::Integrity(_))
        ));
        assert!(!temporary_path.exists());
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

    #[test]
    fn missing_and_incompatible_choices_fail_before_any_download_or_write() {
        let tree = TestTree::new();
        fs::write(tree.file("parent"), b"local").unwrap();
        let baseline = BTreeMap::new();
        let local = entries("touchHLE_apps/parent", file_entry(b"local"));
        let remote = RemoteTip {
            commit_id: Uuid::new_v4(),
            snapshot: entries("touchHLE_apps/parent/child", file_entry(b"remote")),
        };
        let plan = plan_sync(&baseline, &local, &[remote.clone()]).unwrap();
        assert_eq!(plan.conflicts.len(), 2);
        let mut reads = 0;
        let mut reader = |_: &[u8; 32]| {
            reads += 1;
            Ok(b"remote".to_vec())
        };
        assert!(matches!(
            super::stage_remote_files(&tree.0, &plan, &[], &mut reader),
            Err(SyncError::UnresolvedConflicts(_))
        ));
        let choices = plan
            .conflicts
            .iter()
            .map(|conflict| ConflictChoice {
                path: conflict.path.clone(),
                selected: if conflict.path.as_str() == "touchHLE_apps/parent" {
                    super::super::model::LocalOrRemote::Local
                } else {
                    super::super::model::LocalOrRemote::Remote
                },
                remote_commit_id: (conflict.path.as_str() != "touchHLE_apps/parent")
                    .then_some(remote.commit_id),
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            super::stage_remote_files(&tree.0, &plan, &choices, &mut reader),
            Err(SyncError::UnresolvedConflicts(_))
        ));
        assert_eq!(reads, 0);
        assert_eq!(fs::read(tree.file("parent")).unwrap(), b"local");
    }

    #[test]
    fn stages_automatic_remote_and_explicit_remote_choice_only() {
        let tree = TestTree::new();
        let auto = RelativePath::new("touchHLE_apps/auto").unwrap();
        let chosen = RelativePath::new("touchHLE_apps/chosen").unwrap();
        let retained = RelativePath::new("touchHLE_apps/local").unwrap();
        let baseline = BTreeMap::from([
            (chosen.clone(), file_entry(b"base")),
            (retained.clone(), file_entry(b"base")),
        ]);
        let local = BTreeMap::from([
            (chosen.clone(), file_entry(b"local")),
            (retained.clone(), file_entry(b"local")),
        ]);
        let tip = RemoteTip {
            commit_id: Uuid::new_v4(),
            snapshot: BTreeMap::from([
                (auto.clone(), file_entry(b"auto")),
                (chosen.clone(), file_entry(b"remote")),
                (retained.clone(), file_entry(b"remote")),
            ]),
        };
        let plan = plan_sync(&baseline, &local, &[tip.clone()]).unwrap();
        assert_eq!(plan.conflicts.len(), 2);
        let choices = vec![
            ConflictChoice {
                path: chosen.clone(),
                selected: LocalOrRemote::Remote,
                remote_commit_id: Some(tip.commit_id),
            },
            ConflictChoice {
                path: retained.clone(),
                selected: LocalOrRemote::Local,
                remote_commit_id: None,
            },
        ];
        let mut reads = Vec::new();
        let auto_hash: [u8; 32] = Sha256::digest(b"auto").into();
        let staged = super::stage_remote_files(&tree.0, &plan, &choices, |hash| {
            reads.push(*hash);
            if *hash == auto_hash {
                Ok(b"auto".to_vec())
            } else {
                Ok(b"remote".to_vec())
            }
        })
        .unwrap();
        assert_eq!(staged.len(), 2);
        assert!(staged.iter().any(|file| file.path == auto));
        assert!(staged.iter().any(|file| file.path == chosen));
        assert!(!staged.iter().any(|file| file.path == retained));
        assert_eq!(reads.len(), 2);
    }

    #[test]
    fn final_tombstone_check_preserves_intervening_bytes_and_baseline() {
        let tree = TestTree::new();
        fs::write(tree.file("save"), b"initial").unwrap();
        let baseline = tree.0.join(".touchHLE_sync/state.json");
        fs::create_dir(baseline.parent().unwrap()).unwrap();
        fs::write(&baseline, b"old baseline").unwrap();
        let staged = stage_remote_files(
            &tree.0,
            &entries("touchHLE_apps/save", SnapshotEntry::Tombstone),
            |_| panic!("tombstone must not download"),
        )
        .unwrap();
        assert!(matches!(
            apply_staged_files_with_hook(&tree.0, staged, || {
                fs::write(tree.file("save"), b"intervening").unwrap();
            }),
            Err(SyncError::Integrity(_))
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"intervening");
        let hash: [u8; 32] = Sha256::digest(b"intervening").into();
        assert_eq!(
            fs::read(recovery_path(&tree.0, &hash)).unwrap(),
            b"intervening"
        );
        assert_eq!(fs::read(baseline).unwrap(), b"old baseline");
    }

    #[test]
    fn final_rename_check_preserves_intervening_bytes_and_baseline() {
        let tree = TestTree::new();
        fs::write(tree.file("save"), b"initial").unwrap();
        let baseline = tree.0.join(".touchHLE_sync/state.json");
        fs::create_dir(baseline.parent().unwrap()).unwrap();
        fs::write(&baseline, b"old baseline").unwrap();
        let staged = stage_remote_files(
            &tree.0,
            &entries("touchHLE_apps/save", file_entry(b"remote")),
            |_| Ok(b"remote".to_vec()),
        )
        .unwrap();
        assert!(matches!(
            apply_staged_files_with_hook(&tree.0, staged, || {
                fs::write(tree.file("save"), b"intervening").unwrap();
            }),
            Err(SyncError::Integrity(_))
        ));
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"intervening");
        let hash: [u8; 32] = Sha256::digest(b"intervening").into();
        assert_eq!(
            fs::read(recovery_path(&tree.0, &hash)).unwrap(),
            b"intervening"
        );
        assert_eq!(fs::read(baseline).unwrap(), b"old baseline");
    }

    #[test]
    fn partial_multifile_apply_keeps_changed_later_bytes_recoverable() {
        let tree = TestTree::new();
        fs::write(tree.file("a"), b"old a").unwrap();
        fs::write(tree.file("b"), b"old b").unwrap();
        let baseline = tree.0.join(".touchHLE_sync/state.json");
        fs::create_dir(baseline.parent().unwrap()).unwrap();
        fs::write(&baseline, b"old baseline").unwrap();
        let selected = BTreeMap::from([
            (
                RelativePath::new("touchHLE_apps/a").unwrap(),
                file_entry(b"new a"),
            ),
            (
                RelativePath::new("touchHLE_apps/b").unwrap(),
                file_entry(b"new b"),
            ),
        ]);
        let first_hash: [u8; 32] = Sha256::digest(b"new a").into();
        let staged = stage_remote_files(&tree.0, &selected, |hash| {
            if *hash == first_hash {
                Ok(b"new a".to_vec())
            } else {
                Ok(b"new b".to_vec())
            }
        })
        .unwrap();
        assert!(matches!(
            apply_staged_files_with_hook(&tree.0, staged, || {
                fs::write(tree.file("b"), b"intervening b").unwrap();
            }),
            Err(SyncError::Integrity(_))
        ));
        assert_eq!(fs::read(tree.file("a")).unwrap(), b"new a");
        assert_eq!(fs::read(tree.file("b")).unwrap(), b"intervening b");
        let hash: [u8; 32] = Sha256::digest(b"intervening b").into();
        assert_eq!(
            fs::read(recovery_path(&tree.0, &hash)).unwrap(),
            b"intervening b"
        );
        assert_eq!(fs::read(baseline).unwrap(), b"old baseline");
    }

    #[cfg(unix)]
    #[test]
    fn failed_scratch_cleanup_after_apply_does_not_report_failed_live_update() {
        use std::os::unix::fs::symlink;

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
        let staging_dir = temporary_path.parent().unwrap().to_path_buf();
        let parked = tree.0.join("parked-staging");
        apply_staged_files_with_hook(&tree.0, staged, || {
            fs::rename(&staging_dir, &parked).unwrap();
            symlink(&parked, &staging_dir).unwrap();
        })
        .unwrap();
        assert_eq!(fs::read(tree.file("save")).unwrap(), b"remote");
    }

    #[test]
    fn recovery_copy_is_read_only_and_reused_with_verified_hash() {
        let tree = TestTree::new();
        fs::write(tree.file("save"), b"local").unwrap();
        let hash: [u8; 32] = Sha256::digest(b"local").into();
        preserve_local_version(
            &tree.0,
            &RelativePath::new("touchHLE_apps/save").unwrap(),
            &hash,
        )
        .unwrap();
        preserve_local_version(
            &tree.0,
            &RelativePath::new("touchHLE_apps/save").unwrap(),
            &hash,
        )
        .unwrap();
        let recovery = recovery_path(&tree.0, &hash);
        assert!(fs::metadata(&recovery).unwrap().permissions().readonly());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(recovery).unwrap().permissions().mode() & 0o222,
                0
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_leaf_is_not_read_or_backed_up() {
        use std::os::unix::fs::symlink;
        let tree = TestTree::new();
        let outside = tree.0.join("outside");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, tree.file("save")).unwrap();
        let staged = stage_remote_files(
            &tree.0,
            &entries("touchHLE_apps/save", SnapshotEntry::Tombstone),
            |_| panic!("tombstones do not download"),
        )
        .unwrap();
        assert!(apply_staged_files(&tree.0, staged).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"outside");
        assert!(!recovery_path(&tree.0, &Sha256::digest(b"outside").into()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn leaf_swapped_to_symlink_after_metadata_is_not_followed() {
        use std::os::unix::fs::symlink;

        let tree = TestTree::new();
        fs::write(tree.file("save"), b"initial").unwrap();
        let outside = tree.0.join("outside");
        fs::write(&outside, b"outside").unwrap();
        let root = Dir::open_ambient_dir(&tree.0, ambient_authority()).unwrap();
        let result = target_contents_with_hook(
            &root,
            &RelativePath::new("touchHLE_apps/save").unwrap(),
            || {
                fs::rename(tree.file("save"), tree.file("parked")).unwrap();
                symlink(&outside, tree.file("save")).unwrap();
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert!(!recovery_path(&tree.0, &Sha256::digest(b"outside").into()).exists());
    }
}
