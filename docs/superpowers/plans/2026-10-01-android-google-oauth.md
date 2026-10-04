# Android Google OAuth Implementation Plan

> **For agentic workers:** Execute inline in this session; keep each task independently testable.

**Goal:** Replace the unsupported Android browser custom-scheme OAuth flow with Google Identity Services authorization while keeping desktop OAuth unchanged.

**Architecture:** Android obtains short-lived Drive access tokens through `AuthorizationClient`; Rust requests authorization through a JNI bridge and stores tokens in the existing platform-protected store. Desktop continues using its loopback callback and refresh token.

**Tech Stack:** Rust, JNI, Java, Android Gradle, Google Play services Identity authorization API.

**Spec:** `docs/superpowers/specs/2026-10-01-google-drive-sync-design.md` (update Android authentication assumptions as implementation proceeds).

## Global Constraints

- Keep OpenDAL as the Drive provider.
- Do not read, extract, or print signing credentials or certificate fingerprints.
- Keep desktop OAuth behavior unchanged.
- Preserve local-only operation when Google authorization is unavailable.
- Never log access tokens.

---

### Task 1: Separate Android Access Tokens From Desktop Refresh Tokens

**Files:**
- Modify: `src/sync/auth.rs`
- Modify: `src/sync/auth/android.rs`
- Test: `src/sync/auth.rs`
- Test: `src/sync/auth/android.rs`

- [ ] Make stored refresh tokens optional so Android can persist access tokens without pretending to possess a server refresh token.
- [ ] Keep desktop token exchange and refresh behavior unchanged.
- [ ] On Android, request a fresh access token through the Android authorization bridge when the stored token expires.
- [ ] Test Android expiry handling, desktop refresh-token persistence compatibility, and secret redaction.

### Task 2: Replace the Custom URI With Google Identity Services

**Files:**
- Modify: `android/app/build.gradle.kts`
- Modify: `android/app/src/main/java/org/touchhle/android/MainActivity.java`
- Modify: `android/app/src/main/AndroidManifest.xml`
- Modify: `src/sync/auth/android.rs`
- Modify: `src/sync/auth.rs`

- [ ] Add the official Google Play services authorization dependency.
- [ ] Start `AuthorizationClient.authorize()` from the activity, launch its pending intent when consent is required, and return only the access token or a sanitized error to Rust.
- [ ] Add a request-ID keyed JNI callback registry so an interactive picker request and a blocking token renewal cannot consume one another's result.
- [ ] Remove the custom URI intent filter and browser redirect/token-exchange code from Android.
- [ ] Test JNI result parsing and callback URI-independent authorization result delivery.

### Task 3: Align Availability, Documentation, and Builds

**Files:**
- Modify: `src/environment/app_picker.rs`
- Modify: `dev-docs/google-drive-sync.md`
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-10-01-google-drive-sync-design.md`

- [ ] Show Android authorization as available without embedding an Android client ID; retain the desktop client-ID build requirement.
- [ ] Explain that Google Cloud's Android OAuth client is registered with the package name and SHA-1 for the actual app-signing certificate; do not inspect signing material.
- [ ] Document Play services availability and the Android SDK-managed access-token renewal behavior.
- [ ] Run formatting, Rust tests, and the Android debug build; report any OAuth Console setup needed for a live-device test.
