# Google Drive Sync

Google Drive sync is optional and supports desktop and Android builds. It
synchronizes only `touchHLE_apps` and `touchHLE_sandbox`; options, logs, and
other files stay local. The remote data lives in an app-owned Drive folder
named `touchHLE`.

## Build Configuration

Desktop builds require `TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_ID` and
`TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_SECRET` in the build environment.

The desktop build scripts require both values and compile them into the
application. The running app does not require either environment variable,
including when exchanging or refreshing tokens. Do not commit the client
secret or share it as a standalone value. Because the application needs it,
the compiled binary contains the client secret and it can be extracted from
that binary.

Run the script for your host platform:

```sh
./dev-scripts/build-desktop.sh
```

On Windows, run `dev-scripts/build-desktop.ps1` from PowerShell. Both scripts
require `TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_ID` and
`TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_SECRET` in the build environment, apply
the CMake 3.5 policy compatibility setting, and create a native release
binary under `target/release`.

Android uses Google Identity Services' `AuthorizationClient`; it does not
embed an OAuth client ID or use a browser redirect. In Google Cloud, create an
Android OAuth client for the installed app's package name and signing
certificate SHA-1. Run `gradle :app:signingReport` from the `android`
directory, or run the `:app:signingReport` task from Android Studio, and
register the SHA-1 reported for the variant you install. If the branded build
changes the application ID, register that exact package name as well. The
signing fingerprint is public app identity metadata; never share the keystore
or its passwords.

Create the desktop and Android OAuth clients in a Google Cloud project with
the Google Drive API enabled, and configure the consent screen. The app
requests the `https://www.googleapis.com/auth/drive.file` scope. The direct
Drive API adapter creates and accesses only files it creates for touchHLE.
Desktop uses system-browser OAuth with PKCE and a loopback callback.
Android requests access through Google Play services; its SDK manages
short-lived access-token renewal, so the app does not store a server refresh
token. Tokens are stored only in platform-protected credential storage,
never in command-line arguments or sync manifests.

The `drive.file` scope limits access to files created by or explicitly opened
with touchHLE, rather than granting access to every file in the account.
Previously saved credentials used another scope version and must be
reauthorized once after upgrading. Files created through the same OAuth app
are expected to remain available, but verify that the existing folder and
legacy `commits/` and `objects/` data are visible with the new grant before
distributing a build. Migration will not treat a missing saved baseline commit
as an empty history.

Google's consent-screen verification requirements depend on the scopes and
distribution model configured for the OAuth project. Review the current
requirements before distributing beyond test users; a local build and passing
unit tests do not establish approval for public distribution.

References: [Google Android authorization](https://developers.google.com/identity/authorization/android),
[Google Drive API file operations](https://developers.google.com/workspace/drive/api/reference/rest/v3/files),
[Google Drive OAuth scopes](https://developers.google.com/workspace/drive/api/guides/api-specific-auth),
[Google OAuth app verification](https://support.google.com/cloud/answer/13464321).

## Connect and Enable

1. Open the app picker and select **Google Drive Sync**.
2. Select **Connect Google Drive** and complete the platform's Google
   authorization flow. Android may show a Google consent/account screen;
   desktop opens the system browser.
3. Enable Google Drive sync.
4. Return to the app picker and launch an app.

When enabled, touchHLE syncs at startup before listing apps and constructing
the guest app, then attempts another reconciliation after the guest has stopped. If
sync is enabled from the picker during the current run, it checks again before
starting the selected guest. Changes under `touchHLE_apps` refresh the app
picker list automatically; downloaded apps no longer require a restart to
appear in the picker.

While the picker or guest is running, a filesystem observer watches only
`touchHLE_apps` and `touchHLE_sandbox` for local picker refresh and tracks
paths to rescan. It does not upload from the observer or write a private
cloud staging tree. Picker edits may publish on the next pre-launch sync;
changes made during guest execution publish only after the guest exits.
Remote files are not applied while the guest is running. If the observer is
unavailable, periodic local rescans can still detect changes.

Google Drive Sync settings shows whether sync is enabled, observer availability,
local changes to rescan, the last successful full sync (UTC), background
reconciliation state, and the latest error. A local change count is not an
upload receipt. If a run fails, including with `ConfigInvalid`, keep local
files and retry after fixing the error. Provider details and response bodies
are not shown in status.

On Android, background reconciliation waits until the guest session lock is
free; a busy guest defers work for retry. Shutdown stops the observer and
rescans the final local files before reconciliation.

## Drive Files and First Migration

The shared current state is ordinary files and folders in the app-owned Drive
tree, mirroring only the two managed local roots:

```text
touchHLE/
  touchHLE_apps/...
  touchHLE_sandbox/...
  commits/            # legacy history, read only for first migration
  objects/            # legacy content, read only for first migration
```

Each installation keeps its own last-successful per-path baseline, Drive
file/folder IDs, and Drive Changes cursor under `.touchHLE_sync`. This local
checkpoint is comparison state, not a shared cloud manifest. The first
connection inventories the two root-level managed trees and catches up with Changes; later runs
read Changes since that installation's cursor. A missing or unusable current
checkpoint requires a conservative new inventory, not an assumption that
the cloud is empty. Once a migration succeeds, ordinary sync does not read
the legacy history again.

When upgrading from the former `files/` wrapper, move its `touchHLE_apps/`
and `touchHLE_sandbox/` folders directly under `touchHLE/`, then remove the
empty wrapper before launching the new build. The v2 local checkpoint keeps
its SHA-256 baseline but resets the cursor and cached folder IDs for one
metadata-only inventory. If the old wrapper is still present, sync stops
safely instead of interpreting its contents as remote deletions.

On the first run after upgrading a legacy sync installation, touchHLE verifies
the old `commits/` and `objects/` history and materializes the selected
current files into the root-level managed trees. If legacy tips or offline local files disagree,
the resolver asks which version to use for each conflicting path. Migration
is marked complete only after the new tree is verified; an interrupted
migration can resume without treating a partial tree as complete. Existing
legacy commits and objects are left untouched, not automatically deleted.
Do not remove legacy data while a device may still need to migrate.

A warm run with no relevant changes transfers **zero file content** (no
media downloads or uploads) and does **not enumerate commits** or objects.
It still scans the managed local roots and may request Changes metadata.
Changed files alone are downloaded or uploaded as needed.

## Offline and Conflicts

Offline execution remains available. Local edits made offline stay in the
managed folders; there is no remote pending upload. On the next successful
connection, touchHLE compares the local files and current Drive files with
this installation's saved baseline to detect divergent changes. Failed or
cancelled reconciliation does not advance the matching baseline and Changes
cursor; retain your local files for the next retry.

If both local and cloud copies changed differently, the resolver shows each
path and its local/cloud candidates, including byte size and modification
time in UTC. Choose one version for every conflicting path, then apply the
choices. The file contents are opaque and are never merged automatically.
Canceling leaves the choices unapplied. Headless mode stops before guest
execution when startup conflicts need a choice; launch graphically to resolve
them. A conflict discovered after a headless run is kept for a later
graphical launch.

Deleting a local file publishes a Drive deletion; a remote deletion removes
an unchanged local copy. Deletion versus an offline modification is a
conflict: choose the deletion or the modified file (which recreates it).
Renames act as a deletion at the old path and a creation at the new path.

Before writing a path, touchHLE rechecks the remote version and verifies the
result afterward. This catches many stale plans, but Drive updates have no
atomic version-match precondition: truly simultaneous same-path writes on
different devices can still race and are **not** transactional. Do not run
multiple local touchHLE processes against the same data directory. Choosing
a conflict winner does not preserve the losing candidate as a permanent
touchHLE recovery copy or version-history entry. Keep independent backups
of important saves.
