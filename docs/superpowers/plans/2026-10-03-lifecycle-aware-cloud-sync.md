# Lifecycle-Aware Cloud Sync Implementation Plan

> **For agentic workers:** Execute these tasks in order and verify each task before continuing.

**Goal:** Ensure disk-persisted touchHLE changes are eventually reconciled to Google Drive even when Android kills the process before `env.run()` returns.

**Architecture:** Use advisory OS file locks in the shared Rust sync layer to distinguish an active touchHLE writer from a process that has exited and to serialize full reconciliation. AndroidX WorkManager invokes the existing Rust sync engine without an Activity, uses the same app-specific external files directory, persists progress/errors, and retries when Android permits. Existing in-process live upload continues staging stable files privately while a guest is active.

**Tech Stack:** Rust `std::fs::File` locks, existing sync coordinator and `RemoteStore`, Android Java, AndroidX WorkManager, JNI, existing Android keyring and SDL native libraries.

**Spec:** `docs/superpowers/specs/2026-10-03-lifecycle-aware-cloud-sync-design.md`

## Global Constraints

- Never treat `onPause` or `onStop` as proof that the guest exited.
- A worker may publish/apply only while it holds the sync-operation lock and the guest-session lock.
- Keep local data and the previous baseline when sync fails or is interrupted.
- Interactive Google authorization is forbidden in a background worker.
- Use `Context.getExternalFilesDir(null)` for Android workers; do not call SDL path APIs before SDL initialization.
- `Continue offline` applies only to the current launch; it does not disable later retries.
- Do not stage or publish unflushed in-memory game state.

---

## File Map

- `src/sync/locks.rs`: secure creation and acquisition of operation/session lock files.
- `src/sync.rs`: expose the lifecycle-lock module.
- `src/sync/coordinator.rs`: acquire the operation lock around full sync and lock each live pending write; expose a background reconciliation entrypoint.
- `src/sync/live/worker.rs`: route stable pending writes through the shared operation lock.
- `src/lib.rs`: hold the guest-session lock from before environment construction until the environment has been destroyed; release before final sync.
- `src/sync/auth/android.rs`: export JNI functions for background sync and keyring initialization.
- `android/app/src/main/java/org/touchhle/android/NativeSyncBridge.java`: initialize application context for both Activity and worker, load SDL/touchHLE native libraries, and request noninteractive Google tokens without an Activity.
- `android/app/src/main/java/org/touchhle/android/CloudSyncWorker.java`: run durable, network-constrained work and persist worker state.
- `android/app/src/main/java/org/touchhle/android/CloudSyncScheduler.java`: enqueue unique periodic and immediate work.
- `android/app/src/main/java/org/touchhle/android/MainActivity.java`: schedule background work on `onStop` and guest exit, and show startup sync state.
- `android/app/src/main/AndroidManifest.xml`: register worker notification requirements.
- `android/app/build.gradle.kts`: add the AndroidX WorkManager runtime dependency.
- `src/sync/locks.rs`, `src/sync/coordinator.rs`, and `src/sync/auth/android.rs`: test locking, background eligibility, and safe worker outcomes.

## Task 1: Shared Sync And Guest Locks

**Interfaces:**

- `SyncLocks::new(root: &Path) -> Result<SyncLocks, SyncError>`
- `SyncLocks::operation(&self) -> Result<LockGuard, SyncError>`
- `SyncLocks::try_guest(&self) -> Result<Option<LockGuard>, SyncError>`
- `SyncLocks::guest(&self) -> Result<LockGuard, SyncError>`
- `SyncCoordinator::background_reconcile(&mut self) -> Result<BackgroundSyncResult, SyncError>`
- `BackgroundSyncResult` has `Disabled`, `GuestActive`, `Completed`, and `Conflicts` outcomes.

- [x] **Step 1: Add lock tests first**

Add tests in `src/sync/locks.rs` proving that a second file handle cannot acquire either lock while its guard is alive, that dropping the guard permits acquisition, and that a busy guest lock returns `Ok(None)` without blocking.

- [ ] **Step 2: Run the focused lock tests**

Run: `cargo test sync::locks::tests --lib`
Expected: FAIL because the lock module and API do not exist.

The pre-implementation red run was not recorded; only the passing tests are verified.

- [x] **Step 3: Implement lock-file creation and guards**

Create lock files under `.touchHLE_sync` with no-follow semantics and private permissions. Use the same Rust `File::try_lock` primitive from the foreground and JNI worker paths. Ensure all error paths drop open handles and never interpret a lock file's mere existence as an active lock.

- [x] **Step 4: Verify the lock tests**

Run: `cargo test sync::locks::tests --lib`
Expected: PASS, including same-process independent-handle contention.

- [x] **Step 5: Lock coordinator operations and background reconciliation**

Add a lock root to `SyncCoordinator`; hold the operation lock through startup, final sync, conflict resolution, pending cleanup, and baseline updates. `background_reconcile` checks settings, acquires operation then guest lock non-blockingly, returns `GuestActive` if a writer holds the guest lock, otherwise holds both locks through full sync/apply. Route every live `write_pending` operation through the operation lock without holding it for the lifetime of the live uploader.

- [x] **Step 6: Test background decisions and writer serialization**

Add coordinator tests asserting disabled sync returns `Disabled`, a held guest lock returns `GuestActive` without publishing or advancing state, and an idle root completes a normal full sync. Add a contention test proving a live pending write cannot overlap a full sync operation.

- [x] **Step 7: Run Rust sync tests**

Run: `cargo test sync::coordinator::tests --lib`
Expected: PASS, including existing sync/merge/apply scenarios and new lock coverage.

## Task 2: Hold The Guest Lock Across The TouchHLE Session

**Interfaces:**

- `SyncCoordinator::begin_guest_session(&self, root: &Path) -> Result<LockGuard, SyncError>`
- The returned guard remains owned by the caller until the guest `Environment` has been destroyed.

- [x] **Step 1: Add lifecycle ordering tests**

Test lock acquisition around a simulated guest session: while the guard is held, a background reconciliation must return `GuestActive`; after the guard is dropped, the same reconciliation must complete.

- [x] **Step 2: Acquire the guest lock before environment construction**

In `src/lib.rs`, acquire the session guard after startup reconciliation and before `Environment::new`. Keep it while the app picker and guest are active so remote apply cannot race touchHLE file access. Explicitly drop it after `env.run()` returns and before `after_exit`; early picker cancellation and error returns release it by RAII.

- [x] **Step 3: Verify lifecycle ordering**

Run: `cargo test sync::coordinator::tests --lib`
Expected: PASS; the simulated background worker cannot apply while the session guard exists and can reconcile after release.

## Task 3: Activity-Free Android WorkManager Worker

**Interfaces:**

- `NativeSyncBridge.initializeContext(Context)` initializes the native Android keystore backend and retains only application context for silent token acquisition.
- `NativeSyncBridge.runScheduledSync(Context, String) -> int` returns `0` for completed/idle/conflicts, `1` for guest active, `2` for retryable provider/network failure, and `3` when foreground authorization is required; detailed safe status is persisted by Rust.
- `CloudSyncWorker.doWork()` maps completed/idle, guest-active, and foreground-auth-needed to success, transient provider failures to retry, and disabled mode to success without network access.
- `CloudSyncScheduler.schedulePeriodic(Context)` registers unique connected-network reconciliation with WorkManager's supported periodic interval.
- `CloudSyncScheduler.enqueueNow(Context)` registers a unique connected-network one-shot reconciliation.

- [ ] **Step 1: Add worker mapping tests**

Add Android local unit tests for native result mapping: complete, guest active, disabled, and retryable provider failure. Verify an unavailable credential never starts an interactive Activity.

Native result mapping and foreground-auth-needed classification are tested, but an Activity-free Google Play services authorization call requires Android device/instrumentation validation.

- [x] **Step 2: Add WorkManager and application wiring**

Add the WorkManager dependency and implement unique periodic and one-shot requests with a connected-network constraint and exponential backoff. Worker execution loads SDL before touchHLE, initializes keyring and silent Google authorization from `getApplicationContext()`, and obtains the same root through `getExternalFilesDir(null)`. No custom Application subclass is needed.

- [x] **Step 3: Add the native bridge**

Export JNI methods matching `NativeSyncBridge`; call the existing Google Drive coordinator with noninteractive authorization and `background_reconcile`. Return only stable result codes and persist redacted status; never expose token or provider response details to Java.

- [x] **Step 4: Schedule before backgrounding and guest execution**

Schedule periodic work when the Activity starts and enqueue immediate unique work from `onStop` and again after the guest lock is released. The latter uses `APPEND_OR_REPLACE` so a still-running `onStop` job cannot swallow the exit-time request. Leave periodic work registered before the guest can write. The worker checks the persisted Rust sync setting before network access; disabling cloud sync prevents reconciliation.

- [x] **Step 5: Verify Android build and unit tests**

Run: `cd android && ./gradlew :app:testDebugUnitTest :app:assembleDebug`
Expected: PASS; APK compiles with WorkManager and worker bridge, and result mapping tests pass.

## Task 4: Persisted Status And User-Visible Progress

**Interfaces:**

- Extend `LiveStatus` with a serializable background-work state that distinguishes idle, running, guest-active/deferred, auth-needed, completed, conflict, and failed states. WorkManager retains queued requests separately.
- `CloudSyncWorker` shows an indeterminate notification during transfers; the app picker reads the persisted Rust status on the next foreground launch. No byte-level percentage is claimed.

- [x] **Step 1: Test status serialization and redaction**

Extend `src/sync/status.rs` tests to round-trip the new states and ensure errors still pass through `redacted_error`.

- [x] **Step 2: Persist worker transitions**

Write running/completed/deferred/auth-needed/conflict/failed states atomically. WorkManager persists queued work. Never set successful full-sync timestamps for enqueued work, pending-only uploads, guest-active deferrals, conflicts, or failures.

- [x] **Step 3: Display Android startup wait state**

Show a visible syncing/waiting state before the app picker can launch a guest, with retry and continue-offline choices for foreground startup failures. Clear the gate only after successful sync or explicit offline choice.

- [x] **Step 4: Display ongoing worker state**

Show an indeterminate foreground notification during a worker upload and surface its persisted phase/error on the next Activity launch. Conflicts remain pending for foreground resolution.

- [x] **Step 5: Run status and UI checks**

Run: `cargo test sync::status::tests --lib` and `cd android && ./gradlew :app:testDebugUnitTest :app:assembleDebug`
Expected: PASS; status survives process recreation and no failed/deferred state appears as backed up.

## Task 5: Crash-Recovery Regression Coverage

- [x] **Step 1: Simulate process death after local disk write**

Add an integration test that writes a managed file, drops the guest lock without calling `env.run()` or final sync, then invokes background reconciliation and asserts the commit contains the new bytes.

- [x] **Step 2: Simulate an active guest**

Hold the guest lock while running a worker attempt; assert no full commit is published, the local baseline remains unchanged, and staged/live pending data is retained.

- [x] **Step 3: Simulate interruption and retry**

Inject a remote-store failure during background reconciliation; assert local files and baseline remain recoverable, the persisted state is retryable, and a later run publishes the change.

- [x] **Step 4: Run complete relevant suites**

Run: `cargo test sync:: --lib` and `cd android && ./gradlew :app:testDebugUnitTest :app:assembleDebug`
Expected: PASS; no Google credentials or external network are needed by tests.

Verified: `cargo test --lib` (220 passed) and Android `:app:testDebugUnitTest :app:assembleDebug` (passed) with locally installed Gradle 8.11.1/JDK 17. Android tests do not exercise Google Play services or process recreation on a real device.

- [x] **Step 5: Review final diff**

Run: `git diff --check` and inspect only files touched by this plan, preserving all pre-existing user changes.

Verified: `cargo fmt --all -- --check`, `git diff --check`, and a read-only review of headless authorization, auth-needed outcomes, and guest-exit scheduling. Real-device process-death and Google Play services authorization remain to be validated.
