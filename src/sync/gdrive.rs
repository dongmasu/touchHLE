/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::commit_cache::{self, CachedCommit, CommitCache};
use super::model::{validate_entries, Commit, RelativePath, SnapshotEntry, SyncError};
use super::reconcile::{RemoteChange, RemoteChangeBatch, RemoteFile};
use super::store::{RemoteStore, MAX_PARALLEL_OBJECT_WRITES};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use unicode_casefold::UnicodeCaseFold;
use url::Url;
use uuid::Uuid;

const DRIVE_ROOT: &str = "touchHLE";
const OBJECTS_DIR: &str = "touchHLE/objects";
const COMMITS_DIR: &str = "touchHLE/commits";
const MANAGED_ROOT_DIR: &str = DRIVE_ROOT;
const MIGRATION_PROPERTY: &str = "touchhleFilesMigration";
const MIGRATION_COMPLETE: &str = "complete-v1";
const FILE_FIELDS: &str =
    "id,name,mimeType,parents,version,sha256Checksum,size,modifiedTime,trashed";
const DRIVE_API: &str = "https://www.googleapis.com/drive/v3";
const DRIVE_UPLOAD_API: &str = "https://www.googleapis.com/upload/drive/v3";
const MULTIPART_UPLOAD_LIMIT: usize = 5 * 1024 * 1024;
const RESUMABLE_CHUNK_SIZE: usize = 8 * 1024 * 1024;
const MAX_HTTP_ATTEMPTS: usize = 4;
const MAX_PARALLEL_COMMIT_READS: usize = MAX_PARALLEL_OBJECT_WRITES;

pub trait AccessTokenSource: Send {
    fn access_token(&mut self, force_refresh: bool) -> Result<String, SyncError>;
}

pub struct GoogleDriveStore<T: AccessTokenSource> {
    token_source: T,
    access_token: Option<String>,
    http: DriveHttp,
    folder_ids: HashMap<String, String>,
    known_files: HashMap<String, RelativePath>,
    commit_cache_root: Option<PathBuf>,
}

impl<T: AccessTokenSource> GoogleDriveStore<T> {
    pub fn new(token_source: T) -> Self {
        Self {
            token_source,
            access_token: None,
            http: DriveHttp::new(DRIVE_API, DRIVE_UPLOAD_API),
            folder_ids: HashMap::new(),
            known_files: HashMap::new(),
            commit_cache_root: None,
        }
    }

    pub(super) fn with_commit_cache(mut self, root: &Path) -> Self {
        self.commit_cache_root = Some(root.to_path_buf());
        self
    }

    pub(super) fn token(&mut self, force_refresh: bool) -> Result<String, SyncError> {
        if !force_refresh {
            if let Some(token) = &self.access_token {
                return Ok(token.clone());
            }
        }
        let started = Instant::now();
        let token = self.token_source.access_token(force_refresh)?;
        self.access_token = Some(token.clone());
        log!(
            "Google Drive access token {} completed in {} ms",
            if force_refresh { "refresh" } else { "load" },
            started.elapsed().as_millis()
        );
        Ok(token)
    }

    fn with_auth_retry<R>(
        &mut self,
        operation_name: &'static str,
        mut operation: impl FnMut(&mut Self) -> Result<R, SyncError>,
    ) -> Result<R, SyncError> {
        let started = Instant::now();
        match operation(self) {
            Ok(value) => {
                log!(
                    "Google Drive store operation {operation_name} completed in {} ms",
                    started.elapsed().as_millis()
                );
                Ok(value)
            }
            Err(error) if is_unauthorized_sync_error(&error) => {
                log!(
                    "Google Drive store operation {operation_name} received HTTP 401 after {} ms; refreshing token",
                    started.elapsed().as_millis()
                );
                self.access_token = None;
                self.token(true)?;
                let retry_started = Instant::now();
                let result = operation(self);
                log!(
                    "Google Drive store operation {operation_name} retry completed in {} ms ({})",
                    retry_started.elapsed().as_millis(),
                    if result.is_ok() { "ok" } else { "error" }
                );
                result
            }
            Err(error) => {
                log!(
                    "Google Drive store operation {operation_name} failed in {} ms ({})",
                    started.elapsed().as_millis(),
                    if matches!(error, SyncError::Authentication(_)) {
                        "authentication"
                    } else {
                        "provider or integrity"
                    }
                );
                Err(error)
            }
        }
    }

    fn find_folder(&mut self, token: &str, path: &str) -> Result<Option<String>, SyncError> {
        if let Some(id) = self.folder_ids.get(path) {
            return Ok(Some(id.clone()));
        }

        let (parent_id, name) = if path == DRIVE_ROOT {
            ("root".to_owned(), DRIVE_ROOT)
        } else {
            let (parent, name) = path
                .rsplit_once('/')
                .ok_or_else(|| SyncError::Integrity("invalid Google Drive folder key".into()))?;
            let Some(parent_id) = self.find_folder(token, parent)? else {
                return Ok(None);
            };
            (parent_id, name)
        };

        let matches = self.http.find_named_files(token, &parent_id, name)?;
        let Some(folder) = matches
            .into_iter()
            .find(|file| file.mime_type == DRIVE_FOLDER_MIME_TYPE)
        else {
            return Ok(None);
        };
        self.folder_ids.insert(path.to_owned(), folder.id.clone());
        Ok(Some(folder.id))
    }

    fn ensure_folder(&mut self, token: &str, path: &str) -> Result<String, SyncError> {
        if let Some(id) = self.find_folder(token, path)? {
            return Ok(id);
        }
        let (parent_id, name) = if path == DRIVE_ROOT {
            ("root".to_owned(), DRIVE_ROOT)
        } else {
            let (parent, name) = path
                .rsplit_once('/')
                .ok_or_else(|| SyncError::Integrity("invalid Google Drive folder key".into()))?;
            let parent_id = self.ensure_folder(token, parent)?;
            (parent_id, name)
        };
        let folder = self.http.create_folder(token, &parent_id, name)?;
        self.folder_ids.insert(path.to_owned(), folder.id.clone());
        Ok(folder.id)
    }

    fn find_file(
        &mut self,
        token: &str,
        folder_key: &str,
        name: &str,
    ) -> Result<Option<DriveFile>, SyncError> {
        let Some(parent_id) = self.find_folder(token, folder_key)? else {
            return Ok(None);
        };
        Ok(self
            .http
            .find_named_files(token, &parent_id, name)?
            .into_iter()
            .find(|file| file.mime_type != DRIVE_FOLDER_MIME_TYPE))
    }

    fn managed_folder_path(
        http: &DriveHttp,
        folder_ids: &mut HashMap<String, String>,
        token: &str,
        id: &str,
        fail_on_missing: bool,
        visited: &mut HashSet<String>,
        verified_ids: &mut HashSet<String>,
    ) -> Result<Option<String>, SyncError> {
        let cached_paths: Vec<_> = folder_ids
            .iter()
            .filter(|(_, known)| known.as_str() == id)
            .map(|(path, _)| path.clone())
            .collect();
        let cached_path = cached_paths.first();
        if let Some(path) = cached_path {
            if path.starts_with(&format!("{MANAGED_ROOT_DIR}/")) && !is_managed_folder(path) {
                return Ok(None);
            }
            if path == DRIVE_ROOT
                || path == &format!("{MANAGED_ROOT_DIR}/touchHLE_apps")
                || path == &format!("{MANAGED_ROOT_DIR}/touchHLE_sandbox")
                || verified_ids.contains(id)
            {
                return Ok(Some(path.clone()));
            }
        }
        if !visited.insert(id.to_owned()) {
            return Err(SyncError::Integrity("remote folder cycle".into()));
        }
        let folder = http.metadata(token, id)?;
        for path in &cached_paths {
            invalidate_folder_cache(folder_ids, path);
        }
        let folder = match folder {
            Some(folder) => folder,
            None if fail_on_missing
                || cached_paths
                    .iter()
                    .any(|path| path == DRIVE_ROOT || is_managed_folder(path)) =>
            {
                return Err(SyncError::Integrity(format!(
                    "Google Drive folder {id} is missing while resolving managed file ancestry"
                )));
            }
            None => return Ok(None),
        };
        if folder.trashed || folder.mime_type != DRIVE_FOLDER_MIME_TYPE {
            return Ok(None);
        }
        let Some(parent) = folder.parents.first() else {
            return Ok(None);
        };
        let Some(parent_path) = Self::managed_folder_path(
            http,
            folder_ids,
            token,
            parent,
            fail_on_missing,
            visited,
            verified_ids,
        )?
        else {
            return Ok(None);
        };
        let path = format!("{parent_path}/{}", folder.name);
        if is_managed_folder(&path) {
            validate_managed_folder(&path)?;
            ensure_folder_path_available(folder_ids, &path, id)?;
            folder_ids.insert(path.clone(), id.to_owned());
            verified_ids.insert(id.to_owned());
            Ok(Some(path))
        } else {
            Ok(None)
        }
    }

    fn remote_file(
        http: &DriveHttp,
        folder_ids: &mut HashMap<String, String>,
        token: &str,
        file: DriveFile,
        fail_on_missing_ancestry: bool,
        verified_ids: &mut HashSet<String>,
    ) -> Result<Option<RemoteFile>, SyncError> {
        if file.trashed
            || file.mime_type == DRIVE_FOLDER_MIME_TYPE
            || file.mime_type.starts_with("application/vnd.google-apps.")
        {
            return Ok(None);
        }
        let Some(parent) = file.parents.first().cloned() else {
            return Ok(None);
        };
        let Some(folder) = Self::managed_folder_path(
            http,
            folder_ids,
            token,
            &parent,
            fail_on_missing_ancestry,
            &mut HashSet::new(),
            verified_ids,
        )
        .map_err(|error| match error {
            SyncError::Integrity(message) => SyncError::Integrity(format!(
                "Google Drive file {} under parent {parent}: {message}",
                file.id
            )),
            error => error,
        })?
        else {
            return Ok(None);
        };
        let Some(relative) = folder.strip_prefix(&format!("{MANAGED_ROOT_DIR}/")) else {
            return Ok(None);
        };
        let path = RelativePath::new(&format!("{relative}/{}", file.name))?;
        Self::remote_file_at_path(file, path, Some(parent))
    }

    fn remote_file_at_path(
        file: DriveFile,
        path: RelativePath,
        parent_id: Option<String>,
    ) -> Result<Option<RemoteFile>, SyncError> {
        if file.trashed
            || file.mime_type == DRIVE_FOLDER_MIME_TYPE
            || file.mime_type.starts_with("application/vnd.google-apps.")
        {
            return Ok(None);
        }
        if path.as_str().rsplit('/').next() != Some(file.name.as_str()) {
            return Err(SyncError::Integrity(
                "managed Google Drive file name does not match its indexed path".into(),
            ));
        }
        let checksum = file.sha256_checksum.as_deref().ok_or_else(|| {
            SyncError::Integrity("managed Google Drive file missing SHA-256 checksum".into())
        })?;
        let hash = parse_sha256(checksum)?;
        let size = file
            .size
            .as_deref()
            .ok_or_else(|| SyncError::Integrity("managed Google Drive file missing size".into()))?
            .parse::<u64>()
            .map_err(|_| SyncError::Integrity("invalid Google Drive file size".into()))?;
        let version = file.version.filter(|v| !v.is_empty()).ok_or_else(|| {
            SyncError::Integrity("managed Google Drive file missing version".into())
        })?;
        let modified_unix_ms = file
            .modified_time
            .as_deref()
            .map(parse_drive_time)
            .transpose()?
            .unwrap_or(0);
        Ok(Some(RemoteFile {
            path,
            id: file.id,
            version,
            entry: SnapshotEntry::File {
                sha256: hash,
                size,
                modified_unix_ms,
            },
            parent_id,
        }))
    }
}

fn is_managed_folder(path: &str) -> bool {
    ["touchHLE_apps", "touchHLE_sandbox"].iter().any(|root| {
        let managed_root = format!("{MANAGED_ROOT_DIR}/{root}");
        path == managed_root || path.starts_with(&format!("{managed_root}/"))
    })
}

fn ensure_folder_path_available(
    folder_ids: &HashMap<String, String>,
    path: &str,
    id: &str,
) -> Result<(), SyncError> {
    let folded: String = path.case_fold().collect();
    if folder_ids.iter().any(|(existing, existing_id)| {
        existing_id != id && existing.as_str().case_fold().collect::<String>() == folded
    }) {
        return Err(SyncError::Integrity(
            "duplicate or case-colliding managed folder path".into(),
        ));
    }
    Ok(())
}

fn affected_by_folder_changes(
    before: &HashMap<String, String>,
    after: &HashMap<String, String>,
    known_files: &HashMap<String, RelativePath>,
    affected: &mut BTreeSet<String>,
) {
    for (path, id) in before {
        if after.get(path) == Some(id) {
            continue;
        }
        if let Some(prefix) = path.strip_prefix(&format!("{MANAGED_ROOT_DIR}/")) {
            for (file_id, file_path) in known_files {
                if file_path.as_str().starts_with(&format!("{prefix}/")) {
                    affected.insert(file_id.clone());
                }
            }
        }
    }
}

fn validate_managed_folder(path: &str) -> Result<(), SyncError> {
    let relative = path
        .strip_prefix(&format!("{MANAGED_ROOT_DIR}/"))
        .ok_or_else(|| SyncError::Integrity("folder outside managed tree".into()))?;
    RelativePath::new(&format!("{relative}/valid"))?;
    Ok(())
}

fn parse_sha256(text: &str) -> Result<[u8; 32], SyncError> {
    if text.len() != 64 {
        return Err(SyncError::Integrity("invalid Google Drive SHA-256".into()));
    }
    let mut hash = [0; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(pair)
            .map_err(|_| SyncError::Integrity("invalid Google Drive SHA-256".into()))?;
        hash[index] = u8::from_str_radix(pair, 16)
            .map_err(|_| SyncError::Integrity("invalid Google Drive SHA-256".into()))?;
    }
    Ok(hash)
}

fn parse_drive_time(value: &str) -> Result<i64, SyncError> {
    // Drive supplies RFC 3339 timestamps; accept its UTC form and numeric offsets.
    let invalid = || SyncError::Integrity("invalid Google Drive modifiedTime".into());
    let number = |range: std::ops::Range<usize>| -> Result<i64, SyncError> {
        value
            .get(range)
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())
    };
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    if value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(7) != Some(&b'-')
        || value.as_bytes().get(10) != Some(&b'T')
        || value.as_bytes().get(13) != Some(&b':')
        || value.as_bytes().get(16) != Some(&b':')
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err(invalid());
    }
    let tail = value.get(19..).ok_or_else(invalid)?;
    let (fraction, zone) = if let Some(rest) = tail.strip_prefix('.') {
        let count = rest.bytes().take_while(u8::is_ascii_digit).count();
        if count == 0 {
            return Err(invalid());
        }
        (&rest[..count], &rest[count..])
    } else {
        ("", tail)
    };
    let millis = fraction.chars().take(3).collect::<String>();
    let millis = if millis.is_empty() {
        0
    } else {
        format!("{millis:0<3}")
            .parse::<i64>()
            .map_err(|_| invalid())?
    };
    let offset_minutes = if zone == "Z" {
        0
    } else {
        let sign = match zone.as_bytes().first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return Err(invalid()),
        };
        if zone.len() != 6 || zone.as_bytes()[3] != b':' {
            return Err(invalid());
        }
        let hours = zone
            .get(1..3)
            .ok_or_else(invalid)?
            .parse::<i64>()
            .map_err(|_| invalid())?;
        let minutes = zone
            .get(4..6)
            .ok_or_else(invalid)?
            .parse::<i64>()
            .map_err(|_| invalid())?;
        if hours > 23 || minutes > 59 {
            return Err(invalid());
        }
        sign * (hours * 60 + minutes)
    };
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let year_of_era_days = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146097 + year_of_era_days - 719468;
    Ok(((days * 24 + hour) * 60 + minute - offset_minutes) * 60_000 + second * 1000 + millis)
}

const DRIVE_FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";

#[derive(Clone)]
struct DriveHttp {
    client: Client,
    api_base: String,
    upload_base: String,
}

impl DriveHttp {
    fn new(api_base: &str, upload_base: &str) -> Self {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .build()
            .expect("valid Google Drive HTTP client configuration");
        Self {
            client,
            api_base: api_base.trim_end_matches('/').to_owned(),
            upload_base: upload_base.trim_end_matches('/').to_owned(),
        }
    }

    fn log_upload_receipt(response: &DriveResponse) {
        match serde_json::from_slice::<DriveUploadReceipt>(&response.body) {
            Ok(receipt) => {
                log!(
                    "Google Drive media upload response id={} version={} modified_time={}",
                    receipt.id,
                    receipt.version.as_deref().unwrap_or("missing"),
                    receipt.modified_time.as_deref().unwrap_or("missing")
                );
            }
            Err(_) => {
                log!("Google Drive media upload response metadata unavailable");
            }
        }
    }

    fn list_files(&self, token: &str, query: &str) -> Result<Vec<DriveFile>, SyncError> {
        let mut page_token: Option<String> = None;
        let mut files = Vec::new();
        loop {
            let mut url = self.api_url(&["files"], &[]);
            {
                let mut pairs = url.query_pairs_mut();
                pairs
                    .append_pair("q", query)
                    .append_pair("spaces", "drive")
                    .append_pair("pageSize", "1000")
                    .append_pair(
                        "fields",
                        "nextPageToken,files(id,name,mimeType,parents,version,sha256Checksum,size,modifiedTime,trashed)",
                    );
                if let Some(token) = &page_token {
                    pairs.append_pair("pageToken", token);
                }
            }
            let response = self
                .request(
                    token,
                    Method::GET,
                    url,
                    None,
                    None,
                    &[],
                    true,
                    false,
                    "list",
                )
                .map_err(|error| map_api_error("list Google Drive files", error))?;
            let response = expect_success(response, "list Google Drive files")?;
            let page: DriveFileList = serde_json::from_slice(&response.body)
                .map_err(|_| SyncError::Provider("invalid Google Drive list response".into()))?;
            files.extend(page.files);
            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }
        Ok(files)
    }

    fn find_named_files(
        &self,
        token: &str,
        parent_id: &str,
        name: &str,
    ) -> Result<Vec<DriveFile>, SyncError> {
        let query = format!(
            "'{}' in parents and name = '{}' and trashed = false",
            escape_query_literal(parent_id),
            escape_query_literal(name)
        );
        self.list_files(token, &query)
    }

    fn create_folder(
        &self,
        token: &str,
        parent_id: &str,
        name: &str,
    ) -> Result<DriveFile, SyncError> {
        let metadata = DriveFileMetadata {
            name,
            mime_type: Some(DRIVE_FOLDER_MIME_TYPE),
            parents: Some(vec![parent_id]),
        };
        let response = self
            .request(
                token,
                Method::POST,
                self.api_url(&["files"], &[("fields", "id,name,mimeType")]),
                Some(serde_json::to_vec(&metadata).expect("Drive metadata serializes")),
                Some("application/json"),
                &[],
                false,
                false,
                "create",
            )
            .map_err(|error| map_api_error("create Google Drive folder", error))?;
        parse_drive_file(expect_success(response, "create Google Drive folder")?)
    }

    fn download(&self, token: &str, file_id: &str) -> Result<Option<Vec<u8>>, SyncError> {
        let mut url = self.file_url(&self.api_base, file_id)?;
        url.query_pairs_mut().append_pair("alt", "media");
        let response = self
            .request(
                token,
                Method::GET,
                url,
                None,
                None,
                &[],
                true,
                false,
                "download",
            )
            .map_err(|error| map_api_error("download Google Drive file", error))?;
        if response.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(
            expect_success(response, "download Google Drive file")?.body,
        ))
    }

    fn metadata(&self, token: &str, file_id: &str) -> Result<Option<DriveFile>, SyncError> {
        let mut url = self.file_url(&self.api_base, file_id)?;
        url.query_pairs_mut().append_pair("fields", FILE_FIELDS);
        let response = self
            .request(
                token,
                Method::GET,
                url,
                None,
                None,
                &[],
                true,
                false,
                "metadata",
            )
            .map_err(|error| map_api_error("read Google Drive metadata", error))?;
        if response.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(parse_drive_file(expect_success(
            response,
            "read Google Drive metadata",
        )?)?))
    }

    fn account_id(&self, token: &str) -> Result<String, SyncError> {
        let response = self
            .request(
                token,
                Method::GET,
                self.api_url(&["about"], &[("fields", "user(permissionId)")]),
                None,
                None,
                &[],
                true,
                false,
                "account",
            )
            .map_err(|error| map_api_error("read Google Drive account", error))?;
        let response = expect_success(response, "read Google Drive account")?;
        let about: DriveAbout = serde_json::from_slice(&response.body)
            .map_err(|_| SyncError::Provider("invalid Google Drive account response".into()))?;
        if about.user.permission_id.is_empty() {
            return Err(SyncError::Integrity("empty Google Drive account ID".into()));
        }
        Ok(about.user.permission_id)
    }

    fn migration_marker(&self, token: &str, folder_id: &str) -> Result<bool, SyncError> {
        let mut url = self.file_url(&self.api_base, folder_id)?;
        url.query_pairs_mut().append_pair("fields", "id,properties");
        let response = self
            .request(
                token,
                Method::GET,
                url,
                None,
                None,
                &[],
                true,
                false,
                "marker",
            )
            .map_err(|error| map_api_error("read migration marker", error))?;
        let response = expect_success(response, "read migration marker")?;
        let file: DriveFolderMarker = serde_json::from_slice(&response.body)
            .map_err(|_| SyncError::Provider("invalid migration marker response".into()))?;
        if file.id != folder_id {
            return Err(SyncError::Integrity("migration folder ID changed".into()));
        }
        match file.properties.get(MIGRATION_PROPERTY).map(String::as_str) {
            None => Ok(false),
            Some(MIGRATION_COMPLETE) => Ok(true),
            Some(_) => Err(SyncError::Integrity(
                "unsupported Google Drive migration marker".into(),
            )),
        }
    }

    fn complete_migration(&self, token: &str, folder_id: &str) -> Result<(), SyncError> {
        let mut url = self.file_url(&self.api_base, folder_id)?;
        url.query_pairs_mut().append_pair("fields", "id,properties");
        let body = serde_json::json!({
            "properties": {MIGRATION_PROPERTY: MIGRATION_COMPLETE}
        });
        let response = self
            .request(
                token,
                Method::PATCH,
                url,
                Some(serde_json::to_vec(&body)?),
                Some("application/json"),
                &[],
                false,
                false,
                "complete-migration",
            )
            .map_err(|error| map_api_error("mark migration complete", error))?;
        let response = expect_success(response, "mark migration complete")?;
        let file: DriveFolderMarker = serde_json::from_slice(&response.body)
            .map_err(|_| SyncError::Provider("invalid migration marker response".into()))?;
        if file.id != folder_id
            || file.properties.get(MIGRATION_PROPERTY).map(String::as_str)
                != Some(MIGRATION_COMPLETE)
        {
            return Err(SyncError::Integrity(
                "migration marker was not saved".into(),
            ));
        }
        Ok(())
    }

    fn start_page_token(&self, token: &str) -> Result<String, SyncError> {
        let response = self
            .request(
                token,
                Method::GET,
                self.api_url(&["changes", "startPageToken"], &[]),
                None,
                None,
                &[],
                true,
                false,
                "start-token",
            )
            .map_err(|error| map_api_error("read Google Drive start token", error))?;
        let response = expect_success(response, "read Google Drive start token")?;
        let parsed: DriveStartToken = serde_json::from_slice(&response.body)
            .map_err(|_| SyncError::Provider("invalid Google Drive start token".into()))?;
        if parsed.start_page_token.is_empty() {
            return Err(SyncError::Provider("empty Google Drive start token".into()));
        }
        Ok(parsed.start_page_token)
    }

    fn changes(&self, token: &str, cursor: &str) -> Result<(Vec<DriveChange>, String), SyncError> {
        let mut page_token = cursor.to_owned();
        let mut changes = Vec::new();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(page_token.clone()) {
                return Err(SyncError::Provider(
                    "repeated Google Drive changes page token".into(),
                ));
            }
            let mut url = self.api_url(&["changes"], &[]);
            url.query_pairs_mut()
                .append_pair("pageToken", &page_token)
                .append_pair("spaces", "drive")
                .append_pair("includeRemoved", "true")
                .append_pair("pageSize", "1000")
                .append_pair("fields", &format!("nextPageToken,newStartPageToken,changes(fileId,time,removed,file({FILE_FIELDS}))"));
            let response = self
                .request(
                    token,
                    Method::GET,
                    url,
                    None,
                    None,
                    &[],
                    true,
                    false,
                    "changes",
                )
                .map_err(|error| map_api_error("list Google Drive changes", error))?;
            let response = expect_success(response, "list Google Drive changes")?;
            let page: DriveChangesPage = serde_json::from_slice(&response.body)
                .map_err(|_| SyncError::Provider("invalid Google Drive changes response".into()))?;
            changes.extend(page.changes);
            if let Some(next) = page.next_page_token {
                page_token = next;
            } else {
                let final_token = page
                    .new_start_page_token
                    .filter(|token| !token.is_empty())
                    .ok_or_else(|| {
                        SyncError::Provider(
                            "Google Drive changes response missing final token".into(),
                        )
                    })?;
                return Ok((changes, final_token));
            }
        }
    }

    fn trash(&self, token: &str, file_id: &str) -> Result<(), SyncError> {
        let mut url = self.file_url(&self.api_base, file_id)?;
        url.query_pairs_mut().append_pair("fields", "id,trashed");
        let response = self
            .request(
                token,
                Method::PATCH,
                url,
                Some(br#"{"trashed":true}"#.to_vec()),
                Some("application/json"),
                &[],
                true,
                false,
                "trash",
            )
            .map_err(|error| map_api_error("trash Google Drive file", error))?;
        if response.status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        expect_success(response, "trash Google Drive file")?;
        Ok(())
    }

    fn delete(&self, token: &str, file_id: &str) -> Result<(), SyncError> {
        let url = self
            .file_url(&self.api_base, file_id)
            .map_err(|_| SyncError::Provider("invalid Google Drive file identifier".into()))?;
        let response = self
            .request(
                token,
                Method::DELETE,
                url,
                None,
                None,
                &[],
                true,
                false,
                "delete",
            )
            .map_err(|error| map_api_error("delete Google Drive file", error))?;
        if response.status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        expect_success(response, "delete Google Drive file")?;
        Ok(())
    }

    fn upload(
        &self,
        token: &str,
        parent_id: &str,
        name: &str,
        existing_id: Option<&str>,
        bytes: &[u8],
    ) -> Result<String, SyncError> {
        let id = if bytes.len() <= MULTIPART_UPLOAD_LIMIT {
            self.upload_simple(token, parent_id, name, existing_id, bytes)?
        } else {
            self.upload_resumable(token, parent_id, name, existing_id, bytes)?
        };
        Ok(id)
    }

    fn upload_simple(
        &self,
        token: &str,
        parent_id: &str,
        name: &str,
        existing_id: Option<&str>,
        bytes: &[u8],
    ) -> Result<String, SyncError> {
        let (url, method, body, content_type, retry_safe) = if let Some(id) = existing_id {
            let mut url = self.file_url(&self.upload_base, id)?;
            url.query_pairs_mut()
                .append_pair("uploadType", "media")
                .append_pair("fields", "id,version,modifiedTime");
            (
                url,
                Method::PATCH,
                bytes.to_vec(),
                "application/octet-stream".to_owned(),
                true,
            )
        } else {
            let boundary = format!("touchhle-{}", Uuid::new_v4().simple());
            let metadata = DriveFileMetadata {
                name,
                mime_type: None,
                parents: Some(vec![parent_id]),
            };
            let mut body = Vec::with_capacity(bytes.len() + 512);
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n")
                    .as_bytes(),
            );
            body.extend_from_slice(
                &serde_json::to_vec(&metadata)
                    .map_err(|_| SyncError::Provider("serialize upload metadata".into()))?,
            );
            body.extend_from_slice(
                format!("\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n")
                    .as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
            let mut url = Url::parse(&self.upload_base)
                .map_err(|_| SyncError::Provider("invalid Google Drive upload URL".into()))?;
            url.path_segments_mut()
                .map_err(|_| SyncError::Provider("invalid Google Drive upload URL".into()))?
                .pop_if_empty()
                .push("files");
            url.query_pairs_mut()
                .append_pair("uploadType", "multipart")
                .append_pair("fields", "id,name,mimeType");
            (
                url,
                Method::POST,
                body,
                format!("multipart/related; boundary={boundary}"),
                false,
            )
        };
        let response = self
            .request(
                token,
                method,
                url,
                Some(body),
                Some(&content_type),
                &[],
                retry_safe,
                false,
                "upload",
            )
            .map_err(|error| map_api_error("upload Google Drive file", error))?;
        if let Some(id) = existing_id {
            let response = expect_success(response, "update Google Drive file")?;
            Self::log_upload_receipt(&response);
            return Ok(id.to_owned());
        }
        let file = parse_drive_file(expect_success(response, "upload Google Drive file")?)?;
        Ok(file.id)
    }

    fn upload_resumable(
        &self,
        token: &str,
        parent_id: &str,
        name: &str,
        existing_id: Option<&str>,
        bytes: &[u8],
    ) -> Result<String, SyncError> {
        let metadata = DriveFileMetadata {
            name,
            mime_type: None,
            parents: existing_id.is_none().then_some(vec![parent_id]),
        };
        let base = if let Some(id) = existing_id {
            self.file_url(&self.upload_base, id)?
        } else {
            let mut url = Url::parse(&self.upload_base)
                .map_err(|_| SyncError::Provider("invalid Google Drive upload URL".into()))?;
            url.path_segments_mut()
                .map_err(|_| SyncError::Provider("invalid Google Drive upload URL".into()))?
                .pop_if_empty()
                .push("files");
            url
        };
        let mut url = base;
        url.query_pairs_mut()
            .append_pair("uploadType", "resumable")
            .append_pair(
                "fields",
                if existing_id.is_some() {
                    "id,version,modifiedTime"
                } else {
                    "id,name,mimeType"
                },
            );
        let initiation_method = if existing_id.is_some() {
            Method::PATCH
        } else {
            Method::POST
        };
        let initiation = self
            .request(
                token,
                initiation_method,
                url,
                Some(
                    serde_json::to_vec(&metadata)
                        .map_err(|_| SyncError::Provider("serialize upload metadata".into()))?,
                ),
                Some("application/json"),
                &[],
                false,
                false,
                "upload",
            )
            .map_err(|error| map_api_error("start resumable Google Drive upload", error))?;
        let location = expect_success(initiation, "start resumable Google Drive upload")?
            .headers
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| SyncError::Provider("Google Drive upload session missing".into()))?
            .to_owned();
        let location = Url::parse(&location)
            .map_err(|_| SyncError::Provider("invalid Google Drive upload session".into()))?;
        let upload_host = Url::parse(&self.upload_base)
            .expect("configured upload URL")
            .host_str()
            .unwrap_or_default()
            .to_owned();
        if location.scheme() != "https" && location.host_str() != Some("127.0.0.1") {
            return Err(SyncError::Provider(
                "Google Drive upload session was not HTTPS".into(),
            ));
        }
        if location.host_str() != Some(upload_host.as_str()) {
            return Err(SyncError::Provider(
                "Google Drive upload session host mismatch".into(),
            ));
        }

        let mut offset = 0usize;
        let mut final_response = None;
        while offset < bytes.len() {
            let end = (offset + RESUMABLE_CHUNK_SIZE).min(bytes.len());
            let mut no_progress_attempts = 0;
            loop {
                let chunk = bytes[offset..end].to_vec();
                let content_range =
                    format!("bytes {offset}-{}/{total}", end - 1, total = bytes.len());
                let headers = [
                    ("Content-Range", content_range),
                    ("Content-Type", "application/octet-stream".to_owned()),
                ];
                let response = self.request(
                    token,
                    Method::PUT,
                    location.clone(),
                    Some(chunk),
                    None,
                    &headers,
                    false,
                    true,
                    "upload",
                );
                let response = match response {
                    Ok(response)
                        if response.status == StatusCode::PERMANENT_REDIRECT
                            || response.status.is_success() =>
                    {
                        response
                    }
                    Ok(response) if is_retryable(response.status, &response.body) => {
                        thread_sleep(no_progress_attempts);
                        self.query_upload_status(token, &location, bytes.len())?
                    }
                    Ok(response) => expect_success(response, "send resumable Google Drive upload")?,
                    Err(_) => self.query_upload_status(token, &location, bytes.len())?,
                };
                if response.status != StatusCode::PERMANENT_REDIRECT {
                    let response =
                        expect_success(response, "finish resumable Google Drive upload")?;
                    if end != bytes.len() {
                        return Err(SyncError::Provider(
                            "Google Drive completed an upload before all bytes were sent".into(),
                        ));
                    }
                    final_response = Some(response);
                    offset = end;
                    break;
                }

                let range_end = response
                    .headers
                    .get("range")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.rsplit_once('-').map(|(_, end)| end))
                    .and_then(|value| value.parse::<usize>().ok());
                let Some(range_end) = range_end else {
                    no_progress_attempts += 1;
                    if no_progress_attempts >= 3 {
                        return Err(SyncError::Provider(
                            "Google Drive upload session made no progress".into(),
                        ));
                    }
                    thread_sleep(no_progress_attempts);
                    continue;
                };
                let next_offset = range_end.saturating_add(1);
                if next_offset < offset || next_offset > end {
                    return Err(SyncError::Provider(
                        "Google Drive upload session returned an invalid range".into(),
                    ));
                }
                if next_offset == offset {
                    no_progress_attempts += 1;
                    if no_progress_attempts >= 3 {
                        return Err(SyncError::Provider(
                            "Google Drive upload session made no progress".into(),
                        ));
                    }
                    thread_sleep(no_progress_attempts);
                    continue;
                }
                offset = next_offset;
                break;
            }
        }
        let response =
            final_response.ok_or_else(|| SyncError::Provider("empty resumable upload".into()))?;
        let response = expect_success(response, "finish resumable Google Drive upload")?;
        if let Some(existing_id) = existing_id {
            Self::log_upload_receipt(&response);
            return Ok(existing_id.to_owned());
        }
        Ok(parse_drive_file(response)?.id)
    }

    fn query_upload_status(
        &self,
        token: &str,
        location: &Url,
        total_bytes: usize,
    ) -> Result<DriveResponse, SyncError> {
        let headers = [
            ("Content-Length", "0".to_owned()),
            ("Content-Range", format!("bytes */{total_bytes}")),
        ];
        let response = self
            .request(
                token,
                Method::PUT,
                location.clone(),
                None,
                None,
                &headers,
                true,
                true,
                "upload-status",
            )
            .map_err(|error| map_api_error("check resumable Google Drive upload", error))?;
        if response.status == StatusCode::PERMANENT_REDIRECT || response.status.is_success() {
            Ok(response)
        } else {
            expect_success(response, "check resumable Google Drive upload")
        }
    }

    fn request(
        &self,
        token: &str,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
        extra_headers: &[(&str, String)],
        retry_safe: bool,
        allow_resume_status: bool,
        kind: &'static str,
    ) -> Result<DriveResponse, ApiError> {
        static REQUEST_ID: AtomicU64 = AtomicU64::new(1);
        for attempt in 0..MAX_HTTP_ATTEMPTS {
            let request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            let started = Instant::now();
            let request_body_bytes = body.as_ref().map_or(0, Vec::len);
            let mut request = self
                .client
                .request(method.clone(), url.clone())
                .bearer_auth(token);
            if let Some(content_type) = content_type {
                request = request.header(CONTENT_TYPE, content_type);
            }
            for (name, value) in extra_headers {
                let name =
                    HeaderName::from_bytes(name.as_bytes()).map_err(|_| ApiError::Transport)?;
                let value = HeaderValue::from_str(value).map_err(|_| ApiError::Transport)?;
                request = request.header(name, value);
            }
            if let Some(body) = &body {
                request = request.body(body.clone());
            }
            match request.send() {
                Ok(response) => {
                    let status = response.status();
                    let headers = response.headers().clone();
                    let response_body = match response.bytes() {
                        Ok(body) => body,
                        Err(_) if retry_safe && attempt + 1 < MAX_HTTP_ATTEMPTS => {
                            log!(
                                "Google Drive HTTP request {request_id} ({kind}) response body failed in {} ms; retrying",
                                started.elapsed().as_millis()
                            );
                            thread_sleep(attempt);
                            continue;
                        }
                        Err(_) => {
                            log!(
                                "Google Drive HTTP request {request_id} ({kind}) response body failed in {} ms",
                                started.elapsed().as_millis()
                            );
                            return Err(ApiError::Transport);
                        }
                    };
                    let retry = retry_safe
                        && attempt + 1 < MAX_HTTP_ATTEMPTS
                        && is_retryable(status, &response_body);
                    log!(
                        "Google Drive HTTP request {request_id} ({kind}) completed in {} ms (status {}, request {} bytes, response {} bytes{})",
                        started.elapsed().as_millis(),
                        status.as_u16(),
                        request_body_bytes,
                        response_body.len(),
                        if retry { ", retrying" } else { "" }
                    );
                    if retry {
                        thread_sleep(attempt);
                        continue;
                    }
                    if allow_resume_status && status == StatusCode::PERMANENT_REDIRECT {
                        return Ok(DriveResponse {
                            status,
                            headers,
                            body: response_body.to_vec(),
                        });
                    }
                    return Ok(DriveResponse {
                        status,
                        headers,
                        body: response_body.to_vec(),
                    });
                }
                Err(_) if retry_safe && attempt + 1 < MAX_HTTP_ATTEMPTS => {
                    log!(
                        "Google Drive HTTP request {request_id} ({kind}) transport failed in {} ms; retrying",
                        started.elapsed().as_millis()
                    );
                    thread_sleep(attempt);
                }
                Err(_) => {
                    log!(
                        "Google Drive HTTP request {request_id} ({kind}) transport failed in {} ms",
                        started.elapsed().as_millis()
                    );
                    return Err(ApiError::Transport);
                }
            }
        }
        Err(ApiError::Transport)
    }

    fn api_url(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = Url::parse(&self.api_base).expect("configured Drive API URL");
        {
            let mut path = url
                .path_segments_mut()
                .expect("Drive API URL is hierarchical");
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        url
    }

    fn file_url(&self, base: &str, file_id: &str) -> Result<Url, SyncError> {
        let mut url = Url::parse(base)
            .map_err(|_| SyncError::Provider("invalid Google Drive API base URL".into()))?;
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| SyncError::Provider("invalid Google Drive API base URL".into()))?;
            path.pop_if_empty().push("files").push(file_id);
        }
        Ok(url)
    }
}

#[derive(Debug)]
enum ApiError {
    Transport,
}

struct DriveResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveUploadReceipt {
    id: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    modified_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveFile {
    id: String,
    name: String,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    sha256_checksum: Option<String>,
    #[serde(default)]
    parents: Vec<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    modified_time: Option<String>,
    #[serde(default)]
    trashed: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveAbout {
    user: DriveAboutUser,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveAboutUser {
    permission_id: String,
}

#[derive(Deserialize)]
struct DriveFolderMarker {
    id: String,
    #[serde(default)]
    properties: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveStartToken {
    start_page_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveChangesPage {
    #[serde(default)]
    changes: Vec<DriveChange>,
    next_page_token: Option<String>,
    new_start_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveChange {
    file_id: String,
    #[serde(default)]
    time: Option<String>,
    #[serde(default)]
    removed: bool,
    file: Option<DriveFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveFileList {
    #[serde(default)]
    files: Vec<DriveFile>,
    next_page_token: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DriveFileMetadata<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parents: Option<Vec<&'a str>>,
}

fn expect_success(response: DriveResponse, operation: &str) -> Result<DriveResponse, SyncError> {
    if response.status.is_success() {
        return Ok(response);
    }
    Err(response_error(response.status, operation))
}

fn parse_drive_file(response: DriveResponse) -> Result<DriveFile, SyncError> {
    serde_json::from_slice(&response.body)
        .map_err(|_| SyncError::Provider("invalid Google Drive file response".into()))
}

fn response_error(status: StatusCode, operation: &str) -> SyncError {
    match status {
        StatusCode::UNAUTHORIZED => SyncError::Authentication(format!("{operation}: HTTP 401")),
        StatusCode::FORBIDDEN => SyncError::Authentication(format!("{operation}: HTTP 403")),
        _ => SyncError::Provider(format!("{operation}: HTTP {}", status.as_u16())),
    }
}

fn map_api_error(operation: &str, _error: ApiError) -> SyncError {
    SyncError::Provider(format!("{operation}: network request failed"))
}

fn is_retryable(status: StatusCode, body: &[u8]) -> bool {
    if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    ) {
        return true;
    }
    status == StatusCode::FORBIDDEN
        && (body
            .windows(b"rateLimitExceeded".len())
            .any(|part| part == b"rateLimitExceeded")
            || body
                .windows(b"userRateLimitExceeded".len())
                .any(|part| part == b"userRateLimitExceeded"))
}

fn thread_sleep(attempt: usize) {
    let milliseconds = 200u64.saturating_mul(1u64 << attempt.min(4));
    std::thread::sleep(Duration::from_millis(milliseconds));
}

fn escape_query_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

fn commit_id_for_name(name: &str) -> Result<Option<Uuid>, SyncError> {
    if !name.ends_with(".json") || name.contains('/') {
        return Ok(None);
    }
    name.strip_suffix(".json")
        .and_then(|id| Uuid::parse_str(id).ok())
        .map(Some)
        .ok_or_else(|| SyncError::Integrity("invalid remote commit key".into()))
}

fn commit_for_key(id: Uuid, bytes: &[u8]) -> Result<Commit, SyncError> {
    let commit: Commit = serde_json::from_slice(bytes)?;
    if commit.id != id {
        return Err(SyncError::Integrity(
            "remote commit key does not match its record".into(),
        ));
    }
    Ok(commit)
}

fn read_commits_in_parallel<T, F>(ids: &[(Uuid, String)], read: F) -> Result<Vec<T>, SyncError>
where
    T: Send,
    F: Fn(Uuid, &str) -> Result<T, SyncError> + Sync,
{
    run_bounded(ids, MAX_PARALLEL_COMMIT_READS, |(id, file_id)| {
        read(*id, file_id)
    })
}

fn run_bounded<T, R>(
    items: &[T],
    max_parallel: usize,
    operation: impl Fn(&T) -> Result<R, SyncError> + Send + Sync,
) -> Result<Vec<R>, SyncError>
where
    T: Sync,
    R: Send,
{
    let mut results = Vec::with_capacity(items.len());
    for batch in items.chunks(max_parallel.max(1)) {
        let batch_results = std::thread::scope(|scope| {
            let operation = &operation;
            let workers: Vec<_> = batch
                .iter()
                .map(|item| scope.spawn(move || operation(item)))
                .collect();
            workers
                .into_iter()
                .map(|worker| {
                    worker.join().unwrap_or_else(|_| {
                        Err(SyncError::Provider(
                            "parallel Google Drive operation panicked".into(),
                        ))
                    })
                })
                .collect::<Vec<_>>()
        });

        let mut values = Vec::with_capacity(batch_results.len());
        let mut first_error = None;
        let mut authorization_error = None;
        for result in batch_results {
            match result {
                Ok(value) => values.push(value),
                Err(error) if is_unauthorized_sync_error(&error) => {
                    authorization_error.get_or_insert(error);
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Some(error) = authorization_error.or(first_error) {
            return Err(error);
        }
        results.extend(values);
    }
    Ok(results)
}

fn write_object_verified(
    http: &DriveHttp,
    token: &str,
    folder_id: &str,
    hash: [u8; 32],
    bytes: &[u8],
) -> Result<(), SyncError> {
    validate_object_key(hash, bytes)?;
    let name = hex_hash(&hash);
    let existing = http
        .find_named_files(token, folder_id, &name)?
        .into_iter()
        .next();
    if let Some(file) = existing {
        match http.download(token, &file.id)? {
            Some(existing) if existing == bytes => return Ok(()),
            Some(_) => return Err(SyncError::Integrity("immutable object changed".into())),
            None => {}
        }
    }

    let id = http.upload(token, folder_id, &name, None, bytes)?;
    verify_uploaded_bytes("uploaded object", bytes, http.download(token, &id)?)
}

fn validate_object_key(hash: [u8; 32], bytes: &[u8]) -> Result<(), SyncError> {
    if sha2::Sha256::digest(bytes).as_slice() == hash {
        Ok(())
    } else {
        Err(SyncError::Integrity(
            "object key does not match bytes".into(),
        ))
    }
}

fn verify_uploaded_bytes(
    kind: &str,
    expected: &[u8],
    stored: Option<Vec<u8>>,
) -> Result<(), SyncError> {
    match stored {
        Some(stored) if stored == expected => Ok(()),
        Some(_) => Err(SyncError::Integrity(format!(
            "{kind} did not match its content"
        ))),
        None => Err(SyncError::Integrity(format!(
            "{kind} was missing from the remote store"
        ))),
    }
}

impl<T: AccessTokenSource> RemoteStore for GoogleDriveStore<T> {
    fn identity(&mut self) -> Result<(String, String), SyncError> {
        self.with_auth_retry("remote identity", |store| {
            let token = store.token(false)?;
            let account = store.http.account_id(&token)?;
            let matches = store.http.find_named_files(&token, "root", DRIVE_ROOT)?;
            let roots: Vec<_> = matches
                .into_iter()
                .filter(|file| file.mime_type == DRIVE_FOLDER_MIME_TYPE)
                .collect();
            if roots.len() > 1 {
                return Err(SyncError::Integrity("ambiguous Google Drive root".into()));
            }
            let id = if let Some(root) = roots.first() {
                root.id.clone()
            } else {
                store.http.create_folder(&token, "root", DRIVE_ROOT)?.id
            };
            if store.folder_ids.get(DRIVE_ROOT) != Some(&id) {
                store.folder_ids.clear();
                store.known_files.clear();
            }
            store.folder_ids.insert(DRIVE_ROOT.into(), id.clone());
            Ok((account, id))
        })
    }

    fn migration_completed(&mut self) -> Result<bool, SyncError> {
        self.with_auth_retry("migration status", |store| {
            let token = store.token(false)?;
            let Some(folder) = store.find_folder(&token, DRIVE_ROOT)? else {
                return Ok(false);
            };
            store.http.migration_marker(&token, &folder)
        })
    }

    fn mark_migration_completed(&mut self) -> Result<(), SyncError> {
        self.with_auth_retry("complete migration", |store| {
            let token = store.token(false)?;
            let folder = store.ensure_folder(&token, DRIVE_ROOT)?;
            store.http.complete_migration(&token, &folder)
        })
    }
    fn restore_checkpoint_indexes(
        &mut self,
        file_paths_by_id: &BTreeMap<String, RelativePath>,
        folder_paths_by_id: &BTreeMap<String, String>,
    ) -> Result<(), SyncError> {
        let mut known_files = HashMap::with_capacity(file_paths_by_id.len());
        let mut seen_file_paths = HashSet::new();
        for (id, path) in file_paths_by_id {
            let folded = path.as_str().case_fold().collect::<String>();
            if id.is_empty() || !seen_file_paths.insert(folded) {
                return Err(SyncError::Integrity(
                    "invalid or duplicate checkpoint file mapping".into(),
                ));
            }
            known_files.insert(id.clone(), path.clone());
        }

        let mut folder_ids = HashMap::with_capacity(folder_paths_by_id.len());
        let mut seen_folder_paths = HashSet::new();
        for (id, path) in folder_paths_by_id {
            validate_managed_folder(path)?;
            let folded = path.case_fold().collect::<String>();
            if id.is_empty() || !seen_folder_paths.insert(folded) {
                return Err(SyncError::Integrity(
                    "invalid or duplicate checkpoint folder mapping".into(),
                ));
            }
            folder_ids.insert(path.clone(), id.clone());
        }

        let managed_root_id = self.folder_ids.get(DRIVE_ROOT).cloned();
        self.known_files = known_files;
        self.folder_ids = folder_ids;
        if let Some(id) = managed_root_id {
            self.folder_ids.insert(DRIVE_ROOT.into(), id);
        }
        Ok(())
    }

    fn checkpoint_folder_paths_by_id(&self) -> Result<Option<BTreeMap<String, String>>, SyncError> {
        let mut folder_paths = BTreeMap::new();
        let mut seen_paths = HashSet::new();
        for (path, id) in &self.folder_ids {
            if !is_managed_folder(path) {
                continue;
            }
            validate_managed_folder(path)?;
            if id.is_empty()
                || !seen_paths.insert(path.case_fold().collect::<String>())
                || folder_paths.insert(id.clone(), path.clone()).is_some()
            {
                return Err(SyncError::Integrity(
                    "invalid or ambiguous managed folder index".into(),
                ));
            }
        }
        Ok(Some(folder_paths))
    }

    fn start_page_token(&mut self) -> Result<String, SyncError> {
        self.with_auth_retry("start changes", |store| {
            let token = store.token(false)?;
            store.http.start_page_token(&token)
        })
    }

    fn initial_inventory(&mut self) -> Result<Vec<RemoteFile>, SyncError> {
        self.with_auth_retry("initial inventory", |store| {
            let token = store.token(false)?;
            let managed_root = store.ensure_folder(&token, MANAGED_ROOT_DIR)?;
            let root_children = store.http.list_files(
                &token,
                &format!(
                    "'{}' in parents and trashed = false",
                    escape_query_literal(&managed_root)
                ),
            )?;
            let mut root_names = HashSet::new();
            for child in root_children {
                let folded: String = child.name.as_str().case_fold().collect();
                if folded != "touchhle_apps" && folded != "touchhle_sandbox" {
                    continue;
                }
                if !root_names.insert(folded)
                    || !matches!(child.name.as_str(), "touchHLE_apps" | "touchHLE_sandbox")
                {
                    return Err(SyncError::Integrity(
                        "duplicate or case-colliding managed root".into(),
                    ));
                }
                if child.mime_type != DRIVE_FOLDER_MIME_TYPE {
                    return Err(SyncError::Integrity("managed root is not a folder".into()));
                }
            }
            let apps = store.ensure_folder(&token, &format!("{MANAGED_ROOT_DIR}/touchHLE_apps"))?;
            let sandbox =
                store.ensure_folder(&token, &format!("{MANAGED_ROOT_DIR}/touchHLE_sandbox"))?;
            let mut queue = vec![
                (apps, format!("{MANAGED_ROOT_DIR}/touchHLE_apps")),
                (sandbox, format!("{MANAGED_ROOT_DIR}/touchHLE_sandbox")),
            ];
            let mut seen_ids = HashSet::from([managed_root]);
            let mut seen_paths = BTreeMap::new();
            let mut files = Vec::new();
            let mut verified_ids = HashSet::new();
            let mut index = 0;
            while index < queue.len() {
                let (parent_id, parent_path) = queue[index].clone();
                index += 1;
                if !seen_ids.insert(parent_id.clone()) {
                    return Err(SyncError::Integrity(
                        "remote folder cycle or duplicate".into(),
                    ));
                }
                let children = store.http.list_files(
                    &token,
                    &format!(
                        "'{}' in parents and trashed = false",
                        escape_query_literal(&parent_id)
                    ),
                )?;
                for child in children {
                    if child.trashed {
                        continue;
                    }
                    let child_path = format!("{parent_path}/{}", child.name);
                    let relative = child_path
                        .strip_prefix(&format!("{MANAGED_ROOT_DIR}/"))
                        .ok_or_else(|| {
                            SyncError::Integrity("folder outside managed tree".into())
                        })?;
                    let valid_path = RelativePath::new(relative)?;
                    let folded: String = valid_path.as_str().case_fold().collect();
                    if seen_paths.insert(folded, child_path.clone()).is_some() {
                        return Err(SyncError::Integrity(
                            "duplicate or case-colliding managed path".into(),
                        ));
                    }
                    if child.mime_type == DRIVE_FOLDER_MIME_TYPE {
                        store
                            .folder_ids
                            .insert(child_path.clone(), child.id.clone());
                        verified_ids.insert(child.id.clone());
                        queue.push((child.id, child_path));
                    } else if let Some(file) = Self::remote_file(
                        &store.http,
                        &mut store.folder_ids,
                        &token,
                        child,
                        true,
                        &mut verified_ids,
                    )? {
                        files.push(file);
                    }
                }
            }
            let entries = files
                .iter()
                .map(|f| (f.path.clone(), f.entry.clone()))
                .collect();
            validate_entries(&entries).map_err(SyncError::Integrity)?;
            store.known_files = files
                .iter()
                .map(|f| (f.id.clone(), f.path.clone()))
                .collect();
            Ok(files)
        })
    }

    fn changes_since(&mut self, cursor: &str) -> Result<RemoteChangeBatch, SyncError> {
        self.with_auth_retry("changes since cursor", |store| {
            let token = store.token(false)?;
            let (events, next_page_token) = store.http.changes(&token, cursor)?;
            let mut latest = BTreeMap::new();
            for event in events {
                if event.file_id.is_empty()
                    || event
                        .file
                        .as_ref()
                        .is_some_and(|file| file.id != event.file_id)
                {
                    return Err(SyncError::Integrity(
                        "Google Drive change ID does not match file metadata".into(),
                    ));
                }
                latest.insert(event.file_id.clone(), event);
            }
            let mut changes = Vec::new();
            let mut folder_ids = store.folder_ids.clone();
            let mut known_files = store.known_files.clone();
            let mut verified_ids = HashSet::new();
            let mut affected_files = BTreeSet::new();
            for (id, event) in &latest {
                let is_folder = event
                    .file
                    .as_ref()
                    .is_some_and(|file| file.mime_type == DRIVE_FOLDER_MIME_TYPE);
                let old_paths: Vec<_> = folder_ids
                    .iter()
                    .filter(|(_, cached_id)| *cached_id == id)
                    .map(|(path, _)| path.clone())
                    .collect();
                if old_paths.iter().any(|path| path == DRIVE_ROOT) {
                    let Some(folder) = event
                        .file
                        .as_ref()
                        .filter(|file| !file.trashed && file.mime_type == DRIVE_FOLDER_MIME_TYPE)
                    else {
                        return Err(SyncError::Integrity(
                            "managed Google Drive root was removed or trashed".into(),
                        ));
                    };
                    if folder.name != DRIVE_ROOT {
                        return Err(SyncError::Integrity(
                            "managed Google Drive root was renamed".into(),
                        ));
                    }
                    verified_ids.insert(id.clone());
                    continue;
                }
                if !is_folder && old_paths.is_empty() {
                    continue;
                }
                if verified_ids.contains(id) && !event.removed {
                    if let Some(folder) = event.file.as_ref().filter(|file| !file.trashed) {
                        if let Some(parent) = folder.parents.first() {
                            if let Some(parent_path) = Self::managed_folder_path(
                                &store.http,
                                &mut folder_ids,
                                &token,
                                parent,
                                !old_paths.is_empty(),
                                &mut HashSet::new(),
                                &mut verified_ids,
                            )
                            .map_err(|error| match error {
                                SyncError::Integrity(message) => SyncError::Integrity(format!(
                                    "Google Drive folder change {id} ({}) under parent {parent}: {message}",
                                    folder.name
                                )),
                                error => error,
                            })? {
                                if old_paths.len() == 1
                                    && old_paths[0] == format!("{parent_path}/{}", folder.name)
                                {
                                    continue;
                                }
                            }
                        }
                    }
                }
                for old_path in &old_paths {
                    for (file_id, path) in &known_files {
                        let prefix = old_path.strip_prefix(&format!("{MANAGED_ROOT_DIR}/"));
                        if prefix
                            .is_some_and(|prefix| path.as_str().starts_with(&format!("{prefix}/")))
                        {
                            affected_files.insert(file_id.clone());
                        }
                    }
                    invalidate_folder_cache(&mut folder_ids, old_path);
                }
                if event.removed || event.file.as_ref().is_some_and(|file| file.trashed) {
                    continue;
                }
                let Some(folder) = &event.file else {
                    continue;
                };
                let Some(parent) = folder.parents.first() else {
                    continue;
                };
                let Some(parent_path) = Self::managed_folder_path(
                    &store.http,
                    &mut folder_ids,
                    &token,
                    parent,
                    !old_paths.is_empty(),
                    &mut HashSet::new(),
                    &mut verified_ids,
                )
                .map_err(|error| match error {
                    SyncError::Integrity(message) => SyncError::Integrity(format!(
                        "Google Drive folder change {id} ({}) under parent {parent}: {message}",
                        folder.name
                    )),
                    error => error,
                })?
                else {
                    continue;
                };
                let path = format!("{parent_path}/{}", folder.name);
                if is_managed_folder(&path) {
                    validate_managed_folder(&path)?;
                    ensure_folder_path_available(&folder_ids, &path, id)?;
                    folder_ids.insert(path, id.clone());
                    verified_ids.insert(id.clone());
                }
            }
            affected_by_folder_changes(
                &store.folder_ids,
                &folder_ids,
                &store.known_files,
                &mut affected_files,
            );
            for (id, event) in latest {
                if event
                    .file
                    .as_ref()
                    .is_some_and(|file| file.mime_type == DRIVE_FOLDER_MIME_TYPE)
                    || store.folder_ids.values().any(|folder_id| folder_id == &id)
                {
                    continue;
                }
                let file = if event.removed {
                    None
                } else if let Some(file) = event.file {
                    let fail_on_missing_ancestry = known_files.contains_key(&id);
                    Self::remote_file(
                        &store.http,
                        &mut folder_ids,
                        &token,
                        file,
                        fail_on_missing_ancestry,
                        &mut verified_ids,
                    )?
                } else {
                    None
                };
                if let Some(file) = &file {
                    known_files.insert(id.clone(), file.path.clone());
                } else if known_files.remove(&id).is_none() {
                    continue;
                }
                affected_files.remove(&id);
                changes.push(RemoteChange { file_id: id, file });
            }
            affected_by_folder_changes(
                &store.folder_ids,
                &folder_ids,
                &store.known_files,
                &mut affected_files,
            );
            affected_files.retain(|id| !changes.iter().any(|change| &change.file_id == id));
            let affected_files: Vec<_> = affected_files.into_iter().collect();
            let http = store.http.clone();
            let request_token = token.clone();
            let metadata_results = run_bounded(
                &affected_files,
                MAX_PARALLEL_COMMIT_READS,
                |id| http.metadata(&request_token, id),
            )?;
            for (id, metadata) in affected_files.into_iter().zip(metadata_results) {
                let file = match metadata {
                    Some(file) => Self::remote_file(
                        &store.http,
                        &mut folder_ids,
                        &token,
                        file,
                        true,
                        &mut verified_ids,
                    )?,
                    None => None,
                };
                if let Some(file) = &file {
                    known_files.insert(id.clone(), file.path.clone());
                } else {
                    known_files.remove(&id);
                }
                changes.push(RemoteChange { file_id: id, file });
            }
            let entries: BTreeMap<_, _> = changes
                .iter()
                .filter_map(|change| {
                    change
                        .file
                        .as_ref()
                        .map(|f| (f.path.clone(), f.entry.clone()))
                })
                .collect();
            if entries.len() != changes.iter().filter(|c| c.file.is_some()).count() {
                return Err(SyncError::Integrity(
                    "duplicate managed path in changes".into(),
                ));
            }
            validate_entries(&entries).map_err(SyncError::Integrity)?;
            let mut seen_paths = HashSet::new();
            for path in known_files.values() {
                if !seen_paths.insert(path.as_str().case_fold().collect::<String>()) {
                    return Err(SyncError::Integrity(
                        "duplicate or case-colliding managed path in changes".into(),
                    ));
                }
            }
            let mut seen_folders = HashMap::new();
            for (path, id) in &folder_ids {
                if !is_managed_folder(path) {
                    continue;
                }
                let relative = path.strip_prefix(&format!("{MANAGED_ROOT_DIR}/")).unwrap();
                let folded: String = relative.case_fold().collect();
                if seen_folders.insert(folded.clone(), id).is_some() || seen_paths.contains(&folded)
                {
                    return Err(SyncError::Integrity(
                        "duplicate or case-colliding managed folder path in changes".into(),
                    ));
                }
            }
            store.folder_ids = folder_ids;
            store.known_files = known_files;
            Ok(RemoteChangeBatch {
                changes,
                next_page_token,
            })
        })
    }

    fn diagnose_changes_since(
        &mut self,
        cursor: &str,
        file_ids: &[String],
    ) -> Result<(), SyncError> {
        let file_ids: HashSet<_> = file_ids.iter().map(String::as_str).collect();
        self.with_auth_retry("diagnose changes since cursor", |store| {
            let token = store.token(false)?;
            let (events, _) = store.http.changes(&token, cursor)?;
            let mut matched = 0;
            for event in events
                .into_iter()
                .filter(|event| file_ids.contains(event.file_id.as_str()))
            {
                matched += 1;
                let (version, modified_time, sha256, size) = event.file.as_ref().map_or_else(
                    || ("missing".into(), "missing".into(), "missing".into(), "missing".into()),
                    |file| {
                        (
                            file.version.as_deref().unwrap_or("missing").to_owned(),
                            file.modified_time.as_deref().unwrap_or("missing").to_owned(),
                            file.sha256_checksum.as_deref().unwrap_or("missing").to_owned(),
                            file.size.as_deref().unwrap_or("missing").to_owned(),
                        )
                    },
                );
                log!(
                    "Google Drive diagnostic change id={} time={} removed={} version={} modified_time={} sha256={} size={}",
                    event.file_id,
                    event.time.as_deref().unwrap_or("missing"),
                    event.removed,
                    version,
                    modified_time,
                    sha256,
                    size
                );
            }
            log!(
                "Google Drive diagnostic changes matched_events={} target_files={}",
                matched,
                file_ids.len()
            );
            Ok(())
        })
    }

    fn file_metadata(&mut self, id: &str) -> Result<Option<RemoteFile>, SyncError> {
        self.with_auth_retry("file metadata", |store| {
            let token = store.token(false)?;
            let Some(file) = store.http.metadata(&token, id)? else {
                return Ok(None);
            };
            if file.id == id {
                if let Some(path) = store.known_files.get(id).cloned() {
                    if let Some((parent, name)) = path.as_str().rsplit_once('/') {
                        let parent_key = format!("{MANAGED_ROOT_DIR}/{parent}");
                        if name == file.name {
                            let expected_parent = store.folder_ids.get(&parent_key).cloned();
                            let actual_parent = file.parents.first().cloned();
                            if let (Some(expected_parent), Some(actual_parent)) =
                                (expected_parent, actual_parent)
                            {
                                if expected_parent == actual_parent {
                                    return Self::remote_file_at_path(
                                        file,
                                        path,
                                        Some(actual_parent),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            Self::remote_file(
                &store.http,
                &mut store.folder_ids,
                &token,
                file,
                true,
                &mut HashSet::new(),
            )
        })
    }

    fn read_file(&mut self, id: &str) -> Result<Option<Vec<u8>>, SyncError> {
        self.with_auth_retry("read current file", |store| {
            let token = store.token(false)?;
            store.http.download(&token, id)
        })
    }

    fn write_file(
        &mut self,
        path: &RelativePath,
        existing_id: Option<&str>,
        bytes: &[u8],
    ) -> Result<RemoteFile, SyncError> {
        self.with_auth_retry("write current file", |store| {
            let token = store.token(false)?;
            let (parent, name) = path
                .as_str()
                .rsplit_once('/')
                .ok_or_else(|| SyncError::InvalidPath(path.as_str().into()))?;
            let parent_key = format!("{MANAGED_ROOT_DIR}/{parent}");
            let parent_id = if existing_id.is_some() {
                store
                    .find_folder(&token, &parent_key)?
                    .ok_or_else(|| SyncError::Integrity("managed parent folder missing".into()))?
            } else {
                store.ensure_folder(&token, &parent_key)?
            };
            if let Some(existing_id) = existing_id {
                let metadata = store.http.metadata(&token, existing_id)?.ok_or_else(|| {
                    SyncError::Provider("remote file missing before upload".into())
                })?;
                if metadata.trashed
                    || metadata.parents.first() != Some(&parent_id)
                    || metadata.name != name
                    || metadata.mime_type == DRIVE_FOLDER_MIME_TYPE
                {
                    return Err(SyncError::Integrity(
                        "remote file identity or path changed".into(),
                    ));
                }
            } else {
                let siblings = store.http.list_files(
                    &token,
                    &format!(
                        "'{}' in parents and trashed = false",
                        escape_query_literal(&parent_id)
                    ),
                )?;
                let folded: String = name.case_fold().collect();
                if siblings
                    .iter()
                    .any(|file| file.name.as_str().case_fold().collect::<String>() == folded)
                {
                    return Err(SyncError::RemotePathExists);
                }
            }
            let id = store
                .http
                .upload(&token, &parent_id, name, existing_id, bytes)?;
            let mut metadata = store
                .http
                .metadata(&token, &id)?
                .ok_or_else(|| SyncError::Integrity("uploaded file missing".into()))?;
            let checksum = metadata.sha256_checksum.as_deref();
            let expected_hash: [u8; 32] = sha2::Sha256::digest(bytes).into();
            if let Some(checksum) = checksum {
                if parse_sha256(checksum)? != expected_hash {
                    return Err(SyncError::Integrity(
                        "uploaded file checksum mismatch".into(),
                    ));
                }
            } else {
                verify_uploaded_bytes("uploaded file", bytes, store.http.download(&token, &id)?)?;
                metadata.sha256_checksum = Some(sha256_hex(bytes));
            }
            if metadata.id != id
                || metadata.name != name
                || metadata.parents.first() != Some(&parent_id)
                || metadata.trashed
            {
                return Err(SyncError::Integrity(
                    "uploaded file metadata mismatch".into(),
                ));
            }
            if metadata.size.as_deref() != Some(bytes.len().to_string().as_str()) {
                return Err(SyncError::Integrity("uploaded file size mismatch".into()));
            }
            let file = Self::remote_file(
                &store.http,
                &mut store.folder_ids,
                &token,
                metadata,
                true,
                &mut HashSet::new(),
            )?
            .ok_or_else(|| SyncError::Integrity("uploaded file outside managed tree".into()))?;
            if file.path != *path {
                return Err(SyncError::Integrity("uploaded file path mismatch".into()));
            }
            store.known_files.insert(id, path.clone());
            Ok(file)
        })
    }

    fn delete_file(&mut self, id: &str) -> Result<(), SyncError> {
        self.with_auth_retry("trash current file", |store| {
            let token = store.token(false)?;
            store.http.trash(&token, id)?;
            store.known_files.remove(id);
            Ok(())
        })
    }

    fn list_commits(&mut self) -> Result<Vec<Commit>, SyncError> {
        self.with_auth_retry("list commits", |store| {
            let token = store.token(false)?;
            let Some(commits_folder_id) = store.find_folder(&token, COMMITS_DIR)? else {
                return Ok(Vec::new());
            };
            let files = store.http.list_files(
                &token,
                &format!("'{}' in parents and trashed = false", commits_folder_id),
            )?;
            let mut commit_files = Vec::new();
            for file in files {
                if file.mime_type == DRIVE_FOLDER_MIME_TYPE {
                    continue;
                }
                if let Some(id) = commit_id_for_name(&file.name)? {
                    commit_files.push((id, file.id, file.version, file.sha256_checksum));
                }
            }

            let previous_cache = store
                .commit_cache_root
                .as_deref()
                .and_then(|root| match commit_cache::load(root) {
                    Ok(cache) => cache,
                    Err(_) => {
                        log!("Google Drive commit cache could not be read; downloading full history");
                        None
                    }
                })
                .filter(|cache| cache.folder_id == commits_folder_id);
            let mut next_cache = CommitCache::new(commits_folder_id);
            let mut ordered = Vec::with_capacity(commit_files.len());
            let mut to_download = Vec::new();
            let mut reused_count = 0;
            for (id, file_id, version, checksum) in &commit_files {
                let cached = version
                    .as_deref()
                    .filter(|version| !version.is_empty())
                    .zip(checksum.as_deref())
                    .and_then(|(version, checksum)| {
                        let entry = previous_cache.as_ref()?.entries.get(file_id)?;
                        if entry.version != version
                            || !sha256_hex(entry.json.as_bytes()).eq_ignore_ascii_case(checksum)
                        {
                            return None;
                        }
                        commit_for_key(*id, entry.json.as_bytes())
                            .ok()
                            .map(|commit| (entry, commit))
                    });
                if let Some((entry, commit)) = cached {
                    ordered.push(Some(commit));
                    reused_count += 1;
                    next_cache.entries.insert(file_id.clone(), entry.clone());
                } else {
                    ordered.push(None);
                    to_download.push((
                        ordered.len() - 1,
                        *id,
                        file_id.clone(),
                        version.clone(),
                        checksum.clone(),
                    ));
                }
            }
            let http = store.http.clone();
            let token = token.clone();
            let download_keys: Vec<_> = to_download
                .iter()
                .map(|(_, id, file_id, _, _)| (*id, file_id.clone()))
                .collect();
            let downloaded = read_commits_in_parallel(&download_keys, |id, file_id| {
                let bytes = http
                    .download(&token, file_id)?
                    .ok_or_else(|| SyncError::Provider("remote commit disappeared".into()))?;
                let commit = commit_for_key(id, &bytes)?;
                let json = String::from_utf8(bytes).map_err(|_| {
                    SyncError::Integrity("remote commit was not UTF-8 JSON".into())
                })?;
                Ok((commit, json))
            })?;
            for ((slot_index, id, file_id, version, checksum), (commit, json)) in
                to_download.into_iter().zip(downloaded)
            {
                if commit.id != id {
                    return Err(SyncError::Integrity(
                        "remote commit key does not match its record".into(),
                    ));
                }
                if let Some(checksum) = &checksum {
                    if !sha256_hex(json.as_bytes()).eq_ignore_ascii_case(checksum) {
                        return Err(SyncError::Integrity(
                            "remote commit content did not match Drive checksum".into(),
                        ));
                    }
                }
                if let (Some(version), Some(_)) =
                    (version.filter(|version| !version.is_empty()), checksum)
                {
                    next_cache.entries.insert(
                        file_id,
                        CachedCommit {
                            version,
                            json,
                        },
                    );
                }
                ordered[slot_index] = Some(commit);
            }
            let download_count = ordered.len() - reused_count;
            let commits = ordered
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| SyncError::Integrity("remote commit cache incomplete".into()))?;
            if let Some(root) = &store.commit_cache_root {
                if commit_cache::save(root, &next_cache).is_err() {
                    log!("Google Drive commit cache could not be saved; next sync may download history");
                }
            }
            log!(
                "Google Drive commit index read {} records: {} reused, {} downloaded (max {})",
                commits.len(),
                reused_count,
                download_count,
                MAX_PARALLEL_COMMIT_READS
            );
            Ok(commits)
        })
    }

    fn read_object(&mut self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, SyncError> {
        self.with_auth_retry("read object", |store| {
            let token = store.token(false)?;
            let Some(file) = store.find_file(&token, OBJECTS_DIR, &hex_hash(hash))? else {
                return Ok(None);
            };
            store.http.download(&token, &file.id)
        })
    }

    fn write_object(&mut self, hash: [u8; 32], bytes: &[u8]) -> Result<(), SyncError> {
        self.with_auth_retry("write object", |store| {
            let token = store.token(false)?;
            let folder_id = store.ensure_folder(&token, OBJECTS_DIR)?;
            write_object_verified(&store.http, &token, &folder_id, hash, bytes)
        })
    }

    fn write_objects_and_verify(
        &mut self,
        objects: &[([u8; 32], Vec<u8>)],
    ) -> Result<(), SyncError> {
        self.with_auth_retry("write and verify content objects", |store| {
            let token = store.token(false)?;
            let folder_id = store.ensure_folder(&token, OBJECTS_DIR)?;
            let http = store.http.clone();
            run_bounded(objects, MAX_PARALLEL_OBJECT_WRITES, |(hash, bytes)| {
                write_object_verified(&http, &token, &folder_id, *hash, bytes)
            })
            .map(|_| ())
        })
    }

    fn write_commit(&mut self, commit: &Commit) -> Result<(), SyncError> {
        let bytes = serde_json::to_vec(commit)?;
        self.with_auth_retry("write commit", |store| {
            let token = store.token(false)?;
            let folder_id = store.ensure_folder(&token, COMMITS_DIR)?;
            let name = format!("{}.json", commit.id);
            let existing = store
                .http
                .find_named_files(&token, &folder_id, &name)?
                .into_iter()
                .next();
            if let Some(file) = existing {
                let stored = store
                    .http
                    .download(&token, &file.id)?
                    .ok_or_else(|| SyncError::Provider("remote commit disappeared".into()))?;
                if stored == bytes {
                    return Ok(());
                }
                return Err(SyncError::Integrity("immutable commit changed".into()));
            }
            let id = store.http.upload(&token, &folder_id, &name, None, &bytes)?;
            let stored = store
                .http
                .download(&token, &id)?
                .ok_or_else(|| SyncError::Integrity("uploaded commit was missing".into()))?;
            verify_uploaded_bytes("uploaded commit", &bytes, Some(stored))
        })
    }
}

fn invalidate_folder_cache(folder_ids: &mut HashMap<String, String>, path: &str) {
    let descendants = format!("{path}/");
    folder_ids
        .retain(|cached_path, _| cached_path != path && !cached_path.starts_with(&descendants));
}

fn is_unauthorized_sync_error(error: &SyncError) -> bool {
    matches!(error, SyncError::Authentication(message) if message.ends_with("HTTP 401"))
}

fn hex_hash(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let hash: [u8; 32] = sha2::Sha256::digest(bytes).into();
    hex_hash(&hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::engine::{SyncEngine, SyncOutcome};
    use crate::sync::model::{CurrentSyncState, RelativePath, SnapshotEntry};
    use crate::sync::store::MemoryRemoteStore;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    struct TestTokenSource {
        calls: Arc<AtomicUsize>,
        refreshes: Arc<AtomicUsize>,
    }

    impl AccessTokenSource for TestTokenSource {
        fn access_token(&mut self, force_refresh: bool) -> Result<String, SyncError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if force_refresh {
                self.refreshes.fetch_add(1, Ordering::SeqCst);
            }
            Ok("test-only-token".into())
        }
    }

    fn token_source() -> (TestTokenSource, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let refreshes = Arc::new(AtomicUsize::new(0));
        (
            TestTokenSource {
                calls: Arc::clone(&calls),
                refreshes: Arc::clone(&refreshes),
            },
            calls,
            refreshes,
        )
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        String::from_utf8_lossy(&request).into_owned()
    }

    fn read_request_body(stream: &mut std::net::TcpStream, request: &str) -> Vec<u8> {
        let content_length = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or_default();
        let mut body = vec![0; content_length];
        stream.read_exact(&mut body).unwrap();
        body
    }

    fn respond(
        stream: &mut std::net::TcpStream,
        status: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        )
        .unwrap();
        for (name, value) in headers {
            write!(stream, "{name}: {value}\r\n").unwrap();
        }
        stream.write_all(b"\r\n").unwrap();
        stream.write_all(body).unwrap();
    }

    fn test_http(listener: &TcpListener) -> DriveHttp {
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}/drive/v3");
        let upload = format!("http://{address}/upload/drive/v3");
        DriveHttp::new(&base, &upload)
    }

    struct SyncTree(PathBuf);

    impl SyncTree {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("touchhle-gdrive-roundtrip-{}", Uuid::new_v4()));
            std::fs::create_dir_all(path.join("touchHLE_apps/Old/Sub")).unwrap();
            std::fs::write(path.join("touchHLE_apps/Old/Sub/save"), b"nested bytes").unwrap();
            Self(path)
        }

        fn state_path(&self) -> PathBuf {
            self.0.join(".touchHLE_sync/state.json")
        }
    }

    impl Drop for SyncTree {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn legacy_migration_absent_commit_folder_is_read_only() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("GET "));
            assert!(request.contains("commits"));
            respond(&mut stream, "200 OK", &[], br#"{"files":[]}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE".into(), "root-folder".into());
        assert!(store.list_commits().unwrap().is_empty());
        server.join().unwrap();
    }

    #[test]
    fn account_identity_reads_stable_drive_permission_id() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("GET /drive/v3/about?"));
            assert!(request.contains("fields=user%28permissionId%29"));
            respond(
                &mut stream,
                "200 OK",
                &[],
                br#"{"user":{"permissionId":"account-123"}}"#,
            );
        });
        assert_eq!(http.account_id("token").unwrap(), "account-123");
        server.join().unwrap();
    }

    #[test]
    fn migration_marker_uses_shared_folder_properties_and_verifies_write() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("GET /drive/v3/files/files-folder?"));
            assert!(request.contains("id%2Cproperties"));
            respond(&mut stream, "200 OK", &[], br#"{"id":"files-folder"}"#);

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("PATCH /drive/v3/files/files-folder?"));
            let body: serde_json::Value =
                serde_json::from_slice(&read_request_body(&mut stream, &request)).unwrap();
            assert_eq!(
                body["properties"]["touchhleFilesMigration"],
                MIGRATION_COMPLETE
            );
            assert!(body.get("appProperties").is_none());
            respond(
                &mut stream,
                "200 OK",
                &[],
                br#"{"id":"files-folder","properties":{"touchhleFilesMigration":"complete-v1"}}"#,
            );
        });
        assert!(!http.migration_marker("token", "files-folder").unwrap());
        http.complete_migration("token", "files-folder").unwrap();
        server.join().unwrap();
    }

    #[test]
    fn unknown_migration_marker_does_not_trigger_legacy_replay() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            respond(
                &mut stream,
                "200 OK",
                &[],
                br#"{"id":"files-folder","properties":{"touchhleFilesMigration":"future-version"}}"#,
            );
        });
        assert!(matches!(
            http.migration_marker("token", "files-folder"),
            Err(SyncError::Integrity(_))
        ));
        server.join().unwrap();
    }

    #[test]
    fn changes_pages_use_final_token_and_keep_removals_without_media() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            for (expected, body) in [
                ("pageToken=begin", br#"{"nextPageToken":"middle","changes":[{"fileId":"file-1","file":{"id":"file-1","name":"old","mimeType":"application/octet-stream","parents":["apps"],"version":"1","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"1"}}]}"#.as_slice()),
                ("pageToken=middle", br#"{"newStartPageToken":"final","changes":[{"fileId":"file-1","file":{"id":"file-1","name":"new","mimeType":"application/octet-stream","parents":["apps"],"version":"2","sha256Checksum":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":"2"}},{"fileId":"gone","removed":true}]}"#.as_slice()),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.starts_with("GET /drive/v3/changes?"));
                assert!(request.contains(expected));
                assert!(request.contains("includeRemoved=true"));
                respond(&mut stream, "200 OK", &[], body);
            }
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        store.known_files.insert(
            "gone".into(),
            RelativePath::new("touchHLE_apps/gone").unwrap(),
        );
        let batch = store.changes_since("begin").unwrap();
        server.join().unwrap();
        assert_eq!(batch.next_page_token, "final");
        assert_eq!(batch.changes.len(), 2);
        assert_eq!(
            batch.changes[0].file.as_ref().unwrap().path.as_str(),
            "touchHLE_apps/new"
        );
        assert_eq!(batch.changes[1].file, None);
    }

    #[test]
    fn engine_checkpoint_restores_in_fresh_store_and_nested_folder_removal_survives_restart() {
        let tree = SyncTree::new();
        let path = RelativePath::new("touchHLE_apps/Old/Sub/save").unwrap();
        let folders = BTreeMap::from([
            ("apps-folder".into(), "touchHLE/touchHLE_apps".into()),
            ("old-folder".into(), "touchHLE/touchHLE_apps/Old".into()),
            ("sub-folder".into(), "touchHLE/touchHLE_apps/Old/Sub".into()),
        ]);
        let mut memory = MemoryRemoteStore::default();
        memory.seed_current_file(
            RemoteFile {
                path: path.clone(),
                id: "nested-file".into(),
                version: "1".into(),
                entry: SnapshotEntry::File {
                    sha256: sha2::Sha256::digest(b"nested bytes").into(),
                    size: 12,
                    modified_unix_ms: 0,
                },
                parent_id: Some("sub-folder".into()),
            },
            b"nested bytes".to_vec(),
        );
        memory.seed_folder_paths_by_id(folders.clone());

        let mut initial = SyncEngine::new(memory, tree.0.clone(), tree.state_path());
        initial.synchronize().unwrap();
        let checkpoint = std::fs::read(tree.state_path()).unwrap();
        let state: CurrentSyncState = serde_json::from_slice(&checkpoint).unwrap();
        assert_eq!(state.folder_paths_by_id, folders);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.starts_with("GET /drive/v3/about?"));
                respond(
                    &mut stream,
                    "200 OK",
                    &[],
                    br#"{"user":{"permissionId":"test-account"}}"#,
                );
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.starts_with("GET /drive/v3/files?"));
                respond(
                    &mut stream,
                    "200 OK",
                    &[],
                    br#"{"files":[{"id":"test-root","name":"touchHLE","mimeType":"application/vnd.google-apps.folder"}]}"#,
                );
                if request.contains("pageToken=warm") {
                    unreachable!();
                }
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.starts_with("GET /drive/v3/changes?"));
                if request.contains("pageToken=0") {
                    respond(
                        &mut stream,
                        "200 OK",
                        &[],
                        br#"{"newStartPageToken":"warm","changes":[]}"#,
                    );
                } else {
                    assert!(request.contains("pageToken=warm"));
                    respond(
                        &mut stream,
                        "200 OK",
                        &[],
                        br#"{"newStartPageToken":"removed","changes":[{"fileId":"old-folder","removed":true}]}"#,
                    );
                }
            }

            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.starts_with("GET /drive/v3/files/nested-file?"));
                respond(&mut stream, "404 Not Found", &[], b"");
            }
        });

        let (source, _, _) = token_source();
        let mut warm_store = GoogleDriveStore::new(source);
        warm_store.http = http.clone();
        let mut warm = SyncEngine::new(warm_store, tree.0.clone(), tree.state_path());
        assert!(matches!(warm.synchronize().unwrap(), SyncOutcome::UpToDate));
        assert_eq!(
            serde_json::from_slice::<CurrentSyncState>(&std::fs::read(tree.state_path()).unwrap())
                .unwrap()
                .folder_paths_by_id,
            folders
        );

        let (source, _, _) = token_source();
        let mut restarted_store = GoogleDriveStore::new(source);
        restarted_store.http = http;
        let mut restarted = SyncEngine::new(restarted_store, tree.0.clone(), tree.state_path());
        assert!(matches!(
            restarted.synchronize().unwrap(),
            SyncOutcome::Applied
        ));
        assert!(!tree.0.join(path.as_str()).exists());
        let final_state: CurrentSyncState =
            serde_json::from_slice(&std::fs::read(tree.state_path()).unwrap()).unwrap();
        assert_eq!(
            final_state.folder_paths_by_id,
            BTreeMap::from([("apps-folder".into(), "touchHLE/touchHLE_apps".into())])
        );
        server.join().unwrap();
    }

    #[test]
    fn failed_late_page_preserves_both_caches_and_retries_same_cursor() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            for _ in 0..2 {
                for (cursor, body) in [
                    ("original", br#"{"nextPageToken":"page-two","changes":[{"fileId":"new-folder","file":{"id":"new-folder","name":"New","mimeType":"application/vnd.google-apps.folder","parents":["apps"]}}]}"#.as_slice()),
                    ("page-two", br#"{"newStartPageToken":"done","changes":[{"fileId":"zz-bad","file":{"id":"zz-bad","name":"bad?","mimeType":"application/octet-stream","parents":["apps"],"version":"1","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"1"}}]}"#.as_slice()),
                ] {
                    let (mut stream, _) = listener.accept().unwrap();
                    assert!(read_request(&mut stream).contains(&format!("pageToken={cursor}")));
                    respond(&mut stream, "200 OK", &[], body);
                }
            }
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        store.known_files.insert(
            "existing".into(),
            RelativePath::new("touchHLE_apps/old").unwrap(),
        );
        let before_folders = store.folder_ids.clone();
        let before_files = store.known_files.clone();
        for _ in 0..2 {
            assert!(store.changes_since("original").is_err());
            assert_eq!(store.folder_ids, before_folders);
            assert_eq!(store.known_files, before_files);
        }
        server.join().unwrap();
    }

    #[test]
    fn folder_collision_rejects_batch_without_publishing_cache() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"folder","file":{"id":"folder","name":"save","mimeType":"application/vnd.google-apps.folder","parents":["apps"]}}]}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Save".into(), "other-folder".into());
        let before = store.folder_ids.clone();
        assert!(matches!(
            store.changes_since("old"),
            Err(SyncError::Integrity(_))
        ));
        assert_eq!(store.folder_ids, before);
        server.join().unwrap();
    }

    #[test]
    fn folder_event_invalidates_all_aliases_for_one_id() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"folder","file":{"id":"folder","name":"Current","mimeType":"application/vnd.google-apps.folder","parents":["apps"]}}]}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old".into(), "folder".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Ghost".into(), "folder".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Ghost/Sub".into(), "sub".into());
        store.changes_since("old").unwrap();
        assert_eq!(
            store
                .folder_ids
                .get("touchHLE/touchHLE_apps/Current")
                .map(String::as_str),
            Some("folder")
        );
        assert!(!store.folder_ids.contains_key("touchHLE/touchHLE_apps/Old"));
        assert!(!store
            .folder_ids
            .contains_key("touchHLE/touchHLE_apps/Ghost"));
        assert!(!store
            .folder_ids
            .contains_key("touchHLE/touchHLE_apps/Ghost/Sub"));
        server.join().unwrap();
    }

    #[test]
    fn child_folder_event_before_parent_keeps_final_descendant_cache() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"a-child","file":{"id":"a-child","name":"Sub2","mimeType":"application/vnd.google-apps.folder","parents":["z-parent"]}},{"fileId":"z-parent","file":{"id":"z-parent","name":"New","mimeType":"application/vnd.google-apps.folder","parents":["apps"]}}]}"#);
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/z-parent?"));
            respond(&mut stream, "200 OK", &[], br#"{"id":"z-parent","name":"New","mimeType":"application/vnd.google-apps.folder","parents":["apps"]}"#);
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/sibling?"));
            respond(&mut stream, "200 OK", &[], br#"{"id":"sibling","name":"other","mimeType":"application/octet-stream","parents":["z-parent"],"version":"2","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"1"}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old".into(), "z-parent".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old/Sub".into(), "a-child".into());
        store.known_files.insert(
            "sibling".into(),
            RelativePath::new("touchHLE_apps/Old/other").unwrap(),
        );
        let batch = store.changes_since("old").unwrap();
        assert_eq!(batch.changes.len(), 1);
        assert_eq!(
            batch.changes[0].file.as_ref().unwrap().path.as_str(),
            "touchHLE_apps/New/other"
        );
        assert_eq!(
            store
                .folder_ids
                .get("touchHLE/touchHLE_apps/New/Sub2")
                .map(String::as_str),
            Some("a-child")
        );
        server.join().unwrap();
    }

    #[test]
    fn unknown_unmanaged_parent_is_ignored_without_path_validation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"unrelated","file":{"id":"unrelated","name":"bad?","mimeType":"application/octet-stream","parents":["other"],"version":"1","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"1"}}]}"#);
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/other?"));
            respond(&mut stream, "200 OK", &[], br#"{"id":"other","name":"other","mimeType":"application/vnd.google-apps.folder","parents":["files-root"]}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert(MANAGED_ROOT_DIR.into(), "files-root".into());
        let batch = store.changes_since("old").unwrap();
        assert!(batch.changes.is_empty());
        assert_eq!(batch.next_page_token, "new");
        assert!(!store.folder_ids.contains_key("touchHLE/other"));
        server.join().unwrap();
    }

    #[test]
    fn cached_child_folder_is_checked_against_current_metadata_without_folder_event() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"file","file":{"id":"file","name":"save","mimeType":"application/octet-stream","parents":["folder"],"version":"1","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"1"}}]}"#);
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/folder?"));
            respond(&mut stream, "200 OK", &[], br#"{"id":"folder","name":"New","mimeType":"application/vnd.google-apps.folder","parents":["apps"]}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old".into(), "folder".into());
        let batch = store.changes_since("old").unwrap();
        assert_eq!(
            batch.changes[0].file.as_ref().unwrap().path.as_str(),
            "touchHLE_apps/New/save"
        );
        assert!(!store.folder_ids.contains_key("touchHLE/touchHLE_apps/Old"));
        server.join().unwrap();
    }

    #[test]
    fn checkpoint_restore_keeps_managed_root_for_nested_folder_changes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"preferences","file":{"id":"preferences","name":"Preferences","mimeType":"application/vnd.google-apps.folder","parents":["library"]}}]}"#);

            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/library?"));
            respond(&mut stream, "200 OK", &[], br#"{"id":"library","name":"Library","mimeType":"application/vnd.google-apps.folder","parents":["sandbox"]}"#);

            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/sandbox?"));
            respond(&mut stream, "200 OK", &[], br#"{"id":"sandbox","name":"touchHLE_sandbox","mimeType":"application/vnd.google-apps.folder","parents":["managed-root"]}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert(DRIVE_ROOT.into(), "managed-root".into());
        store
            .restore_checkpoint_indexes(
                &BTreeMap::new(),
                &BTreeMap::from([("apps".into(), "touchHLE/touchHLE_apps".into())]),
            )
            .unwrap();

        let batch = store.changes_since("old").unwrap();
        server.join().unwrap();

        assert_eq!(batch.next_page_token, "new");
        assert!(batch.changes.is_empty());
        assert_eq!(
            store.folder_ids.get(DRIVE_ROOT).map(String::as_str),
            Some("managed-root")
        );
        assert_eq!(
            store
                .folder_ids
                .get("touchHLE/touchHLE_sandbox/Library/Preferences")
                .map(String::as_str),
            Some("preferences")
        );
    }

    #[test]
    fn unknown_folder_event_with_unreadable_external_ancestor_is_ignored() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"outside-folder","file":{"id":"outside-folder","name":"Outside","mimeType":"application/vnd.google-apps.folder","parents":["unreadable-parent"]}}]}"#);

            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/unreadable-parent?"));
            respond(&mut stream, "404 Not Found", &[], b"");
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert(DRIVE_ROOT.into(), "managed-root".into());
        store
            .restore_checkpoint_indexes(
                &BTreeMap::new(),
                &BTreeMap::from([("apps".into(), "touchHLE/touchHLE_apps".into())]),
            )
            .unwrap();

        let batch = store.changes_since("old").unwrap();
        server.join().unwrap();

        assert_eq!(batch.next_page_token, "new");
        assert!(batch.changes.is_empty());
        assert!(store.known_files.is_empty());
        assert_eq!(
            store.folder_ids.get(DRIVE_ROOT).map(String::as_str),
            Some("managed-root")
        );
    }

    #[test]
    fn live_known_file_with_missing_parent_fails_without_removing_checkpoint_index() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"file","file":{"id":"file","name":"save","mimeType":"application/octet-stream","parents":["missing-parent"],"version":"2","sha256Checksum":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":"1"}}]}"#);
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/missing-parent?"));
            respond(&mut stream, "404 Not Found", &[], b"");
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        store.known_files.insert(
            "file".into(),
            RelativePath::new("touchHLE_apps/save").unwrap(),
        );
        let before_folders = store.folder_ids.clone();
        let before_files = store.known_files.clone();

        assert!(matches!(
            store.changes_since("old"),
            Err(SyncError::Integrity(_))
        ));
        assert_eq!(store.folder_ids, before_folders);
        assert_eq!(store.known_files, before_files);
        server.join().unwrap();
    }

    #[test]
    fn folder_change_with_unresolvable_live_descendant_fails_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"folder","file":{"id":"folder","name":"New","mimeType":"application/vnd.google-apps.folder","parents":["apps"]}}]}"#);
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/file?"));
            respond(&mut stream, "200 OK", &[], br#"{"id":"file","name":"save","mimeType":"application/octet-stream","parents":["missing-parent"],"version":"2","sha256Checksum":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":"1"}"#);
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/missing-parent?"));
            respond(&mut stream, "404 Not Found", &[], b"");
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old".into(), "folder".into());
        store.known_files.insert(
            "file".into(),
            RelativePath::new("touchHLE_apps/Old/save").unwrap(),
        );
        let before_folders = store.folder_ids.clone();
        let before_files = store.known_files.clone();

        assert!(matches!(
            store.changes_since("old"),
            Err(SyncError::Integrity(_))
        ));
        assert_eq!(store.folder_ids, before_folders);
        assert_eq!(store.known_files, before_files);
        server.join().unwrap();
    }

    #[test]
    fn mismatched_change_and_file_ids_leave_caches_unchanged() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"different","file":{"id":"file","name":"save","mimeType":"application/octet-stream","parents":["apps"],"version":"1","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"1"}}]}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        let before = store.folder_ids.clone();
        assert!(matches!(
            store.changes_since("old"),
            Err(SyncError::Integrity(_))
        ));
        assert_eq!(store.folder_ids, before);
        assert!(store.known_files.is_empty());
        server.join().unwrap();
    }

    #[test]
    fn renamed_folder_replaces_cached_descendant_paths() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"folder","file":{"id":"folder","name":"New","mimeType":"application/vnd.google-apps.folder","parents":["apps"]}},{"fileId":"z-child","file":{"id":"z-child","name":"save","mimeType":"application/octet-stream","parents":["sub"],"version":"1","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"1"}}]}"#);
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/sub?"));
            respond(&mut stream, "200 OK", &[], br#"{"id":"sub","name":"Sub","mimeType":"application/vnd.google-apps.folder","parents":["folder"]}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old".into(), "folder".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old/Sub".into(), "sub".into());
        let result = store.changes_since("old").unwrap();
        assert_eq!(
            result.changes[0].file.as_ref().unwrap().path.as_str(),
            "touchHLE_apps/New/Sub/save"
        );
        assert!(!store
            .folder_ids
            .contains_key("touchHLE/touchHLE_apps/Old/Sub"));
        server.join().unwrap();
    }

    #[test]
    fn removed_folder_discards_descendants_and_unknown_removals() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"folder","removed":true},{"fileId":"unrelated","removed":true},{"fileId":"known","removed":true}]}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old".into(), "folder".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old/Sub".into(), "sub".into());
        store.known_files.insert(
            "known".into(),
            RelativePath::new("touchHLE_apps/Old/Sub/save").unwrap(),
        );
        let result = store.changes_since("old").unwrap();
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].file_id, "known");
        assert_eq!(result.changes[0].file, None);
        assert!(!store.folder_ids.contains_key("touchHLE/touchHLE_apps/Old"));
        assert!(!store
            .folder_ids
            .contains_key("touchHLE/touchHLE_apps/Old/Sub"));
        server.join().unwrap();
    }

    #[test]
    fn fresh_store_restores_file_index_for_removed_change_after_restart() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=saved-cursor"));
            respond(
                &mut stream,
                "200 OK",
                &[],
                br#"{"newStartPageToken":"next","changes":[{"fileId":"known-file","removed":true}]}"#,
            );
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        let path = RelativePath::new("touchHLE_apps/save").unwrap();
        let files = BTreeMap::from([("known-file".into(), path.clone())]);
        let folders = BTreeMap::from([("apps-folder".into(), "touchHLE/touchHLE_apps".into())]);

        store.restore_checkpoint_indexes(&files, &folders).unwrap();
        let batch = store.changes_since("saved-cursor").unwrap();

        assert_eq!(batch.next_page_token, "next");
        assert_eq!(batch.changes.len(), 1);
        assert_eq!(batch.changes[0].file_id, "known-file");
        assert_eq!(batch.changes[0].file, None);
        assert!(!store.known_files.contains_key("known-file"));
        server.join().unwrap();
    }

    #[test]
    fn fresh_store_restores_folder_index_to_remove_known_descendants() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=saved-cursor"));
            respond(
                &mut stream,
                "200 OK",
                &[],
                br#"{"newStartPageToken":"next","changes":[{"fileId":"old-folder","removed":true}]}"#,
            );
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/known-file?"));
            respond(&mut stream, "404 Not Found", &[], b"");
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        let path = RelativePath::new("touchHLE_apps/Old/Sub/save").unwrap();
        let files = BTreeMap::from([("known-file".into(), path)]);
        let folders = BTreeMap::from([
            ("apps-folder".into(), "touchHLE/touchHLE_apps".into()),
            ("old-folder".into(), "touchHLE/touchHLE_apps/Old".into()),
            ("sub-folder".into(), "touchHLE/touchHLE_apps/Old/Sub".into()),
        ]);

        store.restore_checkpoint_indexes(&files, &folders).unwrap();
        assert_eq!(
            store
                .folder_ids
                .get("touchHLE/touchHLE_apps/Old")
                .map(String::as_str),
            Some("old-folder")
        );
        let batch = store.changes_since("saved-cursor").unwrap();

        assert_eq!(batch.changes.len(), 1);
        assert_eq!(batch.changes[0].file_id, "known-file");
        assert_eq!(batch.changes[0].file, None);
        assert!(!store.folder_ids.contains_key("touchHLE/touchHLE_apps/Old"));
        assert!(!store
            .folder_ids
            .contains_key("touchHLE/touchHLE_apps/Old/Sub"));
        server.join().unwrap();
    }

    #[test]
    fn moved_folder_re_resolves_known_descendant_without_child_change() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"folder","file":{"id":"folder","name":"Moved","mimeType":"application/vnd.google-apps.folder","parents":["sandbox"]}}]}"#);
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/known?"));
            respond(&mut stream, "200 OK", &[], br#"{"id":"known","name":"save","mimeType":"application/octet-stream","parents":["sub"],"version":"2","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"1"}"#);
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/sub?"));
            respond(&mut stream, "200 OK", &[], br#"{"id":"sub","name":"Sub","mimeType":"application/vnd.google-apps.folder","parents":["folder"]}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_sandbox".into(), "sandbox".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old".into(), "folder".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old/Sub".into(), "sub".into());
        store.known_files.insert(
            "known".into(),
            RelativePath::new("touchHLE_apps/Old/Sub/save").unwrap(),
        );
        let batch = store.changes_since("old").unwrap();
        assert_eq!(batch.changes.len(), 1);
        assert_eq!(
            batch.changes[0].file.as_ref().unwrap().path.as_str(),
            "touchHLE_sandbox/Moved/Sub/save"
        );
        assert!(!store
            .folder_ids
            .contains_key("touchHLE/touchHLE_apps/Old/Sub"));
        server.join().unwrap();
    }

    #[test]
    fn trashed_folder_emits_only_known_descendant_removals() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"newStartPageToken":"new","changes":[{"fileId":"folder","file":{"id":"folder","name":"Old","mimeType":"application/vnd.google-apps.folder","parents":["apps"],"trashed":true}},{"fileId":"unrelated","removed":true}]}"#);
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/known?"));
            respond(&mut stream, "404 Not Found", &[], b"");
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old".into(), "folder".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Old/Sub".into(), "sub".into());
        store.known_files.insert(
            "known".into(),
            RelativePath::new("touchHLE_apps/Old/Sub/save").unwrap(),
        );
        let batch = store.changes_since("old").unwrap();
        assert_eq!(batch.changes.len(), 1);
        assert_eq!(batch.changes[0].file_id, "known");
        assert_eq!(batch.changes[0].file, None);
        assert!(!store
            .folder_ids
            .contains_key("touchHLE/touchHLE_apps/Old/Sub"));
        server.join().unwrap();
    }

    #[test]
    fn unmanaged_ancestry_is_ignored_but_managed_invalid_path_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            for (cursor, file_name, parent) in
                [("other", "bad?", "other"), ("managed", "bad?", "apps")]
            {
                let (mut stream, _) = listener.accept().unwrap();
                assert!(read_request(&mut stream).contains(&format!("pageToken={cursor}")));
                let body = format!(
                    r#"{{"newStartPageToken":"next","changes":[{{"fileId":"file","file":{{"id":"file","name":"{file_name}","mimeType":"application/octet-stream","parents":["{parent}"],"version":"1","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"1"}}}}]}}"#
                );
                respond(&mut stream, "200 OK", &[], body.as_bytes());
            }
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert(MANAGED_ROOT_DIR.into(), "files-root".into());
        store
            .folder_ids
            .insert("touchHLE/other".into(), "other".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        assert!(store.changes_since("other").unwrap().changes.is_empty());
        assert!(matches!(
            store.changes_since("managed"),
            Err(SyncError::InvalidPath(_))
        ));
        server.join().unwrap();
    }

    #[test]
    fn inventory_stays_in_managed_roots_and_rejects_case_collisions() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            for (parent, body) in [
                ("files-root", br#"{"files":[{"id":"apps","name":"touchHLE_apps","mimeType":"application/vnd.google-apps.folder"},{"id":"sandbox","name":"touchHLE_sandbox","mimeType":"application/vnd.google-apps.folder"}]}"#.as_slice()),
                ("apps", br#"{"files":[{"id":"first","name":"Save","mimeType":"application/octet-stream","parents":["apps"],"version":"1","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"1"},{"id":"second","name":"save","mimeType":"application/octet-stream","parents":["apps"],"version":"1","sha256Checksum":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":"1"}]}"#.as_slice()),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.contains(&format!("q=%27{parent}%27+in+parents")));
                assert!(!request.contains("alt=media"));
                respond(&mut stream, "200 OK", &[], body);
            }
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        for (key, id) in [
            (MANAGED_ROOT_DIR, "files-root"),
            ("touchHLE/touchHLE_apps", "apps"),
            ("touchHLE/touchHLE_sandbox", "sandbox"),
        ] {
            store.folder_ids.insert(key.into(), id.into());
        }
        assert!(matches!(
            store.initial_inventory(),
            Err(SyncError::Integrity(_))
        ));
        server.join().unwrap();
    }

    #[test]
    fn inventory_recurses_under_managed_folders_without_media_reads() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            for (parent, body) in [
                ("files-root", br#"{"files":[{"id":"apps","name":"touchHLE_apps","mimeType":"application/vnd.google-apps.folder"},{"id":"sandbox","name":"touchHLE_sandbox","mimeType":"application/vnd.google-apps.folder"},{"id":"ignored","name":"other","mimeType":"application/vnd.google-apps.folder"}]}"#.as_slice()),
                ("apps", br#"{"files":[{"id":"sub","name":"Sub","mimeType":"application/vnd.google-apps.folder","parents":["apps"]}]}"#.as_slice()),
                ("sandbox", br#"{"files":[]}"#.as_slice()),
                ("sub", br#"{"files":[{"id":"file-1","name":"one.ipa","mimeType":"application/octet-stream","parents":["sub"],"version":"5","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"1","modifiedTime":"1970-01-01T00:00:01Z"}]}"#.as_slice()),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.contains(&format!("q=%27{parent}%27+in+parents")), "{request}");
                assert!(!request.contains("alt=media"));
                respond(&mut stream, "200 OK", &[], body);
            }
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        for (key, id) in [
            (MANAGED_ROOT_DIR, "files-root"),
            ("touchHLE/touchHLE_apps", "apps"),
            ("touchHLE/touchHLE_sandbox", "sandbox"),
        ] {
            store.folder_ids.insert(key.into(), id.into());
        }
        let files = store.initial_inventory().unwrap();
        server.join().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path.as_str(), "touchHLE_apps/Sub/one.ipa");
        assert_eq!(
            files[0].entry,
            SnapshotEntry::File {
                sha256: [0xaa; 32],
                size: 1,
                modified_unix_ms: 1000,
            }
        );
    }

    #[test]
    fn indexed_file_metadata_uses_one_drive_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("GET /drive/v3/files/file-id?fields="));
            respond(
                &mut stream,
                "200 OK",
                &[],
                br#"{"id":"file-id","name":"save.dat","mimeType":"application/octet-stream","parents":["nested"],"version":"8","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"4","modifiedTime":"1970-01-01T00:00:01Z"}"#,
            );
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        let path = RelativePath::new("touchHLE_apps/Sub/save.dat").unwrap();
        store.known_files.insert("file-id".into(), path.clone());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Sub".into(), "nested".into());

        let file = store.file_metadata("file-id").unwrap().unwrap();
        server.join().unwrap();

        assert_eq!(file.path, path);
        assert_eq!(file.parent_id.as_deref(), Some("nested"));
        assert_eq!(file.version, "8");
    }

    #[test]
    fn indexed_file_metadata_rechecks_ancestry_when_parent_changes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/file-id?fields="));
            respond(
                &mut stream,
                "200 OK",
                &[],
                br#"{"id":"file-id","name":"save.dat","mimeType":"application/octet-stream","parents":["other"],"version":"8","sha256Checksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":"4","modifiedTime":"1970-01-01T00:00:01Z"}"#,
            );
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /drive/v3/files/other?fields="));
            respond(
                &mut stream,
                "200 OK",
                &[],
                br#"{"id":"other","name":"Other","mimeType":"application/vnd.google-apps.folder","parents":["apps"]}"#,
            );
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store.known_files.insert(
            "file-id".into(),
            RelativePath::new("touchHLE_apps/Sub/save.dat").unwrap(),
        );
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps/Sub".into(), "nested".into());

        let file = store.file_metadata("file-id").unwrap().unwrap();
        server.join().unwrap();

        assert_eq!(file.path.as_str(), "touchHLE_apps/Other/save.dat");
        assert_eq!(file.parent_id.as_deref(), Some("other"));
    }

    #[test]
    fn changes_page_without_final_cursor_fails_instead_of_advancing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).contains("pageToken=old"));
            respond(&mut stream, "200 OK", &[], br#"{"changes":[]}"#);
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        assert!(store.changes_since("old").is_err());
        server.join().unwrap();
    }

    #[test]
    fn start_token_and_idempotent_trash_use_metadata_only() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            for (prefix, status, body) in [
                (
                    "GET /drive/v3/changes/startPageToken ",
                    "200 OK",
                    br#"{"startPageToken":"token-1"}"#.as_slice(),
                ),
                (
                    "PATCH /drive/v3/files/gone?fields=",
                    "404 Not Found",
                    b"".as_slice(),
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.starts_with(prefix), "{request}");
                if request.starts_with("PATCH") {
                    assert_eq!(
                        read_request_body(&mut stream, &request),
                        br#"{"trashed":true}"#
                    );
                }
                respond(&mut stream, status, &[], body);
            }
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        assert_eq!(store.start_page_token().unwrap(), "token-1");
        store.delete_file("gone").unwrap();
        server.join().unwrap();
    }

    #[test]
    fn current_file_update_uses_same_id_and_verifies_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let checksum = sha256_hex(b"new bytes");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("GET /drive/v3/files/known?fields="));
            respond(&mut stream, "200 OK", &[], br#"{"id":"known","name":"game.ipa","mimeType":"application/octet-stream","parents":["apps"],"version":"7"}"#);
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("PATCH /upload/drive/v3/files/known?uploadType=media"));
            assert!(request.contains("fields=id%2Cversion%2CmodifiedTime"));
            assert_eq!(read_request_body(&mut stream, &request), b"new bytes");
            respond(
                &mut stream,
                "200 OK",
                &[],
                br#"{"id":"known","version":"8","modifiedTime":"2026-10-04T02:26:47.812Z"}"#,
            );
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("GET /drive/v3/files/known?fields="));
            let metadata = format!(
                r#"{{"id":"known","name":"game.ipa","mimeType":"application/octet-stream","parents":["apps"],"version":"8","sha256Checksum":"{checksum}","size":"9"}}"#
            );
            respond(&mut stream, "200 OK", &[], metadata.as_bytes());
        });
        let (source, _, _) = token_source();
        let mut store = GoogleDriveStore::new(source);
        store.http = http;
        store
            .folder_ids
            .insert("touchHLE/touchHLE_apps".into(), "apps".into());
        let path = RelativePath::new("touchHLE_apps/game.ipa").unwrap();
        let file = store
            .write_file(&path, Some("known"), b"new bytes")
            .unwrap();
        server.join().unwrap();
        assert_eq!(file.id, "known");
        assert_eq!(file.version, "8");
    }

    #[test]
    fn paginated_list_uses_drive_query_and_reuses_access_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            for (page, body) in [
                (
                    1,
                    r#"{"files":[{"id":"id-1","name":"one.json","mimeType":"application/json"}],"nextPageToken":"next"}"#,
                ),
                (
                    2,
                    r#"{"files":[{"id":"id-2","name":"two.json","mimeType":"application/json"}]}"#,
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.starts_with("GET /drive/v3/files?"));
                assert!(request.contains("authorization: Bearer test-only-token"));
                if page == 1 {
                    assert!(request.contains("q=%27parent%27+in+parents"));
                } else {
                    assert!(request.contains("pageToken=next"));
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let files = http
            .list_files("test-only-token", "'parent' in parents and trashed = false")
            .unwrap();
        server.join().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].id, "id-2");
    }

    #[test]
    fn safe_reads_retry_rate_limits_but_file_creation_does_not() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let _ = read_request(&mut first);
            respond(
                &mut first,
                "429 Too Many Requests",
                &[("Content-Type", "application/json")],
                br#"{"error":{"errors":[{"reason":"rateLimitExceeded"}]}}"#,
            );
            let (mut second, _) = listener.accept().unwrap();
            let _ = read_request(&mut second);
            respond(&mut second, "200 OK", &[], b"{}");
        });

        let response = http
            .request(
                "test-only-token",
                Method::GET,
                http.api_url(&["files"], &[]),
                None,
                None,
                &[],
                true,
                false,
                "list",
            )
            .unwrap();
        assert_eq!(response.status, StatusCode::OK);
        server.join().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let _ = read_request_body(&mut stream, &request);
            respond(
                &mut stream,
                "429 Too Many Requests",
                &[],
                br#"{"error":{"errors":[{"reason":"rateLimitExceeded"}]}}"#,
            );
        });
        let response = http
            .request(
                "test-only-token",
                Method::POST,
                http.api_url(&["files"], &[]),
                Some(b"{}".to_vec()),
                Some("application/json"),
                &[],
                false,
                false,
                "create",
            )
            .unwrap();
        assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
        server.join().unwrap();
    }

    #[test]
    fn new_immutable_object_uses_multipart_endpoint_and_is_read_back() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let address = listener.local_addr().unwrap();
        let bytes = b"tiny object";
        let hash: [u8; 32] = sha2::Sha256::digest(bytes).into();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("GET /drive/v3/files?"));
            respond(
                &mut stream,
                "200 OK",
                &[("Content-Type", "application/json")],
                br#"{"files":[]}"#,
            );

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("POST /upload/drive/v3/files?uploadType=multipart"));
            assert!(request
                .to_ascii_lowercase()
                .contains("content-type: multipart/related"));
            let body = read_request_body(&mut stream, &request);
            assert!(body
                .windows(b"tiny object".len())
                .any(|part| part == b"tiny object"));
            respond(
                &mut stream,
                "200 OK",
                &[("Content-Type", "application/json")],
                br#"{"id":"object-id","name":"object"}"#,
            );

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("GET /drive/v3/files/object-id?alt=media"));
            respond(&mut stream, "200 OK", &[], bytes);
        });

        let folder_id = "objects-folder";
        let result = write_object_verified(&http, "test-only-token", folder_id, hash, bytes);
        assert!(result.is_ok(), "{result:?} (mock server at {address})");
        server.join().unwrap();
    }

    #[test]
    fn large_upload_uses_resumable_session_and_content_range() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let address = listener.local_addr().unwrap();
        let bytes = vec![b'x'; MULTIPART_UPLOAD_LIMIT + 1];
        let server_bytes = bytes.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("POST /upload/drive/v3/files?uploadType=resumable"));
            let _metadata = read_request_body(&mut stream, &request);
            let session = format!("http://{address}/upload/session/one");
            respond(&mut stream, "200 OK", &[("Location", &session)], b"");

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("PUT /upload/session/one "));
            assert!(request.contains(&format!(
                "content-range: bytes 0-{}/{total}",
                server_bytes.len() - 1,
                total = server_bytes.len()
            )));
            let body = read_request_body(&mut stream, &request);
            assert_eq!(body, server_bytes);
            respond(
                &mut stream,
                "200 OK",
                &[("Content-Type", "application/json")],
                br#"{"id":"large-object-id","name":"large"}"#,
            );
        });

        let result = http.upload(
            "test-only-token",
            "objects-folder",
            "large-object",
            None,
            &bytes,
        );
        assert_eq!(result.unwrap(), "large-object-id");
        server.join().unwrap();
    }

    #[test]
    fn interrupted_resumable_upload_queries_drive_range_before_resuming() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let address = listener.local_addr().unwrap();
        let bytes = vec![b'y'; MULTIPART_UPLOAD_LIMIT + 1];
        let server_bytes = bytes.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let _ = read_request_body(&mut stream, &request);
            let session = format!("http://{address}/upload/session/recover");
            respond(&mut stream, "200 OK", &[("Location", &session)], b"");

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("PUT /upload/session/recover "));
            let first_attempt = read_request_body(&mut stream, &request);
            assert_eq!(first_attempt, server_bytes);
            drop(stream);

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.contains(&format!("content-range: bytes */{}", server_bytes.len())));
            assert!(read_request_body(&mut stream, &request).is_empty());
            respond(
                &mut stream,
                "308 Resume Incomplete",
                &[("Range", "bytes=0-4")],
                b"",
            );

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.contains(&format!(
                "content-range: bytes 5-{}/{total}",
                server_bytes.len() - 1,
                total = server_bytes.len()
            )));
            assert_eq!(read_request_body(&mut stream, &request), server_bytes[5..]);
            respond(
                &mut stream,
                "200 OK",
                &[("Content-Type", "application/json")],
                br#"{"id":"recovered-object","name":"large"}"#,
            );
        });

        let result = http.upload(
            "test-only-token",
            "objects-folder",
            "large-object",
            None,
            &bytes,
        );
        assert_eq!(result.unwrap(), "recovered-object");
        server.join().unwrap();
    }

    #[test]
    fn resumable_upload_surfaces_unauthorized_before_sending_another_chunk() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let address = listener.local_addr().unwrap();
        let bytes = vec![b'z'; RESUMABLE_CHUNK_SIZE + 1];
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let _ = read_request_body(&mut stream, &request);
            let session = format!("http://{address}/upload/session/unauthorized");
            respond(&mut stream, "200 OK", &[("Location", &session)], b"");

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let _ = read_request_body(&mut stream, &request);
            respond(
                &mut stream,
                "401 Unauthorized",
                &[("Content-Type", "application/json")],
                br#"{"error":"invalid credentials"}"#,
            );
        });

        let error = http
            .upload(
                "test-only-token",
                "objects-folder",
                "large-object",
                None,
                &bytes,
            )
            .unwrap_err();
        assert!(matches!(error, SyncError::Authentication(_)));
        server.join().unwrap();
    }

    #[test]
    fn query_literals_escape_drive_query_metacharacters() {
        assert_eq!(escape_query_literal("a\\b'c"), "a\\\\b\\'c");
    }

    #[test]
    fn request_failures_do_not_expose_tokens_or_response_body() {
        let error = response_error(StatusCode::UNAUTHORIZED, "download Google Drive file");
        assert_eq!(
            error.to_string(),
            "sync authentication failure: download Google Drive file: HTTP 401"
        );
        assert!(!error.to_string().contains("test-only-token"));
        assert!(!error.to_string().contains("private response"));
        assert!(is_retryable(
            StatusCode::FORBIDDEN,
            br#"{"error":{"errors":[{"reason":"rateLimitExceeded"}]}}"#
        ));
        assert!(!is_retryable(
            StatusCode::FORBIDDEN,
            br#"{"error":{"errors":[{"reason":"insufficientPermissions"}]}}"#
        ));
    }

    #[test]
    fn token_is_cached_and_unauthorized_operations_refresh_once() {
        let (source, calls, refreshes) = token_source();
        let mut store = GoogleDriveStore::new(source);
        assert_eq!(store.token(false).unwrap(), "test-only-token");
        assert_eq!(store.token(false).unwrap(), "test-only-token");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let attempts = AtomicUsize::new(0);
        let result = store.with_auth_retry("test operation", |_store| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(SyncError::Authentication("test operation: HTTP 401".into()))
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn verified_commits_are_reused_across_stores_only_while_drive_version_matches() {
        let root = std::env::temp_dir().join(format!("touchhle-commit-cache-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http = test_http(&listener);
        let commit = Commit {
            id: Uuid::new_v4(),
            device_id: Uuid::new_v4(),
            created_unix_ms: 1,
            parents: Vec::new(),
            entries: Default::default(),
        };
        let commit_bytes = serde_json::to_vec(&commit).unwrap();
        let filename = format!("{}.json", commit.id);
        let checksum = sha256_hex(&commit_bytes);
        let server = thread::spawn(move || {
            for (folder, version, download) in [
                ("commits-folder", "1", true),
                ("commits-folder", "1", false),
                ("commits-folder", "1", true),
                ("commits-folder", "2", true),
                ("other-account-folder", "2", true),
                ("other-account-folder", "", false),
                ("other-account-folder", "2", true),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.starts_with("GET /drive/v3/files?"));
                assert!(request.contains("version"));
                assert!(request.contains("sha256Checksum"));
                assert!(request.contains(&format!("q=%27{folder}%27+in+parents")));
                let listing = if version.is_empty() {
                    r#"{"files":[]}"#.to_owned()
                } else {
                    format!(
                        r#"{{"files":[{{"id":"file-1","name":"{filename}","mimeType":"application/json","version":"{version}","sha256Checksum":"{checksum}"}}]}}"#
                    )
                };
                respond(&mut stream, "200 OK", &[], listing.as_bytes());
                if download {
                    let (mut stream, _) = listener.accept().unwrap();
                    let request = read_request(&mut stream);
                    assert!(request.starts_with("GET /drive/v3/files/file-1?alt=media"));
                    respond(&mut stream, "200 OK", &[], &commit_bytes);
                }
            }
        });

        for (index, folder) in [
            "commits-folder",
            "commits-folder",
            "commits-folder",
            "commits-folder",
            "other-account-folder",
            "other-account-folder",
            "other-account-folder",
        ]
        .into_iter()
        .enumerate()
        {
            if index == 2 {
                let mut cached = commit_cache::load(&root).unwrap().unwrap();
                let mut forged = commit.clone();
                forged.created_unix_ms = 999;
                cached.entries.get_mut("file-1").unwrap().json =
                    serde_json::to_string(&forged).unwrap();
                commit_cache::save(&root, &cached).unwrap();
            }
            let (source, _, _) = token_source();
            let mut store = GoogleDriveStore::new(source).with_commit_cache(&root);
            store.http = http.clone();
            store
                .folder_ids
                .insert(OBJECTS_DIR.into(), "objects-folder".into());
            store.folder_ids.insert(COMMITS_DIR.into(), folder.into());
            if index == 5 {
                assert!(store.list_commits().unwrap().is_empty());
            } else {
                assert_eq!(store.list_commits().unwrap(), vec![commit.clone()]);
            }
        }
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parallel_reads_are_bounded_and_preserve_input_order() {
        let values: Vec<_> = (0..12).collect();
        let active = AtomicUsize::new(0);
        let max_active = AtomicUsize::new(0);
        let output = run_bounded(&values, 4, |value| {
            let active_now = active.fetch_add(1, Ordering::SeqCst) + 1;
            max_active.fetch_max(active_now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(5));
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(*value)
        })
        .unwrap();
        assert_eq!(output, values);
        assert!(max_active.load(Ordering::SeqCst) > 1);
        assert!(max_active.load(Ordering::SeqCst) <= 4);
    }

    #[test]
    fn commit_keys_and_managed_paths_remain_validated() {
        let id = Uuid::from_u128(123);
        assert_eq!(commit_id_for_name(&format!("{id}.json")).unwrap(), Some(id));
        assert!(commit_id_for_name("not-a-uuid.json").is_err());

        let path = RelativePath::new("touchHLE_sandbox/save/data").unwrap();
        assert_eq!(path.as_str(), "touchHLE_sandbox/save/data");
        assert!(RelativePath::new("other/escape").is_err());
    }

    #[test]
    fn upload_threshold_uses_multipart_for_small_content() {
        assert!(MULTIPART_UPLOAD_LIMIT > 0);
        assert!(b"small".len() <= MULTIPART_UPLOAD_LIMIT);
        assert!(MULTIPART_UPLOAD_LIMIT + 1 > MULTIPART_UPLOAD_LIMIT);
        assert_eq!(RESUMABLE_CHUNK_SIZE % (256 * 1024), 0);
    }

    #[test]
    fn drive_time_converts_utc_and_offsets() {
        assert_eq!(parse_drive_time("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(parse_drive_time("1970-01-01T09:00:00+09:00").unwrap(), 0);
        assert_eq!(
            parse_drive_time("2026-10-03T01:02:03.456Z").unwrap()
                - parse_drive_time("2026-10-03T01:02:03Z").unwrap(),
            456
        );
        assert!(parse_drive_time("2026-10-03T01:02:03+é:00").is_err());
    }

    #[test]
    fn object_verification_rejects_a_key_that_does_not_match_content() {
        let bytes = b"private save bytes";
        let wrong_hash = [0u8; 32];
        assert!(matches!(
            validate_object_key(wrong_hash, bytes),
            Err(SyncError::Integrity(_))
        ));
        assert!(matches!(
            verify_uploaded_bytes("uploaded object", bytes, Some(b"corrupt".to_vec())),
            Err(SyncError::Integrity(_))
        ));
        assert!(matches!(
            verify_uploaded_bytes("uploaded object", bytes, None),
            Err(SyncError::Integrity(_))
        ));
    }
}
