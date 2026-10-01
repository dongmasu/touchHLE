/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::model::{Commit, LocalOrRemote, RelativePath, SnapshotEntry, SyncError};
use std::collections::{BTreeMap, BTreeSet};
use unicode_casefold::UnicodeCaseFold;
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

impl SyncPlan {
    /// Call before staging, applying, or committing any plan actions.
    /// Individual choices alone cannot establish a valid combined tree.
    pub fn resolved_snapshot(
        &self,
        choices: &[ConflictChoice],
    ) -> Result<BTreeMap<RelativePath, SnapshotEntry>, SyncError> {
        let mut selections = BTreeMap::new();
        for choice in choices {
            choice.validate(self)?;
            if selections.insert(&choice.path, choice).is_some() {
                return Err(SyncError::UnresolvedConflicts(format!(
                    "duplicate choice at {}",
                    choice.path.as_str()
                )));
            }
        }
        if selections.len() != self.conflicts.len() {
            return Err(SyncError::UnresolvedConflicts(
                "a choice is required for every conflict".to_owned(),
            ));
        }

        let mut resolved = self.local_snapshot.clone();
        resolved.extend(self.publish_local.clone());
        resolved.extend(self.apply_remote.clone());
        for conflict in &self.conflicts {
            let choice = selections.get(&conflict.path).ok_or_else(|| {
                SyncError::UnresolvedConflicts(format!(
                    "missing choice at {}",
                    conflict.path.as_str()
                ))
            })?;
            let entry = match choice.selected {
                LocalOrRemote::Local => conflict.local.as_ref(),
                LocalOrRemote::Remote => conflict
                    .remote_candidates
                    .iter()
                    .find(|candidate| {
                        candidate
                            .commit_ids
                            .contains(&choice.remote_commit_id.unwrap())
                    })
                    .and_then(|candidate| candidate.entry.as_ref()),
            };
            resolved.insert(conflict.path.clone(), entry_for_action(entry));
        }

        // A tombstone for an old spelling cannot share a directory with a
        // selected file whose spelling differs only by case.
        let live_paths: Vec<_> = resolved
            .iter()
            .filter_map(|(path, entry)| {
                matches!(entry, SnapshotEntry::File { .. }).then_some(path.clone())
            })
            .collect();
        resolved.retain(|path, entry| {
            !matches!(entry, SnapshotEntry::Tombstone)
                || !live_paths.iter().any(|live| mismatched_prefix(path, live))
        });
        validate_tree(&resolved)?;
        Ok(resolved)
    }
}

fn mismatched_prefix(left: &RelativePath, right: &RelativePath) -> bool {
    for (left, right) in left.as_str().split('/').zip(right.as_str().split('/')) {
        if left.case_fold().collect::<String>() != right.case_fold().collect::<String>() {
            return false;
        }
        if left != right {
            return true;
        }
    }
    false
}

fn incompatible_paths(
    left: (&RelativePath, &SnapshotEntry),
    right: (&RelativePath, &SnapshotEntry),
) -> bool {
    if left.0 == right.0 {
        return false;
    }
    let left_parts: Vec<_> = left.0.as_str().split('/').collect();
    let right_parts: Vec<_> = right.0.as_str().split('/').collect();
    for (a, b) in left_parts.iter().zip(&right_parts) {
        if a.case_fold().collect::<String>() != b.case_fold().collect::<String>() {
            return false;
        }
        if a != b {
            return true;
        }
    }
    if left_parts.len() < right_parts.len() {
        matches!(left.1, SnapshotEntry::File { .. })
    } else {
        matches!(right.1, SnapshotEntry::File { .. })
    }
}

fn validate_tree(entries: &BTreeMap<RelativePath, SnapshotEntry>) -> Result<(), SyncError> {
    let mut spellings = BTreeMap::<String, String>::new();
    let live: BTreeSet<_> = entries
        .iter()
        .filter_map(|(path, entry)| {
            matches!(entry, SnapshotEntry::File { .. }).then_some(path.as_str())
        })
        .collect();
    for path in entries.keys() {
        let mut prefix = String::new();
        for component in path.as_str().split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let folded = prefix.as_str().case_fold().collect::<String>();
            if let Some(previous) = spellings.insert(folded, prefix.clone()) {
                if previous != prefix {
                    return Err(SyncError::UnresolvedConflicts(format!(
                        "case-insensitive path collision: {previous} and {prefix}"
                    )));
                }
            }
        }
    }
    for (path, entry) in entries {
        if !matches!(entry, SnapshotEntry::File { .. }) {
            continue;
        }
        let mut current = path.as_str();
        while let Some((parent, _)) = current.rsplit_once('/') {
            if live.contains(parent) {
                return Err(SyncError::UnresolvedConflicts(format!(
                    "file/descendant collision: {parent} and {}",
                    path.as_str()
                )));
            }
            current = parent;
        }
    }
    Ok(())
}

fn mark_cross_tree_collisions(
    left: &BTreeMap<RelativePath, SnapshotEntry>,
    right: &BTreeMap<RelativePath, SnapshotEntry>,
    paths: &mut BTreeSet<RelativePath>,
) {
    for l in left {
        for r in right {
            if incompatible_paths(l, r) {
                paths.insert(l.0.clone());
                paths.insert(r.0.clone());
            }
        }
    }
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
    let mut structural_paths = BTreeSet::new();
    for (index, tip) in tips.iter().enumerate() {
        mark_cross_tree_collisions(local, &tip.snapshot, &mut structural_paths);
        for later in &tips[index + 1..] {
            mark_cross_tree_collisions(&tip.snapshot, &later.snapshot, &mut structural_paths);
        }
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
        if structural_paths.contains(&path)
            || changed_candidates.iter().skip(1).any(|candidate| {
                !same_content(
                    changed_candidates[0].entry.as_ref(),
                    candidate.entry.as_ref(),
                )
            })
        {
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

    #[test]
    fn file_and_descendant_in_opposite_trees_require_compatible_path_choices() {
        for (local_name, remote_name) in [("a", "a/b"), ("a/b", "a")] {
            let local = snapshot(&[(local_name, Some(file(1)))]);
            let remote = tip(2, &[(remote_name, Some(file(2)))]);
            let plan = plan_sync(&BTreeMap::new(), &local, &[remote.clone()]).unwrap();
            assert!(plan.apply_remote.is_empty());
            assert!(plan.publish_local.is_empty());
            assert_eq!(
                plan.conflicts.iter().map(|c| &c.path).collect::<Vec<_>>(),
                vec![&path("a"), &path("a/b")]
            );
            let remote_at_local = &plan
                .conflicts
                .iter()
                .find(|c| c.path == path(local_name))
                .unwrap()
                .remote_candidates;
            assert_eq!(
                remote_at_local,
                &vec![RemoteVersion {
                    commit_ids: vec![remote.commit_id],
                    entry: None,
                }]
            );
            let remote_at_remote = &plan
                .conflicts
                .iter()
                .find(|c| c.path == path(remote_name))
                .unwrap()
                .remote_candidates;
            assert_eq!(
                remote_at_remote,
                &vec![RemoteVersion {
                    commit_ids: vec![remote.commit_id],
                    entry: Some(file(2)),
                }]
            );
            let choices = [
                ConflictChoice {
                    path: path(local_name),
                    selected: LocalOrRemote::Local,
                    remote_commit_id: None,
                },
                ConflictChoice {
                    path: path(remote_name),
                    selected: LocalOrRemote::Remote,
                    remote_commit_id: Some(remote.commit_id),
                },
            ];
            assert!(matches!(
                plan.resolved_snapshot(&choices),
                Err(SyncError::UnresolvedConflicts(_))
            ));
            let keep_local = [
                ConflictChoice {
                    path: path(local_name),
                    selected: LocalOrRemote::Local,
                    remote_commit_id: None,
                },
                ConflictChoice {
                    path: path(remote_name),
                    selected: LocalOrRemote::Local,
                    remote_commit_id: None,
                },
            ];
            assert_eq!(
                plan.resolved_snapshot(&keep_local)
                    .unwrap()
                    .get(&path(local_name)),
                Some(&file(1))
            );
            let keep_remote = [
                ConflictChoice {
                    path: path(local_name),
                    selected: LocalOrRemote::Remote,
                    remote_commit_id: Some(remote.commit_id),
                },
                ConflictChoice {
                    path: path(remote_name),
                    selected: LocalOrRemote::Remote,
                    remote_commit_id: Some(remote.commit_id),
                },
            ];
            assert_eq!(
                plan.resolved_snapshot(&keep_remote)
                    .unwrap()
                    .get(&path(remote_name)),
                Some(&file(2))
            );
            assert_eq!(plan.merge_parents, vec![remote.commit_id]);
            assert_eq!(plan.local_snapshot, local);
            assert_eq!(plan.remote_tips, vec![remote]);
        }
    }

    #[test]
    fn cross_tree_case_collisions_require_compatible_path_choices() {
        for (local_name, remote_name) in [
            ("Save", "save"),
            ("straße", "strasse"),
            ("Save/one", "save/two"),
            ("Save", "save/child"),
            ("Save/child", "save"),
        ] {
            let local = snapshot(&[(local_name, Some(file(1)))]);
            let remote = tip(2, &[(remote_name, Some(file(2)))]);
            let plan = plan_sync(&BTreeMap::new(), &local, &[remote]).unwrap();
            assert!(plan.apply_remote.is_empty(), "{local_name} / {remote_name}");
            assert!(
                plan.publish_local.is_empty(),
                "{local_name} / {remote_name}"
            );
            assert_eq!(plan.conflicts.len(), 2, "{local_name} / {remote_name}");
            let incompatible = [
                ConflictChoice {
                    path: path(local_name),
                    selected: LocalOrRemote::Local,
                    remote_commit_id: None,
                },
                ConflictChoice {
                    path: path(remote_name),
                    selected: LocalOrRemote::Remote,
                    remote_commit_id: Some(Uuid::from_u128(2)),
                },
            ];
            assert!(
                plan.resolved_snapshot(&incompatible).is_err(),
                "{local_name} / {remote_name}"
            );
            let local_choices = [
                ConflictChoice {
                    path: path(local_name),
                    selected: LocalOrRemote::Local,
                    remote_commit_id: None,
                },
                ConflictChoice {
                    path: path(remote_name),
                    selected: LocalOrRemote::Local,
                    remote_commit_id: None,
                },
            ];
            let selected_local = plan.resolved_snapshot(&local_choices).unwrap();
            assert_eq!(selected_local.get(&path(local_name)), Some(&file(1)));
            assert!(!selected_local.contains_key(&path(remote_name)));
            let remote_choices = [
                ConflictChoice {
                    path: path(local_name),
                    selected: LocalOrRemote::Remote,
                    remote_commit_id: Some(Uuid::from_u128(2)),
                },
                ConflictChoice {
                    path: path(remote_name),
                    selected: LocalOrRemote::Remote,
                    remote_commit_id: Some(Uuid::from_u128(2)),
                },
            ];
            let selected_remote = plan.resolved_snapshot(&remote_choices).unwrap();
            assert_eq!(selected_remote.get(&path(remote_name)), Some(&file(2)));
            assert!(!selected_remote.contains_key(&path(local_name)));
        }
    }

    #[test]
    fn all_conflict_choices_are_required_once_before_resolution() {
        let plan = plan_sync(
            &BTreeMap::new(),
            &snapshot(&[("a", Some(file(1)))]),
            &[tip(2, &[("a/b", Some(file(2)))])],
        )
        .unwrap();
        let choice = ConflictChoice {
            path: path("a"),
            selected: LocalOrRemote::Local,
            remote_commit_id: None,
        };
        assert!(plan.resolved_snapshot(&[]).is_err());
        assert!(plan.resolved_snapshot(&[choice.clone()]).is_err());
        assert!(plan.resolved_snapshot(&[choice.clone(), choice]).is_err());
    }

    #[test]
    fn resolved_snapshot_preserves_noncolliding_deletion_tombstones() {
        let baseline = snapshot(&[("entry", Some(file(1)))]);
        let local = snapshot(&[("entry", Some(file(2)))]);
        let plan = plan_sync(&baseline, &local, &[tip(2, &[("entry", None)])]).unwrap();
        let resolved = plan
            .resolved_snapshot(&[ConflictChoice {
                path: path("entry"),
                selected: LocalOrRemote::Remote,
                remote_commit_id: Some(Uuid::from_u128(2)),
            }])
            .unwrap();
        assert_eq!(
            resolved.get(&path("entry")),
            Some(&SnapshotEntry::Tombstone)
        );
    }

    #[test]
    fn structural_conflicts_block_overlapping_actions_but_not_unrelated_changes() {
        let local = snapshot(&[("a", Some(file(1))), ("free", Some(file(4)))]);
        let tips = [
            tip(2, &[("a/b", Some(file(2))), ("remote", Some(file(3)))]),
            tip(3, &[("a/c", Some(file(5))), ("remote", Some(file(3)))]),
        ];
        let plan = plan_sync(&BTreeMap::new(), &local, &tips).unwrap();
        assert_eq!(
            plan.conflicts
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>(),
            vec!["touchHLE_apps/a", "touchHLE_apps/a/b", "touchHLE_apps/a/c"]
        );
        assert_eq!(
            plan.conflicts[1].remote_candidates,
            vec![
                RemoteVersion {
                    commit_ids: vec![Uuid::from_u128(2)],
                    entry: Some(file(2))
                },
                RemoteVersion {
                    commit_ids: vec![Uuid::from_u128(3)],
                    entry: None
                },
            ]
        );
        assert_eq!(plan.publish_local, snapshot(&[("free", Some(file(4)))]));
        assert_eq!(plan.apply_remote, snapshot(&[("remote", Some(file(3)))]));
        assert_eq!(
            plan.merge_parents,
            vec![Uuid::from_u128(2), Uuid::from_u128(3)]
        );
        let choices = [
            ConflictChoice {
                path: path("a"),
                selected: LocalOrRemote::Remote,
                remote_commit_id: Some(Uuid::from_u128(2)),
            },
            ConflictChoice {
                path: path("a/b"),
                selected: LocalOrRemote::Remote,
                remote_commit_id: Some(Uuid::from_u128(2)),
            },
            ConflictChoice {
                path: path("a/c"),
                selected: LocalOrRemote::Remote,
                remote_commit_id: Some(Uuid::from_u128(2)),
            },
        ];
        assert_eq!(
            plan.resolved_snapshot(&choices).unwrap().get(&path("a/b")),
            Some(&file(2))
        );
    }

    #[test]
    fn incompatible_remote_tips_without_local_files_are_not_auto_applied() {
        let tips = [
            tip(2, &[("a", Some(file(1)))]),
            tip(3, &[("a/b", Some(file(2)))]),
        ];
        let plan = plan_sync(&BTreeMap::new(), &BTreeMap::new(), &tips).unwrap();
        assert!(plan.apply_remote.is_empty());
        assert!(plan.publish_local.is_empty());
        assert_eq!(plan.conflicts.len(), 2);
        assert_eq!(plan.conflicts[0].remote_candidates.len(), 2);
        let incompatible = [
            ConflictChoice {
                path: path("a"),
                selected: LocalOrRemote::Remote,
                remote_commit_id: Some(Uuid::from_u128(2)),
            },
            ConflictChoice {
                path: path("a/b"),
                selected: LocalOrRemote::Remote,
                remote_commit_id: Some(Uuid::from_u128(3)),
            },
        ];
        assert!(plan.resolved_snapshot(&incompatible).is_err());
    }

    #[test]
    fn tombstone_ancestor_does_not_block_a_live_descendant() {
        let local = snapshot(&[("a", Some(SnapshotEntry::Tombstone))]);
        let remote = tip(2, &[("a/b", Some(file(2)))]);
        let plan = plan_sync(&BTreeMap::new(), &local, &[remote]).unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.apply_remote, snapshot(&[("a/b", Some(file(2)))]));
    }
}
