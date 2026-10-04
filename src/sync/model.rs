/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::reconcile::FileBaseline;
use crate::paths::{APPS_DIR, SANDBOX_DIR};
use serde::{
    de::{Error as _, MapAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::collections::BTreeMap;
use std::fmt;
use unicode_casefold::UnicodeCaseFold;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RelativePath(String);

impl RelativePath {
    pub fn new(path: &str) -> Result<Self, SyncError> {
        if path.chars().any(|c| matches!(c, '\\' | '\0' | ':')) || path.starts_with('/') {
            return Err(SyncError::InvalidPath(path.to_owned()));
        }
        let mut parts = path.split('/');
        if !matches!(parts.next(), Some(APPS_DIR | SANDBOX_DIR))
            || parts.clone().next().is_none()
            || parts.any(invalid_component)
        {
            return Err(SyncError::InvalidPath(path.to_owned()));
        }
        Ok(Self(path.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub fn is_ignored_sync_file(path: &RelativePath) -> bool {
    let Some(name) = path.as_str().rsplit('/').next() else {
        return false;
    };
    [".DS_Store", "Thumbs.db", "ehthumbs.db", "desktop.ini"]
        .iter()
        .any(|ignored| name.eq_ignore_ascii_case(ignored))
}

fn invalid_component(part: &str) -> bool {
    if part.is_empty()
        || part == "."
        || part == ".."
        || part.ends_with(['.', ' '])
        || part
            .chars()
            .any(|c| c <= '\u{1f}' || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return true;
    }
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ');
    let reserved = stem.to_ascii_uppercase();
    matches!(reserved.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (reserved.len() == 4
            && (reserved.starts_with("COM") || reserved.starts_with("LPT"))
            && matches!(reserved.as_bytes()[3], b'1'..=b'9'))
        || (stem.get(..3).is_some_and(|prefix| {
            prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT")
        }) && stem[3..].chars().count() == 1
            && matches!(stem[3..].chars().next(), Some('¹' | '²' | '³')))
}

impl<'de> Deserialize<'de> for RelativePath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SnapshotEntry {
    File {
        sha256: [u8; 32],
        size: u64,
        modified_unix_ms: i64,
    },
    Tombstone,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LocalOrRemote {
    Local,
    Remote,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Commit {
    pub id: Uuid,
    pub device_id: Uuid,
    pub created_unix_ms: i64,
    pub parents: Vec<Uuid>,
    #[serde(deserialize_with = "deserialize_entries")]
    pub entries: BTreeMap<RelativePath, SnapshotEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    pub device_id: Uuid,
    pub last_applied_commit: Option<Uuid>,
    #[serde(deserialize_with = "deserialize_entries")]
    pub baseline: BTreeMap<RelativePath, SnapshotEntry>,
}

pub type LegacySyncState = SyncState;

pub const CURRENT_SCHEMA_VERSION: u32 = 3;
const PREVIOUS_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentSyncState {
    pub schema_version: u32,
    pub device_id: Uuid,
    pub account_id: Option<String>,
    pub root_folder_id: Option<String>,
    pub changes_cursor: Option<String>,
    #[serde(default)]
    pub migration_marker_confirmed: bool,
    #[serde(deserialize_with = "deserialize_baseline")]
    pub baseline: BTreeMap<RelativePath, FileBaseline>,
    pub file_paths_by_id: BTreeMap<String, RelativePath>,
    pub folder_paths_by_id: BTreeMap<String, String>,
}

impl CurrentSyncState {
    pub fn new(device_id: Uuid) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            device_id,
            account_id: None,
            root_folder_id: None,
            changes_cursor: None,
            migration_marker_confirmed: false,
            baseline: BTreeMap::new(),
            file_paths_by_id: BTreeMap::new(),
            folder_paths_by_id: BTreeMap::new(),
        }
    }

    // A deleted path is useful only while this installation still has local
    // bytes to compare against the last observed value.
    pub fn prune_deleted_paths(
        &mut self,
        local: &BTreeMap<RelativePath, SnapshotEntry>,
        remote: &BTreeMap<RelativePath, super::reconcile::RemoteFile>,
    ) {
        self.baseline
            .retain(|path, _| local.contains_key(path) || remote.contains_key(path));
        self.file_paths_by_id.retain(|id, path| {
            is_ignored_sync_file(path)
                || self
                    .baseline
                    .get(path)
                    .is_some_and(|baseline| baseline.remote_id.as_deref() == Some(id.as_str()))
        });
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadedSyncState {
    Current(CurrentSyncState),
    Legacy(LegacySyncState),
    Missing,
}

pub fn load_sync_state(bytes: Option<&[u8]>) -> Result<LoadedSyncState, SyncError> {
    let Some(bytes) = bytes else {
        return Ok(LoadedSyncState::Missing);
    };
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| SyncError::Integrity(format!("invalid sync checkpoint: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| SyncError::Integrity("sync checkpoint must be a JSON object".into()))?;
    if object.contains_key("schema_version") {
        let state: CurrentSyncState = serde_json::from_slice(bytes).map_err(|error| {
            SyncError::Integrity(format!("invalid current checkpoint: {error}"))
        })?;
        match state.schema_version {
            CURRENT_SCHEMA_VERSION => return Ok(LoadedSyncState::Current(state)),
            PREVIOUS_SCHEMA_VERSION => {
                let mut migrated = state;
                migrated.schema_version = CURRENT_SCHEMA_VERSION;
                migrated.changes_cursor = None;
                migrated.migration_marker_confirmed = false;
                migrated.file_paths_by_id.clear();
                migrated.folder_paths_by_id.clear();
                return Ok(LoadedSyncState::Current(migrated));
            }
            _ => {
                return Err(SyncError::Integrity(
                    "unsupported sync checkpoint version".into(),
                ));
            }
        }
    }
    if !object.contains_key("device_id")
        || !object.contains_key("last_applied_commit")
        || !object.contains_key("baseline")
    {
        return Err(SyncError::Integrity(
            "incomplete legacy sync checkpoint".into(),
        ));
    }
    let state = serde_json::from_slice(bytes)
        .map_err(|error| SyncError::Integrity(format!("invalid legacy checkpoint: {error}")))?;
    Ok(LoadedSyncState::Legacy(state))
}

fn deserialize_baseline<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<RelativePath, FileBaseline>, D::Error> {
    struct BaselineVisitor;
    impl<'de> Visitor<'de> for BaselineVisitor {
        type Value = BTreeMap<RelativePath, FileBaseline>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a baseline with distinct paths")
        }

        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut entries: BTreeMap<RelativePath, FileBaseline> = BTreeMap::new();
            while let Some((path, baseline)) = map.next_entry()? {
                if entries.insert(path, baseline).is_some() {
                    return Err(M::Error::custom("duplicate baseline path"));
                }
            }
            let paths = entries
                .iter()
                .map(|(path, baseline)| {
                    (
                        path.clone(),
                        baseline.sha256.map_or(SnapshotEntry::Tombstone, |sha256| {
                            SnapshotEntry::File {
                                sha256,
                                size: 0,
                                modified_unix_ms: 0,
                            }
                        }),
                    )
                })
                .collect();
            validate_entries(&paths).map_err(M::Error::custom)?;
            Ok(entries)
        }
    }
    deserializer.deserialize_map(BaselineVisitor)
}

pub(crate) fn validate_entries(
    entries: &BTreeMap<RelativePath, SnapshotEntry>,
) -> Result<(), String> {
    let mut seen = BTreeMap::<String, String>::new();
    for key in entries.keys() {
        let mut prefix = String::new();
        for part in key.as_str().split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let folded = prefix.as_str().case_fold().collect::<String>();
            if let Some(previous) = seen.insert(folded, prefix.clone()) {
                if previous != prefix {
                    return Err(format!(
                        "case-insensitive collision: {previous} and {prefix}"
                    ));
                }
            }
        }
    }
    for (key, entry) in entries {
        if !matches!(entry, SnapshotEntry::File { .. }) {
            continue;
        }
        let mut prefix = key.as_str();
        while let Some((parent, _)) = prefix.rsplit_once('/') {
            if matches!(
                entries.get(&RelativePath(parent.to_owned())),
                Some(SnapshotEntry::File { .. })
            ) {
                return Err(format!(
                    "live file is an ancestor of another live file: {parent} and {}",
                    key.as_str()
                ));
            }
            prefix = parent;
        }
    }
    Ok(())
}

fn deserialize_entries<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<RelativePath, SnapshotEntry>, D::Error> {
    struct EntriesVisitor;

    impl<'de> Visitor<'de> for EntriesVisitor {
        type Value = BTreeMap<RelativePath, SnapshotEntry>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a snapshot with distinct case-insensitive paths")
        }

        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut entries = BTreeMap::new();
            while let Some((key, entry)) = map.next_entry::<RelativePath, SnapshotEntry>()? {
                if entries.insert(key, entry).is_some() {
                    return Err(M::Error::custom("duplicate snapshot path"));
                }
            }
            validate_entries(&entries).map_err(M::Error::custom)?;
            Ok(entries)
        }
    }

    deserializer.deserialize_map(EntriesVisitor)
}

#[derive(Debug)]
pub enum SyncError {
    Io(std::io::Error),
    InvalidPath(String),
    Serialization(serde_json::Error),
    Integrity(String),
    RemotePathExists,
    Provider(String),
    Authentication(String),
    UnresolvedConflicts(String),
    MigrationRequired,
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "sync I/O error: {e}"),
            Self::InvalidPath(path) => write!(f, "invalid sync path: {path}"),
            Self::Serialization(e) => write!(f, "sync serialization error: {e}"),
            Self::Integrity(message) => write!(f, "sync integrity failure: {message}"),
            Self::RemotePathExists => write!(f, "remote sync path already exists"),
            Self::Provider(message) => write!(f, "sync provider failure: {message}"),
            Self::Authentication(message) => write!(f, "sync authentication failure: {message}"),
            Self::UnresolvedConflicts(message) => write!(f, "unresolved sync conflicts: {message}"),
            Self::MigrationRequired => write!(f, "legacy sync state requires migration"),
        }
    }
}

impl std::error::Error for SyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Serialization(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SyncError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for SyncError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::reconcile::FileBaseline;
    use std::collections::BTreeMap;

    #[test]
    fn commits_and_state_round_trip_with_stable_json() {
        let first = RelativePath::new("touchHLE_apps/a.ipa").unwrap();
        let second = RelativePath::new("touchHLE_sandbox/save/data").unwrap();
        let entries = BTreeMap::from([
            (
                first,
                SnapshotEntry::File {
                    sha256: [42; 32],
                    size: 12,
                    modified_unix_ms: -100,
                },
            ),
            (second, SnapshotEntry::Tombstone),
        ]);
        let device_id = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let commit = Commit {
            id: Uuid::new_v4(),
            device_id,
            created_unix_ms: 100,
            parents: vec![parent],
            entries: entries.clone(),
        };
        let state = SyncState {
            device_id,
            last_applied_commit: Some(commit.id),
            baseline: entries,
        };

        let commit_json = serde_json::to_string(&commit).unwrap();
        let state_json = serde_json::to_string(&state).unwrap();
        assert_eq!(
            serde_json::from_str::<Commit>(&commit_json).unwrap(),
            commit
        );
        assert_eq!(
            serde_json::from_str::<SyncState>(&state_json).unwrap(),
            state
        );
        assert_eq!(serde_json::to_string(&commit).unwrap(), commit_json);
        assert_eq!(serde_json::to_string(&state).unwrap(), state_json);
        assert!(
            commit_json.find("touchHLE_apps").unwrap()
                < commit_json.find("touchHLE_sandbox").unwrap()
        );
    }

    #[test]
    fn loader_distinguishes_missing_legacy_and_current_checkpoints() {
        assert!(matches!(
            load_sync_state(None).unwrap(),
            LoadedSyncState::Missing
        ));
        let old = SyncState {
            device_id: Uuid::new_v4(),
            last_applied_commit: Some(Uuid::new_v4()),
            baseline: BTreeMap::from([(
                RelativePath::new("touchHLE_apps/save").unwrap(),
                SnapshotEntry::Tombstone,
            )]),
        };
        assert!(matches!(
            load_sync_state(Some(&serde_json::to_vec(&old).unwrap())).unwrap(),
            LoadedSyncState::Legacy(state) if state == old
        ));
        let path = RelativePath::new("touchHLE_apps/save").unwrap();
        let current = CurrentSyncState {
            schema_version: CURRENT_SCHEMA_VERSION,
            device_id: Uuid::new_v4(),
            account_id: Some("account".into()),
            root_folder_id: Some("root".into()),
            changes_cursor: Some("cursor".into()),
            migration_marker_confirmed: false,
            baseline: BTreeMap::from([(
                path.clone(),
                FileBaseline {
                    sha256: Some([42; 32]),
                    remote_id: Some("id".into()),
                    remote_version: Some("9".into()),
                },
            )]),
            file_paths_by_id: BTreeMap::from([("id".into(), path.clone())]),
            folder_paths_by_id: BTreeMap::new(),
        };
        assert!(matches!(
            load_sync_state(Some(&serde_json::to_vec(&current).unwrap())).unwrap(),
            LoadedSyncState::Current(state) if state == current
        ));
        let mut pre_marker: serde_json::Value = serde_json::to_value(&current).unwrap();
        pre_marker
            .as_object_mut()
            .unwrap()
            .remove("migration_marker_confirmed");
        assert!(matches!(
            load_sync_state(Some(&serde_json::to_vec(&pre_marker).unwrap())).unwrap(),
            LoadedSyncState::Current(state) if !state.migration_marker_confirmed
        ));
    }

    #[test]
    fn previous_checkpoint_rebuilds_remote_indexes_without_losing_baseline() {
        let path = RelativePath::new("touchHLE_apps/save").unwrap();
        let old = CurrentSyncState {
            schema_version: PREVIOUS_SCHEMA_VERSION,
            device_id: Uuid::new_v4(),
            account_id: Some("account".into()),
            root_folder_id: Some("root".into()),
            changes_cursor: Some("old-cursor".into()),
            migration_marker_confirmed: true,
            baseline: BTreeMap::from([(
                path.clone(),
                FileBaseline {
                    sha256: Some([7; 32]),
                    remote_id: Some("drive-file".into()),
                    remote_version: Some("12".into()),
                },
            )]),
            file_paths_by_id: BTreeMap::from([("drive-file".into(), path)]),
            folder_paths_by_id: BTreeMap::from([(
                "apps-folder".into(),
                "touchHLE/touchHLE_apps".into(),
            )]),
        };
        let expected_baseline = old.baseline.clone();

        let LoadedSyncState::Current(migrated) =
            load_sync_state(Some(&serde_json::to_vec(&old).unwrap())).unwrap()
        else {
            panic!("old current checkpoint should migrate in place");
        };

        assert_eq!(migrated.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(migrated.baseline, expected_baseline);
        assert!(migrated.changes_cursor.is_none());
        assert!(!migrated.migration_marker_confirmed);
        assert!(migrated.file_paths_by_id.is_empty());
        assert!(migrated.folder_paths_by_id.is_empty());
    }

    #[test]
    fn loader_rejects_corruption_unknown_versions_and_incomplete_legacy() {
        for json in [
            b"not json".as_slice(),
            br#"{}"#,
            br#"{"device_id":"00000000-0000-0000-0000-000000000000","baseline":{}}"#,
            br#"{"schema_version":999}"#,
            br#"{"device_id":"00000000-0000-0000-0000-000000000000","last_applied_commit":null,"baseline":{"touchHLE_apps/x":"Tombstone","touchHLE_apps/x":"Tombstone"}}"#,
        ] {
            assert!(matches!(
                load_sync_state(Some(json)),
                Err(SyncError::Integrity(_))
            ));
        }
    }

    #[test]
    fn current_baseline_rejects_live_file_ancestors_but_allows_deleted_ancestor() {
        let parent = RelativePath::new("touchHLE_apps/foo").unwrap();
        let child = RelativePath::new("touchHLE_apps/foo/bar").unwrap();
        let mut current = CurrentSyncState::new(Uuid::new_v4());
        current.baseline.insert(
            parent.clone(),
            FileBaseline {
                sha256: Some([1; 32]),
                remote_id: Some("parent-id".into()),
                remote_version: Some("1".into()),
            },
        );
        current.baseline.insert(
            child.clone(),
            FileBaseline {
                sha256: Some([2; 32]),
                remote_id: Some("child-id".into()),
                remote_version: Some("1".into()),
            },
        );
        let bytes = serde_json::to_vec(&current).unwrap();
        assert!(matches!(
            load_sync_state(Some(&bytes)),
            Err(SyncError::Integrity(_))
        ));

        current.baseline.get_mut(&parent).unwrap().sha256 = None;
        let bytes = serde_json::to_vec(&current).unwrap();
        assert!(matches!(
            load_sync_state(Some(&bytes)),
            Ok(LoadedSyncState::Current(_))
        ));
    }

    #[test]
    fn deleted_baselines_are_kept_only_for_pending_local_comparison() {
        let path = RelativePath::new("touchHLE_apps/deleted").unwrap();
        let mut state = CurrentSyncState::new(Uuid::new_v4());
        state.baseline.insert(
            path.clone(),
            FileBaseline {
                sha256: Some([1; 32]),
                remote_id: Some("id".into()),
                remote_version: Some("1".into()),
            },
        );
        state.file_paths_by_id.insert("id".into(), path.clone());
        state.prune_deleted_paths(
            &BTreeMap::from([(
                path.clone(),
                SnapshotEntry::File {
                    sha256: [2; 32],
                    size: 1,
                    modified_unix_ms: 0,
                },
            )]),
            &BTreeMap::new(),
        );
        assert!(state.baseline.contains_key(&path));
        state.prune_deleted_paths(&BTreeMap::new(), &BTreeMap::new());
        assert!(state.baseline.is_empty());
        assert!(state.file_paths_by_id.is_empty());
    }

    #[test]
    fn invalid_paths_are_rejected_even_in_remote_manifests() {
        for path in [
            "/absolute",
            "../escape",
            "touchHLE_apps/../escape",
            "touchHLE_apps\\file",
            "touchHLE_apps//file",
            "touchHLE_apps/./file",
            "touchHLE_apps",
            "other/file",
            "touchHLE_apps/C:/file",
            "touchHLE_apps/CON",
            "touchHLE_apps/con.txt",
            "touchHLE_apps/CON .txt",
            "touchHLE_apps/aux.txt",
            "touchHLE_apps/COM1/save",
            "touchHLE_apps/LPT³/file",
            "touchHLE_sandbox/file.",
            "touchHLE_sandbox/folder /save",
            "touchHLE_apps/file?",
            "touchHLE_apps/a*",
            "touchHLE_apps/a|b",
            "touchHLE_apps/a\u{1f}b",
        ] {
            assert!(RelativePath::new(path).is_err(), "{path}");
            assert!(
                serde_json::from_str::<RelativePath>(&serde_json::to_string(path).unwrap())
                    .is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn live_files_cannot_be_ancestors_of_other_live_files() {
        let id = Uuid::new_v4();
        let file = r#"{"File":{"sha256":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"size":0,"modified_unix_ms":0}}"#;
        for paths in [
            format!(r#""touchHLE_apps/foo":{file},"touchHLE_apps/foo/bar":{file}"#),
            format!(r#""touchHLE_apps/foo/bar":{file},"touchHLE_apps/foo":{file}"#),
        ] {
            let commit = format!(
                r#"{{"id":"{id}","device_id":"{id}","created_unix_ms":0,"parents":[],"entries":{{{paths}}}}}"#
            );
            let state = format!(
                r#"{{"device_id":"{id}","last_applied_commit":null,"baseline":{{{paths}}}}}"#
            );
            assert!(serde_json::from_str::<Commit>(&commit).is_err(), "{paths}");
            assert!(
                serde_json::from_str::<SyncState>(&state).is_err(),
                "{paths}"
            );
        }
        let tombstone_ancestor = format!(
            r#"{{"id":"{id}","device_id":"{id}","created_unix_ms":0,"parents":[],"entries":{{"touchHLE_apps/foo":"Tombstone","touchHLE_apps/foo/bar":{file}}}}}"#
        );
        assert!(serde_json::from_str::<Commit>(&tombstone_ancestor).is_ok());
        let tombstone_descendant = format!(
            r#"{{"id":"{id}","device_id":"{id}","created_unix_ms":0,"parents":[],"entries":{{"touchHLE_apps/foo":{file},"touchHLE_apps/foo/bar":"Tombstone"}}}}"#
        );
        assert!(serde_json::from_str::<Commit>(&tombstone_descendant).is_ok());
    }

    #[test]
    fn remote_manifests_reject_case_collisions_and_duplicate_keys() {
        let id = Uuid::new_v4();
        for paths in [
            r#""touchHLE_apps/Save":"Tombstone","touchHLE_apps/save":"Tombstone""#,
            r#""touchHLE_apps/Save/one":"Tombstone","touchHLE_apps/save/two":"Tombstone""#,
            r#""touchHLE_apps/straße":"Tombstone","touchHLE_apps/strasse":"Tombstone""#,
            r#""touchHLE_apps/one":"Tombstone","touchHLE_apps/one":"Tombstone""#,
        ] {
            let commit = format!(
                r#"{{"id":"{id}","device_id":"{id}","created_unix_ms":0,"parents":[],"entries":{{{paths}}}}}"#
            );
            let state = format!(
                r#"{{"device_id":"{id}","last_applied_commit":null,"baseline":{{{paths}}}}}"#
            );
            assert!(serde_json::from_str::<Commit>(&commit).is_err());
            assert!(serde_json::from_str::<SyncState>(&state).is_err());
        }
    }
}
