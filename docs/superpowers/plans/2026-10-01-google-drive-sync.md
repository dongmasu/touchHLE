# Google Drive Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Synchronize `touchHLE_apps` and `touchHLE_sandbox` across desktop and Android through Google Drive while preserving offline changes and all conflicting versions.

**Architecture:** Add a provider-independent Rust sync engine for scanning, immutable snapshots, three-way comparison, and safe local application. Put Google Drive operations and per-platform OAuth/token storage behind adapters, then coordinate sync around the existing app-picker, guest launch, and exit flows.

**Tech Stack:** Rust 2021, Google Drive v3 REST API via reqwest, SHA-256, serde JSON, existing UIKit-like app-picker UI, Android Java/Gradle integration, deterministic fake remote store for tests.

**Spec:** `docs/superpowers/specs/2026-10-01-google-drive-sync-design.md`

> **Progress (2026-10-03):** The user later approved replacing OpenDAL with direct Google Drive REST calls while retaining `RemoteStore` and the sync engine. The adapter now uses the narrower `drive.file` scope; credentials from prior scope versions require one interactive reauthorization.

## Global Constraints

- Synchronize only `touchHLE_apps` and `touchHLE_sandbox`; keep their existing local paths.
- Support Android, desktop, graphical launches, and headless command-line launches.
- Allow offline execution and retain local changes for later synchronization.
- Synchronize before guest launch and after guest exit; never replace sandbox files while the guest is running.
- Require exclusive local-tree writers during apply; a second process or external writer must not modify the synchronized roots until apply finishes.
- Use immutable SHA-256 objects and append-only commits with parent IDs; do not introduce a shared mutable `HEAD`.
- Compare local and remote state against the last successfully applied baseline; use tombstones for deletions.
- Show each conflicting version's size and modification time and require a per-path choice.
- Preserve unselected content and prior history; v1 must not automatically delete or garbage-collect remote versions.
- Use an app-owned Drive folder named `touchHLE` and the `drive.file` OAuth scope.
- Keep tokens out of command-line arguments and logs; use platform-protected storage.
- Verify downloaded hashes and stage local changes before applying them; do not advance the baseline after partial failure.
- Keep hashing and provider I/O off the SDL/UI event thread.

---

### Task 1: Define Sync Records and Safe Tree Scanning

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/sync.rs`
- Create: `src/sync/model.rs`
- Create: `src/sync/scan.rs`
- Modify: `src/lib.rs`
- Modify: `src/paths.rs`
- Test: `src/sync/model.rs`
- Test: `src/sync/scan.rs`

**Interfaces:**
- `RelativePath`: validated, normalized path whose first component is exactly `touchHLE_apps` or `touchHLE_sandbox`; reject absolute paths, parent traversal, invalid components, and case-folding collisions that would overwrite a different file on a case-insensitive filesystem.
- `SnapshotEntry`: `File { sha256: [u8; 32], size: u64, modified_unix_ms: i64 }` or `Tombstone`.
- `LocalOrRemote`: enum used by conflict resolution to select one version.
- `SyncError`: typed error variants for I/O, invalid paths, serialization, integrity failures, provider failures, authentication failures, and unresolved conflicts; implement `Display` and `std::error::Error`.
- `Commit`: `{ id: Uuid, device_id: Uuid, created_unix_ms: i64, parents: Vec<Uuid>, entries: BTreeMap<RelativePath, SnapshotEntry> }`; `entries` is the complete snapshot at that commit, including tombstones.
- `SyncState`: `{ device_id: Uuid, last_applied_commit: Option<Uuid>, baseline: BTreeMap<RelativePath, SnapshotEntry> }`.
- `scan_roots(base: &Path) -> Result<BTreeMap<RelativePath, SnapshotEntry>, SyncError>` hashes regular files under `touchHLE_apps` and `touchHLE_sandbox`; it skips symlinks and non-regular files.
- Store local state and recovery copies beneath `user_data_base_path()/.touchHLE_sync/`, outside the synchronized roots; add path helpers in `src/paths.rs`.
- Add direct `serde`/`serde_json` and `sha2` dependencies; enable UUID serde support while retaining the existing v4 generation feature.

- [ ] **Step 1: Add record serialization tests**

Add tests for stable JSON round-tripping of commits and state, plus path rejection for `/absolute`, `../escape`, and paths containing a platform separator mismatch.

- [ ] **Step 2: Run the focused tests and confirm they fail**

Run: `cargo test --lib sync::model`
Expected: compile/test failure until the sync module and record types exist.

- [ ] **Step 3: Implement the records and path validation**

Use ordered maps for deterministic serialized manifests. Keep path validation independent of filesystem access so remote manifests can be checked before use.

- [ ] **Step 4: Add scanner tests using isolated temporary directories**

Test regular files in both roots, empty roots, nested files, symlink skipping, stable content hashes, and rejection of paths that collide after case folding. Use unique directories under `std::env::temp_dir()` and remove only the test-created directory.

- [ ] **Step 5: Implement the scanner and run focused tests**

Run: `cargo test --lib sync::`
Expected: all model and scanner tests pass.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/paths.rs src/sync.rs src/sync/model.rs src/sync/scan.rs
git commit -m "feat: add sync records and local scanner"
```

### Task 2: Implement Three-Way Change Planning

**Files:**
- Create: `src/sync/merge.rs`
- Modify: `src/sync.rs`
- Test: `src/sync/merge.rs`

**Interfaces:**
- `RemoteTip`: a commit ID and its fully resolved snapshot.
- `RemoteVersion`: `{ commit_ids: Vec<Uuid>, entry: Option<SnapshotEntry> }`; `None` means the path is absent from that snapshot.
- `Conflict`: `{ path: RelativePath, local: Option<SnapshotEntry>, remote_candidates: Vec<RemoteVersion> }`.
- `ConflictChoice`: `{ path: RelativePath, selected: LocalOrRemote, remote_commit_id: Option<Uuid> }`; a Remote choice names one of that path's candidate commit IDs, while a Local choice has no remote ID.
- `SyncPlan`: `{ apply_remote: BTreeMap<RelativePath, SnapshotEntry>, publish_local: BTreeMap<RelativePath, SnapshotEntry>, conflicts: Vec<Conflict>, merge_parents: Vec<Uuid>, local_snapshot: BTreeMap<RelativePath, SnapshotEntry>, remote_tips: Vec<RemoteTip> }`.
- `resolve_remote_tips(commits: &[Commit]) -> Result<Vec<RemoteTip>, SyncError>` validates parent references and cycles, derives all tip IDs, and materializes each tip's snapshot.
- `plan_sync(baseline, local, remote_tips) -> Result<SyncPlan, SyncError>` computes one-sided changes, identical convergence, divergent tips, additions, and deletions. When cloud tips disagree on a path, retain every distinct cloud version with its source commit IDs instead of collapsing them into one remote value.

- [ ] **Step 1: Write table-driven planner tests**

Cover unchanged files; local-only and remote-only modifications; matching edits on both sides; different edits to one path; local and remote additions; local and remote deletions; delete-versus-edit; two remote commits published from the same parent; two cloud tips with different edits to the same path; local-versus-multiple-cloud variants with every candidate retained; missing parent IDs; and commit cycles.

- [ ] **Step 2: Run the planner tests and confirm the expected failures**

Run: `cargo test --lib sync::merge`
Expected: test compilation fails because `plan_sync` and planner types have not been implemented.

- [ ] **Step 3: Implement deterministic three-way planning**

Compare content hashes and tombstones against the baseline, not timestamps. Return conflicts sorted by `RelativePath`, retain each distinct remote candidate and its source commit IDs, and retain every remote tip as a merge parent when producing a resolution.

- [ ] **Step 4: Add merge ancestry tests and run the planner suite**

Assert both concurrent tip IDs are retained as parents and that a conflict-free merge does not create a conflict entry.

- [ ] **Step 5: Commit**

```bash
git add src/sync.rs src/sync/merge.rs
git commit -m "feat: plan three-way sync and conflicts"
```

### Task 3: Add Atomic Local Apply and Backup Preservation

**Files:**
- Create: `src/sync/apply.rs`
- Modify: `src/sync.rs`
- Test: `src/sync/apply.rs`

**Interfaces:**
- `StagedFile`: validated relative path, temporary path, expected SHA-256, and destination.
- `stage_remote_files(root, entries, read_object: impl Fn(&[u8; 32]) -> Result<Vec<u8>, SyncError>) -> Result<Vec<StagedFile>, SyncError>` downloads into a private staging directory and verifies each content hash before returning.
- `apply_staged_files(root, staged) -> Result<(), SyncError>` applies only the already-selected remote entries using same-filesystem temporary files and atomic rename; the coordinator constructs this staged set from automatic changes and conflict choices.
- `preserve_local_version(root, path, content_hash) -> Result<(), SyncError>` retains the replaced local version in `.touchHLE_sync/recovery/` before overwrite.

- [ ] **Step 1: Add tests for integrity and path safety**

Test that a hash mismatch leaves the destination unchanged, a malicious remote path is rejected, and a valid staged file is atomically installed.

- [ ] **Step 2: Run the apply tests and confirm the expected failures**

Run: `cargo test --lib sync::apply`
Expected: test compilation fails until the staging/apply API is present.

- [ ] **Step 3: Implement staging, verification, backup, and apply**

Do not write remote bytes directly to live destinations. Preserve the prior local bytes before replacing a file and keep deletion application reversible through the same history mechanism.

- [ ] **Step 4: Test partial failure behavior**

Inject a reader failure after one successful staged object and assert that no live destination or baseline has changed.

- [ ] **Step 5: Run focused tests and commit**

Run: `cargo test --lib sync::apply`

```bash
git add src/sync.rs src/sync/apply.rs
git commit -m "feat: safely apply and preserve synced files"
```

### Task 4: Define Remote Storage and Exercise the Version Protocol

**Files:**
- Create: `src/sync/store.rs`
- Create: `src/sync/engine.rs`
- Modify: `src/sync.rs`
- Test: `src/sync/store.rs`
- Test: `src/sync/engine.rs`

**Interfaces:**
- `RemoteStore`: `list_commits() -> Result<Vec<Commit>, SyncError>`, `read_object(hash: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError>`, `write_object(hash: [u8; 32], bytes: &[u8]) -> Result<(), SyncError>`, and `write_commit(commit: &Commit) -> Result<(), SyncError>`; commits and objects are addressed by immutable IDs/hashes.
- `MemoryRemoteStore`: deterministic in-memory fake implementing the same trait and supporting injected failures.
- `SyncEngine<S: RemoteStore>`: owns a store, local state path, and the two local roots.
- `SyncEngine::synchronize() -> Result<SyncOutcome, SyncError>` scans local files, discovers all remote tips, plans changes, uploads/verifies objects before commits, stages/applies remote changes, and persists the baseline only after success.
- `SyncOutcome`: `Offline`, `UpToDate`, `Applied`, `Published`, or `Conflicts(SyncPlan)` so the resolver retains snapshots and all remote merge-parent IDs.

- [ ] **Step 1: Add an in-memory fake store and protocol tests**

Test first publish, deduplicated identical objects, empty-local initial download, both-populated first connection with identical and differing same-path files, two concurrent commits, retry of an already written object, and preservation of both commit tips.

- [ ] **Step 2: Run engine tests and confirm they fail**

Run: `cargo test --lib sync::engine`
Expected: compilation fails until the store trait and engine are implemented.

- [ ] **Step 3: Implement the engine transaction order**

Write and verify all immutable objects before publishing a commit. For downloads, verify all staged files before local apply. Save `SyncState` only after remote and local work succeeds.

- [ ] **Step 4: Add failure-injection tests**

Make the fake store fail on object upload, commit publication, commit listing, and object download. Assert retries are idempotent, local files remain recoverable, and the baseline does not advance.

- [ ] **Step 5: Run the engine suite and commit**

Run: `cargo test --lib sync::engine`

```bash
git add src/sync.rs src/sync/store.rs src/sync/engine.rs
git commit -m "feat: add transactional sync engine"
```

### Task 5: Implement the Google Drive REST Adapter

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/sync/gdrive.rs`
- Modify: `src/sync.rs`
- Test: `src/sync/gdrive.rs`

**Interfaces:**
- `GoogleDriveStore`: implements `RemoteStore` using Drive v3 REST for the app-owned `touchHLE` folder.
- `GoogleDriveStore::connect(config: DriveConfig) -> Result<Self, SyncError>` receives an access-token provider and never logs token contents.
- Remote objects use `objects/<sha256>` and `commits/<uuid>.json`; validate paths returned by the provider before parsing or applying them.

- [ ] **Step 1: Verify the Drive v3 operations and reqwest target support**

Verify folder creation, paginated listing, metadata lookup, media download, multipart upload, resumable upload, and deletion on desktop and Android using `drive.file`.

- [ ] **Step 2: Preserve the existing adapter contract**

Keep `RemoteStore` unchanged. Cache folder IDs for the lifetime of the store, reuse a single HTTP connection pool, refresh once on HTTP 401, and do not blindly retry non-idempotent creates.

- [ ] **Step 3: Add HTTP-boundary adapter tests**

Assert pagination, safe request logging, bounded retry classification, token refresh, content-addressed object verification, and exact Drive upload endpoints.

- [ ] **Step 4: Implement the REST adapter**

Use multipart upload for small objects and resumable sessions for objects larger than 5 MiB. Read every new immutable object back and compare the full bytes before allowing its commit to publish.

- [ ] **Step 5: Run adapter checks and commit**

Run: `cargo test --lib sync::gdrive`

```bash
git add Cargo.toml Cargo.lock src/sync.rs src/sync/gdrive.rs
git commit -m "feat: add Google Drive sync store"
```

### Task 6: Add OAuth, Device Identity, and Protected Token Storage

**Files:**
- Create: `src/sync/auth.rs`
- Create: `src/sync/auth/desktop.rs`
- Create: `src/sync/auth/android.rs`
- Modify: `src/sync.rs`
- Modify: `android/app/src/main/java/org/touchhle/android/MainActivity.java`
- Modify: `android/app/src/main/AndroidManifest.xml`
- Test: `src/sync/auth.rs`

**Interfaces:**
- `TokenStore`: `load() -> Result<Option<TokenSet>, AuthError>`, `save(TokenSet)`, and `clear()`, with implementations backed by the platform's protected credential facility.
- `TokenSet`: access token, refresh token, and expiry; redact all token fields from `Debug`.
- `AccessTokenProvider::access_token() -> Result<SecretString, AuthError>` reuses the cached access token until Google Drive rejects it, then refreshes through OAuth and retries the operation once; never expose token material through `Debug`.
- `DriveConfig` receives the platform's public OAuth client ID; do not add a client secret to the repository or command-line arguments.
- `authorize_interactively() -> Result<TokenSet, AuthError>` uses system-browser OAuth authorization code with PKCE and requests `drive.file`.
- Headless authorization reads existing stored credentials and returns a non-interactive error if none exist.
- Each installation persists a stable random `device_id` in sync state; it is not derived from a username or credential.

- [ ] **Step 1: Test token redaction and headless no-prompt behavior**

Assert formatted debug output contains neither access nor refresh token and that missing credentials in headless mode return an actionable error without opening a browser.

- [ ] **Step 2: Implement desktop protected storage, token refresh, and PKCE callback**

Use the operating system's secure credential store and a loopback callback bound to localhost. Validate state and PKCE verifier before exchanging the authorization code.

- [ ] **Step 3: Implement Android browser return, token refresh, and Android Keystore-backed token storage**

Use an app-owned callback URI, validate OAuth state, and ensure authorization return is delivered to the active SDL activity without placing tokens in intents exposed to other applications.

- [ ] **Step 4: Test refresh, expiry, cancellation, and storage failures**

Use a fake OAuth endpoint/token store for deterministic tests. Confirm cancellation and unavailable secure storage leave local-only operation available.

- [ ] **Step 5: Run auth tests and commit**

Run: `cargo test --lib sync::auth`

```bash
git add src/sync.rs src/sync/auth.rs src/sync/auth/desktop.rs src/sync/auth/android.rs android/app/src/main/java/org/touchhle/android/MainActivity.java android/app/src/main/AndroidManifest.xml
git commit -m "feat: add secure Google OAuth authentication"
```

### Task 7: Add Sync Configuration and User-Visible Status

**Files:**
- Modify: `src/environment/app_picker.rs`
- Create: `src/sync/coordinator.rs`
- Modify: `src/sync.rs`
- Test: `src/sync/coordinator.rs`

**Interfaces:**
- `SyncSettings`: stores the user's local opt-in state and account-connected status beneath `.touchHLE_sync/`; it is not synchronized with game data.
- `SyncMode`: `Disabled`, `Enabled`, or `Headless`; headless mode honors previously stored opt-in and credentials but never starts interactive authorization.
- `SyncCoordinator::connect_account() -> Result<(), AuthError>` starts browser authorization and stores the resulting tokens using the platform adapter.
- The app picker exposes Connect, Enable/Disable, and current sync status; local execution remains available if authorization is canceled or unavailable.
- `SyncCoordinator::before_launch(mode: SyncMode) -> Result<PreLaunch, SyncError>` returns `Continue`, `NeedsResolution(SyncPlan)`, or `LocalOnly(reason)`; `SyncMode::Headless` carries headless behavior without a duplicate boolean, and the full plan preserves every remote candidate.
- `SyncCoordinator::after_exit() -> Result<SyncOutcome, SyncError>` snapshots and publishes local changes without preventing exit on offline/provider failure.

- [ ] **Step 1: Add tests for persisted opt-in and account state**

Test default-disabled behavior, enabling/disabling without losing device ID or baseline, and account cancellation without changing local files.

- [ ] **Step 2: Implement sync-settings persistence and coordinator orchestration**

Keep local-only operation as the default until the user connects an account and opts in. Preserve pending state when offline or credentials are missing. Reuse the protected token adapter from Task 6.

- [ ] **Step 3: Add app-picker controls for account connection and sync status**

Show disconnected, disabled, syncing, last-synced, and offline/pending states. The Connect action starts PKCE authorization; cancellation returns to the app picker without making cloud access mandatory.

- [ ] **Step 4: Add coordinator tests**

Use `MemoryRemoteStore` to test offline startup/shutdown, unconfigured local-only mode, successful pre-launch convergence, and a pre-launch conflict response.

- [ ] **Step 5: Run tests and commit**

Run: `cargo test --lib sync::coordinator`

```bash
git add src/environment/app_picker.rs src/sync.rs src/sync/coordinator.rs
git commit -m "feat: add sync opt-in and coordinator"
```

### Task 8: Integrate Pre-Launch Sync and Headless Conflict Behavior

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/environment/app_picker.rs`
- Modify: `src/sync/coordinator.rs`
- Test: `src/sync/coordinator.rs`

**Interfaces:**
- Resolve the selected app's local path only after startup sync has applied non-conflicting remote changes.
- Graphical launches present pre-launch conflicts to the resolver before constructing the guest `Environment`.
- Headless launches return a non-zero error before guest execution when conflicts require user input; print the graphical-mode resolution instruction.
- Run local apply before guest construction, with no other process writing either synchronized root.

- [ ] **Step 1: Add launch-flow tests around a fake coordinator**

Assert the guest environment is not constructed before pre-launch sync completes and that a headless conflict exits before guest execution.

- [ ] **Step 2: Integrate sync before bundle opening and guest construction**

Run sync after command-line parsing and app selection are known, but before opening mutable guest state. Do not delay `--help`, `--copyright`, or `--info` with cloud access.

- [ ] **Step 3: Verify offline execution continues**

Add a test where provider access fails before launch and confirm the local app remains launchable with pending changes retained.

- [ ] **Step 4: Run launch-flow tests and commit**

Run: `cargo test --lib sync::coordinator`

```bash
git add src/lib.rs src/environment/app_picker.rs src/sync/coordinator.rs
git commit -m "feat: sync before guest launch"
```

### Task 9: Route Normal Guest Exit Through Shutdown Sync

**Files:**
- Modify: `src/environment.rs`
- Modify: `src/frameworks/uikit/ui_application.rs`
- Modify: `src/libc/stdlib.rs`
- Modify: `src/environment/app_picker.rs`
- Modify: `src/lib.rs`
- Test: `src/environment.rs`
- Test: `src/sync/coordinator.rs`

**Interfaces:**
- Replace normal guest `std::process::exit` calls with an `Environment` exit request carrying an exit code.
- Make `Environment::run` return the requested exit code after the guest coroutine and teardown complete.
- Have `src/lib.rs::main` invoke `after_exit()` exactly once before returning to the binary entry point. If shutdown discovers conflicts, open the resolver in a fresh app-picker `Environment` after the guest environment returns but before `main` completes.
- Apply shutdown changes only after guest teardown and while no other process writes the synchronized roots.
- Keep fatal-abort paths distinct; never run guest lifecycle callbacks twice to trigger synchronization.

- [ ] **Step 1: Add tests for guest exit request propagation**

Test that UIKit termination and guest libc `exit()` each produce one host exit request and that ordinary coroutine return also completes without process termination inside the environment.

- [ ] **Step 2: Refactor normal guest exit paths to return control**

Preserve existing termination notifications and `NSUserDefaults` synchronization before setting the exit request. Ensure `Environment::run` exits its event loop without re-entering guest callbacks.

- [ ] **Step 3: Invoke shutdown sync once from the host entry flow**

On successful guest startup, call `after_exit()` for normal termination. Log sync failure without discarding local state or changing the guest's exit code.

- [ ] **Step 4: Add tests for offline, failure, and concurrent-shutdown outcomes**

Verify offline shutdown keeps local state pending, failed upload does not advance the baseline, and remote divergence preserves both commit tips for later graphical resolution.

- [ ] **Step 5: Run lifecycle tests and commit**

Run: `cargo test --lib sync::coordinator`

```bash
git add src/environment.rs src/frameworks/uikit/ui_application.rs src/libc/stdlib.rs src/environment/app_picker.rs src/lib.rs
git commit -m "feat: sync after guest exit"
```

### Task 10: Build Per-File Conflict Resolution UI

**Files:**
- Modify: `src/environment/app_picker.rs`
- Modify: `src/sync/coordinator.rs`
- Test: `src/sync/coordinator.rs`

**Interfaces:**
- `ConflictChoice`: `{ path: RelativePath, selected: LocalOrRemote, remote_commit_id: Option<Uuid> }`.
- `resolve_conflicts(plan: SyncPlan) -> Result<Vec<ConflictChoice>, SyncError>` displays the relative path, local state, and every distinct cloud candidate with size and modification time; one explicit choice is returned per path and a cloud choice names its source commit. It is callable both before launch and after guest shutdown, with shutdown conflicts presented before `main` returns.
- Validate all choices together through `SyncPlan::resolved_snapshot` before staging, applying, or committing. If structurally incompatible path choices produce an invalid tree, keep the files untouched and ask for a compatible selection.
- A completed selection creates a merge commit with all resolved remote tip IDs as parents. The unselected object remains stored and recoverable.
- `restore_version(path, commit_id) -> Result<(), SyncError>` lets the user restore a prior immutable version from the history view.
- GUI construction remains on the existing app-picker thread; hashing and Drive transfers remain on a worker thread and report progress through a channel.

- [ ] **Step 1: Add resolver model tests**

Test exactly one choice per path, selection of a specific candidate among multiple divergent cloud versions, preservation of every unselected hash, a merge commit whose parent list includes every remote tip, and restoration of a selected historical object.

- [ ] **Step 2: Add the per-file UI to the app picker**

Use existing UIKit-like controls and event polling. Display size in bytes and modification time in a stable local-time presentation for local and every cloud candidate; represent missing versions as “deleted”. Add a history view that can list retained versions for a path and invoke `restore_version`.

- [ ] **Step 3: Test cancel and partial-resolution behavior**

Cancel must leave local files, remote commits, and baseline unchanged. Applying choices must not drop unresolved conflicts or overwrite before verified staging succeeds. A set of individually valid but structurally incompatible choices must be rejected before any remote read or local write.

- [ ] **Step 4: Run resolver tests and commit**

Run: `cargo test --lib sync::coordinator`

```bash
git add src/environment/app_picker.rs src/sync/coordinator.rs
git commit -m "feat: resolve per-file sync conflicts"
```

### Task 11: Document Setup, Offline Behavior, and Recovery

**Files:**
- Modify: `README.md`
- Create: `dev-docs/google-drive-sync.md`
- Test: documentation review

- [ ] **Step 1: Document account connection and opt-in**

Explain supported platforms, Google account authorization with the `drive.file` scope, one-time reauthorization after upgrading old credentials, the public OAuth client setup/consent requirements, and the exact synchronized directories. Do not publish client secrets or tokens.

- [ ] **Step 2: Document offline, headless, and conflict behavior**

Explain that offline launches continue locally; headless mode stops on a pre-launch conflict; shutdown divergence is retained for later GUI resolution; and every non-selected version remains recoverable.

- [ ] **Step 3: Document storage growth and recovery**

Explain that v1 never prunes cloud history, content is deduplicated by SHA-256, and users can restore a preserved version from the conflict/history UI.

- [ ] **Step 4: Review the docs against the approved spec and commit**

```bash
git add README.md dev-docs/google-drive-sync.md
git commit -m "docs: explain Google Drive sync behavior"
```

### Task 12: Run Platform and Regression Validation

**Files:**
- Modify: CI workflow files only if required to add deterministic sync tests.
- Test: Rust unit tests, desktop compile, Android compile, and manual OAuth/conflict smoke tests.

- [ ] **Step 1: Run sync-focused and library tests**

Run: `cargo test --lib sync::`
Expected: all deterministic model, planner, apply, store, auth, and coordinator tests pass.

- [ ] **Step 2: Run the full library test suite**

Run: `cargo test --lib`
Expected: pass once the checkout has its required vendored files and compatible CMake setup. The baseline attempt on 2026-10-01 failed before tests because SDL2's bundled CMake rejects the installed CMake policy version and `vendor/stb/stb_image.h` is absent.

- [ ] **Step 3: Build desktop and Android targets**

Run the repository's documented desktop build and `cargo ndk -t arm64-v8a build` for Android. Record missing SDK/NDK or dependency prerequisites without weakening platform support.

- [ ] **Step 4: Run existing emulator integration tests only when their prerequisites are present**

The `tests/integration.rs` test deletes and recreates `tests/stubs`; do not run it unless that path is confirmed disposable and the test SDK/compiler prerequisites exist.

- [ ] **Step 5: Perform a manual end-to-end smoke test**

Verify first connection, app upload/download, offline launch and return online, two-client divergent save, per-file choice, unselected-version restoration, headless conflict refusal, and Android token persistence.

- [ ] **Step 6: Review the complete diff and commit any final fixes**

Run `git diff --check`, inspect `git status --short`, and ensure no credentials, tokens, or live account artifacts were added.
