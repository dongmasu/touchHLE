# Shared Google Drive File Sync

- Status: approved direction; pending written-spec review
- Date: 2026-10-03
- Branch: `feat/google-drive-sync`
- Supersedes: `2026-10-01-google-drive-sync-design.md` remote snapshot/commit model

## Problem

The current remote model appends a full-tree commit after changes and discovers
current state by listing every commit record. A warm local cache avoids some
commit-body downloads, but every sync still enumerates history and validates
the commit graph. Work and remote metadata therefore grow with historical
syncs, not with the current files or changes. The append-only policy also
retains losing conflict versions indefinitely, contrary to the desired
behavior.

## Goals

- Make the shared current files in Google Drive the synchronization authority.
- Mirror `touchHLE_apps` and `touchHLE_sandbox` as ordinary files and folders.
- Use Drive Changes to discover remote updates since each installation's
  locally saved cursor.
- Transfer file content only when a file actually needs to be applied or
  published.
- Detect offline divergent edits from a per-installation local baseline and
  ask the user to select the local or remote version.
- Keep the selected version after conflict resolution and discard the losing
  version from touchHLE's sync state.
- Keep offline execution and retry behavior without a remote pending-upload
  tree or append-only commit history.
- Preserve desktop, Android, graphical, and headless operation.

## Non-Goals

- A general-purpose multi-provider abstraction.
- Permanent version history or automatic historical browsing.
- Retaining the losing conflict candidate after the user resolves it.
- Merging opaque save-file bytes.
- Syncing files outside `touchHLE_apps` and `touchHLE_sandbox`.
- Providing strict transactional coordination for two devices that upload the
  same path at the exact same time; Drive's documented `files.update` request
  does not expose a version-match precondition. The engine will recheck remote
  versions before writes and verify writes afterward, but this is not an
  atomic compare-and-swap.

## Approaches Considered

### A. Shared File Tree With Per-Installation Local Baselines (Selected)

Each Drive file is the shared current value for its path. Each installation
keeps only its local last-observed baseline and Changes page token. This
supports multi-device sync without a remote manifest or commit graph. Changes
feed pagination and changed-file metadata replace history enumeration.

### B. One Shared Manifest Plus Content Objects

A common manifest would require every device to update the same coordination
record. Concurrent updates would still need conflict handling and would
reintroduce a global metadata bottleneck. It is unnecessary when the files
themselves can be the shared state.

### C. Append-Only Snapshot Commits

This is the existing approach. Its request and validation cost grows with
historical commits, and its retention policy preserves versions the user
explicitly chose to discard. It is replaced.

## Architecture

### Remote Layout

The app-owned Drive folder contains a `files` subtree mirroring the two
managed local roots:

```text
touchHLE/
  files/
    touchHLE_apps/...
    touchHLE_sandbox/...
  commits/                  # legacy data, read only during migration
  objects/                  # legacy data, read only during migration
```

Drive file IDs are cached locally to avoid repeated folder and path lookup.
Drive file IDs, `version`, checksums, and parent IDs are provider metadata;
the remote file content and path are the shared current state. Updating a
file keeps its Drive identity where possible. A rename is represented as a
delete at the old path and a create at the new path.

No new commits, content-addressed object tree, remote per-device head,
manifest, or cloud pending tree is written. Drive's own revision history is
not treated as a touchHLE backup guarantee.

### Local Checkpoint

Each installation stores under `.touchHLE_sync`:

- Its Google account/root-folder identity and latest fully processed Changes
  page token.
- A per-path baseline containing the last successfully observed content hash,
  Drive file ID/version when present, and deletion state.
- A mapping from known Drive folder IDs to relative managed paths.
- Any transaction journal needed to resume an interrupted apply or publish.

This checkpoint is local comparison state, not an authoritative cloud
manifest. Another device does not read or update it. Losing it causes a
one-time remote inventory and conservative reconciliation, not data deletion.

Baseline and Changes token advance only after all selected downloads/uploads,
verification, local apply, and conflict decisions in that batch complete.
Persist the cursor atomically with the corresponding baseline.

## Change Discovery and Data Flow

### Initial Connection

1. Obtain a Drive Changes start token.
2. Enumerate the app-owned `files` subtree and construct its current path
   inventory.
3. Read Changes from the saved start token to cover modifications that raced
   with the inventory.
4. Reconcile the resulting remote state with local files and the local
   baseline, then persist the final token and baseline.

The initial inventory is the only normal full remote listing. An account
switch, missing/corrupt checkpoint, or incompatible state schema requires the
same conservative rebuild.

### Warm Synchronization

1. Scan the managed local roots and load the local baseline.
2. Fetch Changes pages from the saved cursor, including removals. Ignore
   unrelated Drive changes and coalesce repeated changes by file ID to the
   latest state in the batch.
3. Resolve changed managed file IDs to paths using the local mapping, changed
   file metadata, or a narrow parent lookup when necessary.
4. Compare local state, last successful baseline, and current Drive state by
   path.
5. Download only remote files that must be applied or shown as conflict
   candidates. Upload only locally changed files selected for publication.
6. Verify downloaded bytes before apply and verify published file metadata
   and checksum before advancing state.
7. Atomically persist the new local baseline and Changes cursor.

When no relevant changes exist, the warm remote path performs no file-content
download or upload and does not enumerate the legacy commit/object folders.
Drive may return unrelated account changes; these are filtered without
downloading their content.

### Change Comparison

For each path, let `B` be the locally saved baseline, `L` the current local
file or absence, and `R` the current Drive file or absence:

- If neither side changed from `B`, do nothing.
- If only `L` changed, publish `L` or its deletion.
- If only `R` changed, download/apply `R` or its deletion.
- If both changed to the same content hash or both deleted, converge without
  a conflict.
- If both changed differently, pause before mutation and ask the user to
  choose local or remote.

For a conflict, stage the remote candidate only as long as needed for the
resolver. After a choice succeeds, publish/apply the selected value and drop
the losing candidate from touchHLE state. Temporary transaction files are
removed after the baseline advances. A failed or cancelled operation keeps
the source files and does not advance the checkpoint.

Modification times and sizes are display/optimization metadata, not content
identity. SHA-256 determines whether two file contents are equal.

### Deletions

Local deletion publishes a Drive deletion; remote deletion is represented in
the Changes feed and the local baseline. A stale device that modified the
deleted path has a local-vs-remote conflict and can choose to recreate its
local version. A device whose local file still matches its baseline applies
the remote deletion.

Do not create an unbounded tombstone log. The local baseline records deletion
only for paths needed to compare that installation's offline changes.

## Concurrent Writes

Before publishing a changed path, fetch its current Drive metadata and compare
its version/checksum with the version in the plan. If it changed, discard the
stale plan and re-run comparison so divergent offline edits reach the conflict
resolver before overwrite. After upload, read back metadata and verify that
the published content is still current before advancing local state.

Drive's documented `files.update` method supports replacing file content but
does not document an `If-Match`/version precondition. Consequently, exact
simultaneous same-path uploads cannot be guaranteed atomic by this design.
The preflight and post-write checks narrow and detect many races but do not
provide compare-and-swap semantics. Tests must cover observed races and the
implementation must never claim transactional locking across devices. If
strict zero-loss simultaneous writes become a requirement, that needs a
separate coordination design rather than hidden reliance on Drive revisions.

## Migration From Snapshot Commits

The first run with this schema detects the legacy `commits/` and `objects/`
layout and does not interpret it as the new live file tree.

- If the legacy history has one unambiguous current tip, materialize that tip
  into the new `files/` tree after verifying every referenced object.
- If legacy tips diverge, use the existing conflict resolver to select the
  current value per path before materializing.
- If local files also diverged from the selected legacy baseline, reconcile
  them with the normal three-way rules and ask for choices where needed.
- Publish and verify the complete new tree before marking migration complete.
- Keep legacy `commits/` and `objects/` untouched during migration and do not
  read them again after success. No automatic legacy deletion is performed.
- On interruption, resume idempotently from a local migration journal. Never
  treat a partial `files/` tree as a completed migration or replace the legacy
  baseline with an empty state.

The existing `.touchHLE_sync` state and commit cache require a versioned local
schema migration. An invalid or old cache is discarded as a cache, but the
saved baseline needed for the one-time remote migration must be preserved
until the new file tree is verified.

## Startup, Shutdown, and Offline Behavior

- Startup sync completes before app enumeration, as today; the progress UI
  remains visible and the guest launch gate remains closed until sync succeeds
  or the user explicitly chooses offline mode.
- Shutdown sync runs after the guest stops writing. On failure, preserve local
  data and leave the old baseline/cursor so the next run retries.
- Offline execution requires no remote staging. Local changes are found by
  comparing the next successful local scan with the saved baseline.
- Android background work may sync only when the guest-session lock is free.
  Existing lifecycle and sync-operation locking remains applicable; workers
  must not advance the baseline while the guest is writing.
- A headless run never opens an interactive conflict resolver; it leaves
  files and checkpoint unchanged and reports that graphical resolution is
  required.

## Performance Requirements

- No warm sync lists or downloads commit records; commit count has no effect
  on warm-sync request count or planning time.
- No-change warm sync performs zero remote media downloads and zero uploads.
- A remote change downloads only changed content that must be applied or
  presented for conflict resolution.
- A local change uploads only that file's content; unchanged files are not
  uploaded.
- Measure startup and shutdown wall time, Changes pages, file metadata calls,
  media downloads/uploads, and bytes transferred separately.
- Preserve correctness-first SHA-256 validation. Any local scan shortcut must
  be separately justified and must not make size/mtime the sole content
  identity.

## Failure and Integrity Rules

- Never advance the Changes cursor without atomically saving the matching
  baseline.
- Never overwrite a remote file from a plan whose observed Drive version is
  stale; re-plan first.
- Download to staging, validate the expected path and content hash, then
  atomically apply.
- Verify uploads before recording their Drive version/hash in the baseline.
- Keep the last known-good local files and checkpoint on authentication,
  network, provider, validation, or user-cancellation errors.
- Treat a missing/corrupt Changes cursor as a full inventory and conservative
  reconciliation, never as an empty remote.
- Validate all remote paths, reject case-insensitive collisions, and retain
  existing symlink protections.
- Do not log credentials, file contents, or sensitive response bodies.

## Testing

- Test initial inventory, cursor catch-up, paginated feed, unrelated changes,
  repeated changes to one file, trashed files, and removed/inaccessible files.
- Test the three-way planner for unchanged, local-only, remote-only, identical,
  divergent, create, delete, and delete-vs-modify cases.
- Verify divergent offline edits show the resolver and only the selected
  bytes remain in the new sync state.
- Inject failures between download, apply, upload, verification, baseline
  save, and cursor save; confirm retries are idempotent and do not lose local
  data.
- Test stale-version preflight and post-write verification. Document the
  exact simultaneous-upload limitation in integration test results.
- Test first migration from one legacy tip, divergent legacy tips, missing
  object, partially materialized tree, and interrupted migration. Confirm
  legacy data is never deleted.
- Assert a warm no-change sync makes no remote content transfer and performs
  no commit/object-folder listing regardless of legacy commit count.
- Test graphical and headless conflict behavior and Android worker lock
  behavior.

## References

- [Google Drive Changes API](https://developers.google.com/workspace/drive/api/guides/manage-changes)
- [Google Drive changes.list](https://developers.google.com/workspace/drive/api/reference/rest/v3/changes/list)
- [Google Drive files.update](https://developers.google.com/workspace/drive/api/reference/rest/v3/files/update)
- [Google Drive revisions](https://developers.google.com/workspace/drive/api/guides/manage-revisions)
