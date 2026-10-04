/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::model::{validate_entries, LocalOrRemote, RelativePath, SnapshotEntry, SyncError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteFile {
    pub path: RelativePath,
    pub id: String,
    pub version: String,
    pub entry: SnapshotEntry,
    pub parent_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteChange {
    pub file_id: String,
    pub file: Option<RemoteFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteChangeBatch {
    pub changes: Vec<RemoteChange>,
    pub next_page_token: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileBaseline {
    pub sha256: Option<[u8; 32]>,
    pub remote_id: Option<String>,
    pub remote_version: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteVersionId {
    DriveFile(String),
    LegacyCommit(Uuid),
    NoDriveFile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteCandidate {
    pub id: RemoteVersionId,
    pub entry: Option<SnapshotEntry>,
    pub file: Option<RemoteFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileConflict {
    pub path: RelativePath,
    pub local: Option<SnapshotEntry>,
    pub remote_candidates: Vec<RemoteCandidate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictChoice {
    pub path: RelativePath,
    pub selected: LocalOrRemote,
    pub remote_version_id: Option<RemoteVersionId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathAction {
    Unchanged,
    PublishLocal(Option<SnapshotEntry>),
    ApplyRemote(RemoteCandidate),
    Conflict(FileConflict),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncPlan {
    pub apply_remote: BTreeMap<RelativePath, RemoteCandidate>,
    pub publish_local: BTreeMap<RelativePath, Option<SnapshotEntry>>,
    pub conflicts: Vec<FileConflict>,
    pub local_snapshot: BTreeMap<RelativePath, SnapshotEntry>,
}

impl SyncPlan {
    pub fn has_same_content_as(&self, other: &Self) -> bool {
        same_entry_map(&self.local_snapshot, &other.local_snapshot)
            && same_optional_entry_map(&self.publish_local, &other.publish_local)
            && same_candidate_map(&self.apply_remote, &other.apply_remote)
            && self.conflicts.len() == other.conflicts.len()
            && self
                .conflicts
                .iter()
                .zip(&other.conflicts)
                .all(|(left, right)| {
                    left.path == right.path
                        && same_optional_entry(left.local.as_ref(), right.local.as_ref())
                        && left.remote_candidates.len() == right.remote_candidates.len()
                        && left
                            .remote_candidates
                            .iter()
                            .zip(&right.remote_candidates)
                            .all(|(a, b)| same_candidate_content(a, b))
                })
    }

    /// Resolve all displayed choices together and reject an invalid final tree
    /// before any selected remote bytes are read or any live files are changed.
    pub fn resolved_snapshot(
        &self,
        choices: &[ConflictChoice],
    ) -> Result<BTreeMap<RelativePath, SnapshotEntry>, SyncError> {
        let mut selected = BTreeMap::new();
        for choice in choices {
            if !self
                .conflicts
                .iter()
                .any(|conflict| conflict.path == choice.path)
            {
                return Err(SyncError::UnresolvedConflicts(format!(
                    "no conflict at path {}",
                    choice.path.as_str()
                )));
            }
            if selected.insert(&choice.path, choice).is_some() {
                return Err(SyncError::UnresolvedConflicts(format!(
                    "duplicate choice at {}",
                    choice.path.as_str()
                )));
            }
        }
        if selected.len() != self.conflicts.len() {
            return Err(SyncError::UnresolvedConflicts(
                "a choice is required for every conflict".into(),
            ));
        }

        let mut resolved = self.local_snapshot.clone();
        for (path, entry) in &self.publish_local {
            resolved.insert(
                path.clone(),
                entry.clone().unwrap_or(SnapshotEntry::Tombstone),
            );
        }
        for (path, candidate) in &self.apply_remote {
            resolved.insert(
                path.clone(),
                candidate.entry.clone().unwrap_or(SnapshotEntry::Tombstone),
            );
        }
        for conflict in &self.conflicts {
            let choice = selected[&conflict.path];
            let action = resolve_conflict(conflict, choice)?;
            let entry = match action {
                PathAction::PublishLocal(entry) => entry.unwrap_or(SnapshotEntry::Tombstone),
                PathAction::ApplyRemote(candidate) => {
                    candidate.entry.unwrap_or(SnapshotEntry::Tombstone)
                }
                _ => unreachable!("validated conflict choices always select a side"),
            };
            resolved.insert(conflict.path.clone(), entry);
        }
        validate_entries(&resolved).map_err(SyncError::UnresolvedConflicts)?;
        Ok(resolved)
    }
}

pub fn rebind_conflict_choices(
    displayed_plan: &SyncPlan,
    current_plan: &SyncPlan,
    choices: &[ConflictChoice],
) -> Result<Vec<ConflictChoice>, SyncError> {
    choices
        .iter()
        .map(|choice| {
            let mut rebound = choice.clone();
            if choice.selected == LocalOrRemote::Remote {
                let displayed = displayed_plan
                    .conflicts
                    .iter()
                    .find(|conflict| conflict.path == choice.path)
                    .ok_or_else(|| {
                        SyncError::UnresolvedConflicts("displayed conflict is stale".into())
                    })?;
                let selected_id = choice.remote_version_id.as_ref().ok_or_else(|| {
                    SyncError::UnresolvedConflicts("remote choice has no candidate".into())
                })?;
                let selected = displayed
                    .remote_candidates
                    .iter()
                    .find(|candidate| &candidate.id == selected_id)
                    .ok_or_else(|| {
                        SyncError::UnresolvedConflicts("selected remote candidate is stale".into())
                    })?;
                let current = current_plan
                    .conflicts
                    .iter()
                    .find(|conflict| conflict.path == choice.path)
                    .ok_or_else(|| {
                        SyncError::UnresolvedConflicts("current conflict is missing".into())
                    })?;
                let candidate = current
                    .remote_candidates
                    .iter()
                    .find(|candidate| same_candidate_content(selected, candidate))
                    .ok_or_else(|| {
                        SyncError::UnresolvedConflicts("selected remote content changed".into())
                    })?;
                rebound.remote_version_id = Some(candidate.id.clone());
            }
            Ok(rebound)
        })
        .collect()
}

fn same_entry(left: &SnapshotEntry, right: &SnapshotEntry) -> bool {
    match (left, right) {
        (SnapshotEntry::File { sha256: a, .. }, SnapshotEntry::File { sha256: b, .. }) => a == b,
        (SnapshotEntry::Tombstone, SnapshotEntry::Tombstone) => true,
        _ => false,
    }
}

fn same_optional_entry(left: Option<&SnapshotEntry>, right: Option<&SnapshotEntry>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => same_entry(left, right),
        (None, None) => true,
        _ => false,
    }
}

fn same_entry_map(
    left: &BTreeMap<RelativePath, SnapshotEntry>,
    right: &BTreeMap<RelativePath, SnapshotEntry>,
) -> bool {
    left.len() == right.len()
        && left.iter().all(|(path, entry)| {
            right
                .get(path)
                .is_some_and(|other| same_entry(entry, other))
        })
}

fn same_optional_entry_map(
    left: &BTreeMap<RelativePath, Option<SnapshotEntry>>,
    right: &BTreeMap<RelativePath, Option<SnapshotEntry>>,
) -> bool {
    left.len() == right.len()
        && left.iter().all(|(path, entry)| {
            right
                .get(path)
                .is_some_and(|other| same_optional_entry(entry.as_ref(), other.as_ref()))
        })
}

fn same_candidate_content(left: &RemoteCandidate, right: &RemoteCandidate) -> bool {
    same_optional_entry(left.entry.as_ref(), right.entry.as_ref())
        && match (&left.file, &right.file) {
            (Some(left), Some(right)) => {
                left.path == right.path && same_entry(&left.entry, &right.entry)
            }
            (None, None) => true,
            _ => false,
        }
}

fn same_candidate_map(
    left: &BTreeMap<RelativePath, RemoteCandidate>,
    right: &BTreeMap<RelativePath, RemoteCandidate>,
) -> bool {
    left.len() == right.len()
        && left.iter().all(|(path, candidate)| {
            right
                .get(path)
                .is_some_and(|other| same_candidate_content(candidate, other))
        })
}

pub fn resolve_conflict(
    conflict: &FileConflict,
    choice: &ConflictChoice,
) -> Result<PathAction, SyncError> {
    if conflict.path != choice.path {
        return Err(SyncError::Integrity(
            "choice path does not match conflict".into(),
        ));
    }
    match choice.selected {
        LocalOrRemote::Local if choice.remote_version_id.is_none() => {
            Ok(PathAction::PublishLocal(conflict.local.clone()))
        }
        LocalOrRemote::Remote => {
            let selected = choice.remote_version_id.as_ref().ok_or_else(|| {
                SyncError::Integrity("remote choice requires a version ID".into())
            })?;
            let candidate = conflict
                .remote_candidates
                .iter()
                .find(|c| &c.id == selected)
                .ok_or_else(|| SyncError::Integrity("remote version is not a candidate".into()))?;
            Ok(PathAction::ApplyRemote(candidate.clone()))
        }
        LocalOrRemote::Local => Err(SyncError::Integrity("local choice has a remote ID".into())),
    }
}

fn hash(entry: Option<&SnapshotEntry>) -> Option<[u8; 32]> {
    match entry {
        Some(SnapshotEntry::File { sha256, .. }) => Some(*sha256),
        _ => None,
    }
}

fn plan_at_path(
    path: Option<&RelativePath>,
    baseline: Option<&SnapshotEntry>,
    local: Option<&SnapshotEntry>,
    remote: Option<&RemoteCandidate>,
) -> Result<PathAction, SyncError> {
    let base = hash(baseline);
    let local_hash = hash(local);
    let remote_hash = hash(remote.and_then(|candidate| candidate.entry.as_ref()));
    let local_changed = local_hash != base;
    let remote_changed = remote_hash != base;
    Ok(match (local_changed, remote_changed) {
        (false, false) => PathAction::Unchanged,
        (true, false) => PathAction::PublishLocal(local.cloned()),
        (false, true) => PathAction::ApplyRemote(remote.cloned().ok_or_else(|| {
            SyncError::Integrity("remote deletion has no candidate identity".into())
        })?),
        (true, true) if local_hash == remote_hash => PathAction::Unchanged,
        (true, true) => PathAction::Conflict(FileConflict {
            path: path
                .or_else(|| remote.and_then(|candidate| candidate.file.as_ref().map(|f| &f.path)))
                .ok_or_else(|| SyncError::Integrity("conflict path is unknown".into()))?
                .clone(),
            local: local.cloned(),
            remote_candidates: remote.cloned().into_iter().collect(),
        }),
    })
}

pub fn plan_path(
    baseline: Option<&SnapshotEntry>,
    local: Option<&SnapshotEntry>,
    remote: Option<&RemoteCandidate>,
) -> Result<PathAction, SyncError> {
    plan_at_path(None, baseline, local, remote)
}

pub fn plan_sync(
    baseline: &BTreeMap<RelativePath, FileBaseline>,
    local: &BTreeMap<RelativePath, SnapshotEntry>,
    remote: &BTreeMap<RelativePath, RemoteFile>,
) -> Result<SyncPlan, SyncError> {
    validate_entries(local).map_err(SyncError::Integrity)?;
    let remote_entries: BTreeMap<_, _> = remote
        .iter()
        .map(|(path, file)| {
            if &file.path != path || !matches!(file.entry, SnapshotEntry::File { .. }) {
                return Err(SyncError::Integrity("invalid remote file record".into()));
            }
            Ok((path.clone(), file.entry.clone()))
        })
        .collect::<Result<_, _>>()?;
    validate_entries(&remote_entries).map_err(SyncError::Integrity)?;
    let baseline_entries: BTreeMap<_, _> = baseline
        .iter()
        .map(|(path, file)| {
            (
                path.clone(),
                file.sha256
                    .map_or(SnapshotEntry::Tombstone, |sha256| SnapshotEntry::File {
                        sha256,
                        size: 0,
                        modified_unix_ms: 0,
                    }),
            )
        })
        .collect();
    validate_entries(&baseline_entries).map_err(SyncError::Integrity)?;
    let paths: BTreeSet<_> = baseline
        .keys()
        .chain(local.keys())
        .chain(remote.keys())
        .cloned()
        .collect();
    let mut combined: BTreeMap<_, _> = paths
        .iter()
        .cloned()
        .map(|path| (path, SnapshotEntry::Tombstone))
        .collect();
    combined.extend(
        local
            .iter()
            .map(|(path, entry)| (path.clone(), entry.clone())),
    );
    combined.extend(remote_entries);
    validate_entries(&combined).map_err(SyncError::Integrity)?;

    let mut plan = SyncPlan {
        local_snapshot: local.clone(),
        ..SyncPlan::default()
    };
    for path in paths {
        let saved = baseline.get(&path);
        let base_entry = saved.and_then(|b| {
            b.sha256.map(|sha256| SnapshotEntry::File {
                sha256,
                size: 0,
                modified_unix_ms: 0,
            })
        });
        let candidate = remote
            .get(&path)
            .map(|file| RemoteCandidate {
                id: RemoteVersionId::DriveFile(file.id.clone()),
                entry: Some(file.entry.clone()),
                file: Some(file.clone()),
            })
            .or_else(|| {
                saved
                    .and_then(|b| b.remote_id.as_ref())
                    .map(|id| RemoteCandidate {
                        id: RemoteVersionId::DriveFile(id.clone()),
                        entry: None,
                        file: None,
                    })
            });
        match plan_at_path(
            Some(&path),
            base_entry.as_ref(),
            local.get(&path),
            candidate.as_ref(),
        )? {
            PathAction::Unchanged => {}
            PathAction::PublishLocal(entry) => {
                plan.publish_local.insert(path, entry);
            }
            PathAction::ApplyRemote(candidate) => {
                plan.apply_remote.insert(path, candidate);
            }
            PathAction::Conflict(conflict) => plan.conflicts.push(conflict),
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::model::{RelativePath, SnapshotEntry};
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn path() -> RelativePath {
        RelativePath::new("touchHLE_apps/save").unwrap()
    }

    fn entry(hash: u8) -> SnapshotEntry {
        SnapshotEntry::File {
            sha256: [hash; 32],
            size: 1,
            modified_unix_ms: 0,
        }
    }

    fn remote(hash: u8) -> RemoteFile {
        RemoteFile {
            path: path(),
            id: "file-id".into(),
            version: "9".into(),
            entry: entry(hash),
            parent_id: None,
        }
    }

    #[test]
    fn three_way_one_path_cases() {
        for (name, base, local, remote_hash, expected) in [
            ("unchanged", Some(1), Some(1), Some(1), "unchanged"),
            ("local update", Some(1), Some(2), Some(1), "publish"),
            ("remote update", Some(1), Some(1), Some(2), "apply"),
            ("same update", Some(1), Some(2), Some(2), "unchanged"),
            ("divergent", Some(1), Some(2), Some(3), "conflict"),
            ("local create", None, Some(2), None, "publish"),
            ("remote create", None, None, Some(2), "apply"),
            ("both delete", Some(1), None, None, "unchanged"),
            (
                "local delete remote update",
                Some(1),
                None,
                Some(2),
                "conflict",
            ),
            (
                "local update remote delete",
                Some(1),
                Some(2),
                None,
                "conflict",
            ),
        ] {
            let baseline = base.map(|hash| FileBaseline {
                sha256: Some([hash; 32]),
                remote_id: Some("file-id".into()),
                remote_version: Some("8".into()),
            });
            let local = local.map(entry);
            let remote = remote_hash.map(|hash| {
                let mut file = remote(hash);
                if Some(hash) == base {
                    file.version = "8".into();
                }
                file
            });
            let plan = plan_sync(
                &baseline
                    .map(|b| BTreeMap::from([(path(), b)]))
                    .unwrap_or_default(),
                &local
                    .map(|l| BTreeMap::from([(path(), l)]))
                    .unwrap_or_default(),
                &remote
                    .map(|r| BTreeMap::from([(path(), r)]))
                    .unwrap_or_default(),
            )
            .unwrap();
            let actual = if !plan.conflicts.is_empty() {
                assert!(plan.apply_remote.is_empty(), "{name}");
                assert!(plan.publish_local.is_empty(), "{name}");
                assert_eq!(plan.conflicts.len(), 1, "{name}");
                "conflict"
            } else if !plan.apply_remote.is_empty() {
                "apply"
            } else if !plan.publish_local.is_empty() {
                "publish"
            } else {
                "unchanged"
            };
            assert_eq!(actual, expected, "{name}");
        }
    }

    #[test]
    fn same_content_remote_revision_change_does_not_require_choice() {
        let baseline = BTreeMap::from([(
            path(),
            FileBaseline {
                sha256: Some([1; 32]),
                remote_id: Some("file-id".into()),
                remote_version: Some("8".into()),
            },
        )]);
        let local = BTreeMap::from([(path(), entry(1))]);
        let mut version_changed = remote(1);
        version_changed.version = "10".into();
        let mut id_changed = remote(1);
        id_changed.id = "replacement-file-id".into();
        let revision_changes = [version_changed, id_changed];

        for remote in revision_changes {
            let plan = plan_sync(
                &baseline,
                &local,
                &BTreeMap::from([(path(), remote.clone())]),
            )
            .unwrap();

            assert!(plan.publish_local.is_empty());
            assert!(plan.apply_remote.is_empty());
            assert!(plan.conflicts.is_empty());
        }
    }

    #[test]
    fn conflict_choice_rebinds_after_revision_or_id_change_but_not_content_change() {
        let baseline = BTreeMap::from([(
            path(),
            FileBaseline {
                sha256: Some([1; 32]),
                remote_id: Some("old-file-id".into()),
                remote_version: Some("8".into()),
            },
        )]);
        let local = BTreeMap::from([(path(), entry(3))]);
        let displayed_remote = RemoteFile {
            id: "old-file-id".into(),
            version: "9".into(),
            ..remote(2)
        };
        let displayed = plan_sync(
            &baseline,
            &local,
            &BTreeMap::from([(path(), displayed_remote)]),
        )
        .unwrap();
        let choice = ConflictChoice {
            path: path(),
            selected: LocalOrRemote::Remote,
            remote_version_id: Some(RemoteVersionId::DriveFile("old-file-id".into())),
        };

        let current_remote = RemoteFile {
            id: "replacement-file-id".into(),
            version: "10".into(),
            ..remote(2)
        };
        let current = plan_sync(
            &baseline,
            &local,
            &BTreeMap::from([(path(), current_remote)]),
        )
        .unwrap();

        assert!(displayed.has_same_content_as(&current));
        let rebound = rebind_conflict_choices(&displayed, &current, &[choice.clone()]).unwrap();
        assert_eq!(
            rebound[0].remote_version_id,
            Some(RemoteVersionId::DriveFile("replacement-file-id".into()))
        );

        let changed_remote = RemoteFile {
            id: "replacement-file-id".into(),
            version: "11".into(),
            ..remote(4)
        };
        let changed = plan_sync(
            &baseline,
            &local,
            &BTreeMap::from([(path(), changed_remote)]),
        )
        .unwrap();
        assert!(!displayed.has_same_content_as(&changed));
        assert!(rebind_conflict_choices(&displayed, &changed, &[choice]).is_err());
    }

    #[test]
    fn divergent_edits_require_one_local_or_remote_choice() {
        let baseline = entry(1);
        let local = entry(2);
        let remote = RemoteCandidate {
            id: RemoteVersionId::DriveFile("file-id".into()),
            entry: Some(entry(3)),
            file: Some(remote(3)),
        };
        assert!(matches!(
            plan_path(Some(&baseline), Some(&local), Some(&remote)).unwrap(),
            PathAction::Conflict(_)
        ));
    }

    #[test]
    fn resolver_supports_legacy_tips_and_one_drive_candidate() {
        let legacy = vec![
            RemoteCandidate {
                id: RemoteVersionId::LegacyCommit(Uuid::from_u128(1)),
                entry: Some(entry(1)),
                file: None,
            },
            RemoteCandidate {
                id: RemoteVersionId::LegacyCommit(Uuid::from_u128(2)),
                entry: Some(entry(2)),
                file: None,
            },
        ];
        let drive = vec![RemoteCandidate {
            id: RemoteVersionId::DriveFile("file-id".into()),
            entry: Some(entry(3)),
            file: Some(remote(3)),
        }];
        assert_eq!(legacy.len(), 2);
        assert_eq!(drive.len(), 1);
        assert!(legacy
            .iter()
            .all(|c| matches!(c.id, RemoteVersionId::LegacyCommit(_))));
        assert!(matches!(drive[0].id, RemoteVersionId::DriveFile(_)));
        for (candidates, ids) in [
            (
                &legacy,
                vec![
                    RemoteVersionId::LegacyCommit(Uuid::from_u128(1)),
                    RemoteVersionId::LegacyCommit(Uuid::from_u128(2)),
                ],
            ),
            (&drive, vec![RemoteVersionId::DriveFile("file-id".into())]),
        ] {
            let conflict = FileConflict {
                path: path(),
                local: Some(entry(4)),
                remote_candidates: candidates.clone(),
            };
            for id in ids {
                let choice = ConflictChoice {
                    path: path(),
                    selected: LocalOrRemote::Remote,
                    remote_version_id: Some(id.clone()),
                };
                assert!(matches!(
                    resolve_conflict(&conflict, &choice).unwrap(),
                    PathAction::ApplyRemote(candidate) if candidate.id == id
                ));
            }
            assert!(matches!(
                resolve_conflict(
                    &conflict,
                    &ConflictChoice {
                        path: path(),
                        selected: LocalOrRemote::Local,
                        remote_version_id: None
                    }
                )
                .unwrap(),
                PathAction::PublishLocal(Some(_))
            ));
        }
    }

    #[test]
    fn plan_rejects_case_and_directory_collisions() {
        let a = RelativePath::new("touchHLE_apps/Save").unwrap();
        let b = RelativePath::new("touchHLE_apps/save").unwrap();
        assert!(matches!(
            plan_sync(
                &BTreeMap::new(),
                &BTreeMap::from([(a, entry(1)), (b, entry(2))]),
                &BTreeMap::new()
            ),
            Err(SyncError::Integrity(_))
        ));
        let a = RelativePath::new("touchHLE_apps/folder").unwrap();
        let b = RelativePath::new("touchHLE_apps/folder/child").unwrap();
        assert!(matches!(
            plan_sync(
                &BTreeMap::new(),
                &BTreeMap::from([(a, entry(1)), (b, entry(2))]),
                &BTreeMap::new()
            ),
            Err(SyncError::Integrity(_))
        ));
    }

    #[test]
    fn plan_rejects_live_file_ancestor_across_local_and_remote_trees() {
        let local_path = RelativePath::new("touchHLE_apps/foo").unwrap();
        let remote_path = RelativePath::new("touchHLE_apps/foo/bar").unwrap();
        let remote_file = RemoteFile {
            path: remote_path.clone(),
            id: "child-id".into(),
            version: "1".into(),
            entry: entry(2),
            parent_id: None,
        };
        let result = plan_sync(
            &BTreeMap::new(),
            &BTreeMap::from([(local_path, entry(1))]),
            &BTreeMap::from([(remote_path, remote_file)]),
        );
        assert!(matches!(result, Err(SyncError::Integrity(_))));
    }

    #[test]
    fn resolved_snapshot_requires_all_choices_and_rejects_incompatible_tree() {
        let parent = RelativePath::new("touchHLE_apps/parent").unwrap();
        let child = RelativePath::new("touchHLE_apps/parent/child").unwrap();
        let plan = SyncPlan {
            local_snapshot: BTreeMap::from([(parent.clone(), entry(1))]),
            conflicts: vec![
                FileConflict {
                    path: parent.clone(),
                    local: Some(entry(1)),
                    remote_candidates: vec![RemoteCandidate {
                        id: RemoteVersionId::DriveFile("parent-id".into()),
                        entry: None,
                        file: None,
                    }],
                },
                FileConflict {
                    path: child.clone(),
                    local: None,
                    remote_candidates: vec![RemoteCandidate {
                        id: RemoteVersionId::DriveFile("child-id".into()),
                        entry: Some(entry(2)),
                        file: Some(RemoteFile {
                            path: child.clone(),
                            id: "child-id".into(),
                            version: "1".into(),
                            entry: entry(2),
                            parent_id: None,
                        }),
                    }],
                },
            ],
            ..SyncPlan::default()
        };
        assert!(plan.resolved_snapshot(&[]).is_err());

        let incompatible = [
            ConflictChoice {
                path: parent,
                selected: LocalOrRemote::Local,
                remote_version_id: None,
            },
            ConflictChoice {
                path: child,
                selected: LocalOrRemote::Remote,
                remote_version_id: Some(RemoteVersionId::DriveFile("child-id".into())),
            },
        ];
        assert!(matches!(
            plan.resolved_snapshot(&incompatible),
            Err(SyncError::UnresolvedConflicts(_))
        ));
    }
}
