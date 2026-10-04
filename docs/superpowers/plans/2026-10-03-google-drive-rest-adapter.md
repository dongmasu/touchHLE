# Google Drive REST Adapter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Replace OpenDAL's Google Drive service with direct Drive REST calls and reduce OAuth access to `drive.file` without changing the sync engine or `RemoteStore` protocol.

**Architecture:** Keep token-source mechanics and the sync engine intact, but version scope grants so old credentials are reauthorized once. Implement a reusable blocking HTTP client, Drive file-ID/folder operations, small multipart and large resumable uploads, bounded retries, authentication refresh, and upload read-back verification in the Google Drive adapter.

**Tech Stack:** Rust, reqwest blocking client, Google Drive API v3, existing serde/SHA-256 types.

**Spec:** `docs/superpowers/specs/2026-10-01-google-drive-sync-design.md`

## Global Constraints

- Keep `RemoteStore` and sync-engine behavior unchanged.
- Store only inside the app-owned `touchHLE` folder.
- Request only `https://www.googleapis.com/auth/drive.file`; force reauthorization for prior scope versions.
- Preserve immutable SHA-256 objects, commit validation, pending-file cleanup, and read-back verification before publishing commits.
- Never log access tokens, file names, paths, query values, or response bodies.
- Reuse one HTTP client and connection pool for the lifetime of the store.
- Keep retries bounded and avoid blindly retrying non-idempotent file creation.

---

### Task 1: Replace the OpenDAL Adapter

**Files:**
- Modify: `src/sync/gdrive.rs`
- Test: `src/sync/gdrive.rs`

- [ ] Implement Drive v3 listing, metadata, media download, multipart upload, resumable upload, folder creation, and deletion using one reusable reqwest client.
- [ ] Preserve paginated commit listing, parallel commit reads/object writes, 401 refresh, pending recursion, and verified immutable writes.
- [ ] Switch desktop and Android OAuth requests to `drive.file`, bump the stored scope version, and reject old credentials until interactive reauthorization.
- [ ] Add deterministic HTTP-boundary tests for query pagination, error classification, retry limits, upload selection, auth refresh, and integrity failures.
- [ ] Run focused sync adapter and auth tests.

### Task 2: Remove OpenDAL and Align Documentation

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `docs/superpowers/specs/2026-10-01-google-drive-sync-design.md`
- Modify: `docs/superpowers/plans/2026-10-01-google-drive-sync.md`

- [ ] Replace OpenDAL with the reqwest version already used by OAuth.
- [ ] Update only provider-specific design and plan references; retain unrelated existing edits.
- [ ] Confirm no OpenDAL references remain in the active dependency graph or sync implementation.

### Task 3: Verify and Compare

- [ ] Run focused tests, all library tests, Android compilation, and a release build.
- [ ] Run the desktop app and capture privacy-safe Drive request counts and timings.
- [ ] Compare startup and shutdown against the prior instrumented baseline; report any remaining network or Keychain delay separately.
