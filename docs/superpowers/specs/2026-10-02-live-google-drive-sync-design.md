# Live Google Drive Backup Design

- Status: approved design
- Date: 2026-10-02
- Related design: [Google Drive Sync Design](2026-10-01-google-drive-sync-design.md)

## Summary

Keep touchHLE running when the user opens the file manager. While touchHLE is
running, observe `touchHLE_apps` and `touchHLE_sandbox` and back up stable local
file changes to Google Drive. Do not publish those changes as a usable snapshot
while the guest app is running. After the guest stops, run the existing
bidirectional synchronization and publish the complete snapshot; other devices
can apply it only after that commit exists.

This changes the earlier design's non-goal of live synchronization. The live
phase is upload-only staging, not live application of remote files.

## Goals

- Keep the app picker open after opening the host file manager.
- Detect nested file additions, changes, removals, and renames in both managed
  directories while touchHLE is running.
- Back up changed file contents to Drive shortly after writes settle.
- Prevent other devices from applying an in-progress guest save.
- Apply remote changes and resolve conflicts only after the guest has stopped.
- Preserve pending staged data on network, provider, or conflict failures.
- Make the sync state and most recent result understandable to the user.

## Non-Goals

- Applying remote changes to a running guest.
- Exposing live staged files as a normal touchHLE snapshot to other devices.
- Importing an arbitrary `touchHLE_apps` directory manually created in Drive;
  committed snapshots continue to use the existing `objects/` and `commits/`
  format.
- Syncing paths outside `touchHLE_apps` and `touchHLE_sandbox`.
- Retaining every intermediate version of a file changed during one run.

## Architecture

### File Change Observer

Run a recursive filesystem observer for the user-data roots. Watch only
`touchHLE_apps` and `touchHLE_sandbox`; also observe their parent sufficiently
to detect when either root is created or replaced. Filter unrelated events.
The observer is active while the graphical picker or a guest is running, and
also during headless guest runs when cloud sync is enabled.

Coalesce event bursts with an initial one-second quiet interval. For changed
files, read only after the file appears stable; compare metadata before and
after reading and requeue the path if it changed during the read. Directory
events trigger a scan of that subtree. Deletions do not upload file contents;
they are represented by the complete snapshot at final synchronization.
Reject symlinks and paths that fail the existing sync path validation.

The watcher must not run provider I/O on the SDL/UI event thread. It sends
coalesced changes to one serialized sync worker. A failed watcher or event
overflow triggers a rescan of the two managed roots rather than silently
discarding dirty paths.

### Pending Drive Staging

Add a per-device staging namespace to the app-owned Drive folder:

```text
touchHLE/
  objects/<sha256>
  commits/<commit-uuid>.json
  pending/<device-id>/touchHLE_apps/...
  pending/<device-id>/touchHLE_sandbox/...
```

The pending namespace contains the latest uploaded bytes per path and is
mutable. Later changes overwrite the same pending path; the live phase does not
create immutable snapshot objects or commit records. Normal sync clients ignore
`pending/`, so no other device can apply an in-progress save. Local deletions
and renames are represented by the final committed snapshot; stale pending
copies are harmless and remain isolated until cleanup.

The remote-store interface gains operations to write a pending file and remove
one device's pending subtree. Pending writes use validated relative paths and
the stable device ID from local sync state.

### Runtime Flow

1. On startup, run the existing full synchronization before enumerating apps.
   Clear stale pending data for this device only after a complete successful
   synchronization and any required conflict resolution. A disabled, offline,
   authentication/provider-skipped, or unresolved run must preserve pending
   data.
2. Start the observer for the two managed roots. Cloud staging is active only
   when cloud sync is enabled; app-list refresh remains available in the
   graphical picker.
3. After a stable file-change batch, upload current file contents to the
   device's pending paths. Do not fetch remote commits, apply remote files, or
   publish a commit during this phase.
4. If `touchHLE_apps` changes while the picker is visible, re-enumerate apps
   and refresh the picker grid. If it changes while a guest is running, the
   current guest is unaffected; the updated files are available on the next
   launch.
5. When the guest exits, stop and drain the live uploader, then run the
   existing full bidirectional synchronization. This publishes the final
   snapshot and applies remote changes or opens the conflict resolver.
6. Remove this device's pending subtree only after the final synchronization
   and any required conflict resolution succeed. On failure or cancellation,
   keep pending data for a later successful run.
7. Opening the file manager returns to the picker instead of terminating the
   process.

The one-second quiet interval is a debounce, not a transaction guarantee.
If a guest performs a multi-file save non-atomically, pending paths may
represent different moments. They are not visible to normal sync clients
until the final snapshot commit is published.

### Status and Failure Handling

Report observer startup, queued changes, upload progress, completion, and
failures without logging OAuth tokens, client secrets, or file contents. Persist
the latest sync status locally so the picker settings view can show whether
live backup is active, when the last upload/full sync succeeded, and any
pending error.

Provider and network errors leave local files untouched and preserve pending
work. Retry dirty paths with bounded exponential backoff and retry them again
at shutdown. A `ConfigInvalid` error is displayed as a failed/skipped upload,
not as successful synchronization. If cloud sync is disabled, do not upload
pending files.

The existing full sync remains the only operation that publishes manifests,
downloads remote objects, applies tombstones, advances the baseline, or
resolves conflicts.

## Data Safety

- Never call the existing full `synchronize()` operation while a guest is
  running; it may apply or delete files under the guest's filesystem.
- Pending files are isolated from committed manifests and ignored by other
  devices.
- Verify the staged bytes are stable while being read. If not, requeue them.
- Keep pending files until the corresponding final snapshot is committed and
  any conflicts are resolved.
- On unexpected process termination, pending staging may remain. The next
  successful startup sync reconciles the local tree before clearing this
  device's stale pending subtree.
- Temporary pending files store only the latest content per path during a run;
  they do not create a permanent history of every autosave.

## Testing

- Test event coalescing, root creation, nested changes, rename/delete batches,
  watcher overflow, and retry queuing with an injectable event source.
- With a fake remote store, verify pending writes overwrite by device/path and
  never create a commit or apply remote files.
- Verify the final full sync publishes the latest local snapshot and pending
  data is removed only after success.
- Verify failed upload, failed final sync, and unresolved/cancelled conflict
  resolution preserve pending data.
- Verify pending data is ignored by a second device until a normal commit is
  published.
- Verify app-picker enumeration refreshes after `touchHLE_apps` changes and
  that opening the file manager does not terminate touchHLE.
- Verify a file changing during upload is requeued rather than treated as a
  successful stable backup.
- Test the filesystem observer and Google Drive staging on the desktop build;
  keep deterministic behavior covered by unit tests using the fake store.

## Risks

- A quiet interval cannot guarantee a multi-file game save is transactionally
  consistent. It only keeps intermediate staged files from being applied by
  normal clients.
- Staged bytes are not a committed snapshot and cannot be restored through the
  normal sync flow until a final commit is published.
- Provider failures such as `ConfigInvalid` prevent live backup; local files
  remain authoritative and the pending queue must remain visible.
- Recursive app-bundle observation may generate large event bursts. Events
  must be coalesced, and unchanged files must not be uploaded again.
- Verify the selected observer backend supports every target that enables
  Google Drive sync. Android behavior must not silently claim live backup if
  its filesystem backend is unavailable.
