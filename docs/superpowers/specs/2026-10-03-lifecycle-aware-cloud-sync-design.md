# Lifecycle-Aware Cloud Sync

## Problem

The initial full sync currently runs before the app picker is constructed, and
the final full sync runs synchronously after the guest returns. A long transfer
can look like a hang. On Android, users commonly background touchHLE, and the
OS may kill its process before the guest returns normally. Depending only on
`env.run()` returning can therefore leave locally saved changes unpublished.

## Goals

- Show visible startup and shutdown sync progress instead of appearing frozen.
- Do not allow a guest to start until startup sync reaches a terminal result.
- Upload stable changed files during guest execution to the device-private
  `pending/` area, without publishing a snapshot while the guest may still
  write.
- On Android, reconcile and finish sync using durable scheduled work after the
  app is backgrounded or its process is killed.
- Preserve local files and pending remote data until a complete snapshot is
  successfully committed.
- Retry automatically when the system permits and reconcile again on the next
  foreground launch.

## Non-Goals And Limits

- Sync cannot recover game state that exists only in memory or that the game
  never wrote to `touchHLE_sandbox`. A file that was not flushed to disk before
  process death may also be lost; staging is not a substitute for a game save.
- Background execution cannot be guaranteed while the device is offline, the
  app is force-stopped, or the app is uninstalled. In those cases local data
  remains only while the app's storage is retained; force-stop requires a
  subsequent user launch to retry, and uninstall may erase local data.
- A system-scheduled transfer has no guaranteed start time. Do not report
  "backed up" merely because a job was enqueued or a pending object was staged.
- Changes uploaded to private `pending/` during a guest session are not visible
  to other devices until a full sync publishes a commit.

## User Flow

### Startup

- Construct and display a small sync/wait screen before doing a potentially
  long initial sync.
- Show phases and useful counts, such as scanning, uploading, downloading,
  applying, and completed. Do not claim a smooth byte-level percentage while a
  blocking object transfer is in progress.
- After successful sync, enumerate local apps and show the app picker.
- On offline or provider failure, offer `Retry` and `Continue offline`.
- Keep the guest launch gate closed until sync succeeds or the user explicitly
  chooses offline mode. `Continue offline` bypasses the launch gate for this
  session only; it does not disable cloud sync or cancel future retries.
- If a previous run died before final sync, first reconcile its on-disk changes
  under the same launch gate before enumerating apps or starting another guest.

### Guest Running

- Hold an OS guest-session file lock from before the guest can first write
  through destruction of its environment, not only the `env.run()` call.
- Register the persistent Android periodic reconciliation before allowing a
  guest to write, even if the guest later runs offline. On backgrounding,
  additionally enqueue an immediate unique reconciliation request; do not
  depend on receiving a lifecycle callback before process death.
- Continue watching `touchHLE_apps` and `touchHLE_sandbox`. Upload stable files
  to this device's private pending area after the existing quiet interval.
- Do not publish a full snapshot, apply remote files, or advance the local
  baseline while the guest-session lock is held.
- Desktop continues using the in-process uploader. Android also has durable
  WorkManager reconciliation scheduled while cloud sync is enabled.

### Android Background And Process Death

- Treat `onPause` and `onStop` as visibility/lifecycle signals, never as proof
  that the guest has exited.
- Use a unique, network-constrained WorkManager reconciliation task and a
  periodic safety reconciliation. The worker scans managed roots rather than
  trusting an in-memory dirty queue.
- If the guest-session lock is held, the worker may upload stable local files
  to private pending storage only. It must not publish a commit, apply remote
  files, or advance the baseline.
- If the lock is free, the guest has returned or its process is gone. The
  worker may run a full reconciliation, publish a commit, and apply remote
  files if there are no unresolved conflicts. The worker must hold the acquired
  guest lock through the entire full sync and local apply. This describes
  absence of a local writer, not proof of a clean game save.
- A normal guest return releases the lock and enqueues an immediate unique
  final-sync request. If Android kills the process, the OS releases the lock;
  a later scheduled reconciliation detects the stale session and finalizes
  without requiring an `env.run()` callback.
- Serialize WorkManager, the live uploader, startup sync, and final sync with
  a separate sync-operation lock. Only one component may mutate remote sync
  state or local sync baselines at a time. Acquire the operation lock first;
  under it, acquire the guest lock before admitting a new guest or attempting
  a full sync. A worker that cannot acquire the guest lock non-blockingly may
  only stage stable files; it releases the operation lock between batches.
  Keep the guest lock until local writers stop, then release it to allow a
  later full sync. This same-process and cross-process lock protocol must be
  tested on Android; a stale lock file alone is not an active session.
- Initialize Android credential storage from application-level context so a
  WorkManager worker can access credentials without `MainActivity` being
  created. Interactive authorization is forbidden in a worker: persist an
  auth-needed result for the next foreground launch instead of looping.
- Resolve the same app-specific external files root for the worker as for
  foreground touchHLE, without relying on SDL being initialized or an Activity
  existing. Do not run the existing `SDL_AndroidGetExternalStoragePath`-based
  lookup from an uninitialized worker.
- Persist worker progress/error status for the next foreground screen. Use a
  user-visible notification if a transfer must run as a long-running worker.

### Guest Exit And Shutdown

- Desktop keeps touchHLE open on a final-sync progress screen. On failure,
  offer retry or exit while preserving local data for the next run.
- Android schedules final sync as durable work after the guest-session lock is
  released. If the Activity remains visible, show progress; otherwise let the
  scheduled worker continue when eligible.
- Resolve conflicts only while a UI is available. Background workers preserve
  the conflict and defer interactive resolution to the next foreground run.

## Data Safety

- A process crash must not clear pending data or advance the applied baseline.
- A disabled sync setting prevents scheduling/uploading new work; re-enabling
  sync registers periodic work and scans the entire managed tree before
  claiming successful backup.
- Remote objects are immutable and may be uploaded more than once; repeated
  worker execution must be idempotent.
- A worker stopped after publishing a commit but before finishing local apply
  or updating its baseline must recover safely on retry; do not treat that
  partially completed run as successfully backed up.
- The worker must re-scan local files after restart so changes written shortly
  before process death are not dependent on a volatile observer event. File
  removals and changes that cannot be staged while a guest runs are handled by
  full reconciliation once the guest lock is free.
- Preserve displaced local bytes using the existing recovery mechanism before
  applying remote changes.
- If a worker cannot prove that no guest or other sync writer is active, it
  must stage only or defer; it must not perform full sync/apply.

## Validation

- Startup screen appears before a deliberately slow sync; the guest cannot
  start until sync succeeds or offline mode is explicitly selected.
- Backgrounding an active guest leaves the guest-session lock held and permits
  pending uploads without publishing or applying a snapshot.
- Simulated process death releases the guest lock; a restarted worker discovers
  the saved local change and eventually publishes it.
- A killed process need not enqueue a final request; the already-registered
  periodic work or a later foreground launch completes reconciliation.
- Choosing offline for one launch still permits a later eligible worker to
  retry when network and credentials are available.
- A worker started without SDL or `MainActivity` resolves the correct storage
  root and uses noninteractive credentials; missing credentials defer to UI.
- Starting a guest races safely with a worker attempting full sync: the guest
  cannot write during remote apply, and the worker cannot apply after guest
  admission.
- A worker that runs while the guest lock is held never advances the baseline
  or clears pending data.
- Offline, authentication failure, worker retry, and process death during
  upload preserve local data and do not publish an incomplete commit.
- Interrupting a worker after commit publication but during local apply
  recovers or presents a conflict without silently overwriting local changes.
- Concurrent live uploader and WorkManager runs serialize through the shared
  sync-operation lock.
- A conflict found in background remains unapplied and is surfaced for
  foreground resolution.
- Reopening touchHLE with pending or stale-session state reconciles before
  listing apps or launching a guest.
- Tests use fake stores and temporary directories; they do not require Google
  credentials, network access, or a paid/external model.

## Existing Code Context

- `src/lib.rs`: startup sync currently precedes app enumeration; final sync
  currently follows `env.run()`.
- `src/sync/live/worker.rs`: live watcher uploads stable files to private
  pending storage.
- `src/sync/engine.rs`: full sync requires guest and other local writers to be
  quiescent.
- `src/sync/apply.rs`: multi-file apply can partially progress and requires
  exclusive local-writer control.
- `android/app/src/main/java/org/touchhle/android/MainActivity.java`:
  currently initializes Android keyring context from `onCreate`.
- `src/paths.rs`: Android user-data lookup currently calls SDL's external
  storage path API and cannot simply be reused from an uninitialized worker.
