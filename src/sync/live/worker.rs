/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::{affects_apps, classify_event, DirtySet, EventTarget, LiveEvent, LiveObserver, QUIET};
use crate::paths::APPS_DIR;
use crate::sync::model::{RelativePath, SyncError};
use crate::sync::status::{load_live_status, redacted_error, save_live_status, LiveStatus};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
#[cfg(test)]
use std::sync::{Arc, Barrier};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

enum LiveCommand {
    SetEnabled(bool),
    Stop,
}

pub struct LiveControl {
    commands: Sender<LiveCommand>,
    picker_events: Receiver<LiveEvent>,
    status_events: Option<Receiver<LiveStatus>>,
}

impl LiveControl {
    pub fn set_enabled(&self, enabled: bool) -> Result<(), SyncError> {
        self.commands
            .send(LiveCommand::SetEnabled(enabled))
            .map_err(|_| SyncError::Integrity("local observer is stopped".into()))
    }

    pub fn try_picker_event(&self) -> Option<LiveEvent> {
        self.picker_events.try_recv().ok()
    }

    pub fn try_status_event(&self) -> Option<LiveStatus> {
        self.status_events.as_ref()?.try_recv().ok()
    }

    pub(crate) fn attach_status_events(&mut self, receiver: Receiver<LiveStatus>) {
        self.status_events = Some(receiver);
    }
}

pub struct LiveObserverWorker {
    commands: Sender<LiveCommand>,
    worker: Option<JoinHandle<Result<(), SyncError>>>,
}

impl Drop for LiveObserverWorker {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.commands.send(LiveCommand::Stop);
            let _ = worker.join();
        }
    }
}

struct Timing {
    poll: Duration,
    reconcile: Duration,
    #[cfg(test)]
    fail_observer: bool,
    #[cfg(test)]
    initial_scan_gate: Option<(Arc<Barrier>, Arc<Barrier>)>,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(100),
            reconcile: Duration::from_secs(5),
            #[cfg(test)]
            fail_observer: false,
            #[cfg(test)]
            initial_scan_gate: None,
        }
    }
}

impl LiveObserverWorker {
    pub fn start(
        root: PathBuf,
        status_sender: Sender<LiveStatus>,
    ) -> Result<(Self, LiveControl), SyncError> {
        Self::start_with_timing(root, status_sender, Timing::default())
    }

    #[cfg(test)]
    pub(crate) fn start_without_observer(
        root: PathBuf,
        status_sender: Sender<LiveStatus>,
    ) -> Result<(Self, LiveControl), SyncError> {
        Self::start_with_timing(
            root,
            status_sender,
            Timing {
                fail_observer: true,
                ..Timing::default()
            },
        )
    }

    fn start_with_timing(
        root: PathBuf,
        status_sender: Sender<LiveStatus>,
        timing: Timing,
    ) -> Result<(Self, LiveControl), SyncError> {
        let (events_tx, events_rx) = mpsc::channel();
        #[cfg(test)]
        let observer = if timing.fail_observer {
            Err(SyncError::Integrity("injected observer failure".into()))
        } else {
            LiveObserver::start(&root, events_tx.clone())
        };
        #[cfg(not(test))]
        let observer = LiveObserver::start(&root, events_tx.clone());
        let (picker_tx, picker_events) = mpsc::channel();
        let (commands, command_rx) = mpsc::channel();
        let mut status = load_live_status(&root).unwrap_or_else(|_| {
            eprintln!("warning: could not load local observer status (details redacted)");
            LiveStatus::default()
        });
        status.active = observer.is_ok();
        status.queued = 0;
        if observer.is_err() {
            status.error =
                Some("filesystem observer unavailable; using periodic reconciliation".into());
        }
        if save_live_status(&root, &status).is_err() {
            eprintln!("warning: could not persist local observer status (details redacted)");
        }
        let _ = status_sender.send(status.clone());
        let worker = thread::spawn(move || {
            run(
                root,
                observer.ok(),
                events_rx,
                command_rx,
                picker_tx,
                status_sender,
                status,
                timing,
            )
        });
        Ok((
            Self {
                commands: commands.clone(),
                worker: Some(worker),
            },
            LiveControl {
                commands,
                picker_events,
                status_events: None,
            },
        ))
    }

    pub fn stop_and_drain(mut self) -> Result<(), SyncError> {
        self.commands
            .send(LiveCommand::Stop)
            .map_err(|_| SyncError::Integrity("local observer already stopped".into()))?;
        self.worker
            .take()
            .unwrap()
            .join()
            .map_err(|_| SyncError::Integrity("local observer panicked".into()))?
    }
}

fn publish(root: &Path, status: &LiveStatus, sender: &Sender<LiveStatus>) {
    if save_live_status(root, status).is_err() {
        eprintln!("warning: could not persist local observer status (details redacted)");
    }
    let _ = sender.send(status.clone());
}

fn note_status(
    root: &Path,
    status: &LiveStatus,
    sender: &Sender<LiveStatus>,
    saved: &mut LiveStatus,
) {
    if status == saved {
        return;
    }
    publish(root, status, sender);
    *saved = status.clone();
}

#[allow(clippy::too_many_arguments)]
fn run(
    root: PathBuf,
    mut observer: Option<LiveObserver>,
    events: Receiver<LiveEvent>,
    commands: Receiver<LiveCommand>,
    picker: Sender<LiveEvent>,
    sender: Sender<LiveStatus>,
    mut status: LiveStatus,
    timing: Timing,
) -> Result<(), SyncError> {
    let mut dirty = DirtySet::new(root.clone());
    let mut saved = status.clone();
    let mut last_event = None;
    let mut last_reconcile = Instant::now();
    let mut picker_notified_paths = BTreeSet::<RelativePath>::new();
    #[cfg(test)]
    let initial_reconcile_at = timing
        .initial_scan_gate
        .as_ref()
        .map_or_else(Instant::now, |_| Instant::now() - QUIET);
    #[cfg(not(test))]
    let initial_reconcile_at = Instant::now();
    dirty.record(LiveEvent::Reconcile, initial_reconcile_at);
    let mut draining = false;
    #[cfg(test)]
    let mut first_scan = true;
    loop {
        while let Ok(command) = commands.try_recv() {
            match command {
                LiveCommand::SetEnabled(enabled) => {
                    if enabled {
                        dirty.record(LiveEvent::Reconcile, Instant::now());
                    }
                }
                LiveCommand::Stop => {
                    draining = true;
                    observer.take();
                }
            }
        }
        loop {
            match events.try_recv() {
                Ok(event) => {
                    dirty.record(event.clone(), Instant::now());
                    last_event = Some(Instant::now());
                    if !matches!(event, LiveEvent::Reconcile) {
                        if affects_apps(&root, &event) {
                            if let LiveEvent::Changed(path) = &event {
                                if let Some(EventTarget::Path(path)) = classify_event(&root, path) {
                                    picker_notified_paths.insert(path);
                                }
                            }
                        }
                        let _ = picker.send(event);
                    }
                    status.queued = status.queued.max(1);
                    note_status(&root, &status, &sender, &mut saved);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        let now = Instant::now();
        if !draining && now.duration_since(last_reconcile) >= timing.reconcile {
            dirty.record(LiveEvent::Reconcile, now);
            last_reconcile = now;
        }
        #[cfg(test)]
        if first_scan {
            if let Some((reached_scan, release_scan)) = &timing.initial_scan_gate {
                reached_scan.wait();
                release_scan.wait();
            }
            first_scan = false;
        }
        let (ready_paths, _) =
            dirty.take_due_with_reconciled(if draining { now + QUIET } else { now });
        if let Some(error) = dirty.take_scan_error() {
            status.error = Some(redacted_error(&error));
        }
        if dirty.take_observer_error().is_some() {
            status.active = false;
            status.error = Some("filesystem observer failed; using periodic reconciliation".into());
        }
        for path in ready_paths {
            let already_notified = picker_notified_paths.remove(&path);
            if !already_notified && path.as_str().split('/').next() == Some(APPS_DIR) {
                let _ = picker.send(LiveEvent::Changed(root.join(path.as_str())));
            }
        }
        status.queued = 0;
        if last_event.is_some_and(|at: Instant| now.duration_since(at) < QUIET) {
            status.queued = status.queued.max(1);
        }
        note_status(&root, &status, &sender, &mut saved);
        if draining {
            status.active = false;
            status.queued = 0;
            note_status(&root, &status, &sender, &mut saved);
            return Ok(());
        }
        match commands.recv_timeout(timing.poll) {
            Ok(LiveCommand::SetEnabled(enabled)) => {
                if enabled {
                    dirty.record(LiveEvent::Reconcile, Instant::now());
                }
            }
            Ok(LiveCommand::Stop) => {
                draining = true;
                observer.take();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                draining = true;
                observer.take();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(test)]
mod local_observer_tests {
    use super::*;
    use crate::sync::status::load_live_status;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn observer_notifies_picker_without_advancing_cloud_status() {
        let root = std::env::temp_dir().join(format!("touchhle-observer-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join(APPS_DIR)).unwrap();
        let (sender, _) = mpsc::channel();
        let (observer, control) = LiveObserverWorker::start(root.clone(), sender).unwrap();
        let app = root.join(APPS_DIR).join("game.ipa");
        fs::write(&app, b"game").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if control
                .try_picker_event()
                .is_some_and(|event| super::super::affects_apps(&root, &event))
            {
                break;
            }
            assert!(Instant::now() < deadline, "picker missed local app change");
            thread::sleep(Duration::from_millis(20));
        }
        observer.stop_and_drain().unwrap();
        let status = load_live_status(&root).unwrap();
        assert_eq!(status.last_upload_unix_ms, None);
        assert_eq!(status.queued, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fallback_scan_refreshes_picker_after_observer_failure() {
        let root = std::env::temp_dir().join(format!("touchhle-fallback-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join(APPS_DIR)).unwrap();
        let (sender, _) = mpsc::channel();
        let (observer, control) =
            LiveObserverWorker::start_without_observer(root.clone(), sender).unwrap();
        thread::sleep(Duration::from_millis(1300));
        let app = root.join(APPS_DIR).join("game.ipa");
        fs::write(&app, b"game").unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if control
                .try_picker_event()
                .is_some_and(|event| matches!(event, LiveEvent::Changed(path) if path == app))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fallback missed local app change"
            );
            thread::sleep(Duration::from_millis(20));
        }
        observer.stop_and_drain().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_fallback_scan_refreshes_picker_for_app_created_before_scan() {
        let root = std::env::temp_dir().join(format!("touchhle-first-fallback-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join(APPS_DIR)).unwrap();
        let (sender, _) = mpsc::channel();
        let reached_scan = Arc::new(Barrier::new(2));
        let release_scan = Arc::new(Barrier::new(2));
        let timing = Timing {
            poll: Duration::from_millis(5),
            reconcile: Duration::from_secs(30),
            fail_observer: true,
            initial_scan_gate: Some((reached_scan.clone(), release_scan.clone())),
        };
        let (observer, control) =
            LiveObserverWorker::start_with_timing(root.clone(), sender, timing).unwrap();

        reached_scan.wait();
        let app = root.join(APPS_DIR).join("new-game.ipa");
        fs::write(&app, b"new app").unwrap();
        release_scan.wait();
        let event = control
            .picker_events
            .recv_timeout(Duration::from_millis(750));
        observer.stop_and_drain().unwrap();
        fs::remove_dir_all(root).unwrap();

        assert!(matches!(
            event.expect("first fallback scan did not refresh the picker"),
            LiveEvent::Changed(path) if path == app
        ));
    }
}
