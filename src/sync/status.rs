/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::model::SyncError;
use crate::paths::SYNC_DIR;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use uuid::Uuid;

const STATUS_FILE: &str = "live-status.json";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundSyncState {
    #[default]
    Idle,
    Running,
    Deferred,
    NeedsAuthorization,
    Completed,
    Conflict,
    Failed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LiveStatus {
    pub active: bool,
    pub queued: usize,
    pub last_upload_unix_ms: Option<i64>,
    pub last_full_sync_unix_ms: Option<i64>,
    pub error: Option<String>,
    #[serde(default)]
    pub background_sync: BackgroundSyncState,
}

fn status_dir(root: &Path, create: bool) -> Result<Option<Dir>, SyncError> {
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
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    match root.open_dir_nofollow(SYNC_DIR) {
        Ok(dir) => Ok(Some(dir)),
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn load_live_status(root: &Path) -> Result<LiveStatus, SyncError> {
    let Some(dir) = status_dir(root, false)? else {
        return Ok(LiveStatus::default());
    };
    match dir.symlink_metadata(STATUS_FILE) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(SyncError::Integrity("status is not a regular file".into()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LiveStatus::default());
        }
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = dir.open_with(STATUS_FILE, &options)?;
    if !file.metadata()?.is_file() {
        return Err(SyncError::Integrity("status is not a regular file".into()));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn save_live_status(root: &Path, status: &LiveStatus) -> Result<(), SyncError> {
    let dir = status_dir(root, true)?.unwrap();
    let temporary = format!(".live-status-{}", Uuid::new_v4());
    let result = (|| {
        let mut file =
            dir.open_with(&temporary, OpenOptions::new().write(true).create_new(true))?;
        file.write_all(&serde_json::to_vec(status)?)?;
        file.sync_all()?;
        drop(file);
        dir.rename(&temporary, &dir, STATUS_FILE)?;
        Ok::<_, SyncError>(())
    })();
    let _ = dir.remove_file(&temporary);
    result
}

/// Do not serialize provider details: they may contain HTTP response bodies.
pub fn redacted_error(error: &SyncError) -> String {
    match error {
        SyncError::Authentication(_) => SyncError::Authentication("unavailable".into()).to_string(),
        SyncError::Provider(message) => {
            let detail = if message.ends_with(": ConfigInvalid") {
                "ConfigInvalid"
            } else {
                "unavailable"
            };
            SyncError::Provider(detail.into()).to_string()
        }
        SyncError::Io(_) => SyncError::Integrity("local I/O failed".into()).to_string(),
        SyncError::Serialization(_) => {
            SyncError::Integrity("invalid local status or sync data".into()).to_string()
        }
        SyncError::InvalidPath(_) => SyncError::InvalidPath("local path".into()).to_string(),
        SyncError::Integrity(_) => SyncError::Integrity("validation failed".into()).to_string(),
        SyncError::RemotePathExists => SyncError::RemotePathExists.to_string(),
        SyncError::UnresolvedConflicts(_) => {
            SyncError::UnresolvedConflicts("resolution required".into()).to_string()
        }
        SyncError::MigrationRequired => SyncError::MigrationRequired.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_response_body_is_not_saved_as_error_text() {
        let root = std::env::temp_dir().join(format!("touchhle-status-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let mut status = LiveStatus::default();
        status.error = Some(redacted_error(&SyncError::Provider(
            "HTTP 500 response: secret body".into(),
        )));
        save_live_status(&root, &status).unwrap();
        let saved = std::fs::read(root.join(SYNC_DIR).join(STATUS_FILE)).unwrap();
        assert!(!String::from_utf8(saved).unwrap().contains("secret body"));
        assert_eq!(load_live_status(&root).unwrap().error, status.error);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_exact_config_invalid_provider_code_survives_redaction() {
        assert_eq!(
            redacted_error(&SyncError::Provider(
                "prepare Google Drive folder: ConfigInvalid".into()
            )),
            "sync provider failure: ConfigInvalid"
        );
        let unsafe_error = redacted_error(&SyncError::Provider(
            "secret response body: ConfigInvalid extra-token".into(),
        ));
        assert_eq!(unsafe_error, "sync provider failure: unavailable");
    }

    #[test]
    fn background_sync_state_round_trips_and_defaults_for_legacy_status() {
        let mut status = LiveStatus::default();
        status.background_sync = BackgroundSyncState::Running;
        let encoded = serde_json::to_vec(&status).unwrap();
        assert_eq!(
            serde_json::from_slice::<LiveStatus>(&encoded)
                .unwrap()
                .background_sync,
            BackgroundSyncState::Running
        );
        status.background_sync = BackgroundSyncState::NeedsAuthorization;
        assert_eq!(
            serde_json::from_slice::<LiveStatus>(&serde_json::to_vec(&status).unwrap())
                .unwrap()
                .background_sync,
            BackgroundSyncState::NeedsAuthorization
        );

        let legacy = br#"{"active":false,"queued":0,"last_upload_unix_ms":null,"last_full_sync_unix_ms":null,"error":null}"#;
        assert_eq!(
            serde_json::from_slice::<LiveStatus>(legacy)
                .unwrap()
                .background_sync,
            BackgroundSyncState::Idle
        );
    }
}
