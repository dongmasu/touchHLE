/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::auth::{AccessTokenProvider, AuthError, PlatformTokenStore};
use super::engine::{SyncEngine, SyncOutcome};
use super::gdrive::GoogleDriveStore;
use super::live::{LiveControl, LiveObserverWorker};
use super::locks::{LockGuard, SyncLocks};
use super::model::SyncError;
use super::progress::SyncProgress;
use super::reconcile::{ConflictChoice, SyncPlan};
use super::status::{load_live_status, redacted_error, save_live_status, BackgroundSyncState};
use super::store::RemoteStore;
use crate::paths::SYNC_DIR;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub type GoogleDriveSyncCoordinator =
    SyncCoordinator<GoogleDriveStore<AccessTokenProvider<PlatformTokenStore>>>;

pub fn google_drive_coordinator(
    root: &Path,
    headless: bool,
) -> Result<GoogleDriveSyncCoordinator, SyncError> {
    let client_id = super::auth::configured_client_id().unwrap_or_default();
    let token_provider = AccessTokenProvider::new(PlatformTokenStore, client_id)
        .with_interactive_authorization(!headless);
    let store = GoogleDriveStore::new(token_provider);
    let engine = SyncEngine::new(
        store,
        root.to_path_buf(),
        root.join(SYNC_DIR).join("state.json"),
    );
    SyncCoordinator::from_settings(engine, root, headless)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncMode {
    Disabled,
    Enabled,
    Headless,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreLaunchResult {
    Continue,
    NeedsResolution(SyncPlan),
    LocalOnly(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackgroundSyncResult {
    Disabled,
    GuestActive,
    NeedsAuthorization,
    Completed,
    Conflicts,
    RetryableFailure,
}

fn needs_foreground_authorization(error: &SyncError) -> bool {
    let SyncError::Authentication(message) = error else {
        return false;
    };
    [
        AuthError::MissingCredentials,
        AuthError::ReauthorizationRequired,
        AuthError::ForegroundRequired,
    ]
    .iter()
    .any(|reason| message == &reason.to_string())
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SyncSettings {
    pub enabled: bool,
}

pub struct SyncCoordinator<S: RemoteStore> {
    engine: SyncEngine<S>,
    mode: SyncMode,
    live: Option<LiveObserverWorker>,
}

impl<S: RemoteStore + Send + 'static> SyncCoordinator<S> {
    pub fn new(engine: SyncEngine<S>, mode: SyncMode) -> Self {
        Self {
            engine,
            mode,
            live: None,
        }
    }

    pub fn from_settings(
        engine: SyncEngine<S>,
        root: &Path,
        headless: bool,
    ) -> Result<Self, SyncError> {
        let settings = load_settings(&settings_path(root))?;
        let mode = if !settings.enabled {
            SyncMode::Disabled
        } else if headless {
            SyncMode::Headless
        } else {
            SyncMode::Enabled
        };
        Ok(Self::new(engine, mode))
    }

    pub fn before_launch(&mut self) -> Result<PreLaunchResult, SyncError> {
        self.before_launch_with_progress(&mut |_| {})
    }

    pub fn before_launch_with_progress(
        &mut self,
        progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<PreLaunchResult, SyncError> {
        if self.mode == SyncMode::Disabled {
            return Ok(PreLaunchResult::LocalOnly(
                "cloud sync is disabled".to_owned(),
            ));
        }
        let locks = SyncLocks::new(self.engine.root());
        let _operation = locks.operation()?;
        let Some(guest) = locks.try_guest()? else {
            self.record_background_state(BackgroundSyncState::Deferred);
            return Ok(PreLaunchResult::LocalOnly(
                "guest session is active; local files are unchanged".into(),
            ));
        };
        if self.engine.persisted_device_id().is_err() {
            return Ok(PreLaunchResult::LocalOnly(
                "cloud backup unavailable: device identity could not be persisted".into(),
            ));
        }
        drop(guest);
        drop(_operation);
        let started = Instant::now();
        let result = self.full_sync_with_progress(progress);
        log_sync_timing("startup", started.elapsed(), &result);
        match result {
            Ok(SyncOutcome::Conflicts(plan)) => Ok(PreLaunchResult::NeedsResolution(plan)),
            Ok(SyncOutcome::Offline) => Ok(PreLaunchResult::LocalOnly(
                "cloud is unavailable; local files are unchanged".to_owned(),
            )),
            Ok(_) => Ok(PreLaunchResult::Continue),
            Err(error @ SyncError::Authentication(_)) | Err(error @ SyncError::Provider(_)) => {
                Ok(PreLaunchResult::LocalOnly(redacted_error(&error)))
            }
            Err(error) => Err(error),
        }
    }

    pub fn resolve_conflicts(
        &mut self,
        plan: &SyncPlan,
        choices: &[ConflictChoice],
    ) -> Result<SyncOutcome, SyncError> {
        self.resolve_conflicts_with_progress(plan, choices, &mut |_| {})
    }

    pub fn resolve_conflicts_with_progress(
        &mut self,
        plan: &SyncPlan,
        choices: &[ConflictChoice],
        progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncOutcome, SyncError> {
        let locks = SyncLocks::new(self.engine.root());
        let _operation = locks.operation()?;
        let _guest = locks.guest()?;
        self.require_stopped()?;
        self.engine.persisted_device_id()?;
        let result = self
            .engine
            .resolve_conflicts_with_progress(plan, choices, progress);
        let result = self.finish_sync(result, progress);
        if self.record_result(&result).is_err() {
            eprintln!("warning: could not persist local live-sync status (details redacted)");
        }
        result
    }

    pub fn after_exit(&mut self) -> Result<SyncOutcome, SyncError> {
        self.after_exit_with_progress(&mut |_| {})
    }

    pub fn after_exit_with_progress(
        &mut self,
        progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncOutcome, SyncError> {
        if self.mode == SyncMode::Disabled {
            return Ok(SyncOutcome::Offline);
        }
        let started = Instant::now();
        log!("Google Drive shutdown full sync started");
        let result = self.full_sync_with_progress(progress);
        log_sync_timing("shutdown", started.elapsed(), &result);
        result
    }

    /// Stop local observation and reconcile the final disk snapshot.
    pub fn shutdown_sync(&mut self) -> Result<SyncOutcome, SyncError> {
        self.shutdown_sync_with_progress(&mut |_| {})
    }

    pub fn shutdown_sync_with_progress(
        &mut self,
        progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncOutcome, SyncError> {
        let started = Instant::now();
        log!("Google Drive shutdown transaction started");
        progress(SyncProgress::RescanningLocalFiles);
        let drain = self.stop_live();
        let result = self.after_exit_with_progress(progress);
        let elapsed_ms = started.elapsed().as_millis();

        if let Err(error) = drain {
            log!(
                "Google Drive local observer stopped with an error; final sync will rescan: {}",
                redacted_error(&error)
            );
        }

        match &result {
            Ok(SyncOutcome::UpToDate | SyncOutcome::Applied | SyncOutcome::Published) => {
                log!("Google Drive shutdown reconciliation completed in {elapsed_ms} ms");
            }
            Ok(SyncOutcome::Conflicts(_)) => {
                log!(
                    "Google Drive shutdown transaction paused for conflict resolution after {elapsed_ms} ms"
                );
            }
            Ok(SyncOutcome::Offline) => {
                log!(
                    "Google Drive shutdown transaction could not commit while offline ({elapsed_ms} ms)"
                );
            }
            Err(error) => {
                log!(
                    "Google Drive shutdown transaction failed after {elapsed_ms} ms; local data was retained: {}",
                    redacted_error(error)
                );
            }
        }
        result
    }

    pub fn begin_guest_session(&self) -> Result<LockGuard, SyncError> {
        let locks = SyncLocks::new(self.engine.root());
        let operation = locks.operation()?;
        let guest = locks.guest()?;
        drop(operation);
        Ok(guest)
    }

    pub fn background_reconcile(&mut self) -> Result<BackgroundSyncResult, SyncError> {
        if self.mode == SyncMode::Disabled {
            return Ok(BackgroundSyncResult::Disabled);
        }
        if self.live.is_some() {
            self.record_background_state(BackgroundSyncState::Deferred);
            return Ok(BackgroundSyncResult::GuestActive);
        }

        let locks = SyncLocks::new(self.engine.root());
        let _operation = locks.operation()?;
        let Some(_guest) = locks.try_guest()? else {
            self.record_background_state(BackgroundSyncState::Deferred);
            return Ok(BackgroundSyncResult::GuestActive);
        };

        self.record_background_state(BackgroundSyncState::Running);
        let result = self.engine.persisted_device_id().and_then(|_| {
            let result = self.engine.synchronize_for_background();
            self.finish_sync(result, &mut |_| {})
        });
        let background_state = match &result {
            Ok(SyncOutcome::Conflicts(_)) => BackgroundSyncState::Conflict,
            Err(error) if needs_foreground_authorization(error) => {
                BackgroundSyncState::NeedsAuthorization
            }
            Ok(SyncOutcome::Offline) | Err(_) => BackgroundSyncState::Failed,
            Ok(SyncOutcome::UpToDate | SyncOutcome::Applied | SyncOutcome::Published) => {
                BackgroundSyncState::Completed
            }
        };
        self.record_background_state(background_state);
        if self.record_result(&result).is_err() {
            eprintln!("warning: could not persist local live-sync status (details redacted)");
        }
        match result {
            Ok(SyncOutcome::Conflicts(_)) => Ok(BackgroundSyncResult::Conflicts),
            Ok(SyncOutcome::Offline) => Ok(BackgroundSyncResult::RetryableFailure),
            Ok(SyncOutcome::UpToDate | SyncOutcome::Applied | SyncOutcome::Published) => {
                Ok(BackgroundSyncResult::Completed)
            }
            Err(error) if needs_foreground_authorization(&error) => {
                Ok(BackgroundSyncResult::NeedsAuthorization)
            }
            Err(SyncError::Authentication(_) | SyncError::Provider(_)) => {
                Ok(BackgroundSyncResult::RetryableFailure)
            }
            Err(error) => Err(error),
        }
    }

    fn record_background_state(&self, state: BackgroundSyncState) {
        match load_live_status(self.engine.root()) {
            Ok(mut status) => {
                status.background_sync = state;
                if save_live_status(self.engine.root(), &status).is_err() {
                    eprintln!(
                        "warning: could not persist background sync state (details redacted)"
                    );
                }
            }
            Err(_) => {
                eprintln!("warning: could not load background sync state (details redacted)");
            }
        }
    }

    fn full_sync_with_progress(
        &mut self,
        progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncOutcome, SyncError> {
        let locks = SyncLocks::new(self.engine.root());
        let _operation = locks.operation()?;
        let Some(_guest) = locks.try_guest()? else {
            self.record_background_state(BackgroundSyncState::Deferred);
            return Ok(SyncOutcome::Offline);
        };
        self.require_stopped()?;
        self.engine.persisted_device_id()?;
        let result = self.engine.synchronize_with_progress(progress);
        let result = self.finish_sync(result, progress);
        if self.record_result(&result).is_err() {
            eprintln!("warning: could not persist local live-sync status (details redacted)");
        }
        result
    }

    fn finish_sync(
        &mut self,
        result: Result<SyncOutcome, SyncError>,
        progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncOutcome, SyncError> {
        match result {
            Ok(outcome) => {
                progress(SyncProgress::Finalizing);
                Ok(outcome)
            }
            Err(error) => Err(error),
        }
    }

    fn require_stopped(&self) -> Result<(), SyncError> {
        if self.live.is_some() {
            return Err(SyncError::Integrity(
                "local observer must stop before full sync".into(),
            ));
        }
        Ok(())
    }

    pub fn start_live_with_store(
        &mut self,
        root: &Path,
        _store: S,
    ) -> Result<LiveControl, SyncError> {
        if self.live.is_some() {
            return Err(SyncError::Integrity(
                "local observer is already running".into(),
            ));
        }
        let (sender, receiver) = mpsc::channel();
        let (live, mut control) = LiveObserverWorker::start(root.to_path_buf(), sender)?;
        self.live = Some(live);
        control.attach_status_events(receiver);
        Ok(control)
    }

    pub fn stop_live(&mut self) -> Result<(), SyncError> {
        if let Some(live) = self.live.take() {
            let started = Instant::now();
            log!("Google Drive local observer stop started");
            let result = live.stop_and_drain();
            match &result {
                Ok(()) => {
                    log!(
                        "Google Drive local observer stopped in {} ms",
                        started.elapsed().as_millis()
                    );
                }
                Err(error) => {
                    log!(
                        "Google Drive local observer stop failed in {} ms: {}; final sync will rescan local files",
                        started.elapsed().as_millis(),
                        redacted_error(error)
                    );
                }
            }
            result
        } else {
            Ok(())
        }
    }

    fn record_result(&self, result: &Result<SyncOutcome, SyncError>) -> Result<(), SyncError> {
        let root = self.engine.root();
        let mut status = load_live_status(root)?;
        status.background_sync = match result {
            Ok(SyncOutcome::Conflicts(_)) => BackgroundSyncState::Conflict,
            Err(error) if needs_foreground_authorization(error) => {
                BackgroundSyncState::NeedsAuthorization
            }
            Ok(SyncOutcome::Offline) | Err(_) => BackgroundSyncState::Failed,
            Ok(SyncOutcome::UpToDate | SyncOutcome::Applied | SyncOutcome::Published) => {
                BackgroundSyncState::Completed
            }
        };
        status.error = match result {
            Ok(SyncOutcome::Offline) => Some("cloud is unavailable".into()),
            Ok(SyncOutcome::Conflicts(_)) => Some("sync conflicts require resolution".into()),
            Err(error) => Some(redacted_error(error)),
            Ok(_) => {
                status.last_full_sync_unix_ms = Some(
                    i64::try_from(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|error| SyncError::Integrity(error.to_string()))?
                            .as_millis(),
                    )
                    .map_err(|_| SyncError::Integrity("invalid current time".into()))?,
                );
                None
            }
        };
        save_live_status(root, &status)
    }

    pub fn refresh_mode(&mut self, root: &Path, headless: bool) -> Result<(), SyncError> {
        let settings = load_settings(&settings_path(root))?;
        self.mode = if !settings.enabled {
            SyncMode::Disabled
        } else if headless {
            SyncMode::Headless
        } else {
            SyncMode::Enabled
        };
        Ok(())
    }

    pub fn mode(&self) -> SyncMode {
        self.mode
    }
}

impl GoogleDriveSyncCoordinator {
    pub fn start_live(&mut self, root: &Path) -> Result<LiveControl, SyncError> {
        let (sender, receiver) = mpsc::channel();
        if self.live.is_some() {
            return Err(SyncError::Integrity(
                "local observer is already running".into(),
            ));
        }
        let (live, mut control) = LiveObserverWorker::start(root.to_path_buf(), sender)?;
        self.live = Some(live);
        control.attach_status_events(receiver);
        Ok(control)
    }
}

fn log_sync_timing(phase: &str, elapsed: Duration, result: &Result<SyncOutcome, SyncError>) {
    let summary = match result {
        Ok(SyncOutcome::Offline) => "offline".to_owned(),
        Ok(SyncOutcome::UpToDate) => "up to date".to_owned(),
        Ok(SyncOutcome::Applied) => "remote changes applied".to_owned(),
        Ok(SyncOutcome::Published) => "local changes published".to_owned(),
        Ok(SyncOutcome::Conflicts(_)) => "conflicts require resolution".to_owned(),
        Err(SyncError::Integrity(message)) => {
            format!("sync integrity failure: {message}")
        }
        Err(error) => redacted_error(error),
    };
    log!(
        "Google Drive {phase} full sync finished in {} ms ({summary})",
        elapsed.as_millis()
    );
}

pub fn load_settings(path: &Path) -> Result<SyncSettings, SyncError> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SyncSettings::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn save_settings(path: &Path, settings: &SyncSettings) -> Result<(), SyncError> {
    let parent = path
        .parent()
        .ok_or_else(|| SyncError::InvalidPath(path.display().to_string()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".settings-{}", Uuid::new_v4()));
    let result = (|| {
        let bytes = serde_json::to_vec(settings)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        use std::io::Write;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(target_os = "android")]
        super::auth::android::notify_sync_enabled(settings.enabled);
        Ok::<_, SyncError>(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

pub fn settings_path(root: &Path) -> PathBuf {
    root.join(SYNC_DIR).join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::engine::SyncOutcome;
    use crate::sync::model::{CurrentSyncState, RelativePath};
    use crate::sync::status::{load_live_status, save_live_status, LiveStatus};
    use crate::sync::store::{MemoryRemoteStore, RemoteStore, SharedRemoteStore, StoreOperation};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn temporary_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!("touchhle-coordinator-{}", Uuid::new_v4()));
        fs::create_dir_all(path.join("touchHLE_apps")).unwrap();
        path
    }

    fn shared_coordinator(
        root: &Path,
        store: SharedRemoteStore,
        mode: SyncMode,
    ) -> SyncCoordinator<SharedRemoteStore> {
        SyncCoordinator::new(
            SyncEngine::new(
                store,
                root.to_path_buf(),
                root.join(SYNC_DIR).join("state.json"),
            ),
            mode,
        )
    }

    fn save_path() -> RelativePath {
        RelativePath::new("touchHLE_apps/save").unwrap()
    }

    fn current_paths(store: &SharedRemoteStore) -> Vec<RelativePath> {
        store
            .0
            .lock()
            .unwrap()
            .initial_inventory()
            .unwrap()
            .into_iter()
            .map(|file| file.path)
            .collect()
    }

    fn current_file_bytes(store: &SharedRemoteStore, path: &RelativePath) -> Option<Vec<u8>> {
        let mut store = store.0.lock().unwrap();
        let file = store
            .initial_inventory()
            .unwrap()
            .into_iter()
            .find(|file| file.path == *path)?;
        store.read_file(&file.id).unwrap()
    }

    #[derive(Clone)]
    struct OfflineStore {
        shared: SharedRemoteStore,
        offline: Arc<AtomicBool>,
        provider_secret: bool,
    }

    impl RemoteStore for OfflineStore {
        fn restore_checkpoint_indexes(
            &mut self,
            files: &std::collections::BTreeMap<String, RelativePath>,
            folders: &std::collections::BTreeMap<String, String>,
        ) -> Result<(), SyncError> {
            self.shared.restore_checkpoint_indexes(files, folders)
        }
        fn start_page_token(&mut self) -> Result<String, SyncError> {
            if self.offline.swap(false, Ordering::SeqCst) {
                return Err(if self.provider_secret {
                    SyncError::Provider("Bearer secret-sentinel response body".into())
                } else {
                    SyncError::Authentication("offline".into())
                });
            }
            self.shared.start_page_token()
        }
        fn initial_inventory(
            &mut self,
        ) -> Result<Vec<crate::sync::reconcile::RemoteFile>, SyncError> {
            self.shared.initial_inventory()
        }
        fn changes_since(
            &mut self,
            cursor: &str,
        ) -> Result<crate::sync::reconcile::RemoteChangeBatch, SyncError> {
            self.shared.changes_since(cursor)
        }
        fn file_metadata(
            &mut self,
            id: &str,
        ) -> Result<Option<crate::sync::reconcile::RemoteFile>, SyncError> {
            self.shared.file_metadata(id)
        }
        fn read_file(&mut self, id: &str) -> Result<Option<Vec<u8>>, SyncError> {
            self.shared.read_file(id)
        }
        fn write_file(
            &mut self,
            path: &RelativePath,
            existing_id: Option<&str>,
            bytes: &[u8],
        ) -> Result<crate::sync::reconcile::RemoteFile, SyncError> {
            self.shared.write_file(path, existing_id, bytes)
        }
        fn delete_file(&mut self, id: &str) -> Result<(), SyncError> {
            self.shared.delete_file(id)
        }
        fn list_commits(&mut self) -> Result<Vec<crate::sync::model::Commit>, SyncError> {
            self.shared.list_commits()
        }
        fn read_object(&mut self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError> {
            self.shared.read_object(hash)
        }
        fn write_object(&mut self, hash: [u8; 32], bytes: &[u8]) -> Result<(), SyncError> {
            self.shared.write_object(hash, bytes)
        }
        fn write_commit(&mut self, commit: &crate::sync::model::Commit) -> Result<(), SyncError> {
            self.shared.write_commit(commit)
        }
    }

    #[test]
    fn provider_details_are_redacted_from_local_only_launch() {
        let root = temporary_root();
        let secret = "Bearer secret-sentinel response body";
        let store = OfflineStore {
            shared: SharedRemoteStore::default(),
            offline: Arc::new(AtomicBool::new(true)),
            provider_secret: true,
        };
        let mut coordinator = SyncCoordinator::new(
            SyncEngine::new(store, root.clone(), root.join(SYNC_DIR).join("state.json")),
            SyncMode::Enabled,
        );
        let result = coordinator.before_launch().unwrap();
        assert_eq!(
            result,
            PreLaunchResult::LocalOnly("sync provider failure: unavailable".into())
        );
        assert!(!format!("{result:?}").contains(secret));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn device_id_is_stable_without_advancing_the_baseline() {
        let root = temporary_root();
        let state = root.join(SYNC_DIR).join("state.json");
        let mut engine = SyncEngine::new(MemoryRemoteStore::default(), root.clone(), state.clone());
        let first = engine.persisted_device_id().unwrap();
        assert_eq!(engine.persisted_device_id().unwrap(), first);
        let saved: CurrentSyncState = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        assert_eq!(saved.device_id, first);
        assert_eq!(saved.changes_cursor, None);
        assert!(saved.baseline.is_empty());
        assert!(engine.store().initial_inventory().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_first_sync_keeps_the_persisted_id_across_restart() {
        let root = temporary_root();
        fs::write(root.join("touchHLE_apps/save"), b"retry this local file").unwrap();
        let state = root.join(SYNC_DIR).join("state.json");
        let mut store = MemoryRemoteStore::default();
        store.fail_next(crate::sync::store::StoreOperation::ChangesSince);
        let mut engine = SyncEngine::new(store, root.clone(), state.clone());
        let id = engine.persisted_device_id().unwrap();
        assert!(engine.synchronize().is_err());
        let mut restarted = SyncEngine::new(MemoryRemoteStore::default(), root.clone(), state);
        assert_eq!(restarted.persisted_device_id().unwrap(), id);
        restarted.synchronize().unwrap();
        assert_eq!(restarted.persisted_device_id().unwrap(), id);
        assert_eq!(
            restarted
                .store()
                .initial_inventory()
                .unwrap()
                .first()
                .map(|file| file.path.as_str()),
            Some("touchHLE_apps/save")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_baseline_is_unchanged_when_getting_device_id() {
        let root = temporary_root();
        fs::write(root.join("touchHLE_apps/save"), b"save").unwrap();
        let state = root.join(SYNC_DIR).join("state.json");
        let mut engine = SyncEngine::new(MemoryRemoteStore::default(), root.clone(), state.clone());
        engine.synchronize().unwrap();
        let before = fs::read(&state).unwrap();
        let id = serde_json::from_slice::<CurrentSyncState>(&before)
            .unwrap()
            .device_id;
        assert_eq!(engine.persisted_device_id().unwrap(), id);
        assert_eq!(fs::read(&state).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_status_defaults_and_round_trips_outside_synced_roots() {
        let root = temporary_root();
        assert_eq!(load_live_status(&root).unwrap(), LiveStatus::default());
        let status = LiveStatus {
            active: true,
            queued: 2,
            error: Some("offline".into()),
            ..Default::default()
        };
        save_live_status(&root, &status).unwrap();
        assert_eq!(load_live_status(&root).unwrap(), status);
        assert!(root.join(SYNC_DIR).join("live-status.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn live_status_does_not_follow_symlinked_file() {
        let root = temporary_root();
        fs::create_dir_all(root.join(SYNC_DIR)).unwrap();
        let outside = root.join("outside.json");
        fs::write(&outside, b"untouched").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(SYNC_DIR).join("live-status.json")).unwrap();
        assert!(load_live_status(&root).is_err());
        save_live_status(&root, &LiveStatus::default()).unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
        assert_eq!(load_live_status(&root).unwrap(), LiveStatus::default());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn successful_full_sync_records_status_without_touching_upload_time() {
        let root = temporary_root();
        let engine = SyncEngine::new(
            MemoryRemoteStore::default(),
            root.clone(),
            root.join(SYNC_DIR).join("state.json"),
        );
        let mut coordinator = SyncCoordinator::new(engine, SyncMode::Enabled);
        assert_eq!(
            coordinator.before_launch().unwrap(),
            PreLaunchResult::Continue
        );
        let status = load_live_status(&root).unwrap();
        assert!(status.last_full_sync_unix_ms.is_some());
        assert_eq!(status.last_upload_unix_ms, None);
        assert_eq!(status.error, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_control_delivers_status_to_picker() {
        let root = temporary_root();
        let engine = SyncEngine::new(
            MemoryRemoteStore::default(),
            root.clone(),
            root.join(SYNC_DIR).join("state.json"),
        );
        let mut coordinator = SyncCoordinator::new(engine, SyncMode::Enabled);
        let control = coordinator
            .start_live_with_store(&root, MemoryRemoteStore::default())
            .unwrap();
        let status = control
            .try_status_event()
            .expect("initial status reaches picker");
        assert_eq!(status.queued, 0);
        coordinator.stop_live().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_full_sync_records_redacted_provider_error_and_keeps_id() {
        let root = temporary_root();
        let state = root.join(SYNC_DIR).join("state.json");
        let mut store = MemoryRemoteStore::default();
        store.fail_next(crate::sync::store::StoreOperation::ChangesSince);
        let engine = SyncEngine::new(store, root.clone(), state);
        let mut coordinator = SyncCoordinator::new(engine, SyncMode::Enabled);
        assert!(matches!(
            coordinator.before_launch().unwrap(),
            PreLaunchResult::LocalOnly(_)
        ));
        let status = load_live_status(&root).unwrap();
        assert_eq!(
            status.error.as_deref(),
            Some("sync provider failure: unavailable")
        );
        assert_eq!(status.last_full_sync_unix_ms, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_storage_failure_does_not_abort_successful_full_sync() {
        let root = temporary_root();
        let state = root.join(SYNC_DIR).join("state.json");
        let sync_dir = root.join(SYNC_DIR);
        let mut seed = SyncEngine::new(MemoryRemoteStore::default(), root.clone(), state.clone());
        seed.synchronize().unwrap();
        let store = seed.into_store();
        fs::write(
            root.join("touchHLE_apps/save"),
            b"published despite status failure",
        )
        .unwrap();
        let status_target = sync_dir.join("live-status.json");
        fs::create_dir_all(&sync_dir).unwrap();
        fs::create_dir(&status_target).unwrap();
        let engine = SyncEngine::new(store, root.clone(), state.clone());
        let mut coordinator = SyncCoordinator::new(engine, SyncMode::Enabled);

        let result = coordinator.before_launch();
        assert_eq!(result.unwrap(), PreLaunchResult::Continue);
        assert!(load_live_status(&root).is_err());
        let saved: CurrentSyncState = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        assert!(saved.changes_cursor.is_some());
        assert!(saved.baseline.contains_key(&save_path()));
        assert!(status_target.is_dir());
        assert_eq!(
            coordinator
                .engine
                .store()
                .initial_inventory()
                .unwrap()
                .len(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_storage_failure_does_not_replace_full_sync_error() {
        let root = temporary_root();
        let state = root.join(SYNC_DIR).join("state.json");
        let sync_dir = root.join(SYNC_DIR);
        let mut seed = SyncEngine::new(MemoryRemoteStore::default(), root.clone(), state.clone());
        seed.synchronize().unwrap();
        let mut store = seed.into_store();
        store.fail_next(crate::sync::store::StoreOperation::ChangesSince);
        let checkpoint_before = fs::read(&state).unwrap();
        let status_target = sync_dir.join("live-status.json");
        fs::create_dir(&status_target).unwrap();
        let engine = SyncEngine::new(store, root.clone(), state);
        let mut coordinator = SyncCoordinator::new(engine, SyncMode::Enabled);

        let result = coordinator.after_exit();
        assert!(
            matches!(result, Err(SyncError::Provider(message)) if message == "injected ChangesSince failure")
        );
        assert_eq!(
            fs::read(root.join(SYNC_DIR).join("state.json")).unwrap(),
            checkpoint_before
        );
        assert!(load_live_status(&root).is_err());
        assert!(status_target.is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_default_to_disabled_and_round_trip() {
        let root = temporary_root();
        let path = settings_path(&root);
        assert_eq!(load_settings(&path).unwrap(), SyncSettings::default());
        let settings = SyncSettings { enabled: true };
        save_settings(&path, &settings).unwrap();
        assert_eq!(load_settings(&path).unwrap(), settings);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_select_enabled_or_headless_mode() {
        let root = temporary_root();
        let engine = SyncEngine::new(
            MemoryRemoteStore::default(),
            root.clone(),
            root.join(SYNC_DIR).join("state.json"),
        );
        let disabled = SyncCoordinator::from_settings(engine, &root, false).unwrap();
        assert_eq!(disabled.mode(), SyncMode::Disabled);

        save_settings(&settings_path(&root), &SyncSettings { enabled: true }).unwrap();
        let engine = SyncEngine::new(
            MemoryRemoteStore::default(),
            root.clone(),
            root.join(SYNC_DIR).join("state.json"),
        );
        let graphical = SyncCoordinator::from_settings(engine, &root, false).unwrap();
        assert_eq!(graphical.mode(), SyncMode::Enabled);

        let engine = SyncEngine::new(
            MemoryRemoteStore::default(),
            root.clone(),
            root.join(SYNC_DIR).join("state.json"),
        );
        let headless = SyncCoordinator::from_settings(engine, &root, true).unwrap();
        assert_eq!(headless.mode(), SyncMode::Headless);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refresh_mode_observes_settings_changed_in_the_app_picker() {
        let root = temporary_root();
        let engine = SyncEngine::new(
            MemoryRemoteStore::default(),
            root.clone(),
            root.join(SYNC_DIR).join("state.json"),
        );
        let mut coordinator = SyncCoordinator::new(engine, SyncMode::Disabled);
        save_settings(&settings_path(&root), &SyncSettings { enabled: true }).unwrap();

        coordinator.refresh_mode(&root, false).unwrap();

        assert_eq!(coordinator.mode(), SyncMode::Enabled);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disabled_mode_does_not_contact_remote_store() {
        let root = temporary_root();
        let state = root.join(SYNC_DIR).join("state.json");
        let engine = SyncEngine::new(MemoryRemoteStore::default(), root.clone(), state.clone());
        let mut coordinator = SyncCoordinator::new(engine, SyncMode::Disabled);
        assert!(matches!(
            coordinator.before_launch().unwrap(),
            PreLaunchResult::LocalOnly(_)
        ));
        assert!(!state.exists());
        assert!(matches!(
            coordinator.after_exit().unwrap(),
            SyncOutcome::Offline
        ));
        assert!(!state.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enabled_mode_publishes_before_launch_and_after_exit() {
        let root = temporary_root();
        fs::write(root.join("touchHLE_apps/save"), b"local").unwrap();
        let state = root.join(SYNC_DIR).join("state.json");
        let engine = SyncEngine::new(MemoryRemoteStore::default(), root.clone(), state);
        let mut coordinator = SyncCoordinator::new(engine, SyncMode::Enabled);
        assert_eq!(
            coordinator.before_launch().unwrap(),
            PreLaunchResult::Continue
        );
        assert!(matches!(
            coordinator.after_exit().unwrap(),
            SyncOutcome::UpToDate
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn background_sync_makes_no_remote_calls_under_guest_lock_then_publishes_current_file() {
        let root = temporary_root();
        fs::write(
            root.join("touchHLE_apps/save"),
            b"saved before process death",
        )
        .unwrap();
        let shared = SharedRemoteStore::default();
        let mut coordinator = shared_coordinator(&root, shared.clone(), SyncMode::Enabled);
        coordinator.engine.persisted_device_id().unwrap();
        let state_path = root.join(SYNC_DIR).join("state.json");
        let original_state = fs::read(&state_path).unwrap();
        let initial_cursor = serde_json::from_slice::<CurrentSyncState>(&original_state)
            .unwrap()
            .changes_cursor;
        let guest = coordinator.begin_guest_session().unwrap();
        fs::write(root.join("touchHLE_apps/save"), b"changed while playing").unwrap();
        let (before_transfers, before_requests) = {
            let store = shared.0.lock().unwrap();
            (store.content_transfer_count(), store.change_request_count())
        };

        assert_eq!(
            coordinator.background_reconcile().unwrap(),
            BackgroundSyncResult::GuestActive
        );
        assert!(current_paths(&shared).is_empty());
        assert_eq!(fs::read(&state_path).unwrap(), original_state);
        let store = shared.0.lock().unwrap();
        assert_eq!(store.content_transfer_count(), before_transfers);
        assert_eq!(store.change_request_count(), before_requests);
        drop(store);
        assert_eq!(
            serde_json::from_slice::<CurrentSyncState>(&fs::read(&state_path).unwrap())
                .unwrap()
                .changes_cursor,
            initial_cursor
        );
        assert_eq!(
            load_live_status(&root).unwrap().background_sync,
            BackgroundSyncState::Deferred
        );

        drop(guest);
        assert_eq!(
            coordinator.background_reconcile().unwrap(),
            BackgroundSyncResult::Completed
        );
        assert_eq!(
            current_file_bytes(&shared, &save_path()).as_deref(),
            Some(&b"changed while playing"[..])
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn foreground_full_sync_defers_without_touching_checkpoint_or_remote() {
        let root = temporary_root();
        let shared = SharedRemoteStore::default();
        let mut coordinator = shared_coordinator(&root, shared.clone(), SyncMode::Enabled);
        assert_eq!(
            coordinator.before_launch().unwrap(),
            PreLaunchResult::Continue
        );
        let state_path = root.join(SYNC_DIR).join("state.json");
        let before = fs::read(&state_path).unwrap();
        let cursor = serde_json::from_slice::<CurrentSyncState>(&before)
            .unwrap()
            .changes_cursor;
        let guest = coordinator.begin_guest_session().unwrap();
        fs::write(root.join("touchHLE_apps/save"), b"guest data").unwrap();
        let (requests, transfers) = {
            let guard = shared.0.lock().unwrap();
            (guard.change_request_count(), guard.content_transfer_count())
        };
        assert_eq!(
            coordinator.before_launch().unwrap(),
            PreLaunchResult::LocalOnly("guest session is active; local files are unchanged".into())
        );
        assert!(matches!(
            coordinator.after_exit().unwrap(),
            SyncOutcome::Offline
        ));
        assert_eq!(fs::read(&state_path).unwrap(), before);
        assert_eq!(
            serde_json::from_slice::<CurrentSyncState>(&fs::read(&state_path).unwrap())
                .unwrap()
                .changes_cursor,
            cursor
        );
        let guard = shared.0.lock().unwrap();
        assert_eq!(guard.change_request_count(), requests);
        assert_eq!(guard.content_transfer_count(), transfers);
        drop(guard);
        drop(guest);
        assert!(matches!(
            coordinator.after_exit().unwrap(),
            SyncOutcome::Published
        ));
        assert_eq!(
            current_file_bytes(&shared, &save_path()).as_deref(),
            Some(&b"guest data"[..])
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn locked_worker_does_not_consume_remote_request() {
        let root = temporary_root();
        let shared = SharedRemoteStore::default();
        let mut coordinator = shared_coordinator(&root, shared.clone(), SyncMode::Enabled);
        assert_eq!(
            coordinator.before_launch().unwrap(),
            PreLaunchResult::Continue
        );
        let checkpoint = fs::read(root.join(SYNC_DIR).join("state.json")).unwrap();
        shared
            .0
            .lock()
            .unwrap()
            .fail_next(StoreOperation::ChangesSince);
        let guest = coordinator.begin_guest_session().unwrap();
        assert_eq!(
            coordinator.background_reconcile().unwrap(),
            BackgroundSyncResult::GuestActive
        );
        assert_eq!(
            fs::read(root.join(SYNC_DIR).join("state.json")).unwrap(),
            checkpoint
        );
        drop(guest);
        assert_eq!(
            coordinator.background_reconcile().unwrap(),
            BackgroundSyncResult::RetryableFailure
        );
        assert_eq!(
            fs::read(root.join(SYNC_DIR).join("state.json")).unwrap(),
            checkpoint
        );
        assert_eq!(
            coordinator.background_reconcile().unwrap(),
            BackgroundSyncResult::Completed
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn background_sync_does_not_touch_remote_store_when_disabled() {
        let root = temporary_root();
        let shared = SharedRemoteStore::default();
        let mut coordinator = shared_coordinator(&root, shared.clone(), SyncMode::Disabled);

        assert_eq!(
            coordinator.background_reconcile().unwrap(),
            BackgroundSyncResult::Disabled
        );
        assert!(current_paths(&shared).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn foreground_only_auth_is_not_treated_as_a_transient_network_error() {
        for reason in [
            AuthError::MissingCredentials,
            AuthError::ReauthorizationRequired,
            AuthError::ForegroundRequired,
        ] {
            assert!(needs_foreground_authorization(&SyncError::Authentication(
                reason.to_string()
            )));
        }
        assert!(!needs_foreground_authorization(&SyncError::Authentication(
            "Google authorization failed: network unavailable".into()
        )));
    }

    #[test]
    fn background_sync_failure_keeps_local_save_and_retry_publishes_it() {
        let root = temporary_root();
        let save = root.join("touchHLE_apps/save");
        fs::write(&save, b"keep this save").unwrap();
        let shared = SharedRemoteStore::default();
        shared
            .0
            .lock()
            .unwrap()
            .fail_next(StoreOperation::ChangesSince);
        let mut coordinator = shared_coordinator(&root, shared.clone(), SyncMode::Enabled);

        assert_eq!(
            coordinator.background_reconcile().unwrap(),
            BackgroundSyncResult::RetryableFailure
        );
        assert_eq!(fs::read(&save).unwrap(), b"keep this save");
        assert!(current_paths(&shared).is_empty());
        assert_eq!(
            load_live_status(&root).unwrap().background_sync,
            BackgroundSyncState::Failed
        );

        drop(coordinator);
        let mut coordinator = shared_coordinator(&root, shared.clone(), SyncMode::Enabled);
        assert_eq!(
            coordinator.background_reconcile().unwrap(),
            BackgroundSyncResult::Completed
        );
        assert_eq!(
            current_file_bytes(&shared, &save_path()).as_deref(),
            Some(&b"keep this save"[..])
        );
        assert_eq!(
            load_live_status(&root).unwrap().background_sync,
            BackgroundSyncState::Completed
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn headless_mode_returns_plan_instead_of_choosing_a_conflict() {
        let local_root = temporary_root();
        fs::write(local_root.join("touchHLE_apps/save"), b"local").unwrap();
        let remote_root = temporary_root();
        fs::write(remote_root.join("touchHLE_apps/save"), b"remote").unwrap();
        let remote_engine = SyncEngine::new(
            MemoryRemoteStore::default(),
            remote_root.clone(),
            remote_root.join(SYNC_DIR).join("state.json"),
        );
        let mut remote_engine = remote_engine;
        remote_engine.synchronize().unwrap();
        let store = remote_engine.into_store();
        let engine = SyncEngine::new(
            store,
            local_root.clone(),
            local_root.join(SYNC_DIR).join("state.json"),
        );
        let mut coordinator = SyncCoordinator::new(engine, SyncMode::Headless);

        assert!(matches!(
            coordinator.before_launch().unwrap(),
            PreLaunchResult::NeedsResolution(_)
        ));
        assert_eq!(
            fs::read(local_root.join("touchHLE_apps/save")).unwrap(),
            b"local"
        );
        fs::remove_dir_all(local_root).unwrap();
        fs::remove_dir_all(remote_root).unwrap();
    }
}
