# Shared Google Drive File Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace unbounded whole-snapshot commits with fast, multi-device synchronization of the shared current files under Google Drive `touchHLE/files`.

**Architecture:** Introduce a provider-independent per-path reconciliation model backed by a local baseline and Drive Changes cursor. Make Drive v3 expose a mirrored managed file tree, changed-file feed, and direct file reads/writes. Migrate legacy commit state once, then stop reading or writing commits, objects, and pending uploads.

**Tech Stack:** Rust 2021, serde/serde_json, reqwest blocking Drive v3 API, SHA-256, existing filesystem staging and locking, Android JNI/WorkManager.

**Spec:** `docs/superpowers/specs/2026-10-03-shared-drive-file-sync-design.md`

## Global Constraints

- The shared current Drive files, not per-device cloud heads or a manifest, are the synchronization authority.
- Each installation's baseline and Changes page token are local comparison/checkpoint data only.
- A warm no-change sync performs zero remote media downloads/uploads and never enumerates legacy commits or objects.
- Resolve divergent same-path changes in the existing UI; only the chosen version remains in touchHLE sync state.
- Keep legacy commits and objects untouched until the new file tree is verified; never automatically delete legacy data.
- Do not claim cross-device atomic compare-and-swap; preflight and post-write checks are best effort.
- Keep current path validation, SHA-256 verification, no-follow filesystem access, staged apply, and local operation/guest locks.
- Remove cloud `pending/` writes; do not publish or apply remote changes while the guest-session lock is held.
- Preserve all unrelated and pre-existing worktree changes; inspect each target file's current diff before modifying it.
- Add no dependency unless a demonstrated Drive API requirement cannot be met with existing dependencies.

## File Map And Interfaces

- `src/sync/model.rs`: retain `RelativePath` and shared errors; add current-state checkpoint types while preserving a legacy-state decoder.
- `src/sync/reconcile.rs` (new): pure path-based three-way planner and file-based `SyncPlan`/conflict types.
- `src/sync/store.rs`: define the direct current-file/Changes protocol and deterministic memory implementation; temporarily retain legacy-read access for migration.
- `src/sync/gdrive.rs`: implement recursive inventory, Changes pagination, ID/path mapping, current-file read/write/trash, metadata verification, and legacy read-only loading.
- `src/sync/engine.rs`: orchestrate baseline comparison, conflict resolution, transfers, staged apply, and atomic state/cursor advancement.
- `src/sync/apply.rs`: stage selected `RemoteFile` bytes by path and reuse existing safe atomic local apply.
- `src/sync/merge.rs`: retain only the legacy-tip resolution path needed during migration, then remove commit graph code after migration coverage exists.
- `src/sync/commit_cache.rs`: remove the normal-sync commit cache once no production path reads it.
- `src/sync/coordinator.rs`, `src/sync/live.rs`, `src/sync/live/worker.rs`, `src/lib.rs`, `src/environment/app_picker.rs`: stop live cloud pending uploads; preserve picker file-change observation, progress, guest locks, and final sync.
- `android/app/src/main/java/org/touchhle/android/CloudSyncWorker.java`, `CloudSyncScheduler.java`, `NativeSyncBridge.java`, `MainActivity.java`, Android tests: keep durable reconciliation deferred while a guest lock is held and retry after guest exit.
- `dev-docs/google-drive-sync.md`, `README.md`: document the new layout, first migration, conflicts, deletions, and performance behavior.

The provider boundary introduced in Task 1 uses these model shapes (field names may be adjusted to existing Rust conventions, but semantics stay fixed):

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteFile {
    pub id: String,
    pub path: RelativePath,
    pub version: String,
    pub sha256: Option<[u8; 32]>,
    pub size: u64,
    pub modified_unix_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteChange {
    Upsert(RemoteFile),
    Removed { file_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteChangeBatch {
    pub changes: Vec<RemoteChange>,
    pub new_start_page_token: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileBaseline {
    pub sha256: [u8; 32],
    pub size: u64,
    pub modified_unix_ms: i64,
    pub drive_file_id: String,
    pub drive_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteVersionId {
    DriveFile(String),
    LegacyCommit(uuid::Uuid),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteCandidate {
    pub id: RemoteVersionId,
    pub entry: Option<SnapshotEntry>,
    pub file: Option<RemoteFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictChoice {
    pub path: RelativePath,
    pub selected: LocalOrRemote,
    pub remote_version_id: Option<RemoteVersionId>,
}
```

Normal file synchronization has one current remote candidate per path.
Migration may expose multiple legacy commit candidates until the user resolves
them. The conflict UI keeps displaying the selected candidate's size and
modification time; choosing a side discards the other candidates after
successful migration/resolution.

The new `RemoteStore` operations are:

```rust
fn start_page_token(&mut self) -> Result<String, SyncError>;
fn list_files(&mut self) -> Result<Vec<RemoteFile>, SyncError>;
fn changes_since(&mut self, token: &str) -> Result<RemoteChangeBatch, SyncError>;
fn file_metadata(&mut self, file_id: &str) -> Result<Option<RemoteFile>, SyncError>;
fn read_file(&mut self, file_id: &str) -> Result<Option<Vec<u8>>, SyncError>;
fn write_file(
    &mut self,
    path: &RelativePath,
    existing_id: Option<&str>,
    bytes: &[u8],
) -> Result<RemoteFile, SyncError>;
fn delete_file(&mut self, file_id: &str) -> Result<(), SyncError>;
```

During migration, add `read_legacy_commits() -> Result<Option<Vec<Commit>>, SyncError>` and `read_legacy_object(hash: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError>`, then remove them only after migration tests pass and no normal sync depends on them. `changes_since` consumes every API page and returns only the final `newStartPageToken`; callers never persist an intermediate page token.

---

### Task 1: Add The Path-Based Reconciliation Model

**Files:**
- Create: `src/sync/reconcile.rs`
- Modify: `src/sync.rs`
- Modify: `src/sync/model.rs`
- Test: `src/sync/reconcile.rs`
- Test: `src/sync/model.rs`

**Interfaces:**
- Consumes: `RelativePath`, `SnapshotEntry`, `SyncError` from `model.rs`.
- Produces: `RemoteFile`, `RemoteChange`, `RemoteChangeBatch`, `FileBaseline`, versioned `SyncState`, generic `RemoteVersionId`/`RemoteCandidate`/`ConflictChoice`, and a path-based `SyncPlan`.
- Keep commit-specific `Commit` and old `SyncState` decoding available until Task 4 completes migration.

- [ ] **Step 1: Write planner tests for all one-path three-way cases.**

Add table-driven tests in `reconcile.rs` covering unchanged, local-only update, remote-only update, identical concurrent update, divergent update, local create, remote create, both delete, local delete vs remote update, and local update vs remote delete. A divergent path must produce no automatic apply/publish action and exactly one conflict. Add a resolver test where two `LegacyCommit` candidates remain selectable, while a normal file path has exactly one `DriveFile` candidate. Declare `pub mod reconcile;` in `sync.rs` so these tests compile as an intentionally failing test target.

```rust
#[test]
fn divergent_edits_require_one_local_or_remote_choice() {
    let baseline = entry(b"base");
    let local = entry(b"local");
    let remote = RemoteCandidate {
        id: RemoteVersionId::DriveFile("file-id".into()),
        entry: Some(entry(b"remote")),
        file: Some(remote_file(b"remote", "file-id", "9")),
    };
    let action = plan_path(Some(&baseline), Some(&local), Some(&remote)).unwrap();

    assert!(matches!(action, PathAction::Conflict(_)));
}
```

- [ ] **Step 2: Run only the new reconciliation tests and confirm they fail because the planner API is absent.**

Run: `cargo test --lib sync::reconcile::tests`
Expected: compilation fails because `plan_path` and its planner types do not exist yet.

- [ ] **Step 3: Implement the minimal pure planner and validate selected trees.**

Represent absence as `None`; do not persist a tombstone for every deleted path. Keep remote identity/version on the `RemoteFile` candidate and retain the baseline's file ID so removed Changes entries can map back to paths.

```rust
pub struct FileConflict {
    pub path: RelativePath,
    pub local: Option<SnapshotEntry>,
    pub remote_candidates: Vec<RemoteCandidate>,
}

pub enum PathAction {
    Unchanged,
    PublishLocal(Option<SnapshotEntry>),
    ApplyRemote(RemoteCandidate),
    Conflict(FileConflict),
}

pub struct SyncPlan {
    pub apply_remote: BTreeMap<RelativePath, RemoteCandidate>,
    pub publish_local: BTreeMap<RelativePath, Option<SnapshotEntry>>,
    pub conflicts: Vec<FileConflict>,
}

pub enum RemoteVersionId {
    DriveFile(String),
    LegacyCommit(uuid::Uuid),
}

pub struct RemoteCandidate {
    pub id: RemoteVersionId,
    pub entry: Option<SnapshotEntry>,
    pub file: Option<RemoteFile>,
}

pub struct ConflictChoice {
    pub path: RelativePath,
    pub selected: LocalOrRemote,
    pub remote_version_id: Option<RemoteVersionId>,
}

pub fn plan_path(
    baseline: Option<&SnapshotEntry>,
    local: Option<&SnapshotEntry>,
    remote: Option<&RemoteCandidate>,
) -> Result<PathAction, SyncError>;

pub fn plan_sync(
    baseline: &BTreeMap<RelativePath, FileBaseline>,
    local: &BTreeMap<RelativePath, SnapshotEntry>,
    remote: &BTreeMap<RelativePath, RemoteFile>,
) -> Result<SyncPlan, SyncError>;
```

Compare byte identity by SHA-256, not size or timestamps. Validate case-insensitive path collisions and file/directory ancestor collisions using the same invariants as the current snapshot decoder.

- [ ] **Step 4: Add versioned current checkpoint types without discarding the old JSON shape.**

Add schema version, optional Changes cursor, current file baseline map, and the ID-to-path lookup needed to interpret removed-file events. Add a loader result that distinguishes `Current(SyncState)`, `Legacy(LegacySyncState)`, and missing state instead of deserializing an old state as empty. `LegacySyncState` retains the exact old `device_id`, `last_applied_commit`, and snapshot `baseline` fields until migration completes.

```rust
pub enum LoadedSyncState {
    Current(SyncState),
    Legacy(LegacySyncState),
    Missing,
}
```

- [ ] **Step 5: Test current and legacy checkpoint round trips and malformed-state rejection.**

Preserve current atomic/no-follow state-file protections. Tests must show an old state with `last_applied_commit` and `baseline` loads as `Legacy`, a new state preserves its token and remote IDs, and corrupt JSON returns an integrity error.

Run: `cargo test --lib sync::model`
Expected: PASS, including existing path validation tests.

- [ ] **Step 6: Run the focused planner/model test set and inspect only Task 1 changes.**

Run: `cargo test --lib sync::reconcile`
Run: `cargo test --lib sync::model`
Expected: PASS. Confirm `git diff --check` passes and no existing legacy-state tests were removed.

### Task 2: Implement Drive Changes And Current-File Operations

**Files:**
- Modify: `src/sync/store.rs`
- Modify: `src/sync/gdrive.rs`
- Test: `src/sync/store.rs`
- Test: `src/sync/gdrive.rs`

**Interfaces:**
- Consumes: Task 1 `RemoteFile`, `RemoteChangeBatch`, and `RelativePath`.
- Produces: the new `RemoteStore` methods; `MemoryRemoteStore` models file IDs, versions, changes, deletion, pagination, and injected failures.
- Retain legacy commit-read methods temporarily; normal current-file methods must never call them.

- [ ] **Step 1: Add deterministic memory-store tests for file writes and Changes cursors.**

Test creating a path, updating it in place with an incremented version, reading by file ID, deleting it, and returning a removal event. Add a request/change counter so a no-change cursor returns an empty batch without reading file bytes.

- [ ] **Step 2: Implement the new `RemoteStore` protocol in `store.rs`.**

Keep current old methods temporarily so the app compiles while engine migration is incremental. Model Changes as an indexed read-only feed: the same input token returns the same events after a failed sync, and the engine's checkpoint advances only after successful reconciliation.

```rust
fn changes_since(&mut self, token: &str)
    -> Result<RemoteChangeBatch, SyncError>;
```

- [ ] **Step 3: Extend Drive response types and add Changes API pagination.**

In `gdrive.rs`, request only the metadata needed for path reconciliation (`id`, `name`, `mimeType`, `parents`, `version`, `sha256Checksum`, `size`, `modifiedTime`, `trashed`). Implement `getStartPageToken`, `changes.list`, removed IDs, page traversal, and final `newStartPageToken`. Coalesce repeated events for one file to its final state.

- [ ] **Step 4: Add an initial recursive inventory under `touchHLE/files`.**

Resolve/create `touchHLE/files`, `touchHLE_apps`, and `touchHLE_sandbox`. Traverse child folders by parent ID, reject duplicate/case-colliding managed paths, and return only regular managed files. Cache folder IDs within the store as the current adapter already does.

- [ ] **Step 5: Implement direct file read, update/create, and remote deletion.**

Reuse the current multipart/resumable upload and shared HTTP client. Update an existing Drive ID when known; create the file and parent folders when absent. Use Drive trash semantics for deletion so Changes reports the removal; treat already removed IDs idempotently.

- [ ] **Step 6: Verify changed-file uploads and stale-version preflight at the adapter boundary.**

After a write, fetch the returned file metadata and verify expected SHA-256 when Drive returns it; otherwise read back and hash only that changed file. Provide `file_metadata(file_id)` for the engine's preflight check; do not hide preflight version comparison in a batch-wide store call.

- [ ] **Step 7: Add HTTP-boundary tests for token, pagination, path mapping, and transfer counts.**

Tests must prove Changes pages use the returned next-page token, only the final new-start token is exposed, removed IDs survive parsing, media downloads occur only when explicitly requested, and writes preserve an existing file ID.

Run: `cargo test --lib sync::store`
Run: `cargo test --lib sync::gdrive`
Expected: PASS; failures do not advance the in-memory cursor.

### Task 3: Switch The Sync Engine And Resolver To Current Files

**Files:**
- Modify: `src/sync/engine.rs`
- Modify: `src/sync/reconcile.rs`
- Modify: `src/sync/apply.rs`
- Modify: `src/sync/merge.rs`
- Modify: `src/environment/app_picker.rs`
- Test: `src/sync/engine.rs`
- Test: `src/sync/apply.rs`
- Test: `src/sync/reconcile.rs`

**Interfaces:**
- Consumes: Task 1 planner/checkpoint and Task 2 `RemoteStore`.
- Produces: `SyncEngine::synchronize_with_progress` and `resolve_conflicts` using file IDs/versions and the local Changes cursor; normal resolver choices select `Local` or the current `DriveFile` candidate, while the migration-only resolver can select a `LegacyCommit` candidate.
- Preserve startup/shutdown public coordinator outcomes so UI and Android JNI do not need a simultaneous API redesign.

- [ ] **Step 1: Add engine tests for no-change and one-sided changes before replacing the old flow.**

Seed the memory store and current checkpoint. Assert a no-change sync reads the Changes feed but zero remote file bytes and performs no upload; local-only change uploads that path only; remote-only change downloads/applies that path only.

- [ ] **Step 2: Replace commit-index loading in normal sync with cursor-based change loading.**

Load local files and current checkpoint, fetch `changes_since(cursor)`, map removals through the saved file-ID index, and reconcile per path. A legacy checkpoint must return a migration-required result without being interpreted as an empty state; Task 4 handles that result. Do not call `list_commits`, `resolve_remote_tips`, or commit ancestry helpers from the normal path.

- [ ] **Step 3: Implement initial inventory and catch-up from the pre-inventory token.**

When state is missing, get the start token first, list the current `files/` tree, then read changes from that token. Reconcile the combined latest remote state and only then persist the final feed cursor.

- [ ] **Step 4: Stage selected remote bytes by file ID and relative path.**

Adapt `apply.rs` so the engine downloads each required file into a private staging file, verifies SHA-256 and expected size, then reuses atomic local apply. A selected remote deletion stages a tombstone; a selected local deletion calls `delete_file`. Hold displaced local bytes only in the apply transaction directory until the complete batch and checkpoint succeed; on failure, roll back already-applied paths. Do not leave the losing conflict version in the permanent `.touchHLE_sync/recovery` directory.

- [ ] **Step 5: Recheck remote versions before writes and verify after writes.**

Immediately before publishing a path, compare current Drive metadata against the version used to build the plan. On mismatch, return to planning and surface a conflict instead of overwriting from a stale plan. After upload, verify the returned ID/version/checksum before advancing state. Keep the spec's explicit non-atomic race limitation.

- [ ] **Step 6: Update conflict UI data without changing user choice semantics.**

Change `ConflictChoice` to the typed `RemoteVersionId` from Task 1. Preserve path, size, and modification-time display in `app_picker.rs`; revalidate the displayed plan before applying any choice. Normal sync exposes one current Drive candidate; migration can temporarily expose divergent legacy commit candidates. Selecting one side must not save the other side in recovery or cloud sync state after successful resolution.

- [ ] **Step 7: Commit state and Changes cursor together only after successful apply/publication.**

Use the existing atomic state-file replacement helper with the new state shape. Record transaction phase and staged/displaced paths in an atomic journal. On cancellation, provider failure, verification failure, or partial apply, restore displaced local bytes and keep the old checkpoint/cursor so retry reprocesses the same Changes batch. After the checkpoint is durably replaced, remove the journal and temporary losing-version bytes.

- [ ] **Step 8: Replace old graph tests with path-sync behavior tests and run focused suites.**

Retain only tests needed by legacy migration; move normal sync tests to file IDs, versions, Changes pages, and conflict selections.

Run: `cargo test --lib sync::reconcile`
Run: `cargo test --lib sync::apply`
Run: `cargo test --lib sync::engine`
Expected: PASS, including interrupted transfer, stale displayed plan, create/delete, and retry-without-cursor-advance cases.

### Task 4: Migrate Legacy Commits Once And Retire Normal History Reads

**Files:**
- Modify: `src/sync/engine.rs`
- Modify: `src/sync/gdrive.rs`
- Modify: `src/sync/store.rs`
- Modify: `src/sync/merge.rs`
- Delete or retire: `src/sync/commit_cache.rs`
- Modify: `src/sync/coordinator.rs`
- Test: `src/sync/engine.rs`
- Test: `src/sync/gdrive.rs`

**Interfaces:**
- Consumes: `LoadedSyncState::Legacy`, the legacy commit/object read methods from Task 2, and the migration-compatible conflict candidates from Task 1.
- Produces: an idempotent migration journal and a verified `files/` tree; after successful migration, ordinary sync has no legacy store calls.

- [ ] **Step 1: Add migration tests for one tip and multiple divergent tips.**

For one tip, verify every referenced object and materialize its snapshot into `files/`. For divergent tips, expose `LegacyCommit` candidates through the same resolver and collect one choice per differing path before publishing any selected snapshot. Missing or corrupt objects must stop migration without changing old state.

- [ ] **Step 2: Add a durable migration journal before writing new remote files.**

Persist migration phase, old local state bytes or typed legacy baseline, selected target snapshot, and already verified new Drive file IDs. Write the journal atomically under `.touchHLE_sync`; resume safely after every injected interruption point. Preserve temporary displaced local bytes until the migration checkpoint is durable, then remove losing candidates and the journal.

- [ ] **Step 3: Reconcile local files with the selected legacy tip.**

Use the legacy baseline and ordinary three-way path rules. Do not silently treat a missing commit or empty legacy folder as an empty cloud if a saved legacy baseline names a commit.

- [ ] **Step 4: Publish and verify `files/` before atomically replacing local state.**

Materialize all selected files, confirm remote IDs/checksums, then write the versioned current checkpoint and mark migration complete. A partial `files/` subtree must resume idempotently and must never make a subsequent run skip migration. If the checkpoint already shows migration complete but the journal remains, startup only cleans up the journal and temporary losing-version bytes; it must not replay the old snapshot.

- [ ] **Step 5: Preserve the old remote history and remove only obsolete local cache behavior.**

Leave Drive `commits/` and `objects/` unchanged. After migration tests pass, remove commit enumeration from regular sync, remove the commit cache from `GoogleDriveStore` construction in `coordinator.rs`, and delete `commit_cache.rs` if no code references remain. Do not automatically delete the local baseline until the new checkpoint has been saved and reread successfully.

- [ ] **Step 6: Remove old ancestry validation from normal code while retaining only migration helpers.**

Keep `resolve_remote_tips` only if migration requires it; move it behind a migration-specific module/function. Delete normal-traffic ancestry validation and tests only after migration coverage passes.

- [ ] **Step 7: Test interrupted migration and prove legacy storage is unchanged.**

Inject failures after inventory, first upload, upload verification, state replacement, and journal cleanup. Retry each case and assert the final file tree is correct, old state was not lost early, and commit/object delete counts stay zero.

Run: `cargo test --lib legacy_migration`
Run: `cargo test --lib sync::gdrive`
Expected: PASS; no legacy Drive deletion request is issued.

### Task 5: Remove Remote Pending Uploads And Preserve Lifecycle Safety

**Files:**
- Modify: `src/sync/store.rs`
- Modify: `src/sync/gdrive.rs`
- Modify: `src/sync/live.rs`
- Modify: `src/sync/live/worker.rs`
- Modify: `src/sync/coordinator.rs`
- Modify: `src/lib.rs`
- Modify: `src/environment/app_picker.rs`
- Modify: `android/app/src/main/java/org/touchhle/android/CloudSyncWorker.java`
- Modify: `android/app/src/main/java/org/touchhle/android/CloudSyncScheduler.java`
- Modify: `android/app/src/main/java/org/touchhle/android/NativeSyncBridge.java`
- Test: `src/sync/coordinator.rs`
- Test: `src/sync/live/worker.rs`
- Test: `android/app/src/test/`

**Interfaces:**
- Consumes: Task 3 current-file sync and Task 4 migration.
- Produces: local-only picker/file observation and scheduled full reconciliation only when the guest lock is free; no `write_pending` or `clear_pending` API.

- [ ] **Step 1: Add regression tests proving a running guest causes zero remote file mutations.**

Hold the guest-session lock, modify a managed file, run background reconciliation, and assert `GuestActive`, no file upload/download/delete, and no baseline/cursor advancement.

- [ ] **Step 2: Separate picker filesystem observation from cloud uploading.**

Keep the event stream used by `app_picker.rs` to refresh the app grid. Remove `RemoteStore` from the watcher worker; it may report local changed paths/status but must not write any remote data during the guest session.

- [ ] **Step 3: Remove remote pending methods and their per-device directory implementation.**

Delete `write_pending`, `clear_pending`, pending recursion, and pending cleanup fields from `RemoteStore`, `GoogleDriveStore`, memory stores, and coordinator. Remove `DrainingLiveUploads` progress only if no UI caller needs it; otherwise map it to a local rescan phase without implying network staging.

- [ ] **Step 4: Keep guest and sync-operation lock ordering unchanged.**

Full startup, post-picker, shutdown, and worker reconciliation must acquire the operation lock and guest lock before local apply or publication. A worker that cannot acquire the guest lock records deferred status and returns without Drive mutations.

- [ ] **Step 5: Preserve Android durable retry after guest exit.**

Keep WorkManager's network constraint and retry behavior. Ensure the scheduled worker runs a complete current-file reconciliation only after the native bridge confirms the guest lock is free; process death leaves local files and old cursor for the next eligible run.

- [ ] **Step 6: Update lifecycle tests and picker status wording.**

Assert observer events still refresh the picker, background sync defers while the guest is active, post-exit sync publishes local files directly, and no UI text claims files were uploaded to a private pending area.

Run: `cargo test --lib sync::live`
Run: `cargo test --lib sync::coordinator`
Expected: PASS; static search finds no production `write_pending`, `clear_pending`, or `touchHLE/pending` path.

### Task 6: Update User Documentation And Run Full Verification

**Files:**
- Modify: `dev-docs/google-drive-sync.md`
- Modify: `README.md` only if its short feature summary mentions commits/pending storage
- Test: Rust library, desktop binary, Android Gradle project

**Interfaces:**
- Consumes: completed Tasks 1-5.
- Produces: user-facing description of direct file mirroring, migration, offline conflicts, deletions, and warm-sync behavior.

- [ ] **Step 1: Replace commit/pending documentation with the new Drive layout and migration behavior.**

Document `touchHLE/files`, local per-install baseline/cursor, first-run migration from `commits/objects`, resolver choices, and that legacy data is left untouched. Remove instructions that promise permanent conflict history, permanent local recovery copies for the losing choice, or private pending uploads.

- [ ] **Step 2: Add exact performance and concurrency limits to documentation.**

State that a warm no-change run transfers no file content and does not enumerate commits. Explain the best-effort concurrent same-path write detection and do not claim atomic cross-device writes.

- [ ] **Step 3: Search active sources and docs for obsolete normal-sync behavior.**

Run: `rg -n 'list_commits|write_commit|write_pending|clear_pending|commit-cache|touchHLE/pending|immutable commits' src/sync src/environment dev-docs/google-drive-sync.md`
Expected: remaining commit references are migration-only/tests; no production pending-write path remains.

- [ ] **Step 4: Run all Rust library tests and formatting checks.**

Run: `cargo fmt --all -- --check`
Run: `cargo test --lib`
Expected: PASS without unrelated formatting changes.

- [ ] **Step 5: Build the desktop binary.**

Run: `CMAKE_POLICY_VERSION_MINIMUM=3.5 cargo check --bin touchHLE`
Expected: PASS. Then build with the repository's `dev-scripts/build-desktop.sh` when a distributable executable is needed.

- [ ] **Step 6: Build and test Android integration.**

Run: `gradle build` from `android/` with the configured Android SDK/NDK.
Expected: Rust JNI library and Java/Gradle tests compile successfully.

- [ ] **Step 7: Verify end-to-end against a test Drive folder.**

On a clean test account/folder, measure first migration, unchanged startup, one changed local file, one changed remote file, offline divergent edits, delete-vs-modify, and interrupted retry. Record request counts, media bytes, and elapsed time; confirm the warm no-change case performs zero content transfer and no commit/object listing.
