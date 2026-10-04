/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
#[cfg(not(target_os = "android"))]
use oauth2::TokenResponse;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "android")]
pub mod android;
#[cfg(not(target_os = "android"))]
pub mod desktop;

#[cfg(target_os = "android")]
pub use android::PendingAuthorization;
#[cfg(not(target_os = "android"))]
pub use desktop::PendingAuthorization;

const KEYRING_SERVICE: &str = "org.touchhle.google-drive-sync";
const KEYRING_ACCOUNT: &str = "default";
#[cfg_attr(target_os = "android", allow(dead_code))]
pub const GOOGLE_DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
const DRIVE_FILE_SCOPE_VERSION: u8 = 2;
const TOKEN_REFRESH_SKEW_SECONDS: u64 = 60;

type TokenCache = Mutex<Option<Option<TokenSet>>>;

fn platform_token_cache() -> &'static TokenCache {
    static CACHE: OnceLock<TokenCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn cached_token_load(
    cache: &TokenCache,
    load: impl FnOnce() -> Result<Option<TokenSet>, AuthError>,
) -> Result<Option<TokenSet>, AuthError> {
    let started = std::time::Instant::now();
    let mut cached = cache.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(tokens) = cached.as_ref() {
        log!(
            "Secure credential cache hit in {} ms",
            started.elapsed().as_millis()
        );
        return Ok(tokens.clone());
    }
    log!("Secure credential cache miss; loading platform credential store");
    let tokens = load()?;
    *cached = Some(tokens.clone());
    log!(
        "Platform credential store load completed in {} ms",
        started.elapsed().as_millis()
    );
    Ok(tokens)
}

fn cached_token_save(
    cache: &TokenCache,
    tokens: &TokenSet,
    save: impl FnOnce() -> Result<(), AuthError>,
) -> Result<(), AuthError> {
    save()?;
    *cache.lock().unwrap_or_else(|error| error.into_inner()) = Some(Some(tokens.clone()));
    Ok(())
}

fn cache_platform_token(tokens: &TokenSet) {
    *platform_token_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(Some(tokens.clone()));
}

fn save_refreshed_tokens<S: TokenStore>(
    store: &mut S,
    previous_refresh_token: Option<&str>,
    refreshed: &TokenSet,
) -> Result<(), AuthError> {
    if refreshed.refresh_token() == previous_refresh_token {
        store.cache(refreshed)
    } else {
        store.save(refreshed)
    }
}

#[cfg(test)]
fn invalidate_platform_token_cache() {
    *platform_token_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = None;
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TokenSet {
    access_token: String,
    refresh_token: Option<String>,
    expires_unix_seconds: u64,
    #[serde(default)]
    authorization_scope_version: u8,
}

impl TokenSet {
    #[cfg_attr(target_os = "android", allow(dead_code))]
    pub fn new(access_token: String, refresh_token: String, expires_unix_seconds: u64) -> Self {
        Self {
            access_token,
            refresh_token: Some(refresh_token),
            expires_unix_seconds,
            authorization_scope_version: DRIVE_FILE_SCOPE_VERSION,
        }
    }

    #[cfg(target_os = "android")]
    pub fn new_android(access_token: String, expires_unix_seconds: u64) -> Self {
        Self {
            access_token,
            refresh_token: None,
            expires_unix_seconds,
            authorization_scope_version: DRIVE_FILE_SCOPE_VERSION,
        }
    }

    fn supports_current_scope(&self) -> bool {
        self.authorization_scope_version == DRIVE_FILE_SCOPE_VERSION
    }

    #[cfg(test)]
    pub fn expires_unix_seconds(&self) -> u64 {
        self.expires_unix_seconds
    }

    fn access_token(&self) -> &str {
        &self.access_token
    }

    #[cfg_attr(target_os = "android", allow(dead_code))]
    fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_unix_seconds", &self.expires_unix_seconds)
            .finish()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AuthError {
    MissingCredentials,
    ReauthorizationRequired,
    ForegroundRequired,
    Storage(String),
    OAuth(String),
    InvalidResponse,
    Cancelled,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCredentials => f.write_str("Google Drive is not connected"),
            Self::ReauthorizationRequired => {
                f.write_str("Reconnect Google Drive to grant access to touchHLE sync files")
            }
            Self::ForegroundRequired => {
                f.write_str("Google authorization requires foreground approval")
            }
            Self::Storage(message) => write!(f, "secure credential storage failed: {message}"),
            Self::OAuth(message) => write!(f, "Google authorization failed: {message}"),
            Self::InvalidResponse => f.write_str("Google returned an incomplete token response"),
            Self::Cancelled => f.write_str("Google authorization was cancelled"),
        }
    }
}

impl std::error::Error for AuthError {}

pub trait TokenStore: Send {
    fn load(&mut self) -> Result<Option<TokenSet>, AuthError>;
    fn save(&mut self, tokens: &TokenSet) -> Result<(), AuthError>;
    fn cache(&mut self, tokens: &TokenSet) -> Result<(), AuthError> {
        self.save(tokens)
    }
    #[cfg(test)]
    fn clear(&mut self) -> Result<(), AuthError>;
}

pub fn configured_client_id() -> Option<&'static str> {
    option_env!("TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_ID")
        .map(str::trim)
        .filter(|client_id| !client_id.is_empty())
}

#[cfg(not(target_os = "android"))]
pub fn configured_client_secret() -> Option<String> {
    std::env::var("TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_SECRET")
        .ok()
        .filter(|client_secret| !client_secret.trim().is_empty())
}

pub fn authorization_available() -> bool {
    #[cfg(target_os = "android")]
    {
        true
    }
    #[cfg(not(target_os = "android"))]
    {
        configured_client_id().is_some()
    }
}

#[cfg(not(target_os = "android"))]
pub fn start_authorization(
    client_id: String,
    open_browser: impl FnOnce(&str) -> Result<(), String>,
) -> Result<PendingAuthorization, AuthError> {
    desktop::start_authorization(client_id, configured_client_secret(), open_browser)
}

#[cfg(target_os = "android")]
pub fn start_authorization(
    client_id: String,
    open_browser: impl FnOnce(&str) -> Result<(), String>,
) -> Result<PendingAuthorization, AuthError> {
    android::start_authorization(client_id, open_browser)
}

pub fn save_authorized_tokens(tokens: &TokenSet) -> Result<(), AuthError> {
    #[cfg(target_os = "android")]
    {
        let tokens = tokens.clone();
        return run_android_keyring_operation(move || PlatformTokenStore.save(&tokens));
    }
    #[cfg(not(target_os = "android"))]
    {
        PlatformTokenStore.save(tokens)
    }
}

pub fn has_saved_credentials() -> Result<bool, AuthError> {
    #[cfg(target_os = "android")]
    {
        return run_android_keyring_operation(|| {
            Ok(PlatformTokenStore
                .load()?
                .is_some_and(|tokens| tokens.supports_current_scope()))
        });
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(PlatformTokenStore
            .load()?
            .is_some_and(|tokens| tokens.supports_current_scope()))
    }
}

#[cfg(target_os = "android")]
fn run_android_keyring_operation<T>(
    operation: impl FnOnce() -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError>
where
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name("touchHLE-android-keyring".into())
        .stack_size(4 * 1024 * 1024)
        .spawn(operation)
        .map_err(|error| {
            AuthError::Storage(format!("could not start Android keyring worker: {error}"))
        })?
        .join()
        .map_err(|_| AuthError::Storage("Android keyring worker stopped unexpectedly".into()))?
}

pub struct PlatformTokenStore;

#[cfg(not(target_os = "android"))]
impl TokenStore for PlatformTokenStore {
    fn load(&mut self) -> Result<Option<TokenSet>, AuthError> {
        cached_token_load(platform_token_cache(), || {
            let entry_started = std::time::Instant::now();
            let entry = keyring_entry()?;
            log!(
                "macOS Keychain entry initialization completed in {} ms",
                entry_started.elapsed().as_millis()
            );
            static READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let read_number = READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let read_started = std::time::Instant::now();
            let result = entry.get_password();
            log!(
                "macOS Keychain get_password call {read_number} completed in {} ms ({})",
                read_started.elapsed().as_millis(),
                match &result {
                    Ok(_) => "credential found",
                    Err(keyring::Error::NoEntry) => "credential absent",
                    Err(_) => "credential read failed",
                }
            );
            match result {
                Ok(value) => serde_json::from_str(&value)
                    .map(Some)
                    .map_err(|_| AuthError::Storage("saved credential data is invalid".into())),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(AuthError::Storage(error.to_string())),
            }
        })
    }

    fn save(&mut self, tokens: &TokenSet) -> Result<(), AuthError> {
        let serialized = serde_json::to_string(tokens)
            .map_err(|_| AuthError::Storage("could not encode credential data".into()))?;
        cached_token_save(platform_token_cache(), tokens, || {
            let started = std::time::Instant::now();
            keyring_entry()?
                .set_password(&serialized)
                .map_err(|error| AuthError::Storage(error.to_string()))?;
            log!(
                "macOS Keychain set_password completed in {} ms",
                started.elapsed().as_millis()
            );
            Ok(())
        })
    }

    fn cache(&mut self, tokens: &TokenSet) -> Result<(), AuthError> {
        cache_platform_token(tokens);
        Ok(())
    }

    #[cfg(test)]
    fn clear(&mut self) -> Result<(), AuthError> {
        let result = match keyring_entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(AuthError::Storage(error.to_string())),
        };
        invalidate_platform_token_cache();
        result
    }
}

#[cfg(not(target_os = "android"))]
fn keyring_entry() -> Result<keyring::Entry, AuthError> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .map_err(|error| AuthError::Storage(error.to_string()))
}

#[cfg(target_os = "android")]
impl TokenStore for PlatformTokenStore {
    fn load(&mut self) -> Result<Option<TokenSet>, AuthError> {
        cached_token_load(platform_token_cache(), || {
            match android_keyring_entry()?.get_password() {
                Ok(value) => serde_json::from_str(&value)
                    .map(Some)
                    .map_err(|_| AuthError::Storage("saved credential data is invalid".into())),
                Err(keyring_core::Error::NoEntry) => Ok(None),
                Err(error) => Err(AuthError::Storage(error.to_string())),
            }
        })
    }

    fn save(&mut self, tokens: &TokenSet) -> Result<(), AuthError> {
        let serialized = serde_json::to_string(tokens)
            .map_err(|_| AuthError::Storage("could not encode credential data".into()))?;
        cached_token_save(platform_token_cache(), tokens, || {
            android_keyring_entry()?
                .set_password(&serialized)
                .map_err(|error| AuthError::Storage(error.to_string()))
        })
    }

    fn cache(&mut self, tokens: &TokenSet) -> Result<(), AuthError> {
        cache_platform_token(tokens);
        Ok(())
    }

    #[cfg(test)]
    fn clear(&mut self) -> Result<(), AuthError> {
        let result = match android_keyring_entry()?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(AuthError::Storage(error.to_string())),
        };
        invalidate_platform_token_cache();
        result
    }
}

#[cfg(target_os = "android")]
fn android_keyring_entry() -> Result<keyring_core::Entry, AuthError> {
    use std::sync::OnceLock;

    static STORE: OnceLock<Result<(), String>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            let store =
                android_native_keyring_store::Store::new().map_err(|error| error.to_string())?;
            keyring_core::set_default_store(store);
            Ok(())
        })
        .as_ref()
        .map_err(|error| AuthError::Storage(error.clone()))?;
    keyring_core::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .map_err(|error| AuthError::Storage(error.to_string()))
}

pub struct AccessTokenProvider<S: TokenStore> {
    store: S,
    #[cfg(not(target_os = "android"))]
    client_id: String,
    allow_interactive_authorization: bool,
}

impl<S: TokenStore> AccessTokenProvider<S> {
    pub fn new(store: S, client_id: impl Into<String>) -> Self {
        #[cfg(not(target_os = "android"))]
        let client_id = client_id.into();
        #[cfg(target_os = "android")]
        let _ = client_id;
        Self {
            store,
            #[cfg(not(target_os = "android"))]
            client_id: client_id.into(),
            allow_interactive_authorization: true,
        }
    }

    pub fn with_interactive_authorization(mut self, allowed: bool) -> Self {
        self.allow_interactive_authorization = allowed;
        self
    }

    fn access_token_for_request(&mut self, force_refresh: bool) -> Result<String, AuthError> {
        let tokens = self.store.load()?.ok_or(AuthError::MissingCredentials)?;
        if !tokens.supports_current_scope() {
            return Err(AuthError::ReauthorizationRequired);
        }
        let now = unix_seconds();
        let refresh_required = token_needs_refresh(&tokens, now, force_refresh);
        #[cfg(target_os = "android")]
        {
            if !refresh_required {
                return Ok(tokens.access_token().to_owned());
            }
            let tokens = android::request_access_token(self.allow_interactive_authorization)?;
            self.store.save(&tokens)?;
            return Ok(tokens.access_token().to_owned());
        }
        #[cfg(not(target_os = "android"))]
        {
            let mut tokens = tokens;
            if !refresh_required {
                return Ok(tokens.access_token().to_owned());
            }

            let refresh_token = tokens
                .refresh_token()
                .ok_or(AuthError::ReauthorizationRequired)?;
            let client =
                oauth2::basic::BasicClient::new(oauth2::ClientId::new(self.client_id.clone()))
                    .set_token_uri(
                        oauth2::TokenUrl::new("https://oauth2.googleapis.com/token".to_owned())
                            .map_err(|_| AuthError::OAuth("invalid token endpoint".into()))?,
                    );
            let client = if let Some(client_secret) = configured_client_secret() {
                client.set_client_secret(oauth2::ClientSecret::new(client_secret))
            } else {
                client
            };
            let http_client = oauth2::reqwest::blocking::ClientBuilder::new()
                .redirect(oauth2::reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| AuthError::OAuth("could not initialize the HTTPS client".into()))?;
            let response = client
                .exchange_refresh_token(&oauth2::RefreshToken::new(refresh_token.to_owned()))
                .request(&http_client)
                .map_err(|_| AuthError::OAuth("token refresh request failed".into()))?;

            let previous_refresh_token = tokens.refresh_token().map(str::to_owned);
            let refresh_token = response
                .refresh_token()
                .map(|token| token.secret().to_owned())
                .or_else(|| tokens.refresh_token().map(str::to_owned))
                .ok_or(AuthError::InvalidResponse)?;
            let expires_in = response
                .expires_in()
                .map_or(3600, |duration| duration.as_secs());
            tokens = TokenSet::new(
                response.access_token().secret().to_owned(),
                refresh_token,
                now.saturating_add(expires_in),
            );
            // Persist only durable credential rotation, not each short-lived access token.
            save_refreshed_tokens(&mut self.store, previous_refresh_token.as_deref(), &tokens)?;
            Ok(tokens.access_token().to_owned())
        }
    }
}

impl<S: TokenStore> super::gdrive::AccessTokenSource for AccessTokenProvider<S> {
    fn access_token(&mut self, force_refresh: bool) -> Result<String, super::model::SyncError> {
        self.access_token_for_request(force_refresh)
            .map_err(|error| super::model::SyncError::Authentication(error.to_string()))
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn token_needs_refresh(tokens: &TokenSet, now: u64, force_refresh: bool) -> bool {
    force_refresh || tokens.expires_unix_seconds <= now.saturating_add(TOKEN_REFRESH_SKEW_SECONDS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct MemoryTokenStore(Option<TokenSet>);

    impl TokenStore for MemoryTokenStore {
        fn load(&mut self) -> Result<Option<TokenSet>, AuthError> {
            Ok(self.0.clone())
        }

        fn save(&mut self, tokens: &TokenSet) -> Result<(), AuthError> {
            self.0 = Some(tokens.clone());
            Ok(())
        }

        fn clear(&mut self) -> Result<(), AuthError> {
            self.0 = None;
            Ok(())
        }
    }

    struct CountingTokenStore {
        tokens: TokenSet,
        loads: Arc<AtomicUsize>,
    }

    impl TokenStore for CountingTokenStore {
        fn load(&mut self) -> Result<Option<TokenSet>, AuthError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(Some(self.tokens.clone()))
        }

        fn save(&mut self, tokens: &TokenSet) -> Result<(), AuthError> {
            self.tokens = tokens.clone();
            Ok(())
        }

        fn clear(&mut self) -> Result<(), AuthError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RefreshWriteTrackingStore {
        saves: usize,
        cache_updates: usize,
    }

    impl TokenStore for RefreshWriteTrackingStore {
        fn load(&mut self) -> Result<Option<TokenSet>, AuthError> {
            Ok(None)
        }

        fn save(&mut self, _: &TokenSet) -> Result<(), AuthError> {
            self.saves += 1;
            Ok(())
        }

        fn cache(&mut self, _: &TokenSet) -> Result<(), AuthError> {
            self.cache_updates += 1;
            Ok(())
        }

        fn clear(&mut self) -> Result<(), AuthError> {
            Ok(())
        }
    }

    #[test]
    fn cached_drive_token_avoids_repeated_credential_reads() {
        let loads = Arc::new(AtomicUsize::new(0));
        let provider = AccessTokenProvider::new(
            CountingTokenStore {
                tokens: TokenSet::new(
                    "test-access".into(),
                    "test-refresh".into(),
                    unix_seconds().saturating_add(3600),
                ),
                loads: Arc::clone(&loads),
            },
            "client-id",
        );
        let mut store = super::super::gdrive::GoogleDriveStore::new(provider);
        store.token(false).unwrap();
        store.token(false).unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cached_platform_credentials_share_one_secure_store_read() {
        let cache = Mutex::new(None);
        let reads = AtomicUsize::new(0);
        let tokens = TokenSet::new(
            "test-access".into(),
            "test-refresh".into(),
            unix_seconds().saturating_add(3600),
        );

        for _ in 0..2 {
            let loaded = cached_token_load(&cache, || {
                reads.fetch_add(1, Ordering::SeqCst);
                Ok(Some(tokens.clone()))
            })
            .unwrap();
            assert!(loaded.is_some());
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1);

        let refreshed = TokenSet::new(
            "refreshed-access".into(),
            "test-refresh".into(),
            unix_seconds().saturating_add(7200),
        );
        cached_token_save(&cache, &refreshed, || Ok(())).unwrap();
        assert_eq!(
            cached_token_load(&cache, || panic!(
                "cached token should avoid another keyring read"
            ))
            .unwrap()
            .unwrap()
            .access_token(),
            "refreshed-access"
        );
    }

    #[test]
    fn refreshed_access_token_skips_secure_write_without_refresh_token_rotation() {
        let mut store = RefreshWriteTrackingStore::default();
        let old = TokenSet::new("old-access".into(), "stable-refresh".into(), 1);
        let refreshed = TokenSet::new("new-access".into(), "stable-refresh".into(), 2);

        save_refreshed_tokens(&mut store, old.refresh_token(), &refreshed).unwrap();

        assert_eq!(store.saves, 0);
        assert_eq!(store.cache_updates, 1);
    }

    #[test]
    fn rotated_refresh_token_is_persisted_securely() {
        let mut store = RefreshWriteTrackingStore::default();
        let old = TokenSet::new("old-access".into(), "old-refresh".into(), 1);
        let refreshed = TokenSet::new("new-access".into(), "new-refresh".into(), 2);

        save_refreshed_tokens(&mut store, old.refresh_token(), &refreshed).unwrap();

        assert_eq!(store.saves, 1);
        assert_eq!(store.cache_updates, 0);
    }

    #[test]
    fn token_debug_output_redacts_both_credentials() {
        let tokens = TokenSet::new("access-secret".into(), "refresh-secret".into(), 10);
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn oauth_requests_the_narrow_drive_file_scope() {
        assert_eq!(
            GOOGLE_DRIVE_SCOPE,
            "https://www.googleapis.com/auth/drive.file"
        );
        assert!(TokenSet::new("a".into(), "r".into(), 1).supports_current_scope());
    }

    #[test]
    fn legacy_scope_tokens_require_interactive_reauthorization() {
        let legacy_drive_file: TokenSet = serde_json::from_str(
            r#"{"access_token":"old-access","refresh_token":"old-refresh","expires_unix_seconds":4102444800}"#,
        )
        .unwrap();
        let old_full_drive: TokenSet = serde_json::from_str(
            r#"{"access_token":"old-access","refresh_token":"old-refresh","expires_unix_seconds":4102444800,"authorization_scope_version":1}"#,
        )
        .unwrap();

        for old_tokens in [legacy_drive_file, old_full_drive] {
            let mut provider =
                AccessTokenProvider::new(MemoryTokenStore(Some(old_tokens)), "client-id");
            assert_eq!(
                provider.access_token_for_request(false),
                Err(AuthError::ReauthorizationRequired)
            );
        }
    }

    #[test]
    fn absent_credentials_are_noninteractive_and_actionable() {
        let mut provider = AccessTokenProvider::new(MemoryTokenStore::default(), "client-id");
        assert_eq!(
            provider.access_token_for_request(false),
            Err(AuthError::MissingCredentials)
        );
    }

    #[test]
    fn cached_access_token_is_used_while_it_has_a_safe_lifetime() {
        let provider = AccessTokenProvider::new(
            MemoryTokenStore(Some(TokenSet::new(
                "current-access".into(),
                "refresh".into(),
                unix_seconds().saturating_add(TOKEN_REFRESH_SKEW_SECONDS + 60),
            ))),
            "client-id",
        );
        let mut provider = provider;

        assert_eq!(
            provider.access_token_for_request(false).unwrap(),
            "current-access"
        );
    }

    #[test]
    fn expired_or_near_expiry_tokens_are_refreshed_before_drive_requests() {
        let now = 1_000;
        let fresh = TokenSet::new("fresh".into(), "refresh".into(), now + 61);
        let near_expiry = TokenSet::new("near".into(), "refresh".into(), now + 60);
        let expired = TokenSet::new("expired".into(), "refresh".into(), now - 1);

        assert!(!token_needs_refresh(&fresh, now, false));
        assert!(token_needs_refresh(&near_expiry, now, false));
        assert!(token_needs_refresh(&expired, now, false));
        assert!(token_needs_refresh(&fresh, now, true));
    }

    #[test]
    fn token_store_round_trips_without_exposing_secrets_in_debug() {
        let mut store = MemoryTokenStore::default();
        let tokens = TokenSet::new("access-secret".into(), "refresh-secret".into(), 42);
        store.save(&tokens).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.expires_unix_seconds(), 42);
        assert_eq!(loaded.refresh_token(), Some("refresh-secret"));
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn previous_full_drive_tokens_remain_deserializable_but_need_reauthorization() {
        let saved: TokenSet = serde_json::from_str(
            r#"{"access_token":"access","refresh_token":"refresh","expires_unix_seconds":42,"authorization_scope_version":1}"#,
        )
        .unwrap();

        assert_eq!(saved.refresh_token(), Some("refresh"));
        assert!(!saved.supports_current_scope());
    }
}
