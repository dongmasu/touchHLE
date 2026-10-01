/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
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
            let mut seen = BTreeMap::<String, String>::new();
            while let Some((key, entry)) = map.next_entry::<RelativePath, SnapshotEntry>()? {
                let mut prefix = String::new();
                for part in key.as_str().split('/') {
                    if !prefix.is_empty() {
                        prefix.push('/');
                    }
                    prefix.push_str(part);
                    let folded = prefix.as_str().case_fold().collect::<String>();
                    if let Some(previous) = seen.insert(folded, prefix.clone()) {
                        if previous != prefix {
                            return Err(M::Error::custom(format!(
                                "case-insensitive collision: {previous} and {prefix}"
                            )));
                        }
                    }
                }
                if entries.insert(key, entry).is_some() {
                    return Err(M::Error::custom("duplicate snapshot path"));
                }
            }
            for (key, entry) in &entries {
                if !matches!(entry, SnapshotEntry::File { .. }) {
                    continue;
                }
                let mut prefix = key.as_str();
                while let Some((parent, _)) = prefix.rsplit_once('/') {
                    if matches!(
                        entries.get(&RelativePath(parent.to_owned())),
                        Some(SnapshotEntry::File { .. })
                    ) {
                        return Err(M::Error::custom(format!(
                            "live file is an ancestor of another live file: {parent} and {}",
                            key.as_str()
                        )));
                    }
                    prefix = parent;
                }
            }
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
    Provider(String),
    Authentication(String),
    UnresolvedConflicts(String),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "sync I/O error: {e}"),
            Self::InvalidPath(path) => write!(f, "invalid sync path: {path}"),
            Self::Serialization(e) => write!(f, "sync serialization error: {e}"),
            Self::Integrity(message) => write!(f, "sync integrity failure: {message}"),
            Self::Provider(message) => write!(f, "sync provider failure: {message}"),
            Self::Authentication(message) => write!(f, "sync authentication failure: {message}"),
            Self::UnresolvedConflicts(message) => write!(f, "unresolved sync conflicts: {message}"),
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
