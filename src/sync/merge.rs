/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::model::{Commit, LocalOrRemote, RelativePath, SnapshotEntry, SyncError};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteTip {
    pub commit_id: Uuid,
    pub snapshot: BTreeMap<RelativePath, SnapshotEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteVersion {
    pub commit_ids: Vec<Uuid>,
    pub entry: Option<SnapshotEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Conflict {
    pub path: RelativePath,
    pub local: Option<SnapshotEntry>,
    pub remote_candidates: Vec<RemoteVersion>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictChoice {
    pub path: RelativePath,
    pub selected: LocalOrRemote,
    pub remote_commit_id: Option<Uuid>,
}

impl ConflictChoice {
    pub fn validate(&self, plan: &SyncPlan) -> Result<(), SyncError> {
        let conflict = plan
            .conflicts
            .iter()
            .find(|conflict| conflict.path == self.path)
            .ok_or_else(|| {
                SyncError::UnresolvedConflicts(format!(
                    "no conflict at path {}",
                    self.path.as_str()
                ))
            })?;
        match (self.selected, self.remote_commit_id) {
            (LocalOrRemote::Local, None) => Ok(()),
            (LocalOrRemote::Remote, Some(id))
                if conflict
                    .remote_candidates
                    .iter()
                    .any(|candidate| candidate.commit_ids.contains(&id)) =>
            {
                Ok(())
            }
            _ => Err(SyncError::UnresolvedConflicts(format!(
                "invalid conflict choice for {}",
                self.path.as_str()
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncPlan {
    pub apply_remote: BTreeMap<RelativePath, SnapshotEntry>,
    pub publish_local: BTreeMap<RelativePath, SnapshotEntry>,
    pub conflicts: Vec<Conflict>,
    pub merge_parents: Vec<Uuid>,
    pub local_snapshot: BTreeMap<RelativePath, SnapshotEntry>,
    pub remote_tips: Vec<RemoteTip>,
}

pub fn resolve_remote_tips(commits: &[Commit]) -> Result<Vec<RemoteTip>, SyncError> {
    let mut by_id = BTreeMap::new();
    for commit in commits {
        if by_id.insert(commit.id, commit).is_some() {
            return Err(SyncError::Integrity(format!(
                "duplicate commit ID: {}",
                commit.id
            )));
        }
    }

    let mut children = BTreeMap::<Uuid, Vec<Uuid>>::new();
    let mut remaining_parents = BTreeMap::<Uuid, usize>::new();
    for commit in commits {
        remaining_parents.insert(commit.id, commit.parents.len());
        for parent in &commit.parents {
            if !by_id.contains_key(parent) {
                return Err(SyncError::Integrity(format!(
                    "missing parent {parent} for commit {}",
                    commit.id
                )));
            }
            children.entry(*parent).or_default().push(commit.id);
        }
    }

    let mut ready: BTreeSet<_> = remaining_parents
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(*id))
        .collect();
    let mut visited = 0;
    while let Some(id) = ready.pop_first() {
        visited += 1;
        if let Some(descendants) = children.get(&id) {
            for child in descendants {
                let count = remaining_parents.get_mut(child).unwrap();
                *count -= 1;
                if *count == 0 {
                    ready.insert(*child);
                }
            }
        }
    }
    if visited != commits.len() {
        return Err(SyncError::Integrity("cycle in commit ancestry".to_owned()));
    }

    Ok(by_id
        .into_iter()
        .filter(|(id, _)| !children.contains_key(id))
        .map(|(commit_id, commit)| RemoteTip {
            commit_id,
            snapshot: commit.entries.clone(),
        })
        .collect())
}

fn same_content(left: Option<&SnapshotEntry>, right: Option<&SnapshotEntry>) -> bool {
    match (left, right) {
        (
            Some(SnapshotEntry::File { sha256: left, .. }),
            Some(SnapshotEntry::File { sha256: right, .. }),
        ) => left == right,
        (Some(SnapshotEntry::File { .. }), _) | (_, Some(SnapshotEntry::File { .. })) => false,
        _ => true,
    }
}

fn same_version(left: Option<&SnapshotEntry>, right: Option<&SnapshotEntry>) -> bool {
    match (left, right) {
        (None, None) | (Some(SnapshotEntry::Tombstone), Some(SnapshotEntry::Tombstone)) => true,
        (Some(SnapshotEntry::File { .. }), Some(SnapshotEntry::File { .. })) => {
            same_content(left, right)
        }
        _ => false,
    }
}

fn entry_for_action(entry: Option<&SnapshotEntry>) -> SnapshotEntry {
    entry.cloned().unwrap_or(SnapshotEntry::Tombstone)
}

pub fn plan_sync(
    baseline: &BTreeMap<RelativePath, SnapshotEntry>,
    local: &BTreeMap<RelativePath, SnapshotEntry>,
    remote_tips: &[RemoteTip],
) -> Result<SyncPlan, SyncError> {
    let mut tips = remote_tips.to_vec();
    tips.sort_by_key(|tip| tip.commit_id);
    for pair in tips.windows(2) {
        if pair[0].commit_id == pair[1].commit_id {
            return Err(SyncError::Integrity(format!(
                "duplicate remote tip ID: {}",
                pair[0].commit_id
            )));
        }
    }
    let mut plan = SyncPlan {
        apply_remote: BTreeMap::new(),
        publish_local: BTreeMap::new(),
        conflicts: Vec::new(),
        merge_parents: tips.iter().map(|tip| tip.commit_id).collect(),
        local_snapshot: local.clone(),
        remote_tips: tips.clone(),
    };

    let mut paths: BTreeSet<_> = baseline.keys().chain(local.keys()).cloned().collect();
    for tip in &tips {
        paths.extend(tip.snapshot.keys().cloned());
    }
    for path in paths {
        let base = baseline.get(&path);
        let current = local.get(&path);
        let mut candidates: Vec<RemoteVersion> = Vec::new();
        for tip in &tips {
            let entry = tip.snapshot.get(&path);
            if let Some(candidate) = candidates
                .iter_mut()
                .find(|candidate| same_version(candidate.entry.as_ref(), entry))
            {
                candidate.commit_ids.push(tip.commit_id);
            } else {
                candidates.push(RemoteVersion {
                    commit_ids: vec![tip.commit_id],
                    entry: entry.cloned(),
                });
            }
        }

        let changed_candidates: Vec<_> = candidates
            .iter()
            .filter(|candidate| !same_content(base, candidate.entry.as_ref()))
            .collect();
        if changed_candidates.iter().skip(1).any(|candidate| {
            !same_content(
                changed_candidates[0].entry.as_ref(),
                candidate.entry.as_ref(),
            )
        }) {
            plan.conflicts.push(Conflict {
                path,
                local: current.cloned(),
                remote_candidates: candidates,
            });
            continue;
        }
        let remote = changed_candidates
            .first()
            .map_or(base, |candidate| candidate.entry.as_ref());
        let local_changed = !same_content(base, current);
        let remote_changed = !changed_candidates.is_empty();
        if local_changed && remote_changed && !same_content(current, remote) {
            plan.conflicts.push(Conflict {
                path,
                local: current.cloned(),
                remote_candidates: candidates,
            });
        } else if remote_changed && !local_changed {
            plan.apply_remote.insert(path, entry_for_action(remote));
        } else if local_changed && !remote_changed {
            plan.publish_local.insert(path, entry_for_action(current));
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> RelativePath {
        RelativePath::new(&format!("touchHLE_apps/{name}")).unwrap()
    }

    fn file(hash: u8) -> SnapshotEntry {
        SnapshotEntry::File {
            sha256: [hash; 32],
            size: 1,
            modified_unix_ms: 10,
        }
    }

    fn snapshot(
        entries: &[(&str, Option<SnapshotEntry>)],
    ) -> BTreeMap<RelativePath, SnapshotEntry> {
        entries
            .iter()
            .filter_map(|(name, entry)| entry.clone().map(|entry| (path(name), entry)))
            .collect()
    }

    fn tip(id: u128, entries: &[(&str, Option<SnapshotEntry>)]) -> RemoteTip {
        RemoteTip {
            commit_id: Uuid::from_u128(id),
            snapshot: snapshot(entries),
        }
    }

    fn commit(id: u128, parents: &[u128], entries: &[(&str, Option<SnapshotEntry>)]) -> Commit {
        Commit {
            id: Uuid::from_u128(id),
            device_id: Uuid::nil(),
            created_unix_ms: 0,
            parents: parents.iter().copied().map(Uuid::from_u128).collect(),
            entries: snapshot(entries),
        }
    }

    #[test]
    fn single_tip_planning_cases() {
        struct Case {
            name: &'static str,
            baseline: Option<SnapshotEntry>,
            local: Option<SnapshotEntry>,
            remote: Option<SnapshotEntry>,
            apply: Option<SnapshotEntry>,
            publish: Option<SnapshotEntry>,
            conflict: bool,
        }
        let cases = [
            Case {
                name: "unchanged",
                baseline: Some(file(1)),
                local: Some(file(1)),
                remote: Some(file(1)),
                apply: None,
                publish: None,
                conflict: false,
            },
            Case {
                name: "local edit",
                baseline: Some(file(1)),
                local: Some(file(2)),
                remote: Some(file(1)),
                apply: None,
                publish: Some(file(2)),
                conflict: false,
            },
            Case {
                name: "remote edit",
                baseline: Some(file(1)),
                local: Some(file(1)),
                remote: Some(file(2)),
                apply: Some(file(2)),
                publish: None,
                conflict: false,
            },
            Case {
                name: "matching edits",
                baseline: Some(file(1)),
                local: Some(file(2)),
                remote: Some(file(2)),
                apply: None,
                publish: None,
                conflict: false,
            },
            Case {
                name: "different edits",
                baseline: Some(file(1)),
                local: Some(file(2)),
                remote: Some(file(3)),
                apply: None,
                publish: None,
                conflict: true,
            },
            Case {
                name: "local addition",
                baseline: None,
                local: Some(file(2)),
                remote: None,
                apply: None,
                publish: Some(file(2)),
                conflict: false,
            },
            Case {
                name: "remote addition",
                baseline: None,
                local: None,
                remote: Some(file(2)),
                apply: Some(file(2)),
                publish: None,
                conflict: false,
            },
            Case {
                name: "matching additions",
                baseline: None,
                local: Some(file(2)),
                remote: Some(file(2)),
                apply: None,
                publish: None,
                conflict: false,
            },
            Case {
                name: "different additions",
                baseline: None,
                local: Some(file(2)),
                remote: Some(file(3)),
                apply: None,
                publish: None,
                conflict: true,
            },
            Case {
                name: "local deletion",
                baseline: Some(file(1)),
                local: None,
                remote: Some(file(1)),
                apply: None,
                publish: Some(SnapshotEntry::Tombstone),
                conflict: false,
            },
            Case {
                name: "remote deletion",
                baseline: Some(file(1)),
                local: Some(file(1)),
                remote: Some(SnapshotEntry::Tombstone),
                apply: Some(SnapshotEntry::Tombstone),
                publish: None,
                conflict: false,
            },
            Case {
                name: "delete versus edit",
                baseline: Some(file(1)),
                local: None,
                remote: Some(file(2)),
                apply: None,
                publish: None,
                conflict: true,
            },
            Case {
                name: "edit versus delete",
                baseline: Some(file(1)),
                local: Some(file(2)),
                remote: None,
                apply: None,
                publish: None,
                conflict: true,
            },
            Case {
                name: "matching deletion",
                baseline: Some(file(1)),
                local: None,
                remote: Some(SnapshotEntry::Tombstone),
                apply: None,
                publish: None,
                conflict: false,
            },
            Case {
                name: "unchanged tombstone",
                baseline: Some(SnapshotEntry::Tombstone),
                local: None,
                remote: None,
                apply: None,
                publish: None,
                conflict: false,
            },
        ];
        for case in cases {
            let baseline = snapshot(&[("entry", case.baseline)]);
            let local = snapshot(&[("entry", case.local)]);
            let remote = tip(2, &[("entry", case.remote)]);
            let plan = plan_sync(&baseline, &local, &[remote.clone()]).unwrap();
            assert_eq!(
                plan.apply_remote.get(&path("entry")),
                case.apply.as_ref(),
                "{}",
                case.name
            );
            assert_eq!(
                plan.publish_local.get(&path("entry")),
                case.publish.as_ref(),
                "{}",
                case.name
            );
            assert_eq!(
                plan.conflicts.len(),
                usize::from(case.conflict),
                "{}",
                case.name
            );
            assert_eq!(plan.merge_parents, vec![remote.commit_id], "{}", case.name);
            assert_eq!(plan.local_snapshot, local, "{}", case.name);
            assert_eq!(plan.remote_tips, vec![remote], "{}", case.name);
        }
    }

    #[test]
    fn metadata_does_not_create_changes_or_extra_remote_candidates() {
        let baseline = snapshot(&[("entry", Some(file(1)))]);
        let local = snapshot(&[(
            "entry",
            Some(SnapshotEntry::File {
                sha256: [1; 32],
                size: 999,
                modified_unix_ms: 999,
            }),
        )]);
        let tips = [
            tip(2, &[("entry", Some(file(1)))]),
            tip(3, &[("entry", Some(file(1)))]),
        ];
        let plan = plan_sync(&baseline, &local, &tips).unwrap();
        assert!(plan.apply_remote.is_empty());
        assert!(plan.publish_local.is_empty());
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn divergent_tips_retain_every_version_and_parent() {
        let baseline = snapshot(&[("entry", Some(file(1)))]);
        let local = baseline.clone();
        let tips = [
            tip(4, &[("entry", Some(file(3)))]),
            tip(2, &[("entry", Some(file(2)))]),
            tip(3, &[("entry", Some(file(2)))]),
        ];
        let plan = plan_sync(&baseline, &local, &tips).unwrap();
        assert_eq!(
            plan.merge_parents,
            vec![Uuid::from_u128(2), Uuid::from_u128(3), Uuid::from_u128(4)]
        );
        assert!(plan.apply_remote.is_empty());
        assert!(plan.publish_local.is_empty());
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].path, path("entry"));
        assert_eq!(plan.conflicts[0].local, Some(file(1)));
        assert_eq!(
            plan.conflicts[0].remote_candidates,
            vec![
                RemoteVersion {
                    commit_ids: vec![Uuid::from_u128(2), Uuid::from_u128(3)],
                    entry: Some(file(2))
                },
                RemoteVersion {
                    commit_ids: vec![Uuid::from_u128(4)],
                    entry: Some(file(3))
                },
            ]
        );
    }

    #[test]
    fn local_edit_against_multiple_cloud_versions_keeps_all_choices() {
        let baseline = snapshot(&[("entry", Some(file(1)))]);
        let local = snapshot(&[("entry", Some(file(4)))]);
        let tips = [
            tip(2, &[("entry", Some(file(2)))]),
            tip(3, &[("entry", Some(file(3)))]),
        ];
        let plan = plan_sync(&baseline, &local, &tips).unwrap();
        assert_eq!(plan.conflicts[0].local, Some(file(4)));
        assert_eq!(plan.conflicts[0].remote_candidates.len(), 2);
        assert_eq!(
            plan.conflicts[0].remote_candidates[0].commit_ids,
            vec![Uuid::from_u128(2)]
        );
        assert_eq!(
            plan.conflicts[0].remote_candidates[1].commit_ids,
            vec![Uuid::from_u128(3)]
        );
        let choices = [
            ConflictChoice {
                path: path("entry"),
                selected: LocalOrRemote::Local,
                remote_commit_id: None,
            },
            ConflictChoice {
                path: path("entry"),
                selected: LocalOrRemote::Remote,
                remote_commit_id: Some(Uuid::from_u128(3)),
            },
        ];
        for choice in &choices {
            choice.validate(&plan).unwrap();
        }
        for invalid in [
            ConflictChoice {
                path: path("entry"),
                selected: LocalOrRemote::Local,
                remote_commit_id: Some(Uuid::from_u128(2)),
            },
            ConflictChoice {
                path: path("entry"),
                selected: LocalOrRemote::Remote,
                remote_commit_id: None,
            },
            ConflictChoice {
                path: path("entry"),
                selected: LocalOrRemote::Remote,
                remote_commit_id: Some(Uuid::from_u128(9)),
            },
            ConflictChoice {
                path: path("missing"),
                selected: LocalOrRemote::Local,
                remote_commit_id: None,
            },
        ] {
            assert!(matches!(
                invalid.validate(&plan),
                Err(SyncError::UnresolvedConflicts(_))
            ));
        }
    }

    #[test]
    fn conflicts_are_sorted_and_include_unchanged_cloud_candidates() {
        let baseline = snapshot(&[("a", Some(file(1))), ("z", Some(file(1)))]);
        let local = snapshot(&[("a", Some(file(3))), ("z", Some(file(3)))]);
        let plan = plan_sync(
            &baseline,
            &local,
            &[
                tip(3, &[("a", Some(file(2))), ("z", Some(file(2)))]),
                tip(2, &[("a", Some(file(1))), ("z", Some(file(1)))]),
            ],
        )
        .unwrap();
        assert_eq!(
            plan.conflicts
                .iter()
                .map(|conflict| conflict.path.as_str())
                .collect::<Vec<_>>(),
            vec!["touchHLE_apps/a", "touchHLE_apps/z"]
        );
        assert_eq!(
            plan.conflicts[0].remote_candidates,
            vec![
                RemoteVersion {
                    commit_ids: vec![Uuid::from_u128(2)],
                    entry: Some(file(1))
                },
                RemoteVersion {
                    commit_ids: vec![Uuid::from_u128(3)],
                    entry: Some(file(2))
                },
            ]
        );
    }

    #[test]
    fn empty_remote_history_does_not_delete_the_baseline() {
        let baseline = snapshot(&[("entry", Some(file(1)))]);
        let plan = plan_sync(&baseline, &baseline, &[]).unwrap();
        assert!(plan.apply_remote.is_empty());
        assert!(plan.publish_local.is_empty());
        assert!(plan.conflicts.is_empty());
        assert!(plan.merge_parents.is_empty());
    }

    #[test]
    fn absent_and_tombstone_tips_converge_on_deletion() {
        let baseline = snapshot(&[("entry", Some(file(1)))]);
        let plan = plan_sync(
            &baseline,
            &baseline,
            &[
                tip(2, &[("entry", None)]),
                tip(3, &[("entry", Some(SnapshotEntry::Tombstone))]),
            ],
        )
        .unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.apply_remote[&path("entry")], SnapshotEntry::Tombstone);
        assert_eq!(plan.merge_parents.len(), 2);
    }

    #[test]
    fn conflicting_edit_preserves_absence_and_tombstone_as_separate_sources() {
        let baseline = snapshot(&[("entry", Some(file(1)))]);
        let local = snapshot(&[("entry", Some(file(2)))]);
        let plan = plan_sync(
            &baseline,
            &local,
            &[
                tip(2, &[("entry", None)]),
                tip(3, &[("entry", Some(SnapshotEntry::Tombstone))]),
            ],
        )
        .unwrap();
        assert_eq!(
            plan.conflicts[0].remote_candidates,
            vec![
                RemoteVersion {
                    commit_ids: vec![Uuid::from_u128(2)],
                    entry: None
                },
                RemoteVersion {
                    commit_ids: vec![Uuid::from_u128(3)],
                    entry: Some(SnapshotEntry::Tombstone)
                },
            ]
        );
        for id in [2, 3] {
            ConflictChoice {
                path: path("entry"),
                selected: LocalOrRemote::Remote,
                remote_commit_id: Some(Uuid::from_u128(id)),
            }
            .validate(&plan)
            .unwrap();
        }
    }

    #[test]
    fn concurrent_commits_and_conflict_free_merge_keep_both_parents() {
        let root = commit(1, &[], &[("base", Some(file(1)))]);
        let left = commit(2, &[1], &[("base", Some(file(1))), ("left", Some(file(2)))]);
        let right = commit(
            3,
            &[1],
            &[("base", Some(file(1))), ("right", Some(file(3)))],
        );
        let tips = resolve_remote_tips(&[right, root.clone(), left]).unwrap();
        assert_eq!(
            tips.iter().map(|tip| tip.commit_id).collect::<Vec<_>>(),
            vec![Uuid::from_u128(2), Uuid::from_u128(3)]
        );
        let plan = plan_sync(&root.entries, &root.entries, &tips).unwrap();
        assert_eq!(
            plan.merge_parents,
            vec![Uuid::from_u128(2), Uuid::from_u128(3)]
        );
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.apply_remote.len(), 2);
    }

    #[test]
    fn unresolved_graphs_and_duplicate_commit_ids_are_rejected() {
        let missing = commit(2, &[1], &[]);
        assert!(matches!(
            resolve_remote_tips(&[missing]),
            Err(SyncError::Integrity(_))
        ));
        let duplicate = commit(1, &[], &[]);
        assert!(matches!(
            resolve_remote_tips(&[duplicate.clone(), duplicate]),
            Err(SyncError::Integrity(_))
        ));
        let a = commit(1, &[2], &[]);
        let b = commit(2, &[1], &[]);
        assert!(matches!(
            resolve_remote_tips(&[a, b]),
            Err(SyncError::Integrity(_))
        ));
        assert!(matches!(
            resolve_remote_tips(&[commit(1, &[1], &[])]),
            Err(SyncError::Integrity(_))
        ));
        assert!(resolve_remote_tips(&[]).unwrap().is_empty());
    }

    #[test]
    fn duplicate_remote_tip_ids_are_rejected() {
        let remote = tip(2, &[("entry", Some(file(1)))]);
        assert!(matches!(
            plan_sync(
                &BTreeMap::new(),
                &BTreeMap::new(),
                &[remote.clone(), remote]
            ),
            Err(SyncError::Integrity(_))
        ));
    }
}
