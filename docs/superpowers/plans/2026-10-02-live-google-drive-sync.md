# Live Google Drive Backup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep the picker alive after opening Finder and back up stable changes under both managed roots to private Google Drive staging while touchHLE runs, publishing a usable snapshot only after guest exit.

**Architecture:** A filesystem observer feeds one background upload worker; only that worker writes device-scoped `pending/` files when sync is enabled. The existing coordinator retains exclusive control of full sync, stops and joins the worker before full sync, and clears pending files only after an acknowledged success. The picker consumes observer notifications separately for grid refresh, updates the worker's enable state when settings change, and reads a persisted status file; neither file hashing nor provider calls run on its UI loop.

**Tech Stack:** Rust 2021, `notify` 8, `std::sync::mpsc`, existing OpenDAL Google Drive adapter, cap-std path safety, serde JSON, existing UIKit-like picker, `MemoryRemoteStore` for deterministic tests.

**Spec:** `docs/superpowers/specs/2026-10-02-live-google-drive-sync-design.md`

## Global Constraints

- Observe only `touchHLE_apps` and `touchHLE_sandbox`, including nested changes and root creation/replacement.
- One-second initial quiet interval; reject unstable reads and requeue them.
- Never call full `synchronize()` while a guest is running; pending uploads must not write objects or commits or apply local files.
- Pending keys must be scoped by persisted `SyncState.device_id` and validated `RelativePath`; no token, secret, or file content in logs.
- Keep pending data on offline, disabled, provider failure, conflict, or cancellation; only a completed full sync/resolution permits cleanup.
- Preserve normal headless behavior; show watcher unavailability honestly on Android and desktop.
- Do not read or print the OAuth secret in `.zshrc`; do not stage `.touchHLE_sync/` or any unrelated worktree edits.
- Use `dev-scripts/build-desktop.sh` for the final desktop build; run Android build only when its NDK is available.

## File Map

- `src/sync/store.rs`, `src/sync/gdrive.rs`: pending remote API, fake-store failure injection, real Drive key/write/delete operations.
- `src/sync/engine.rs`: expose persisted device identity without changing the baseline; keep full sync intact.
- `src/sync/live.rs` (new), `src/sync/live/worker.rs` (new), `src/sync.rs`, `Cargo.toml`, `Cargo.lock`: observer/reconciliation, event coalescing, stable reads, one uploader worker and status events.
- `src/sync/coordinator.rs`: own worker lifecycle, startup/shutdown ordering, pending cleanup after resolution, durable status.
- `src/environment/app_picker.rs`: stop exiting after opening the file manager, refresh app grid including empty-to-populated transitions, show status.
- `src/lib.rs`: start/stop observer/worker at picker/guest boundaries; pass a control handle to the picker.
- `dev-docs/google-drive-sync.md`: explain pending uploads vs committed snapshots and failure recovery.

---

### Task 1: Device-Scoped Pending Storage

**Files:** Modify `src/sync/store.rs`, `src/sync/gdrive.rs`, `src/sync/engine.rs` (test-only `Offline` implementation); test all three files.

**Interfaces:**
- `RemoteStore::write_pending(&mut self, device_id: Uuid, path: &RelativePath, bytes: &[u8]) -> Result<(), SyncError>`.
- `RemoteStore::clear_pending(&mut self, device_id: Uuid) -> Result<(), SyncError>`.
- `pending_key(device_id: Uuid, path: &RelativePath) -> String`: `pending/{device_id}/{path}`. Do not accept unchecked strings.
- Memory fake gains `StoreOperation::WritePending` and `StoreOperation::ClearPending`, `pending_bytes(device_id, &RelativePath) -> Option<&[u8]>`, and `pending_count(device_id) -> usize`.

- [ ] **Step 1: Add failing fake-store tests** for two devices, overwrite of the same path, failure injection, no new commit/object, and device-scoped cleanup:

```rust
let path = RelativePath::new("touchHLE_sandbox/save/data").unwrap();
let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
store.write_pending(a, &path, b"old").unwrap();
store.write_pending(a, &path, b"new").unwrap();
store.write_pending(b, &path, b"other").unwrap();
assert_eq!(store.pending_bytes(a, &path), Some(&b"new"[..]));
assert_eq!(store.object_count(), 0);
assert!(store.list_commits().unwrap().is_empty());
store.fail_next(StoreOperation::ClearPending);
assert!(store.clear_pending(a).is_err());
assert_eq!(store.pending_count(a), 1);
store.clear_pending(a).unwrap();
assert_eq!(store.pending_bytes(b, &path), Some(&b"other"[..]));
```

- [ ] **Step 2: Run** `cargo test --lib sync::store::tests`; expect compile failure for the new trait methods.
- [ ] **Step 3: Implement** the two trait methods in the fake using `BTreeMap<(Uuid, RelativePath), Vec<u8>>`, checking `StoreOperation` before mutation. Add explicit unreachable/error implementations to the test-only `Offline` store in `engine.rs` so every `RemoteStore` implementation satisfies the extended trait. In the Drive adapter construct keys only from `RelativePath`; `operator.create_dir(&format!("pending/{device_id}/"))` and `operator.write(&pending_key(...), bytes.to_vec())`. For cleanup recursively `operator.list("pending/{device_id}/")`, delete only files beneath that exact UUID prefix, then delete subdirectories bottom-up; treat a missing subtree as success. Delete only this UUID's subtree, never the shared `pending/` parent. Do not list pending files in `list_commits`.
- [ ] **Step 4: Test** `pending_key` exact output for each managed root and confirm Drive methods map failures with `provider_error` without including byte contents. Run `cargo test --lib sync::store` and `cargo test --lib sync::gdrive` separately.
- [ ] **Step 5: Commit only this task's hunks** from `src/sync/store.rs`, `src/sync/gdrive.rs`, and `src/sync/engine.rs` with `git commit -m "feat: add device-scoped pending Drive storage"`; inspect `git diff --cached` before committing because files contain existing uncommitted work.

### Task 2: Stable Device ID and Local Status

**Files:** Modify `src/sync/engine.rs`, `src/sync/coordinator.rs`; create `src/sync/status.rs`; modify `src/sync.rs`; test in `src/sync/coordinator.rs`.

**Interfaces:**
- `SyncEngine::persisted_device_id(&mut self) -> Result<Uuid, SyncError>`: acquire an OS advisory lock on a no-follow `.touchHLE_sync/state.lock` file, recheck state while locked, atomically save `fresh_state()` only if still absent, then return the persisted winner's device ID. Do not require hard-link support; release the lock on every return path.
- `LiveStatus { active: bool, queued: usize, last_upload_unix_ms: Option<i64>, last_full_sync_unix_ms: Option<i64>, error: Option<String> }`, serde-serializable at `.touchHLE_sync/live-status.json`.
- `load_live_status(root: &Path) -> Result<LiveStatus, SyncError>` and `save_live_status(root: &Path, status: &LiveStatus) -> Result<(), SyncError>` using the existing settings-style atomic replace in `.touchHLE_sync/` (do not follow a symlinked status file).

- [ ] **Step 1: Add failing tests** for stable ID across repeated calls, unchanged `last_applied_commit`/baseline, restart after failed sync, a deterministic two-engine first-ID race where a paused late initializer adopts the winner after its baseline advances, status round trip and missing-status defaults:

```rust
let first = engine.persisted_device_id().unwrap();
assert_eq!(engine.persisted_device_id().unwrap(), first);
let saved: SyncState = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
assert_eq!(saved.last_applied_commit, None);
assert!(saved.baseline.is_empty());
let status = LiveStatus { active: true, queued: 2, error: Some("offline".into()), ..Default::default() };
save_live_status(&root, &status).unwrap();
assert_eq!(load_live_status(&root).unwrap(), status);
```

- [ ] **Step 2: Run** `cargo test --lib sync::coordinator`; expect missing API compile errors.
- [ ] **Step 3: Implement** persisted ID with a no-follow lock file opened beneath the validated `.touchHLE_sync` directory and `std::fs::File::lock()`. Hold the lock while checking for `state.json` and, only if still absent, call the existing atomic `save_state`; reread/adopt the persisted winner after a competing initializer. Do not use hard links. Make startup sync preserve an already saved fresh ID. Write status only outside the two synchronized roots. Sanitize error text before storing: use `redacted_error` and never persist HTTP bodies.
- [ ] **Step 4: Run** `cargo test --lib sync::coordinator`, `cargo test --lib sync::engine`, and `cargo test --lib sync::status`; expect pass, including the deterministic first-ID race and successful-sync/status-write-failure tests.
- [ ] **Step 5: Commit this task's file hunks** with a patch-only staged diff if existing uncommitted changes overlap.

### Task 3: Recursive Observer and Coalesced Dirty Set

**Files:** Modify `Cargo.toml`, `Cargo.lock`, `src/sync.rs`; create `src/sync/live.rs`; test in `src/sync/live.rs`.

**Interfaces:**
- `enum ManagedRoot { Apps, Sandbox }`; `enum RootMutationKind { Created, Removed }`; `enum RescanScope { Apps, Sandbox, Both }`; `enum LiveEvent { Changed(PathBuf), RootChanged { root: ManagedRoot, kind: RootMutationKind }, Rescan(RescanScope), Reconcile, Failed(String) }`. `RootChanged` is emitted only for an OS event whose path is exactly the managed root. `Reconcile` is periodic worker-only and must not refresh the picker unless it discovers an app change.
- `enum EventTarget { Path(RelativePath), Rescan(RescanScope) }`; `fn classify_event(root: &Path, path: &Path) -> Option<EventTarget>`: return `None` for outside paths, `Path` for a file, and a root-scoped `Rescan` for a managed root or directory.
- `DirtySet::record(event: LiveEvent, now: Instant)`, `DirtySet::take_due(now: Instant) -> Vec<RelativePath>`; one-second initial quiet interval; preserve the earliest pending rescan deadline so repeated events cannot postpone reconciliation. Scoped rescans scan only the affected root(s); periodic `Reconcile` scans both.
- `DirtySet::current_signature(path)` performs safe targeted no-follow traversal of only that validated path; it must not call a whole-roots scan for every file.
- `affects_apps(root, event)` is true only for an app-path change, `RootChanged { root: Apps, .. }`, `Rescan(Apps|Both)`, or an observer failure whose scope is unknown; `Rescan(Sandbox)` and an uneventful periodic `Reconcile` do not refresh the picker.
- `LiveObserver::start(root: &Path, sender: Sender<LiveEvent>) -> Result<Self, SyncError>` uses `notify::recommended_watcher`; watch the parent non-recursively to catch missing/replaced roots and each existing managed root recursively; re-arm watches on root replacement; implement a bounded periodic reconciliation scan to recover from missed events and backend failures.

- [ ] **Step 1: Add deterministic tests** by injecting `LiveEvent`s and `Instant` rather than sleeping. Verify duplicate writes produce one due path after one second, repeated rescans do not move the earliest deadline, nested rename dirties old and new paths, directory removals and root creation request correctly scoped rescans, an unrelated parent event is ignored, sandbox/rescan-reconcile do not falsely refresh apps, and error/overflow schedules a scoped or full rescan as appropriate. Verify targeted signature lookup does not enumerate unrelated roots.

```rust
let mut dirty = DirtySet::default();
let start = Instant::now();
dirty.record(LiveEvent::Changed(root.join("touchHLE_apps/a.ipa")), start);
dirty.record(LiveEvent::Changed(root.join("touchHLE_apps/a.ipa")), start);
assert!(dirty.take_due(start + Duration::from_millis(999)).is_empty());
assert_eq!(dirty.take_due(start + Duration::from_secs(1)).len(), 1);
```

- [ ] **Step 2: Add `notify = "8"` and run** `cargo test --lib sync::live`; expect failure until the module exists.
- [ ] **Step 3: Implement** the observer and coalescer. `notify::Event.paths` may hold both sides of a rename; for an unclassifiable/empty/error event mark both roots for rescan, never silently drop it. Reconciliation compares a per-path signature (size, mtime, file identity where available) against the last successfully staged signature; it must include deletions and nested roots. Watcher callbacks must only enqueue, never call the provider or load app bundles. Preserve the first due time while a rescan is pending. Distinguish `Reconcile` (worker scan only) from scoped watcher rescans so periodic polling does not rebuild the picker without an app change.
- [ ] **Step 4: Test real filesystem notifications** in temp roots: create missing root, add nested file, rename, remove, and replace root; use bounded receive timeouts. For creation and removal of an empty root, drain queued events before mutation, require exact `RootChanged { root: Apps, kind: Created|Removed }`, and reconcile against a pre-mutation signature baseline. Separately populate the root with a staged file, rename the populated root outside the managed path (preserving its contents), wait for the independent `Reconcile` event rather than accepting a generic queued rescan, and assert `DirtySet` reports the staged file as deleted. Direct filesystem assertions alone never pass these checks. Run `cargo test --lib sync::live`; expect pass on desktop; add cfg-gated Android compile coverage in Task 7.
- [ ] **Step 5: Commit only new watcher/dependency changes**, reviewing the staged diff first.

### Task 4: Stable Upload Worker and Retry

**Files:** Create `src/sync/live/worker.rs`; modify `src/sync/live.rs` only to export the worker API, `src/sync/store.rs` tests, and `src/sync/coordinator.rs`; test in `src/sync/live/worker.rs`.

**Interfaces:**
- `LiveUploader<S: RemoteStore>::start(store: S, root: PathBuf, device_id: Uuid, enabled: bool, status_sender: Sender<LiveStatus>) -> Result<(Self, LiveControl), SyncError>`. `LiveControl` owns an `mpsc::Sender<LiveCommand>` and `mpsc::Receiver<LiveEvent>`, with `set_enabled(&self, enabled: bool)` and `try_picker_event(&self) -> Option<LiveEvent>`; the observer stays active even when uploads are disabled.
- `LiveUploader::stop_and_drain(self) -> Result<(), SyncError>` stops observation, joins the one upload thread, and returns failure without discarding queued dirty work.
- `read_stable(root: &Path, path: &RelativePath) -> Result<Option<Vec<u8>>, SyncError>`: capability-based no-follow open, metadata before/after read (size, mtime and identity where possible); missing/non-regular/symlink => `None`, changed during read => error/requeue.
- Worker uses a per-path last-success signature and retries after bounded exponential delays (e.g. 1, 2, 4, 8, 16 seconds, capped at 30 seconds), then retries once again during drain. Unchanged paths are not reuploaded. Deleted paths have no pending write; final sync publishes tombstones.

- [ ] **Step 1: Add failing fake-store tests** asserting a live change writes pending bytes only, two writes overwrite that key, second-device `list_commits` remains empty, symlink paths upload nothing, and a mid-read mutation is requeued. Inject a read hook in `read_stable` for the mutation test rather than relying on a timed writer.

```rust
store.write_pending(device, &RelativePath::new("touchHLE_apps/save").unwrap(), b"latest").unwrap();
assert_eq!(store.pending_count(device), 1);
assert_eq!(store.object_count(), 0);
assert!(store.list_commits().unwrap().is_empty());
```

- [ ] **Step 2: Run** `cargo test --lib sync::live`; expect failures for the worker and stable-read APIs.
- [ ] **Step 3: Implement** serialized upload processing in `live/worker.rs` on one background thread; keep observer/event types in `live.rs` and do not grow that file with worker implementation. Never hold a mutex across picker/UI work. Recheck file stability after reading; if it changes during provider write, keep the path dirty so a subsequent upload replaces the old pending bytes. Persist status transitions (startup, queued, uploading, success/error) through `save_live_status`. If watcher startup fails, surface `active: false` and use the periodic reconciliation path, never report success for `ConfigInvalid`.
- [ ] **Step 4: Add failure/retry tests** with `fail_next(StoreOperation::WritePending)` and short injected retry intervals; assert pending count is zero after failure, queue/error remains visible, then a retry uploads exactly the latest bytes. Assert disabled mode keeps observing for picker refresh but performs no remote operations, and that enabling it stages subsequent edits.
- [ ] **Step 5: Run** `cargo test --lib sync::live`; commit the tested worker.

### Task 5: Full Sync Boundary and Cleanup

**Files:** Modify `src/sync/coordinator.rs`, `src/lib.rs`; test in `src/sync/coordinator.rs`.

**Interfaces:**
- `SyncCoordinator::start_live(&mut self, root: &Path) -> Result<LiveControl, SyncError>`: obtains persisted device ID, starts the observer even when sync is disabled, and starts an independent store instance for the worker; disabled mode makes no provider calls. `SyncCoordinator::start_live_with_store(&mut self, root: &Path, store: S) -> Result<LiveControl, SyncError>` is the test seam.
- `SyncCoordinator::stop_live(&mut self) -> Result<(), SyncError>`: drain worker before any subsequent full sync or local apply.
- `SyncCoordinator::complete_full_sync(&mut self, outcome: &SyncOutcome) -> Result<(), SyncError>`: clear pending for the current device only for `UpToDate | Applied | Published`, after local apply/baseline save; never for `Offline | Conflicts` or any error. On failed cleanup retain an error state and retry next successful boundary.
- For tests, wrap `MemoryRemoteStore` in a `SharedRemoteStore(Arc<Mutex<MemoryRemoteStore>>)` implementing `RemoteStore`, so worker writes and coordinator sync share the same fake backend. Production `start_live` constructs another `GoogleDriveStore<AccessTokenProvider<PlatformTokenStore>>`; creating it requires no Google request.

- [ ] **Step 1: Add fake-store boundary tests** for startup success clears stale pending; startup offline and unresolved conflicts keep it; live upload never creates a commit; exit full sync commits the latest bytes then clears pending; upload failure and conflict cancellation leave it; cleanup failure keeps status pending until next successful run.

```rust
let outcome = coordinator.after_exit().unwrap();
assert!(matches!(outcome, SyncOutcome::Published | SyncOutcome::UpToDate));
assert_eq!(shared_store.pending_count(device_id), 0);
assert!(!shared_store.list_commits().unwrap().is_empty());
```

- [ ] **Step 2: Run** `cargo test --lib sync::coordinator`; expect new boundary tests to fail.
- [ ] **Step 3: Implement** startup/exit cleanup inside `before_launch`, `after_exit`, and `resolve_conflicts` only on acknowledged outcomes; update `last_full_sync_unix_ms` only on these outcomes. `before_launch` returns `LocalOnly` for provider/auth failures without cleanup; headless conflicts remain unresolved. Modify `lib.rs` to start the observer after startup full sync and before entering picker/guest and pass `LiveControl` into `app_picker`. On toggle or completed connection in the picker, call `control.set_enabled(sync_settings.enabled)` immediately; do not wait for app selection. After picker selection stop/drain before the second prelaunch sync if enabled in picker, then restart observation for the guest; stop/drain before shutdown full sync. On picker-only exit stop the worker but do not run an unsolicited full sync while no guest ran.
- [ ] **Step 4: Run** `cargo test --lib sync::coordinator` and `cargo test --lib sync::engine`; verify no path invokes `synchronize()` while the worker or guest is active. Commit reviewed hunks only.

### Task 6: Picker Stays Open and Live List Refresh

**Files:** Modify `src/environment/app_picker.rs`; test in its `#[cfg(test)]` module.

**Interfaces:**
- `refresh_apps(apps_dir: &Path) -> Result<Vec<AppInfo>, String>`: valid empty result, not fatal.
- `refresh_picker_grid(env: &mut Environment, ..., apps: Vec<AppInfo>)`: discard the old grid and its mappings before rebuilding; if zero apps, show a persistent "add an app" message; clamp current page after removals; never index an absent grid.
- The `LiveControl` picker event receiver reaches the picker loop; on `touchHLE_apps` events mark the grid dirty, then enumerate/rebuild on the UI loop only after changes settle. On `touchHLE_sandbox` changes do not rebuild the grid. File-manager action only opens the URL and returns to the picker.

- [ ] **Step 1: Add failing tests** for event filter (`apps` versus `sandbox`), empty-to-one and one-to-zero grid model transitions, and a regression guard that `openFileManager` does not call `std::process::exit`. Exercise the enumeration helper with a temp empty directory; test model/page calculations without launching SDL.
- [ ] **Step 2: Run** `cargo test --lib environment::app_picker`; expect new tests to fail.
- [ ] **Step 3: Remove** `std::process::exit(0)` and the misleading "exiting" log in `openFileManager`. Refactor initial `app_picker()` so absent/empty roots display a refreshable empty state. Change `app_picker(options: Options, control: LiveControl)` and pass the receiver into `app_picker_inner`; on refresh release/remove the previous grid controls and error label, clear `icon_map`, re-enumerate, rebuild with `make_icon_grid`, and guard icon taps if no grid exists. Do not use a stale index after a refresh.
- [ ] **Step 4: Run** `cargo test --lib environment::app_picker`; manually open the file manager and add/remove an `.ipa` while the picker is visible; confirm the process stays open and updates. Commit only the relevant picker hunks.

### Task 7: Status UI, Documentation, Platform Validation

**Files:** Modify `src/environment/app_picker.rs`, `dev-docs/google-drive-sync.md`; tests in `src/environment/app_picker.rs`.

**Interfaces:**
- `update_cloud_sync_settings` reads `load_live_status` (or a cached `LiveStatus`) and renders enabled/disabled, observer active/unavailable, queued count, last successful upload, last full sync, and latest error. Refresh when a status event arrives and whenever settings are opened; never imply a failed `ConfigInvalid` upload succeeded.

- [ ] **Step 1: Add failing presentation tests** via a pure `format_live_status(&SyncSettings, &LiveStatus) -> String`: disabled, active/queued, failed upload, successful upload, unresolved conflict, and failed watcher; assert no secret or token is included.

```rust
let text = format_live_status(
    &SyncSettings { enabled: true },
    &LiveStatus { active: false, error: Some("prepare Google Drive folder: ConfigInvalid".into()), ..Default::default() },
);
assert!(text.contains("ConfigInvalid"));
assert!(!text.contains("Live backup active"));
```

- [ ] **Step 2: Run** `cargo test --lib environment::app_picker::cloud_sync_ui_tests`; expect missing formatter compile failure. Implement the formatter and event/status refresh in `app_picker_inner`, expanding the settings text viewport as needed without hiding the toggle or Connect button.
- [ ] **Step 3: Document** that edits in either root are staged after a one-second quiet interval, never visible to other devices until full sync after guest exit, deleted files commit only then, remote applies never happen during the guest run, manual Drive folders are not imported, and failed/offline/conflicting runs keep pending data. Explicitly state the last upload/full-sync timestamps and what a pending error means.
- [ ] **Step 4: Run** `cargo fmt --all -- --check`, `cargo test --lib`, `cargo check`; run `dev-scripts/build-desktop.sh` only after checking its expected arguments/output paths. For Android, run the repository's Android build when an NDK is installed; otherwise record that Android compile/device watcher behavior is unverified rather than silently claiming support. Do not run `tests/integration.rs` without checking its destructive `tests/stubs` setup.
- [ ] **Step 5: Manual smoke test** with a real connected account: start picker, open Finder and add an `.ipa`, check grid and pending status; modify sandbox save during guest run, inspect only this device's `pending/` path; verify another device ignores it until guest exit commits; force provider failure and conflict cancellation, verify pending preservation and error reporting. Never print OAuth values. Review `git diff --check` and commit only the new documentation/UI hunks.

## Execution Notes

- Existing worktree is dirty, including `src/lib.rs`, picker, coordinator, store, and Drive adapter. Before each task inspect `git diff`/`git diff --cached`; do not discard existing edits or blindly stage entire mixed files. Where partial staging cannot cleanly separate authorship, leave the task uncommitted and explain why.
- The old plan (`docs/superpowers/plans/2026-10-01-google-drive-sync.md`) is not this feature's implementation checklist. Consult the approved live design before any change that would download remote files during a guest run.
- Keep live upload's own Drive adapter separate from the coordinator's store, and join/drain its thread before calling full sync; failure to drain must not silently clear pending data.
