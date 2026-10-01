# Google Drive Sync Design

- Status: approved design for review
- Date: 2026-10-01
- Branch: `feat/google-drive-sync`, based on `trunk`

## Summary

Add opt-in synchronization of `touchHLE_apps` and `touchHLE_sandbox` through
Google Drive. Use OpenDAL's Google Drive service for remote object operations
and a shared Rust sync engine for manifests, version comparison, backups, and
conflict resolution. Support Android and desktop platforms, including
headless command-line launches.

The local folders remain usable without a network connection. Remote history
is append-only: concurrent clients create separate commits rather than
overwriting a shared manifest or file. This preserves offline and simultaneous
changes for later reconciliation.

## Goals

- Sync the existing `touchHLE_apps` and `touchHLE_sandbox` trees without
  changing their local paths.
- Support Android and desktop builds, including headless command-line mode.
- Synchronize before app launch and after app exit when online.
- Allow offline execution and retain pending local changes for later sync.
- Detect divergent local and remote changes using a persisted common baseline.
- Show per-file local and cloud size and modification time for conflicts.
- Let users choose the desired version for each conflicting path.
- Preserve unselected versions and all prior versions without automatic
  deletion.
- Use a dedicated Google Drive folder named `touchHLE`.
- Use the narrowest practical Drive permission and store OAuth tokens in
  platform-protected storage.

## Non-Goals

- iCloud, other cloud providers, or a generic multi-provider framework.
- Live synchronization while a game is running.
- Merging opaque game-save file contents.
- Synchronizing touchHLE options, logs, or other files outside the two named
  directories.
- Automatic pruning or deletion of prior versions.

## Architecture

### Components

- `SyncEngine`: provider-independent manifest scanning, hashing, three-way
  comparison, merge planning, and local apply/backup operations.
- Google Drive adapter: OpenDAL-backed listing, reading, and writing under
  the app-owned `touchHLE` Drive folder.
- Platform authentication adapters: browser-based OAuth authorization for
  desktop and Android, plus refresh-token storage in each platform's secure
  credential store.
- Sync coordinator: startup and shutdown hooks, pending-work tracking, and
  progress/error reporting.
- Conflict resolver: graphical per-file choices in the app-picker flow.
  Headless mode reports conflicts and directs the user to graphical mode.

The sync engine must run away from the SDL/UI event thread. Provider I/O and
hashing must not freeze rendering or input. The implementation must verify
which OpenDAL runtime/blocking interface is appropriate for every target.

### Remote Layout and Version Model

The app creates and owns a `touchHLE` directory in the authenticated Drive
account. Its internal layout is private implementation data:

```text
touchHLE/
  objects/<sha256>
  commits/<commit-uuid>.json
```

Objects are immutable and named by content hash, so identical content is
uploaded once. A commit is an immutable manifest containing:

- A unique commit ID, creation time, and device ID.
- One or more parent commit IDs.
- Relative paths and entry type for the synchronized trees.
- Content hash, size, and source modification time for each file.
- Tombstones for deletions.

There is no shared mutable `HEAD` file. Clients enumerate commit records and
derive the remote tips. If devices publish from the same parent, both commits
remain available as divergent tips. A resolution creates a merge commit whose
parents include the resolved tips.

Each installation stores its stable device ID, last successfully applied
commit ID, per-path baseline hashes, and pending local state. This local
baseline is required to detect cloud changes after offline execution. The
application commit ID is the authoritative cloud revision for synchronization;
native Drive file IDs or revision metadata may be recorded when OpenDAL
exposes them, but correctness must not depend on their availability.

The old blobs and commits remain available for restoration. There is no
automatic garbage collection in v1. Consequently, Drive usage can grow over
time; deduplication avoids storing unchanged bytes repeatedly.

### Change Detection and Conflict Resolution

For each path, compare the last common baseline (B), the current local file
(L), and the current remote file (R):

- If only L changed, publish L.
- If only R changed, apply R locally.
- If both sides have the same content hash, converge without a conflict.
- If both sides changed differently, require a user choice.
- Represent additions and deletions explicitly; tombstones prevent deleted
  files from reappearing during a later sync.

File size and modification time are display metadata, not the sole change
detector. File contents are opaque and are never automatically merged.

The conflict UI shows local and cloud versions, size, and modification time.
The user chooses a version per conflicting path. The unselected blob remains
in immutable history and is referenced by the resulting merge history so it
can be restored later.

### Startup and Shutdown Flow

Before launch:

1. Scan local files and load the saved baseline.
2. If offline or not yet connected to an account, continue with local files
   and retain pending changes.
3. If online, enumerate remote commits and build a sync plan.
4. Apply non-conflicting remote changes. Resolve conflicts in graphical mode
   before launching the guest app.
5. In headless mode, stop before guest execution when a conflict needs user
   input, and direct the user to launch graphical mode to resolve it.

While the guest app runs, use only local files; do not replace sandbox files
under a running app.

At app exit, snapshot local changes and attempt synchronization before the
process terminates. Upload and verify immutable objects before publishing the
commit record. Advance the local baseline only after remote operations and
local application have succeeded. If offline or a transfer fails, preserve
the local state and pending work and allow the process to exit.

If a concurrent cloud change is discovered during graphical shutdown, offer
the conflict resolver before final process termination. Headless mode retains
both branches and exits; the conflict is resolved on a later graphical run.
Unexpected termination skips shutdown sync, but the next startup compares
local files with the unchanged baseline and recovers pending changes.

The implementation must route every process-exit path through the
pre-termination sync hook. Current guest exit paths call
`std::process::exit`, so this hook must run before that call rather than after
the emulator loop returns.

### First Connection

- Empty remote folder: publish the local trees as the initial commit.
- Empty local trees: download the remote snapshot.
- Both sides populated without a shared baseline: automatically converge
  identical hashes and show conflicts for differing content at the same path.
  Preserve both versions.

### Authentication and Permissions

Use Google OAuth from the system browser with platform-appropriate clients.
The app creates its own `touchHLE` folder and requests `drive.file` rather than
general Drive access. Refresh tokens must be stored using the platform's
secure credential facility; never accept tokens as command-line arguments.
Headless mode reuses previously authorized credentials and never prompts for
interactive authorization.

OpenDAL performs remote file operations after authorization; it does not
provide the initial user-consent flow. OpenDAL's current Google Drive
documentation describes the broader `drive` scope, so the implementation must
prove that `drive.file` is sufficient for creating, listing, reading, and
writing this app-owned folder. Do not silently widen permissions if that
verification fails; stop for a separate decision.

## Failure and Integrity Rules

- Upload immutable content before publishing a commit that references it.
- Download into staging, verify hashes, then atomically apply local changes.
- Do not advance a baseline after partial transfer or failed verification.
- Make retries idempotent using content hashes and unique commit IDs.
- Preserve incomplete local work; unreferenced remote objects are harmless and
  are not automatically deleted in v1.
- Validate remote relative paths before writing them locally.
- Treat clocks and modification times as display data; use content hashes and
  commit ancestry for correctness.
- If credentials are unavailable, report that cloud sync is inactive and
  preserve local-only operation.

## Testing

- Unit-test the change planner for unchanged, one-sided, identical, divergent,
  added, and deleted paths.
- Test divergent commit tips and merge commits from simultaneous clients.
- Use a fake object store to test interrupted uploads/downloads, retries,
  deduplication, hash mismatch, and baseline advancement.
- Test first connection for empty remote, empty local, and both-populated
  cases.
- Verify conflict selection preserves the unselected version and supports
  restoring it.
- Verify offline startup, offline shutdown, headless conflict refusal, and
  headless post-run divergence.
- Build the desktop and Android targets in CI. Keep live Google OAuth tests
  separate from deterministic unit/integration tests.

## Validation Risks

- Confirm the OpenDAL Google Drive service builds for all target platforms
  and supports the required directory, list, read, and write operations.
- Verify `drive.file` access and the required file metadata through the
  OpenDAL service. Add a narrow provider-specific metadata adapter only if
  required; do not broaden OAuth scope without approval.
- Verify `.app` bundles and cross-platform path constraints, including
  case-insensitive filesystems and symlink handling.
- Confirm every guest and host exit route reaches the sync coordinator without
  invoking guest lifecycle callbacks twice.
- Establish the Google OAuth application/client ownership and any required
  consent-screen verification before distributing builds.

## References

- [OpenDAL Google Drive service](https://opendal.apache.org/docs/rust/opendal/services/struct.Gdrive.html)
- [Google Drive files resource](https://developers.google.com/workspace/drive/api/reference/rest/v3/files)
- [Google Drive revisions resource](https://developers.google.com/workspace/drive/api/reference/rest/v3/revisions)
- [Google OAuth for installed applications](https://developers.google.com/identity/protocols/oauth2/native-app)
- [Google Drive API authorization scopes](https://developers.google.com/workspace/drive/api/guides/api-specific-auth)
