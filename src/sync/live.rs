/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::model::{RelativePath, SyncError};
use crate::paths::{APPS_DIR, SANDBOX_DIR};
use cap_fs_ext::DirExt;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, Metadata};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

mod worker;
pub use worker::{LiveControl, LiveObserverWorker};

const QUIET: Duration = Duration::from_secs(1);
const RECONCILE: Duration = Duration::from_secs(5);
const WATCH_POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug)]
pub enum LiveEvent {
    Changed(PathBuf),
    RootChanged {
        root: ManagedRoot,
        kind: RootMutationKind,
    },
    Rescan(RescanScope),
    Reconcile,
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedRoot {
    Apps,
    Sandbox,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootIdentity {
    Missing,
    Present(Option<(u64, u64)>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootMutationKind {
    Created,
    Removed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RescanScope {
    Apps,
    Sandbox,
    Both,
}

impl RescanScope {
    fn includes(self, root_name: &str) -> bool {
        matches!(
            (self, root_name),
            (Self::Apps | Self::Both, APPS_DIR) | (Self::Sandbox | Self::Both, SANDBOX_DIR)
        )
    }

    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Both, _) | (_, Self::Both) => Self::Both,
            (Self::Apps, Self::Sandbox) | (Self::Sandbox, Self::Apps) => Self::Both,
            (scope, _) => scope,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventTarget {
    Path(RelativePath),
    Rescan(RescanScope),
}

fn scope_for_path(root: &Path, path: &Path) -> Option<RescanScope> {
    let relative = relative_to_root(root, path)?;
    let first = relative.components().next()?;
    match first {
        Component::Normal(name) if name == APPS_DIR => Some(RescanScope::Apps),
        Component::Normal(name) if name == SANDBOX_DIR => Some(RescanScope::Sandbox),
        _ => None,
    }
}

pub fn classify_event(root: &Path, path: &Path) -> Option<EventTarget> {
    let relative = relative_to_root(root, path)?;
    let scope = scope_for_path(root, path)?;
    let mut components = relative.components();
    components.next()?;
    if components.next().is_none() {
        return Some(EventTarget::Rescan(scope));
    }
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Some(EventTarget::Rescan(scope));
    }
    // Missing files still need to be represented, to capture deletions.
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => Some(EventTarget::Rescan(scope)),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Some(EventTarget::Rescan(scope))
        }
        _ => match relative
            .to_str()
            .and_then(|name| RelativePath::new(name).ok())
        {
            Some(path) => Some(EventTarget::Path(path)),
            None => Some(EventTarget::Rescan(scope)),
        },
    }
}

pub fn affects_apps(root: &Path, event: &LiveEvent) -> bool {
    match event {
        LiveEvent::Changed(path) => relative_to_root(root, path).is_some_and(|path| {
            matches!(
                path.components().next(),
                Some(Component::Normal(name)) if name == APPS_DIR
            )
        }),
        LiveEvent::RootChanged {
            root: ManagedRoot::Apps,
            ..
        } => true,
        LiveEvent::RootChanged {
            root: ManagedRoot::Sandbox,
            ..
        } => false,
        LiveEvent::Rescan(RescanScope::Apps | RescanScope::Both) | LiveEvent::Failed(_) => true,
        LiveEvent::Rescan(RescanScope::Sandbox) | LiveEvent::Reconcile => false,
    }
}

fn relative_to_root(root: &Path, path: &Path) -> Option<PathBuf> {
    if let Ok(relative) = path.strip_prefix(root) {
        return Some(relative.to_path_buf());
    }
    path.strip_prefix(fs::canonicalize(root).ok()?)
        .ok()
        .map(Path::to_path_buf)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileSignature {
    size: u64,
    modified: Option<SystemTime>,
    identity: Option<(u64, u64)>,
}

fn signature(metadata: &Metadata) -> FileSignature {
    #[cfg(unix)]
    let identity = {
        use cap_std::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    };
    #[cfg(windows)]
    let identity = {
        use cap_std::fs::MetadataExt;
        metadata
            .volume_serial_number()
            .zip(metadata.file_index())
            .map(|(volume, index)| (u64::from(volume), index))
    };
    #[cfg(not(any(unix, windows)))]
    let identity = None;
    FileSignature {
        size: metadata.len(),
        modified: metadata.modified().ok().map(|time| time.into_std()),
        identity,
    }
}

// Traversal uses no-follow directory handles; no symlink or non-regular file is emitted.
struct ScanResult {
    signatures: BTreeMap<RelativePath, FileSignature>,
    validation_error: Option<SyncError>,
}

fn scan_signatures(root: &Path, scope: RescanScope) -> Result<ScanResult, SyncError> {
    let base = Dir::open_ambient_dir(root, ambient_authority())?;
    let mut signatures = BTreeMap::new();
    let mut validation_error = None;
    for name in [APPS_DIR, SANDBOX_DIR] {
        if !scope.includes(name) {
            continue;
        }
        match base.symlink_metadata(name) {
            Ok(metadata) if metadata.is_dir() => {
                scan_directory(
                    &base.open_dir_nofollow(name)?,
                    name,
                    &mut signatures,
                    &mut validation_error,
                )?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(ScanResult {
        signatures,
        validation_error,
    })
}

fn signature_for_path(
    root: &Path,
    path: &RelativePath,
) -> Result<Option<FileSignature>, SyncError> {
    signature_for_path_with(root, path, |_| {})
}

fn signature_for_path_with(
    root: &Path,
    path: &RelativePath,
    mut visited_directory: impl FnMut(&str),
) -> Result<Option<FileSignature>, SyncError> {
    let mut components = path.as_str().split('/');
    let root_name = components
        .next()
        .ok_or_else(|| SyncError::InvalidPath(path.as_str().to_owned()))?;
    if ![APPS_DIR, SANDBOX_DIR].contains(&root_name) {
        return Err(SyncError::InvalidPath(path.as_str().to_owned()));
    }
    let remaining: Vec<_> = components.collect();
    if remaining.is_empty() || remaining.iter().any(|component| component.is_empty()) {
        return Err(SyncError::InvalidPath(path.as_str().to_owned()));
    }
    let base = Dir::open_ambient_dir(root, ambient_authority())?;
    let mut directory = match base.symlink_metadata(root_name) {
        Ok(metadata) if metadata.is_dir() => base.open_dir_nofollow(root_name)?,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    visited_directory(root_name);
    for component in &remaining[..remaining.len() - 1] {
        match directory.symlink_metadata(component) {
            Ok(metadata) if metadata.is_dir() => {
                directory = directory.open_dir_nofollow(component)?;
                visited_directory(component);
            }
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }
    let leaf = remaining.last().unwrap();
    match directory.symlink_metadata(leaf) {
        Ok(metadata) if metadata.is_file() => Ok(Some(signature(&metadata))),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn scan_directory(
    dir: &Dir,
    prefix: &str,
    found: &mut BTreeMap<RelativePath, FileSignature>,
    validation_error: &mut Option<SyncError>,
) -> Result<(), SyncError> {
    for entry in dir.read_dir(".")? {
        let entry = entry?;
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => {
                validation_error
                    .get_or_insert_with(|| SyncError::InvalidPath("managed entry rejected".into()));
                continue;
            }
        };
        let metadata = dir.symlink_metadata(&name)?;
        if !metadata.is_dir() && !metadata.is_file() {
            continue;
        }
        let relative = match RelativePath::new(&format!("{prefix}/{name}")) {
            Ok(relative) => relative,
            Err(_) => {
                validation_error
                    .get_or_insert_with(|| SyncError::InvalidPath("managed entry rejected".into()));
                continue;
            }
        };
        if metadata.is_dir() {
            scan_directory(
                &dir.open_dir_nofollow(&name)?,
                relative.as_str(),
                found,
                validation_error,
            )?;
        } else {
            found.insert(relative, signature(&metadata));
        }
    }
    Ok(())
}

pub struct DirtySet {
    root: PathBuf,
    due: BTreeMap<RelativePath, Instant>,
    rescan_due: Option<(Instant, RescanScope, bool)>,
    staged: BTreeMap<RelativePath, FileSignature>,
    reconciled: BTreeMap<RelativePath, FileSignature>,
    reconciled_roots: BTreeSet<&'static str>,
    reconciled_due: BTreeSet<RelativePath>,
    scan_error: Option<SyncError>,
    observer_error: Option<String>,
}

impl DirtySet {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            due: BTreeMap::new(),
            rescan_due: None,
            staged: BTreeMap::new(),
            reconciled: BTreeMap::new(),
            reconciled_roots: BTreeSet::new(),
            reconciled_due: BTreeSet::new(),
            scan_error: None,
            observer_error: None,
        }
    }

    pub fn record(&mut self, event: LiveEvent, now: Instant) {
        match event {
            LiveEvent::Changed(path) => match classify_event(&self.root, &path) {
                Some(EventTarget::Path(path)) => {
                    self.due.insert(path, now + QUIET);
                }
                Some(EventTarget::Rescan(scope)) => self.schedule_rescan(scope, now, false),
                None => {}
            },
            LiveEvent::RootChanged { root, .. } => self.schedule_rescan(
                match root {
                    ManagedRoot::Apps => RescanScope::Apps,
                    ManagedRoot::Sandbox => RescanScope::Sandbox,
                },
                now,
                false,
            ),
            LiveEvent::Rescan(scope) => self.schedule_rescan(scope, now, false),
            LiveEvent::Reconcile => self.schedule_rescan(RescanScope::Both, now, true),
            LiveEvent::Failed(message) => {
                self.observer_error = Some(message);
                self.schedule_rescan(RescanScope::Both, now, false);
            }
        }
    }

    fn schedule_rescan(&mut self, scope: RescanScope, now: Instant, reconcile: bool) {
        let requested_due = now + QUIET;
        self.rescan_due = Some(match self.rescan_due {
            Some((due, previous_scope, was_reconcile)) => (
                due.min(requested_due),
                previous_scope.merge(scope),
                was_reconcile || reconcile,
            ),
            None => (requested_due, scope, reconcile),
        });
    }

    pub fn current_signature(
        &self,
        path: &RelativePath,
    ) -> Result<Option<FileSignature>, SyncError> {
        signature_for_path(&self.root, path)
    }

    // Pass the signature of the version actually uploaded, not a fresh post-upload stat.
    pub fn mark_staged(&mut self, path: &RelativePath, uploaded: Option<FileSignature>) {
        if let Some(signature) = uploaded {
            self.staged.insert(path.clone(), signature);
        } else {
            self.staged.remove(path);
        }
    }

    pub fn take_scan_error(&mut self) -> Option<SyncError> {
        self.scan_error.take()
    }

    pub fn take_observer_error(&mut self) -> Option<String> {
        self.observer_error.take()
    }

    #[cfg(test)]
    pub fn take_due(&mut self, now: Instant) -> Vec<RelativePath> {
        self.take_due_with_reconciled(now).0
    }

    pub fn take_due_with_reconciled(
        &mut self,
        now: Instant,
    ) -> (Vec<RelativePath>, Vec<RelativePath>) {
        if let Some((_, scope, reconcile)) = self.rescan_due.filter(|(due, _, _)| now >= *due) {
            match scan_signatures(&self.root, scope) {
                Ok(scan) => {
                    let scan_is_valid = scan.validation_error.is_none();
                    let mut changed_since_reconcile = BTreeSet::new();
                    let mut initial_reconcile_roots = BTreeSet::new();
                    if scan_is_valid {
                        for root_name in [APPS_DIR, SANDBOX_DIR] {
                            if !scope.includes(root_name) {
                                continue;
                            }
                            if self.reconciled_roots.contains(root_name) {
                                for (path, previous) in &self.reconciled {
                                    if path.as_str().split('/').next() == Some(root_name)
                                        && scan.signatures.get(path) != Some(previous)
                                    {
                                        changed_since_reconcile.insert(path.clone());
                                    }
                                }
                                for (path, current) in &scan.signatures {
                                    if path.as_str().split('/').next() == Some(root_name)
                                        && self.reconciled.get(path) != Some(current)
                                    {
                                        changed_since_reconcile.insert(path.clone());
                                    }
                                }
                            } else if reconcile {
                                initial_reconcile_roots.insert(root_name);
                            }
                            self.reconciled.retain(|path, _| {
                                path.as_str().split('/').next() != Some(root_name)
                            });
                            self.reconciled.extend(
                                scan.signatures
                                    .iter()
                                    .filter(|(path, _)| {
                                        path.as_str().split('/').next() == Some(root_name)
                                    })
                                    .map(|(path, signature)| (path.clone(), signature.clone())),
                            );
                            self.reconciled_roots.insert(root_name);
                        }
                    }
                    if reconcile && scan_is_valid {
                        for path in changed_since_reconcile {
                            self.due
                                .entry(path.clone())
                                .and_modify(|due| *due = (*due).min(now))
                                .or_insert(now);
                            self.reconciled_due.insert(path);
                        }
                    }
                    // The first full reconciliation establishes the observer's baseline.
                    // Preserve explicit file events received before the scan so changes
                    // racing startup are still uploaded.
                    for (path, signature) in &scan.signatures {
                        let root_name = path.as_str().split('/').next().unwrap_or_default();
                        if initial_reconcile_roots.contains(root_name)
                            && !self.due.contains_key(path)
                        {
                            self.staged
                                .entry(path.clone())
                                .or_insert_with(|| signature.clone());
                            if root_name == APPS_DIR {
                                self.due.entry(path.clone()).or_insert(now);
                                self.reconciled_due.insert(path.clone());
                            }
                        }
                    }
                    for path in scan.signatures.keys().chain(self.staged.keys()) {
                        if scope.includes(path.as_str().split('/').next().unwrap_or_default())
                            && scan.signatures.get(path) != self.staged.get(path)
                        {
                            self.due.entry(path.clone()).or_insert(now);
                        }
                    }
                    if let Some(error) = scan.validation_error {
                        self.scan_error = Some(error);
                        self.rescan_due = Some((now + RECONCILE, scope, reconcile));
                    } else {
                        self.rescan_due = None;
                        self.scan_error = None;
                    }
                }
                Err(error) => {
                    self.scan_error = Some(error);
                    self.rescan_due = Some((now + RECONCILE, scope, reconcile));
                }
            }
        }
        let ready: Vec<_> = self
            .due
            .iter()
            .filter_map(|(path, due)| (now >= *due).then_some(path.clone()))
            .collect();
        for path in &ready {
            self.due.remove(path);
        }
        let reconciled_ready = ready
            .iter()
            .filter(|path| self.reconciled_due.remove(*path))
            .cloned()
            .collect();
        (ready, reconciled_ready)
    }
}

// The worker owns the watcher, so callback threads only enqueue into its channel.
pub struct LiveObserver {
    stop: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl LiveObserver {
    pub fn start(root: &Path, sender: Sender<LiveEvent>) -> Result<Self, SyncError> {
        let root = root.to_path_buf();
        let (events_tx, events_rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = events_tx.send(event);
        })
        .map_err(|error| SyncError::Integrity(format!("observer startup failed: {error}")))?;
        watcher
            .watch(&root, RecursiveMode::NonRecursive)
            .map_err(|error| {
                SyncError::Integrity(format!("observer parent watch failed: {error}"))
            })?;
        let mut root_identities = BTreeMap::new();
        for name in [APPS_DIR, SANDBOX_DIR] {
            let path = root.join(name);
            root_identities.insert(path.clone(), root_identity(&path)?);
        }
        let (stop, stop_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut watched: BTreeMap<PathBuf, FileSignature> = BTreeMap::new();
            let mut failed_at = BTreeMap::new();
            let mut last_scan = Instant::now();
            rearm_roots(
                &mut watcher,
                &root,
                &sender,
                &mut watched,
                &mut failed_at,
                &mut root_identities,
            );
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match events_rx.recv_timeout(WATCH_POLL) {
                    Ok(Ok(event)) => {
                        forward_event(&root, event, &sender);
                        rearm_roots(
                            &mut watcher,
                            &root,
                            &sender,
                            &mut watched,
                            &mut failed_at,
                            &mut root_identities,
                        );
                    }
                    Ok(Err(error)) => {
                        let _ = sender.send(LiveEvent::Failed(error.to_string()));
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let _ = sender.send(LiveEvent::Failed("observer disconnected".into()));
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if last_scan.elapsed() >= RECONCILE {
                    let _ = sender.send(LiveEvent::Reconcile);
                    last_scan = Instant::now();
                }
                rearm_roots(
                    &mut watcher,
                    &root,
                    &sender,
                    &mut watched,
                    &mut failed_at,
                    &mut root_identities,
                );
            }
        });
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for LiveObserver {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn rearm_roots(
    watcher: &mut impl Watcher,
    root: &Path,
    sender: &Sender<LiveEvent>,
    watched: &mut BTreeMap<PathBuf, FileSignature>,
    failed_at: &mut BTreeMap<PathBuf, Instant>,
    root_identities: &mut BTreeMap<PathBuf, RootIdentity>,
) {
    for name in [APPS_DIR, SANDBOX_DIR] {
        let path = root.join(name);
        let (current, identity) = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                let signature = host_signature(&metadata);
                (
                    Some(signature.clone()),
                    RootIdentity::Present(signature.identity),
                )
            }
            Ok(_) => (None, RootIdentity::Missing),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (None, RootIdentity::Missing)
            }
            Err(error) => {
                let _ = sender.send(LiveEvent::Failed(error.to_string()));
                continue;
            }
        };
        let managed_root = if name == APPS_DIR {
            ManagedRoot::Apps
        } else {
            ManagedRoot::Sandbox
        };
        report_root_transition(&path, managed_root, identity, root_identities, sender);
        if watched.get(&path) == current.as_ref() {
            continue;
        }
        if failed_at
            .get(&path)
            .is_some_and(|at| at.elapsed() < RECONCILE)
        {
            continue;
        }
        if watched.remove(&path).is_some() {
            let _ = watcher.unwatch(&path);
        }
        if let Some(identity) = current {
            match watcher.watch(&path, RecursiveMode::Recursive) {
                Ok(()) => {
                    failed_at.remove(&path);
                    watched.insert(path, identity);
                }
                Err(error) => {
                    failed_at.insert(path, Instant::now());
                    let _ = sender.send(LiveEvent::Failed(error.to_string()));
                }
            }
        } else {
            failed_at.remove(&path);
        }
        let scope = if name == APPS_DIR {
            RescanScope::Apps
        } else {
            RescanScope::Sandbox
        };
        let _ = sender.send(LiveEvent::Rescan(scope));
    }
}

fn root_identity(path: &Path) -> Result<RootIdentity, SyncError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            Ok(RootIdentity::Present(host_signature(&metadata).identity))
        }
        Ok(_) => Ok(RootIdentity::Missing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RootIdentity::Missing),
        Err(error) => Err(error.into()),
    }
}

fn root_identity_transitions(
    previous: RootIdentity,
    current: RootIdentity,
) -> Vec<RootMutationKind> {
    match (previous, current) {
        (RootIdentity::Missing, RootIdentity::Present(_)) => vec![RootMutationKind::Created],
        (RootIdentity::Present(_), RootIdentity::Missing) => vec![RootMutationKind::Removed],
        (RootIdentity::Present(Some(previous)), RootIdentity::Present(Some(current)))
            if previous != current =>
        {
            vec![RootMutationKind::Removed, RootMutationKind::Created]
        }
        _ => Vec::new(),
    }
}

fn report_root_transition(
    path: &Path,
    root: ManagedRoot,
    identity: RootIdentity,
    previous: &mut BTreeMap<PathBuf, RootIdentity>,
    sender: &Sender<LiveEvent>,
) {
    // Compare actual root identity, not callback timing or event kind.
    if let Some(last) = previous.insert(path.to_path_buf(), identity) {
        for kind in root_identity_transitions(last, identity) {
            let _ = sender.send(LiveEvent::RootChanged { root, kind });
        }
    }
}

fn host_signature(metadata: &fs::Metadata) -> FileSignature {
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    };
    #[cfg(windows)]
    let identity = {
        use std::os::windows::fs::MetadataExt;
        metadata
            .volume_serial_number()
            .zip(metadata.file_index())
            .map(|(volume, index)| (u64::from(volume), index))
    };
    #[cfg(not(any(unix, windows)))]
    let identity = None;
    FileSignature {
        size: if identity.is_some() {
            0
        } else {
            metadata.len()
        },
        modified: if identity.is_some() {
            None
        } else {
            metadata.modified().ok()
        },
        identity,
    }
}

fn forward_event(root: &Path, event: Event, sender: &Sender<LiveEvent>) {
    if event.paths.is_empty() {
        let _ = sender.send(LiveEvent::Rescan(RescanScope::Both));
        return;
    }
    let need_rescan = event.need_rescan();
    let directory_event = matches!(
        &event.kind,
        EventKind::Remove(_) | EventKind::Any | EventKind::Other
    );
    let mut paths = BTreeSet::new();
    let mut rescan_scope = None;
    let mut path_scope = None;
    for path in event.paths {
        match classify_event(root, &path) {
            Some(EventTarget::Path(_)) if !directory_event => {
                if let Some(scope) = scope_for_path(root, &path) {
                    path_scope = Some(
                        path_scope.map_or(scope, |previous: RescanScope| previous.merge(scope)),
                    );
                }
                paths.insert(path);
            }
            Some(EventTarget::Path(_)) => {
                if let Some(scope) = scope_for_path(root, &path) {
                    rescan_scope = Some(
                        rescan_scope.map_or(scope, |previous: RescanScope| previous.merge(scope)),
                    );
                }
            }
            Some(EventTarget::Rescan(scope)) => {
                rescan_scope =
                    Some(rescan_scope.map_or(scope, |previous: RescanScope| previous.merge(scope)));
            }
            None => {}
        }
    }
    if need_rescan {
        let _ = sender.send(LiveEvent::Rescan(RescanScope::Both));
        return;
    }
    if let Some(scope) = rescan_scope {
        let scope = path_scope.map_or(scope, |path_scope| scope.merge(path_scope));
        let _ = sender.send(LiveEvent::Rescan(scope));
        return;
    }
    for path in paths {
        let _ = sender.send(LiveEvent::Changed(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::APPS_DIR;
    use std::fs;
    use std::sync::mpsc;
    use uuid::Uuid;

    struct TestTree(PathBuf);

    impl TestTree {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("touchhle-live-{}", Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn file(&self, relative: &str) -> PathBuf {
            self.0.join(relative)
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn path(relative: &str) -> RelativePath {
        RelativePath::new(relative).unwrap()
    }

    #[test]
    fn duplicate_writes_wait_for_one_second_and_coalesce() {
        let tree = TestTree::new();
        let file = tree.file("touchHLE_apps/a.ipa");
        let mut dirty = DirtySet::new(tree.0.clone());
        let start = Instant::now();
        dirty.record(LiveEvent::Changed(file.clone()), start);
        dirty.record(LiveEvent::Changed(file), start);
        assert!(dirty
            .take_due(start + Duration::from_millis(999))
            .is_empty());
        assert_eq!(
            dirty.take_due(start + Duration::from_secs(1)),
            vec![path("touchHLE_apps/a.ipa")]
        );
        assert!(dirty.take_due(start + Duration::from_secs(2)).is_empty());
    }

    #[test]
    fn rename_dirties_both_nested_paths() {
        let tree = TestTree::new();
        let mut dirty = DirtySet::new(tree.0.clone());
        let start = Instant::now();
        dirty.record(
            LiveEvent::Changed(tree.file("touchHLE_sandbox/a/old")),
            start,
        );
        dirty.record(
            LiveEvent::Changed(tree.file("touchHLE_sandbox/a/new")),
            start,
        );
        assert_eq!(
            dirty.take_due(start + Duration::from_secs(1)),
            vec![
                path("touchHLE_sandbox/a/new"),
                path("touchHLE_sandbox/a/old")
            ]
        );
    }

    #[test]
    fn repeated_rescans_preserve_first_deadline_and_merge_scopes() {
        let tree = TestTree::new();
        fs::create_dir(tree.file(APPS_DIR)).unwrap();
        fs::create_dir(tree.file(SANDBOX_DIR)).unwrap();
        fs::write(tree.file("touchHLE_apps/a"), b"a").unwrap();
        fs::write(tree.file("touchHLE_sandbox/b"), b"b").unwrap();
        let mut dirty = DirtySet::new(tree.0.clone());
        let start = Instant::now();
        dirty.record(LiveEvent::Rescan(RescanScope::Apps), start);
        dirty.record(
            LiveEvent::Rescan(RescanScope::Sandbox),
            start + Duration::from_millis(500),
        );
        assert!(dirty
            .take_due(start + Duration::from_millis(999))
            .is_empty());
        assert_eq!(
            dirty.take_due(start + Duration::from_secs(1)),
            vec![path("touchHLE_apps/a"), path("touchHLE_sandbox/b")]
        );
        assert!(dirty.take_due(start + Duration::from_secs(2)).is_empty());
    }

    #[test]
    fn directory_removal_root_creation_and_failures_rescan() {
        let tree = TestTree::new();
        let mut dirty = DirtySet::new(tree.0.clone());
        let start = Instant::now();
        fs::create_dir(tree.file(APPS_DIR)).unwrap();
        fs::write(tree.file("touchHLE_apps/first"), b"1").unwrap();
        dirty.record(LiveEvent::Changed(tree.file(APPS_DIR)), start);
        assert_eq!(
            dirty.take_due(start + Duration::from_secs(1)),
            vec![path("touchHLE_apps/first")]
        );
        let file = path("touchHLE_apps/first");
        let staged = dirty.current_signature(&file).unwrap();
        dirty.mark_staged(&file, staged);
        fs::remove_dir_all(tree.file(APPS_DIR)).unwrap();
        dirty.record(
            LiveEvent::Rescan(RescanScope::Apps),
            start + Duration::from_secs(2),
        );
        assert_eq!(
            dirty.take_due(start + Duration::from_secs(3)),
            vec![path("touchHLE_apps/first")]
        );
        dirty.record(
            LiveEvent::Failed("overflow".into()),
            start + Duration::from_secs(4),
        );
        assert_eq!(dirty.take_observer_error().as_deref(), Some("overflow"));
        assert_eq!(
            dirty.take_due(start + Duration::from_secs(5)),
            vec![path("touchHLE_apps/first")]
        );
    }

    #[test]
    fn unrelated_parent_events_are_ignored_and_invalid_managed_paths_rescan() {
        let tree = TestTree::new();
        assert_eq!(classify_event(&tree.0, &tree.file("unrelated")), None);
        assert_eq!(
            classify_event(&tree.0, &tree.file(APPS_DIR)),
            Some(EventTarget::Rescan(RescanScope::Apps))
        );
        assert_eq!(
            classify_event(&tree.0, &tree.file(SANDBOX_DIR)),
            Some(EventTarget::Rescan(RescanScope::Sandbox))
        );
        assert_eq!(
            classify_event(&tree.0, &tree.file("touchHLE_apps/../escape")),
            Some(EventTarget::Rescan(RescanScope::Apps))
        );
        let mut dirty = DirtySet::new(tree.0.clone());
        let start = Instant::now();
        dirty.record(LiveEvent::Changed(tree.file("unrelated")), start);
        assert!(dirty.take_due(start + Duration::from_secs(2)).is_empty());
    }

    #[test]
    fn metadata_reconciliation_catches_missed_changes_and_deletions() {
        let tree = TestTree::new();
        fs::create_dir_all(tree.file("touchHLE_sandbox/nested")).unwrap();
        let file = path("touchHLE_sandbox/nested/save");
        fs::write(tree.file(file.as_str()), b"one").unwrap();
        let mut dirty = DirtySet::new(tree.0.clone());
        let staged = dirty.current_signature(&file).unwrap();
        dirty.mark_staged(&file, staged);
        let start = Instant::now();
        dirty.record(LiveEvent::Rescan(RescanScope::Sandbox), start);
        assert!(dirty.take_due(start + Duration::from_secs(1)).is_empty());
        fs::write(tree.file(file.as_str()), b"longer").unwrap();
        dirty.record(
            LiveEvent::Rescan(RescanScope::Sandbox),
            start + Duration::from_secs(2),
        );
        assert_eq!(
            dirty.take_due(start + Duration::from_secs(3)),
            vec![file.clone()]
        );
        let staged = dirty.current_signature(&file).unwrap();
        dirty.mark_staged(&file, staged);
        fs::remove_file(tree.file(file.as_str())).unwrap();
        dirty.record(
            LiveEvent::Rescan(RescanScope::Sandbox),
            start + Duration::from_secs(4),
        );
        assert_eq!(dirty.take_due(start + Duration::from_secs(5)), vec![file]);
    }

    #[test]
    fn real_observer_detects_missing_root_nested_rename_and_deletion() {
        let tree = TestTree::new();
        let (sender, receiver) = mpsc::channel();
        let _observer = LiveObserver::start(&tree.0, sender).unwrap();
        let mut dirty = DirtySet::new(tree.0.clone());

        drain_observer_events(&receiver);
        fs::create_dir(tree.file(APPS_DIR)).unwrap();
        fs::create_dir(tree.file("touchHLE_apps/nested")).unwrap();
        fs::write(tree.file("touchHLE_apps/nested/a"), b"a").unwrap();
        let old = path("touchHLE_apps/nested/a");
        receive_root_change_and_reconcile(
            &receiver,
            &mut dirty,
            RootMutationKind::Created,
            &[old.clone()],
            Duration::from_secs(8),
        );
        let staged = dirty.current_signature(&old).unwrap();
        dirty.mark_staged(&old, staged);

        fs::rename(
            tree.file("touchHLE_apps/nested/a"),
            tree.file("touchHLE_apps/nested/b"),
        )
        .unwrap();
        let new = path("touchHLE_apps/nested/b");
        receive_reconciled_paths(
            &receiver,
            &tree.0,
            &mut dirty,
            &[old.clone(), new.clone()],
            Duration::from_secs(8),
        );
        assert!(!tree.file(old.as_str()).exists());
        assert!(tree.file(new.as_str()).is_file());
        let staged = dirty.current_signature(&new).unwrap();
        dirty.mark_staged(&new, staged);
        dirty.mark_staged(&old, None);

        let delete_me = path("touchHLE_apps/nested/delete-me");
        fs::write(tree.file(delete_me.as_str()), b"delete me").unwrap();
        receive_reconciled_paths(
            &receiver,
            &tree.0,
            &mut dirty,
            &[delete_me.clone()],
            Duration::from_secs(8),
        );
        let staged = dirty.current_signature(&delete_me).unwrap();
        dirty.mark_staged(&delete_me, staged);
        fs::remove_file(tree.file(delete_me.as_str())).unwrap();
        receive_reconciled_paths(
            &receiver,
            &tree.0,
            &mut dirty,
            &[delete_me.clone()],
            Duration::from_secs(8),
        );
        assert!(!tree.file(delete_me.as_str()).exists());
        dirty.mark_staged(&delete_me, None);
    }

    #[test]
    fn real_observer_reports_exact_empty_root_creation_and_removal() {
        let tree = TestTree::new();
        let (sender, receiver) = mpsc::channel();
        let _observer = LiveObserver::start(&tree.0, sender).unwrap();
        let mut dirty = DirtySet::new(tree.0.clone());
        let staged_path = path("touchHLE_apps/staged.ipa");

        drain_observer_events(&receiver);
        fs::create_dir(tree.file(APPS_DIR)).unwrap();
        receive_root_change_and_reconcile(
            &receiver,
            &mut dirty,
            RootMutationKind::Created,
            &[],
            Duration::from_secs(8),
        );
        fs::write(tree.file(staged_path.as_str()), b"staged contents").unwrap();
        receive_reconciled_paths(
            &receiver,
            &tree.0,
            &mut dirty,
            &[staged_path.clone()],
            Duration::from_secs(8),
        );
        let signature = dirty.current_signature(&staged_path).unwrap();
        dirty.mark_staged(&staged_path, signature);

        fs::remove_file(tree.file(staged_path.as_str())).unwrap();
        receive_reconciled_paths(
            &receiver,
            &tree.0,
            &mut dirty,
            &[staged_path.clone()],
            Duration::from_secs(8),
        );
        wait_for_observer_quiet(
            &receiver,
            Duration::from_millis(250),
            Duration::from_secs(3),
        );
        assert_eq!(fs::read_dir(tree.file(APPS_DIR)).unwrap().count(), 0);
        drain_observer_events(&receiver);

        fs::remove_dir(tree.file(APPS_DIR)).unwrap();
        receive_root_change_and_reconcile(
            &receiver,
            &mut dirty,
            RootMutationKind::Removed,
            &[staged_path.clone()],
            Duration::from_secs(8),
        );
        assert!(!tree.file(APPS_DIR).exists());
    }

    #[test]
    fn real_observer_reconciles_populated_root_moved_outside_managed_path() {
        let tree = TestTree::new();
        fs::create_dir(tree.file(APPS_DIR)).unwrap();
        let staged_path = path("touchHLE_apps/staged.ipa");
        fs::write(tree.file(staged_path.as_str()), b"staged contents").unwrap();

        let (sender, receiver) = mpsc::channel();
        let _observer = LiveObserver::start(&tree.0, sender).unwrap();
        let mut dirty = DirtySet::new(tree.0.clone());
        let staged_signature = dirty.current_signature(&staged_path).unwrap();
        dirty.mark_staged(&staged_path, staged_signature);

        // Establish that the next periodic Reconcile must occur after the move.
        receive_periodic_reconcile(&receiver, Duration::from_secs(8));
        drain_observer_events(&receiver);
        fs::rename(tree.file(APPS_DIR), tree.file("parked-apps")).unwrap();
        assert!(!tree.file(APPS_DIR).exists());
        assert_eq!(
            fs::read(tree.file("parked-apps/staged.ipa")).unwrap(),
            b"staged contents"
        );

        receive_reconcile_and_assert_deleted(
            &receiver,
            &mut dirty,
            &staged_path,
            Duration::from_secs(9),
        );
    }

    #[test]
    fn staged_signature_cannot_acknowledge_a_newer_write() {
        let tree = TestTree::new();
        fs::create_dir(tree.file(APPS_DIR)).unwrap();
        let file = path("touchHLE_apps/save");
        fs::write(tree.file(file.as_str()), b"before").unwrap();
        let mut dirty = DirtySet::new(tree.0.clone());
        let uploaded = dirty.current_signature(&file).unwrap();
        fs::write(tree.file(file.as_str()), b"after upload").unwrap();
        dirty.mark_staged(&file, uploaded);
        let start = Instant::now();
        dirty.record(LiveEvent::Reconcile, start);
        assert_eq!(dirty.take_due(start + QUIET), vec![file]);
    }

    #[cfg(unix)]
    #[test]
    fn failed_scan_reports_error_and_retries_without_discarding_changes() {
        let tree = TestTree::new();
        fs::create_dir(tree.file(APPS_DIR)).unwrap();
        fs::write(tree.file("touchHLE_apps/CON"), b"invalid name").unwrap();
        fs::write(tree.file("touchHLE_apps/valid"), b"ok").unwrap();
        let mut dirty = DirtySet::new(tree.0.clone());
        let start = Instant::now();
        dirty.record(LiveEvent::Rescan(RescanScope::Apps), start);
        assert_eq!(
            dirty.take_due(start + QUIET),
            vec![path("touchHLE_apps/valid")]
        );
        let error = dirty.take_scan_error().unwrap();
        assert!(matches!(
            &error,
            SyncError::InvalidPath(reason) if reason == "managed entry rejected"
        ));
        assert!(!error.to_string().contains("CON"));
        assert_eq!(
            dirty.take_due(start + QUIET + RECONCILE),
            vec![path("touchHLE_apps/valid")]
        );
        assert!(matches!(
            dirty.take_scan_error(),
            Some(SyncError::InvalidPath(reason)) if reason == "managed entry rejected"
        ));
        fs::remove_file(tree.file("touchHLE_apps/CON")).unwrap();
        assert_eq!(
            dirty.take_due(start + QUIET + 2 * RECONCILE),
            vec![path("touchHLE_apps/valid")]
        );
        assert!(dirty.take_scan_error().is_none());
    }

    #[test]
    fn notify_rename_forwards_both_paths_and_ignores_unrelated_parent_events() {
        use notify::event::{ModifyKind, RemoveKind, RenameMode};
        let tree = TestTree::new();
        let (sender, receiver) = mpsc::channel();
        forward_event(
            &tree.0,
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(tree.file("touchHLE_sandbox/old"))
                .add_path(tree.file("touchHLE_sandbox/new")),
            &sender,
        );
        let paths = [receiver.try_recv().unwrap(), receiver.try_recv().unwrap()];
        assert!(paths.iter().any(|event| matches!(event, LiveEvent::Changed(path) if path == &tree.file("touchHLE_sandbox/old"))));
        assert!(paths.iter().any(|event| matches!(event, LiveEvent::Changed(path) if path == &tree.file("touchHLE_sandbox/new"))));
        forward_event(
            &tree.0,
            Event::new(EventKind::Any).add_path(tree.file("unrelated")),
            &sender,
        );
        assert!(receiver.try_recv().is_err());
        forward_event(&tree.0, Event::new(EventKind::Any), &sender);
        assert!(matches!(
            receiver.try_recv(),
            Ok(LiveEvent::Rescan(RescanScope::Both))
        ));
        forward_event(
            &tree.0,
            Event::new(EventKind::Remove(RemoveKind::Folder))
                .add_path(tree.file("touchHLE_sandbox/deleted-directory")),
            &sender,
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(LiveEvent::Rescan(RescanScope::Sandbox))
        ));
    }

    #[test]
    fn mixed_root_directory_rename_rescans_both_roots_and_finds_staged_deletion() {
        use notify::event::{ModifyKind, RenameMode};

        let tree = TestTree::new();
        fs::create_dir_all(tree.file("touchHLE_apps/moved")).unwrap();
        fs::create_dir_all(tree.file("touchHLE_sandbox")).unwrap();
        let old_child = path("touchHLE_apps/moved/save");
        let new_child = path("touchHLE_sandbox/moved/save");
        fs::write(tree.file(old_child.as_str()), b"staged save").unwrap();

        let mut dirty = DirtySet::new(tree.0.clone());
        let staged = dirty.current_signature(&old_child).unwrap();
        dirty.mark_staged(&old_child, staged);
        fs::rename(
            tree.file("touchHLE_apps/moved"),
            tree.file("touchHLE_sandbox/moved"),
        )
        .unwrap();

        let (sender, receiver) = mpsc::channel();
        forward_event(
            &tree.0,
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(tree.file("touchHLE_apps/moved"))
                .add_path(tree.file("touchHLE_sandbox/moved")),
            &sender,
        );
        let events: Vec<_> = receiver.try_iter().collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events.first(),
            Some(LiveEvent::Rescan(RescanScope::Both))
        ));

        let start = Instant::now();
        for event in events {
            dirty.record(event, start);
        }
        let due = dirty.take_due(start + QUIET);
        assert!(due.contains(&old_child), "old staged child must be deleted");
        assert!(due.contains(&new_child), "moved child must be discovered");
    }

    #[test]
    fn root_identity_recheck_reports_only_actual_root_transitions() {
        let missing = RootIdentity::Missing;
        let original = RootIdentity::Present(Some((1, 11)));
        let replacement = RootIdentity::Present(Some((1, 12)));
        assert_eq!(
            root_identity_transitions(missing, original),
            vec![RootMutationKind::Created]
        );
        assert_eq!(
            root_identity_transitions(original, missing),
            vec![RootMutationKind::Removed]
        );
        assert_eq!(
            root_identity_transitions(original, replacement),
            vec![RootMutationKind::Removed, RootMutationKind::Created]
        );
        assert!(root_identity_transitions(original, original).is_empty());
        assert!(root_identity_transitions(missing, missing).is_empty());
        assert!(root_identity_transitions(
            RootIdentity::Present(None),
            RootIdentity::Present(None)
        )
        .is_empty());

        let tree = TestTree::new();
        let root = tree.file(APPS_DIR);
        let (sender, receiver) = mpsc::channel();
        let mut identities = BTreeMap::from([(root.clone(), root_identity(&root).unwrap())]);
        fs::create_dir(&root).unwrap();
        report_root_transition(
            &root,
            ManagedRoot::Apps,
            root_identity(&root).unwrap(),
            &mut identities,
            &sender,
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(LiveEvent::RootChanged {
                root: ManagedRoot::Apps,
                kind: RootMutationKind::Created
            })
        ));
        fs::write(root.join("child"), b"unchanged root").unwrap();
        report_root_transition(
            &root,
            ManagedRoot::Apps,
            root_identity(&root).unwrap(),
            &mut identities,
            &sender,
        );
        assert!(receiver.try_recv().is_err());
        fs::remove_file(root.join("child")).unwrap();
        fs::remove_dir(&root).unwrap();
        report_root_transition(
            &root,
            ManagedRoot::Apps,
            root_identity(&root).unwrap(),
            &mut identities,
            &sender,
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(LiveEvent::RootChanged {
                root: ManagedRoot::Apps,
                kind: RootMutationKind::Removed
            })
        ));
        report_root_transition(
            &root,
            ManagedRoot::Apps,
            root_identity(&root).unwrap(),
            &mut identities,
            &sender,
        );
        assert!(receiver.try_recv().is_err());

        forward_event(
            &tree.0,
            Event::new(EventKind::Create(notify::event::CreateKind::Folder))
                .add_path(tree.file(APPS_DIR)),
            &sender,
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(LiveEvent::Rescan(RescanScope::Apps))
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn only_apps_changes_refresh_picker() {
        let tree = TestTree::new();
        assert!(affects_apps(
            &tree.0,
            &LiveEvent::Changed(tree.file("touchHLE_apps/a"))
        ));
        assert!(!affects_apps(
            &tree.0,
            &LiveEvent::Changed(tree.file("touchHLE_sandbox/a"))
        ));
        assert!(!affects_apps(
            &tree.0,
            &LiveEvent::Changed(tree.file("elsewhere"))
        ));
        assert!(affects_apps(&tree.0, &LiveEvent::Rescan(RescanScope::Apps)));
        assert!(affects_apps(&tree.0, &LiveEvent::Rescan(RescanScope::Both)));
        assert!(!affects_apps(
            &tree.0,
            &LiveEvent::Rescan(RescanScope::Sandbox)
        ));
        assert!(!affects_apps(&tree.0, &LiveEvent::Reconcile));
        assert!(affects_apps(
            &tree.0,
            &LiveEvent::Failed("unknown scope".into())
        ));
        assert!(affects_apps(
            &tree.0,
            &LiveEvent::RootChanged {
                root: ManagedRoot::Apps,
                kind: RootMutationKind::Created,
            }
        ));
        assert!(!affects_apps(
            &tree.0,
            &LiveEvent::RootChanged {
                root: ManagedRoot::Sandbox,
                kind: RootMutationKind::Removed,
            }
        ));
    }

    fn drain_observer_events(receiver: &mpsc::Receiver<LiveEvent>) {
        while receiver.try_recv().is_ok() {}
    }

    fn wait_for_observer_quiet(
        receiver: &mpsc::Receiver<LiveEvent>,
        quiet: Duration,
        timeout: Duration,
    ) {
        let until = Instant::now() + timeout;
        loop {
            let remaining = until
                .checked_duration_since(Instant::now())
                .expect("observer did not become quiet before timeout");
            match receiver.recv_timeout(quiet.min(remaining)) {
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) if remaining >= quiet => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("observer did not become quiet before timeout")
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("observer disconnected while waiting for quiescence")
                }
            }
        }
    }

    fn receive_root_change_and_reconcile(
        receiver: &mpsc::Receiver<LiveEvent>,
        dirty: &mut DirtySet,
        expected_kind: RootMutationKind,
        expected: &[RelativePath],
        timeout: Duration,
    ) {
        let until = Instant::now() + timeout;
        let mut observed = BTreeSet::new();
        let mut root_event_observed = false;
        let mut seen = Vec::new();
        while !root_event_observed || !expected.iter().all(|path| observed.contains(path)) {
            let left = until
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for scoped root reconciliation");
            let event = receiver.recv_timeout(left).unwrap_or_else(|_| {
                panic!("timed out waiting for RootChanged Apps {expected_kind:?}; saw {seen:?}")
            });
            seen.push(format!("{event:?}"));
            let LiveEvent::RootChanged {
                root: ManagedRoot::Apps,
                kind,
            } = event
            else {
                continue;
            };
            if kind != expected_kind {
                continue;
            }
            eprintln!("observed exact root mutation: Apps {kind:?}");
            root_event_observed = true;
            let now = Instant::now();
            dirty.record(
                LiveEvent::RootChanged {
                    root: ManagedRoot::Apps,
                    kind,
                },
                now,
            );
            let ready = dirty.take_due(now + QUIET);
            if expected.is_empty() {
                assert!(
                    ready.is_empty(),
                    "new empty root must match the staged baseline"
                );
            }
            observed.extend(ready);
        }
    }

    fn receive_periodic_reconcile(receiver: &mpsc::Receiver<LiveEvent>, timeout: Duration) {
        let until = Instant::now() + timeout;
        loop {
            let left = until
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for baseline periodic Reconcile");
            if matches!(
                receiver
                    .recv_timeout(left)
                    .expect("observer event timed out"),
                LiveEvent::Reconcile
            ) {
                return;
            }
        }
    }

    fn receive_reconcile_and_assert_deleted(
        receiver: &mpsc::Receiver<LiveEvent>,
        dirty: &mut DirtySet,
        expected_deleted: &RelativePath,
        timeout: Duration,
    ) {
        let until = Instant::now() + timeout;
        loop {
            let left = until
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for periodic Reconcile after root move");
            let event = receiver
                .recv_timeout(left)
                .expect("observer event timed out");
            if !matches!(event, LiveEvent::Reconcile) {
                continue;
            }
            eprintln!("observed independent periodic event: Reconcile");
            let now = Instant::now();
            dirty.record(event, now);
            assert_eq!(
                dirty.take_due(now + QUIET),
                vec![expected_deleted.clone()],
                "periodic Reconcile must find the staged file missing from the managed root"
            );
            return;
        }
    }

    fn receive_reconciled_paths(
        receiver: &mpsc::Receiver<LiveEvent>,
        root: &Path,
        dirty: &mut DirtySet,
        expected: &[RelativePath],
        timeout: Duration,
    ) {
        let until = Instant::now() + timeout;
        let mut observed = BTreeSet::new();
        while !expected.iter().all(|path| observed.contains(path)) {
            let left = until
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for path event or reconciliation");
            let event = receiver
                .recv_timeout(left)
                .expect("observer event timed out");
            let relevant = match &event {
                LiveEvent::Changed(path) => {
                    classify_event(root, path).is_some_and(|target| match target {
                        EventTarget::Path(path) => expected.contains(&path),
                        EventTarget::Rescan(scope) => expected.iter().any(|path| {
                            scope.includes(path.as_str().split('/').next().unwrap_or_default())
                        }),
                    })
                }
                LiveEvent::Rescan(scope) => expected.iter().any(|path| {
                    scope.includes(path.as_str().split('/').next().unwrap_or_default())
                }),
                LiveEvent::RootChanged { root, .. } => expected.iter().any(|path| {
                    matches!(
                        (root, path.as_str().split('/').next().unwrap_or_default()),
                        (ManagedRoot::Apps, APPS_DIR) | (ManagedRoot::Sandbox, SANDBOX_DIR)
                    )
                }),
                LiveEvent::Reconcile | LiveEvent::Failed(_) => true,
            };
            if !relevant {
                continue;
            }
            let now = Instant::now();
            dirty.record(event, now);
            observed.extend(dirty.take_due(now + QUIET));
        }
    }

    #[test]
    fn targeted_signature_does_not_enumerate_unrelated_managed_root() {
        let tree = TestTree::new();
        fs::create_dir(tree.file(APPS_DIR)).unwrap();
        fs::create_dir(tree.file(SANDBOX_DIR)).unwrap();
        fs::create_dir(tree.file("touchHLE_sandbox/nested")).unwrap();
        fs::write(
            tree.file("touchHLE_sandbox/nested/save"),
            b"targeted lookup succeeds",
        )
        .unwrap();
        let mut visited = Vec::new();
        assert!(signature_for_path_with(
            &tree.0,
            &path("touchHLE_sandbox/nested/save"),
            |directory| visited.push(directory.to_owned()),
        )
        .unwrap()
        .is_some());
        assert_eq!(visited, ["touchHLE_sandbox", "nested"]);

        let mut dirty = DirtySet::new(tree.0.clone());
        let start = Instant::now();
        dirty.record(LiveEvent::Rescan(RescanScope::Sandbox), start);
        assert_eq!(
            dirty.take_due(start + QUIET),
            vec![path("touchHLE_sandbox/nested/save")]
        );
    }
}
