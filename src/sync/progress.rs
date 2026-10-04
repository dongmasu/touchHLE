/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncProgress {
    RescanningLocalFiles,
    Scanning {
        files: usize,
        bytes: u64,
    },
    CheckingRemote,
    Comparing,
    Uploading {
        files_done: usize,
        files_total: usize,
        bytes_done: u64,
        bytes_total: u64,
    },
    Downloading {
        files_done: usize,
        files_total: usize,
        bytes_done: u64,
        bytes_total: u64,
    },
    Applying {
        files_total: usize,
    },
    Finalizing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncProgressDisplay {
    pub headline: &'static str,
    pub detail: String,
    pub active_step: usize,
    pub track_progress: u16,
}

impl SyncProgress {
    pub fn display(&self) -> SyncProgressDisplay {
        let (headline, detail, active_step, track_progress) = match self {
            Self::RescanningLocalFiles => (
                "Checking local files",
                "Checking the last saved game files before shutdown.".to_owned(),
                0,
                66,
            ),
            Self::Scanning { files, bytes } => (
                "Scanning local game data",
                format!("{files} files checked - {}", format_sync_bytes(*bytes)),
                0,
                66,
            ),
            Self::CheckingRemote => (
                "Checking Google Drive",
                "Reading the latest saved revisions.".to_owned(),
                1,
                205,
            ),
            Self::Comparing => (
                "Comparing changes",
                "Preparing only the files that need to move.".to_owned(),
                1,
                205,
            ),
            Self::Uploading {
                files_done,
                files_total,
                bytes_done,
                bytes_total,
            } => (
                "Uploading local changes",
                format!(
                    "{files_done} of {files_total} files - {} of {}",
                    format_sync_bytes(*bytes_done),
                    format_sync_bytes(*bytes_total)
                ),
                2,
                356,
            ),
            Self::Downloading {
                files_done,
                files_total,
                bytes_done,
                bytes_total,
            } => (
                "Downloading remote changes",
                format!(
                    "{files_done} of {files_total} files - {} of {}",
                    format_sync_bytes(*bytes_done),
                    format_sync_bytes(*bytes_total)
                ),
                2,
                356,
            ),
            Self::Applying { files_total } => (
                "Applying saved changes",
                format!("Updating {files_total} local files."),
                3,
                526,
            ),
            Self::Finalizing => (
                "Finishing sync",
                "Saving the completed sync state.".to_owned(),
                3,
                526,
            ),
        };
        SyncProgressDisplay {
            headline,
            detail,
            active_step,
            track_progress,
        }
    }
}

fn format_sync_bytes(bytes: u64) -> String {
    if bytes < 1024 * 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_display_reports_phase_and_transfer_counts() {
        let scanning = SyncProgress::Scanning {
            files: 3,
            bytes: 2048,
        }
        .display();
        assert_eq!(scanning.headline, "Scanning local game data");
        assert_eq!(scanning.detail, "3 files checked - 2 KB");
        assert_eq!(scanning.active_step, 0);
        assert_eq!(scanning.track_progress, 66);

        let uploading = SyncProgress::Uploading {
            files_done: 2,
            files_total: 5,
            bytes_done: 1024 * 1024,
            bytes_total: 2 * 1024 * 1024,
        }
        .display();
        assert_eq!(uploading.headline, "Uploading local changes");
        assert_eq!(uploading.detail, "2 of 5 files - 1.0 MB of 2.0 MB");
        assert_eq!(uploading.active_step, 2);
        assert_eq!(uploading.track_progress, 356);

        let applying = SyncProgress::Applying { files_total: 4 }.display();
        assert_eq!(applying.detail, "Updating 4 local files.");
        assert_eq!(applying.active_step, 3);
        assert_eq!(applying.track_progress, 526);
    }

    #[test]
    fn sync_byte_format_matches_desktop_units() {
        assert_eq!(format_sync_bytes(0), "0 KB");
        assert_eq!(format_sync_bytes(1024 * 1024), "1.0 MB");
    }
}
