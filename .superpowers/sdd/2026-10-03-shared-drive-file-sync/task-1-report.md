# Task 1 Report: Path-Based Reconciliation Model

## Status

Implemented and verified. Commit details are provided in the final response.

## Implementation

- Added `src/sync/reconcile.rs` with remote file/change records, per-path
  baseline metadata, generic Drive-file/legacy-commit candidate IDs, conflict
  choices, one-path comparison, whole-tree planning, and candidate selection.
- SHA-256 alone defines byte identity; absence represents deletion. Plans
  produce no apply/publish operation on a divergent path and one conflict.
  Remote deletion uses the saved file ID when available.
- Reused the snapshot decoder's Unicode case-folding and file-ancestor checks
  in the planner and in current baseline decoding. Checked the combined path
  set for case-insensitive collisions.
- Added `CurrentSyncState` (schema version 2) with account/root identity,
  Changes cursor, file baselines, and ID-to-path maps. `load_sync_state`
  distinguishes missing, legacy, and current JSON; invalid or unsupported
  checkpoint JSON produces `SyncError::Integrity`. Existing `SyncState` and
  `Commit` remain unchanged for current callers; `LegacySyncState` names the
  legacy shape explicitly.
- `prune_deleted_paths` drops a baseline and file ID mapping once the file is
  absent locally and remotely, but retains the baseline while local bytes may
  still need offline comparison.
- Added only `pub mod reconcile;` to `src/sync.rs`. Its other uncommitted module
  declarations were present beforehand and were not part of this task.

## TDD Evidence

- RED: `cargo test --lib sync::reconcile::tests` exited 101 with absent
  `RemoteFile`, `FileBaseline`, `RemoteCandidate`, `plan_path`, `plan_sync`,
  and `PathAction` compilation errors.
- GREEN planner: `cargo test --lib sync::reconcile::tests`: 3 passed.
- RED checkpoint: `cargo test --lib sync::model` exited 101 with missing
  `CurrentSyncState`, `LoadedSyncState`, and `load_sync_state` errors.
- GREEN checkpoint: `cargo test --lib sync::model`: 7 passed.
- Final focused: `cargo test --lib sync::reconcile`: 4 passed;
  `cargo test --lib sync::model`: 7 passed.
- Full suite: `cargo test --lib`: 255 passed, 0 failed.
- `git diff --check`: passed. Existing legacy-state validation tests remain.

## Self-Review And Concerns

- Verified that the new loader decodes from the original byte stream after
  classifying JSON, so duplicate baseline keys remain detectable. Invalid
  paths and case/ancestor collisions fail conservatively.
- Existing `engine.rs` no-follow reads and atomic state writes were left
  unchanged. The new loader is a pure bytes-to-state decoder: wiring it into
  protected checkpoint file I/O, Drive Changes discovery, and the engine is
  left to the later migration tasks. No remote/integration behavior is claimed.
- `plan_path` cannot infer a path for a deletion conflict without a remote file
  record; `plan_sync` supplies the path and should be used for such cases.
  Missing saved remote identity when an absent remote requires an apply is an
  integrity error, not a guessed deletion.
- The broad working tree contains pre-existing modifications and untracked
  files. Only the Task 1 source hunks were selected for the commit.

## Fix Round 1/5

### Review Findings Addressed

- **High, planner union validation:** `plan_sync` formerly replaced every path
  in the combined local/remote path set with a tombstone. It now overlays the
  actual local and remote entries before validating. Thus paths that are valid
  separately but put a live file above another live file across trees are
  rejected as an integrity error. Baselines are independently validated using
  their live/deleted state.
- **Medium, current checkpoint baseline validation:** baseline entries with a
  SHA-256 are validated as live files; entries with no SHA-256 are treated as
  deletion records. Checkpoint validation now rejects live file ancestors
  without rejecting a tombstone ancestor.

### Regression Tests And Results

- RED: `cargo test --lib sync::reconcile::tests::plan_rejects_live_file_ancestor_across_local_and_remote_trees`
  failed because the planner returned a plan instead of `SyncError::Integrity`.
- RED: `cargo test --lib sync::model::tests::current_baseline_rejects_live_file_ancestors_but_allows_deleted_ancestor`
  failed because the decoder accepted two live file baselines at `foo` and
  `foo/bar`.
- GREEN: `cargo test --lib sync::reconcile`: **5 passed, 0 failed**.
- GREEN: `cargo test --lib sync::model`: **8 passed, 0 failed**.
- `rustfmt --edition 2021 src/sync/reconcile.rs src/sync/model.rs`: passed.
- `git diff --check`: passed.

### Self-Review

The new planner regression uses an empty baseline and separate valid local and
remote trees, proving rejection comes from validating live entries across the
combined selected tree. The checkpoint regression checks both sides of the
baseline semantics: two SHA-bearing entries conflict, while a missing-hash
ancestor and live child decode successfully. Existing tombstone behavior is
preserved.
