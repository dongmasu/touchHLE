/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! touchHLE is a high-level emulator (HLE) for iPhone OS applications.
//!
//! In various places, the terms "guest" and "host" are used to distinguish
//! between the emulated application (the "guest") and the emulator itself (the
//! "host"), and more generally, their different environments.
//! For example:
//! - The guest is a 32-bit application, so a "guest pointer" is 32 bits.
//! - The host is a 64-bit application, so a "host pointer" is 64 bits.
//! - The guest can only directly access "guest memory".
//! - The host can access both "guest memory" and "host memory".
//! - A "guest function" is emulated Arm code, usually from the app binary.
//! - A "host function" is a Rust function that is part of this emulator.

// Allow the crate to have a non-snake-case name (touchHLE).
// This also allows items in the crate to have non-snake-case names.
#![allow(non_snake_case)]
// The documentation for this crate is intended to include private items.
// rustdoc complains about some public macros that link to private items, but
// we're forced to make those macros public by the weird macro scoping rules,
// so this warning is unhelpful.
#![allow(rustdoc::private_intra_doc_links)]

#[macro_use]
mod log;
mod abi;
mod audio;
mod bundle;
mod cpu;
mod debug;
mod dyld;
mod environment;
mod font;
mod frameworks;
mod fs;
mod gdb;
mod gles;
mod image;
mod libc;
mod licenses;
mod mach_o;
mod matrix;
mod mem;
mod objc;
mod options;
mod paths;
mod stack;
mod sync;
mod window;

// Environment is used very frequently used and used to be in this module, so
// it is re-exported to avoid having to update lots of imports. The other things
// probably shouldn't be, but they need a new home (TODO).
// Unlike its siblings, this module should be considered private and only used
// via re-exports.
use environment::{Environment, MutexId, MutexType, ThreadId, PTHREAD_MUTEX_DEFAULT};

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

pub use touchHLE_version::*;

/// This is the true entry point on Android (SDLActivity calls it after
/// initialization). On other platforms the true entry point is in src/bin.rs.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn SDL_main(
    _argc: std::ffi::c_int,
    _argv: *const *const std::ffi::c_char,
) -> std::ffi::c_int {
    // Rust's default panic handler prints to stderr, but on Android that just
    // gets discarded, so we set a custom hook to make debugging easier.
    std::panic::set_hook(Box::new(|info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s
        } else {
            "(non-string payload)"
        };
        if let Some(location) = info.location() {
            echo!("Panic at {}: {}", location, payload);
        } else {
            echo!("Panic: {}", payload);
        }
    }));

    // Empty args: brings up app picker.
    match main([String::new()].into_iter()) {
        Ok(_) => echo!("touchHLE finished"),
        Err(e) => echo!("touchHLE errored: {e:?}"),
    }
    0
}

const USAGE: &str = "\
Usage:
    touchHLE [PATH] [OPTIONS]

PATH should be a path to a .app bundle or .ipa file.

If no app path or special option is specified, a GUI app picker is displayed.

Special options:
    --help
        Display this help text.

    --copyright
        Display copyright, authorship and license information.

    --info
        Print basic information about the app bundle without running the app.
";

pub fn main<T: Iterator<Item = String>>(mut args: T) -> Result<(), String> {
    echo!(
        "touchHLE {}{}{} — https://touchhle.org/",
        branding(),
        if branding().is_empty() { "" } else { " " },
        VERSION,
    );
    if GITHUB_RUN_ID.is_some() && !branding().is_empty() {
        echo!(
            "Built from branch {:?} of {:?} by GitHub Actions workflow run {}/{}/actions/runs/{}.",
            GITHUB_REF_NAME.unwrap(),
            GITHUB_REPOSITORY.unwrap(),
            GITHUB_SERVER_URL.unwrap(),
            GITHUB_REPOSITORY.unwrap(),
            GITHUB_RUN_ID.unwrap()
        );
    }
    echo!();

    {
        let base_path = paths::user_data_base_path();
        log!("Base path for touchHLE files: {}", base_path.display());
        paths::prepopulate_user_data_dir();
    }

    let _ = args.next().unwrap(); // skip argv[0]

    let mut bundle_path: Option<PathBuf> = None;
    let mut just_info = false;
    let mut option_args = Vec::new();
    let mut options = options::Options::default();
    let mut app_args = None::<Vec<String>>;

    for arg in args {
        if let Some(ref mut app_args) = app_args {
            app_args.push(arg);
        } else if arg == "--args" {
            app_args = Some(Vec::new());
        } else if arg == "--help" {
            echo!("{}", USAGE);
            echo!("{}", options::OPTIONS_HELP);
            return Ok(());
        } else if arg == "--copyright" {
            echo!("{}", licenses::get_text());
            return Ok(());
        } else if arg == "--info" {
            just_info = true;
        // Parse an option and store a backup in option_args so that we can
        // reapply them after file options are loaded. This ensures that
        // command line options take precedence over file options.
        } else if options.parse_argument(&arg)? {
            option_args.push(arg);
        } else if bundle_path.is_none() {
            bundle_path = Some(PathBuf::from(arg));
        } else {
            echo!("{}", USAGE);
            echo!("{}", options::OPTIONS_HELP);
            return Err(format!("Unexpected argument: {arg:?}"));
        }
    }

    if options.dumping_options.symbols {
        let mut file = std::fs::File::create(&options.dumping_file).map_err(|e| e.to_string())?;
        dyld::Dyld::dump_host_symbols(&mut file).unwrap();
        return Ok(());
    }

    if bundle_path.is_none() && options.headless {
        return Err("No app specified. Use the --help flag to see command-line usage.".to_string());
    }

    // Sync before enumerating the app directory so apps present only in Drive
    // are available in the picker on a new device. Informational launches stay
    // local-only and never wait for cloud access.
    let sync_root = paths::user_data_base_path().to_path_buf();
    let mut sync_coordinator = if just_info {
        None
    } else {
        let mut coordinator =
            sync::coordinator::google_drive_coordinator(&sync_root, options.headless).map_err(
                |error| {
                    format!(
                        "Cloud sync settings could not be loaded: {}",
                        sync::status::redacted_error(&error)
                    )
                },
            )?;
        sync_before_guest(&mut coordinator, &options, "startup")?;
        Some(coordinator)
    };
    let mut live_control = sync_coordinator
        .as_mut()
        .filter(|coordinator| sync_mode_is_active(coordinator.mode()))
        .map(|coordinator| coordinator.start_live(&sync_root))
        .transpose()
        .map_err(|error| {
            format!(
                "Cloud observer could not start: {}",
                sync::status::redacted_error(&error)
            )
        })?;
    let mut guest_session = sync_coordinator
        .as_ref()
        .filter(|coordinator| sync_mode_is_active(coordinator.mode()))
        .map(|coordinator| coordinator.begin_guest_session())
        .transpose()
        .map_err(|error| {
            format!(
                "Could not protect the active touchHLE session from background sync: {}",
                sync::status::redacted_error(&error)
            )
        })?;
    let picker_was_used = bundle_path.is_none();
    let mut picker_sync_attempted = false;
    let bundle_path = if let Some(bundle_path) = bundle_path {
        bundle_path
    } else {
        let mut picker_options = options::Options::default();
        // Apply command-line options only (no app-specific options apply)
        for option_arg in &option_args {
            let parse_result = picker_options.parse_argument(option_arg);
            assert!(parse_result == Ok(true));
        }
        echo!(
            "No app specified, opening app picker. Use the --help flag to see command-line usage."
        );

        let sync_context = Rc::new(RefCell::new(PickerSyncContext {
            coordinator: sync_coordinator.take(),
            guest_session: guest_session.take(),
            sync_root: sync_root.clone(),
            options: options.clone(),
            picker_sync_attempted: false,
        }));
        let sync_context_for_picker = Rc::clone(&sync_context);
        let picker_sync_action = move || run_picker_sync(&mut sync_context_for_picker.borrow_mut());
        let picker_result = environment::app_picker::app_picker(
            picker_options,
            live_control.take(),
            picker_sync_action,
        )?;
        let mut sync_context = Rc::try_unwrap(sync_context)
            .unwrap_or_else(|_| unreachable!("picker sync context escaped"))
            .into_inner();
        picker_sync_attempted = sync_context.picker_sync_attempted;
        sync_coordinator = sync_context.coordinator.take();
        guest_session = sync_context.guest_session.take();

        let Some((bundle_path, mut extra_options)) = picker_result else {
            if let Some(coordinator) = sync_coordinator.as_mut() {
                coordinator.stop_live().map_err(|error| {
                    format!(
                        "Cloud observer could not drain: {}",
                        sync::status::redacted_error(&error)
                    )
                })?;
            }
            return Ok(());
        };
        option_args.append(&mut extra_options);
        bundle_path
    };

    if let Some(coordinator) = sync_coordinator.as_mut() {
        coordinator.stop_live().map_err(|error| {
            format!(
                "Cloud observer could not drain: {}",
                sync::status::redacted_error(&error)
            )
        })?;
        coordinator
            .refresh_mode(&sync_root, options.headless)
            .map_err(|error| {
                format!(
                    "Cloud sync settings could not be reloaded: {}",
                    sync::status::redacted_error(&error)
                )
            })?;
        if should_run_picker_sync(
            picker_was_used,
            coordinator.mode() != sync::coordinator::SyncMode::Disabled,
            picker_sync_attempted,
        ) {
            // The picker session is protected too. Release its lock before the
            // sync that reconciles picker edits, then reacquire it before the
            // guest environment can be constructed.
            drop(guest_session.take());
            // Reconcile edits made while the picker was open before starting the guest.
            sync_before_guest(coordinator, &options, "after-picker")?;
            guest_session = Some(coordinator.begin_guest_session().map_err(|error| {
                format!(
                    "Could not protect the guest session from background sync: {}",
                    sync::status::redacted_error(&error)
                )
            })?);
        }
        if sync_mode_is_active(coordinator.mode()) {
            coordinator.start_live(&sync_root).map_err(|error| {
                format!(
                    "Cloud observer could not restart: {}",
                    sync::status::redacted_error(&error)
                )
            })?;
        }
    }

    // When PowerShell does tab-completion on a directory, for some reason it
    // expands it to `'..\My Bundle.app\'` and that trailing \ seems to
    // get interpreted as escaping a double quotation mark?
    #[cfg(windows)]
    if let Some(fixed) = bundle_path.to_str().and_then(|s| s.strip_suffix('"')) {
        log!("Warning: The bundle path has a trailing quotation mark! This often happens accidentally on Windows when tab-completing, because '\\\"' gets interpreted by Rust in the wrong way. Did you meant to write {:?}?", fixed);
    }

    let app_selected_at = Instant::now();
    let app_open_started = Instant::now();
    let bundle_data = fs::BundleData::open_any(&bundle_path)
        .map_err(|e| format!("Could not open app bundle: {e}"))?;
    let (bundle, fs) = match bundle::Bundle::new_bundle_and_fs_from_host_path(
        bundle_data,
        /* read_only_mode: */ false,
    ) {
        Ok(bundle) => bundle,
        Err(err) => {
            return Err(format!("Application bundle error: {err}. Check that the path is to an .app directory or an .ipa file."));
        }
    };
    log!(
        "Selected app bundle opened in {} ms",
        app_open_started.elapsed().as_millis()
    );

    let app_id = bundle.bundle_identifier();
    let minimum_os_version = bundle.minimum_os_version();
    let required_device_capabilities = bundle.required_device_capabilities();
    let device_family = bundle.device_family_array();

    echo!("App bundle info:");
    echo!("- Display name: {}", bundle.display_name());
    echo!("- Version: {}", bundle.bundle_version());
    echo!("- Identifier: {}", app_id);
    if let Some(canonical_name) = bundle.canonical_bundle_name() {
        echo!("- Internal name (canonical): {}.app", canonical_name);
    } else {
        echo!("- Internal name (from FS): {}.app", bundle.bundle_name());
    }
    echo!(
        "- Minimum OS version: {}",
        minimum_os_version.unwrap_or("(not specified)")
    );
    echo!(
        "- Required device capabilities: {}",
        if !required_device_capabilities.is_empty() {
            required_device_capabilities.join(", ")
        } else {
            "(not specified)".to_string()
        }
    );
    echo!(
        "- Device family: {}",
        if !device_family.is_empty() {
            device_family
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            "(not specified)".to_string()
        }
    );
    echo!();

    if let Some(version) = minimum_os_version {
        let (major, minor_etc) = version.split_once('.').unwrap();
        let minor = minor_etc
            .split_once('.')
            .map_or(minor_etc, |(minor, _etc)| minor);
        let major: u32 = major.parse().unwrap();
        let minor: u32 = minor.parse().unwrap();
        if major > 4 || (major == 4 && minor > 0) {
            echo!("Warning: app requires OS version {}. Only apps for iOS 4.0 and earlier are currently supported.", version);
        }
    }

    if required_device_capabilities.contains(&"opengles-2")
        || required_device_capabilities.contains(&"opengles-3")
    {
        echo!("Warning: app requires OpenGL ES 2.0+ support. Only OpenGL ES 1.1 is currently supported.");
    }

    if just_info {
        return Ok(());
    }

    // Apply options from files
    fn apply_options<F: std::io::Read, P: std::fmt::Display>(
        file: F,
        path: P,
        options: &mut options::Options,
        app_id: &str,
    ) -> Result<(), String> {
        match options::get_options_from_file(file, app_id) {
            Ok(Some(options_string)) => {
                echo!(
                    "Using options from {} for this app: {}",
                    path,
                    options_string
                );
                for option_arg in options_string.split_ascii_whitespace() {
                    match options.parse_argument(option_arg) {
                        Ok(true) => (),
                        Ok(false) => return Err(format!("Unknown option {option_arg:?}")),
                        Err(err) => return Err(format!("Invalid option {option_arg:?}: {err}")),
                    }
                }
            }
            Ok(None) => {
                echo!("No options found for this app in {}", path);
            }
            Err(e) => {
                echo!("Warning: {}", e);
            }
        }
        Ok(())
    }
    let default_options_path = paths::DEFAULT_OPTIONS_FILE;
    match paths::ResourceFile::open(default_options_path) {
        Ok(mut file) => apply_options(file.get(), default_options_path, &mut options, app_id)?,
        Err(err) => echo!("Warning: Could not open {}: {}", default_options_path, err),
    }
    let user_options_path = paths::user_data_base_path().join(paths::USER_OPTIONS_FILE);
    match std::fs::File::open(&user_options_path) {
        Ok(file) => apply_options(file, user_options_path.display(), &mut options, app_id)?,
        Err(err) => echo!(
            "Warning: Could not open {}: {}",
            user_options_path.display(),
            err
        ),
    }
    echo!();

    // Apply command-line options
    for option_arg in option_args {
        let parse_result = options.parse_argument(&option_arg);
        assert!(parse_result == Ok(true));
    }

    let environment_init_started = Instant::now();
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Environment::new(bundle, fs, options.clone(), app_args.unwrap_or_default())
    }));
    let mut env = match res {
        Ok(ret) => match ret {
            Ok(env) => env,
            Err(e) => {
                if options.popup_errors {
                    window::show_error_messagebox(None, e.as_str());
                }
                return Err(e);
            }
        },
        Err(e) => {
            if options.popup_errors {
                let error_string = if let Some(s) = e.downcast_ref::<&str>() {
                    s
                } else if let Some(s) = e.downcast_ref::<String>() {
                    s
                } else {
                    "(non-string payload)"
                };
                window::show_error_messagebox(None, error_string);
            }
            std::panic::resume_unwind(e)
        }
    };
    log!(
        "Guest environment initialized in {} ms",
        environment_init_started.elapsed().as_millis()
    );
    if let Some(window) = env.window.as_mut() {
        window.set_guest_launch_started_at(app_selected_at);
    }
    let exit_code = env.run();
    log!("Emulator returned with exit code {exit_code}; beginning cloud shutdown");
    // `Environment::run` consumes and destroys the guest environment before
    // returning, so releasing this lock now makes remote apply safe.
    drop(guest_session);
    #[cfg(target_os = "android")]
    sync::auth::android::enqueue_final_sync();
    if let Some(coordinator) = sync_coordinator
        .as_mut()
        .filter(|coordinator| coordinator.mode() != sync::coordinator::SyncMode::Disabled)
    {
        loop {
            let result = run_shutdown_sync(coordinator);
            match result {
                Ok(sync::engine::SyncOutcome::Conflicts(plan)) if !options.headless => {
                    match environment::app_picker::resolve_conflicts_gui(options.clone(), &plan) {
                        Ok(Some(choices)) => {
                            if let Err(error) =
                                resolve_conflicts_with_progress(coordinator, &plan, &choices)
                            {
                                echo!("Warning: cloud conflicts were preserved but not resolved: {}", sync::status::redacted_error(&error));
                            }
                        }
                        Ok(None) => echo!(
                            "Cloud conflicts were preserved for a later run; no version was selected."
                        ),
                        Err(error) => echo!(
                            "Warning: cloud conflicts were preserved; the resolver could not be opened: {error}"
                        ),
                    }
                    break;
                }
                Ok(sync::engine::SyncOutcome::Offline) if !options.headless => {
                    let reason =
                        "Google Drive is unavailable. Local game data has not been discarded.";
                    match ask_shutdown_sync_retry(reason) {
                        Ok(true) => continue,
                        Ok(false) => echo!(
                            "Exiting with local game data preserved; cloud sync can retry next launch."
                        ),
                        Err(error) => echo!(
                            "Could not show shutdown sync choices ({error}); local game data was preserved."
                        ),
                    }
                    break;
                }
                Ok(sync::engine::SyncOutcome::Offline) => {
                    echo!("Shutdown cloud sync was unavailable; local game data was preserved.");
                    break;
                }
                Ok(sync::engine::SyncOutcome::Conflicts(_)) => {
                    echo!("Cloud conflicts were preserved for a later graphical run.");
                    break;
                }
                Ok(_) => break,
                Err(error) if !options.headless => {
                    let reason = sync::status::redacted_error(&error);
                    match ask_shutdown_sync_retry(&reason) {
                        Ok(true) => continue,
                        Ok(false) => echo!(
                            "Exiting with local game data preserved; cloud sync can retry next launch."
                        ),
                        Err(choice_error) => echo!(
                            "Could not show shutdown sync choices ({choice_error}); local game data was preserved."
                        ),
                    }
                    break;
                }
                Err(error) => {
                    echo!(
                        "Warning: shutdown cloud sync did not complete: {}",
                        sync::status::redacted_error(&error)
                    );
                    break;
                }
            }
        }
    }
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn sync_before_guest(
    coordinator: &mut sync::coordinator::GoogleDriveSyncCoordinator,
    options: &options::Options,
    phase: &'static str,
) -> Result<bool, String> {
    let _timing = SyncGateTiming {
        phase,
        started_at: Instant::now(),
    };
    let mut result = run_prelaunch_sync(coordinator);
    loop {
        match result {
            Err(error) => {
                return Err(format!(
                    "Cloud sync failed before app launch: {}",
                    sync::status::redacted_error(&error)
                ));
            }
            Ok(sync::coordinator::PreLaunchResult::Continue) => return Ok(true),
            Ok(sync::coordinator::PreLaunchResult::LocalOnly(reason))
                if coordinator.mode() != sync::coordinator::SyncMode::Disabled
                    && !options.headless =>
            {
                #[cfg(not(target_os = "android"))]
                {
                    match window::ask_sync_offline_choice(&reason) {
                        Ok(true) => {
                            echo!("Continuing offline for this session: {reason}");
                            return Ok(false);
                        }
                        Ok(false) => {
                            result = run_prelaunch_sync(coordinator);
                        }
                        Err(error) => return Err(error),
                    }
                    continue;
                }
                #[cfg(target_os = "android")]
                {
                    match sync::auth::android::request_offline_sync_decision() {
                        Some(true) => {
                            echo!("Continuing offline for this session: {reason}");
                            return Ok(false);
                        }
                        Some(false) => {
                            result = run_prelaunch_sync(coordinator);
                        }
                        None => {
                            return Err(
                                "Cloud sync decision was not received; no game was started."
                                    .to_owned(),
                            );
                        }
                    }
                    continue;
                }
            }
            Ok(sync::coordinator::PreLaunchResult::LocalOnly(reason))
                if coordinator.mode() != sync::coordinator::SyncMode::Disabled
                    && options.headless =>
            {
                return Err(format!("Cloud sync unavailable in headless mode: {reason}"));
            }
            Ok(sync::coordinator::PreLaunchResult::LocalOnly(reason)) => {
                echo!("Cloud sync skipped: {reason}");
                return Ok(false);
            }
            Ok(sync::coordinator::PreLaunchResult::NeedsResolution(plan)) => {
                if options.headless {
                    return Err(
                        "Cloud sync has file conflicts. Run touchHLE graphically to choose versions."
                            .to_owned(),
                    );
                }
                let choices =
                    environment::app_picker::resolve_conflicts_gui(options.clone(), &plan)?
                        .ok_or_else(|| "Cloud conflict resolution was cancelled.".to_owned())?;
                match resolve_conflicts_with_progress(coordinator, &plan, &choices) {
                    Ok(sync::engine::SyncOutcome::Conflicts(next)) => {
                        result = Ok(sync::coordinator::PreLaunchResult::NeedsResolution(next));
                    }
                    Ok(sync::engine::SyncOutcome::Offline) => {
                        result = run_prelaunch_sync(coordinator);
                    }
                    Ok(_) => return Ok(true),
                    Err(error @ sync::model::SyncError::UnresolvedConflicts(_)) => {
                        log!(
                            "Cloud conflict selection became stale; refreshing conflict plan: {}",
                            sync::status::redacted_error(&error)
                        );
                        result = run_prelaunch_sync(coordinator);
                    }
                    Err(error) => {
                        return Err(format!(
                            "Cloud conflict resolution failed: {}",
                            sync::status::redacted_error(&error)
                        ));
                    }
                }
            }
        }
    }
}

struct PickerSyncContext {
    coordinator: Option<sync::coordinator::GoogleDriveSyncCoordinator>,
    guest_session: Option<sync::locks::LockGuard>,
    sync_root: PathBuf,
    options: options::Options,
    picker_sync_attempted: bool,
}

fn run_picker_sync(
    context: &mut PickerSyncContext,
) -> Result<
    (
        Option<sync::live::LiveControl>,
        Result<environment::app_picker::PickerSyncResult, String>,
    ),
    String,
> {
    let root = context.sync_root.clone();
    let options = context.options.clone();
    let coordinator = context
        .coordinator
        .as_mut()
        .ok_or_else(|| "Cloud sync is unavailable in this session.".to_owned())?;

    context.picker_sync_attempted = true;
    log!("Google Drive sync requested while the app picker is open");
    let stop_result = coordinator.stop_live().map_err(|error| {
        format!(
            "Cloud observer could not drain: {}",
            sync::status::redacted_error(&error)
        )
    });
    let sync_result = match stop_result {
        Err(error) => Err(error),
        Ok(()) => {
            drop(context.guest_session.take());
            match coordinator
                .refresh_mode(&root, options.headless)
                .map_err(|error| {
                    format!(
                        "Cloud sync settings could not be reloaded: {}",
                        sync::status::redacted_error(&error)
                    )
                }) {
                Ok(()) => {
                    sync_before_guest(coordinator, &options, "picker-enable").map(|completed| {
                        if completed {
                            environment::app_picker::PickerSyncResult::Completed
                        } else {
                            environment::app_picker::PickerSyncResult::Offline
                        }
                    })
                }
                Err(error) => Err(error),
            }
        }
    };
    match &sync_result {
        Ok(environment::app_picker::PickerSyncResult::Completed) => {
            log!("Google Drive picker sync completed");
        }
        Ok(environment::app_picker::PickerSyncResult::Offline) => {
            log!("Google Drive picker sync continued with local files");
        }
        Err(error) => {
            log!("Google Drive picker sync failed: {error}");
        }
    }

    context.guest_session = Some(coordinator.begin_guest_session().map_err(|error| {
        format!(
            "Could not protect the picker from background sync: {}",
            sync::status::redacted_error(&error)
        )
    })?);
    let live_control = if sync_mode_is_active(coordinator.mode()) {
        Some(coordinator.start_live(&root).map_err(|error| {
            format!(
                "Cloud observer could not restart: {}",
                sync::status::redacted_error(&error)
            )
        })?)
    } else {
        None
    };

    Ok((live_control, sync_result))
}

fn run_prelaunch_sync(
    coordinator: &mut sync::coordinator::GoogleDriveSyncCoordinator,
) -> Result<sync::coordinator::PreLaunchResult, sync::model::SyncError> {
    if coordinator.mode() == sync::coordinator::SyncMode::Enabled {
        return window::run_with_sync_progress(
            "touchHLE - Starting Google Drive sync",
            |progress| coordinator.before_launch_with_progress(progress),
        );
    }
    coordinator.before_launch()
}

fn resolve_conflicts_with_progress(
    coordinator: &mut sync::coordinator::GoogleDriveSyncCoordinator,
    plan: &sync::reconcile::SyncPlan,
    choices: &[sync::reconcile::ConflictChoice],
) -> Result<sync::engine::SyncOutcome, sync::model::SyncError> {
    if coordinator.mode() == sync::coordinator::SyncMode::Enabled {
        return window::run_with_sync_progress(
            "touchHLE - Applying Google Drive sync choices",
            |progress| coordinator.resolve_conflicts_with_progress(plan, choices, progress),
        );
    }
    coordinator.resolve_conflicts(plan, choices)
}

fn run_shutdown_sync(
    coordinator: &mut sync::coordinator::GoogleDriveSyncCoordinator,
) -> Result<sync::engine::SyncOutcome, sync::model::SyncError> {
    if coordinator.mode() == sync::coordinator::SyncMode::Enabled {
        return window::run_with_sync_progress("touchHLE - Final Google Drive sync", |progress| {
            coordinator.shutdown_sync_with_progress(progress)
        });
    }
    coordinator.shutdown_sync()
}

fn ask_shutdown_sync_retry(reason: &str) -> Result<bool, String> {
    #[cfg(target_os = "android")]
    {
        echo!("Shutdown cloud sync needs a decision: {reason}");
        sync::auth::android::request_offline_sync_decision()
            .map(retry_shutdown_sync_from_android_choice)
            .ok_or_else(|| {
                "No shutdown cloud sync decision was received; local data was preserved.".to_owned()
            })
    }
    #[cfg(not(target_os = "android"))]
    {
        window::ask_sync_shutdown_retry(reason)
    }
}

#[cfg(any(target_os = "android", test))]
fn retry_shutdown_sync_from_android_choice(continue_offline: bool) -> bool {
    !continue_offline
}

struct SyncGateTiming {
    phase: &'static str,
    started_at: Instant,
}

impl Drop for SyncGateTiming {
    fn drop(&mut self) {
        log!(
            "Cloud sync {} gate finished in {} ms",
            self.phase,
            self.started_at.elapsed().as_millis()
        );
    }
}

fn should_run_picker_sync(
    picker_was_used: bool,
    sync_enabled: bool,
    picker_sync_attempted: bool,
) -> bool {
    picker_was_used && sync_enabled && !picker_sync_attempted
}

fn sync_mode_is_active(mode: sync::coordinator::SyncMode) -> bool {
    mode != sync::coordinator::SyncMode::Disabled
}

#[cfg(test)]
mod picker_sync_tests {
    use super::{
        retry_shutdown_sync_from_android_choice, should_run_picker_sync, sync_mode_is_active,
    };

    #[test]
    fn picker_exit_sync_runs_only_when_no_sync_was_attempted_in_picker() {
        assert!(should_run_picker_sync(true, true, false));
        assert!(!should_run_picker_sync(true, false, false));
        assert!(!should_run_picker_sync(false, true, false));
        assert!(!should_run_picker_sync(true, true, true));
    }

    #[test]
    fn disabled_sync_skips_live_observer_and_guest_session_locks() {
        assert!(!sync_mode_is_active(
            super::sync::coordinator::SyncMode::Disabled
        ));
        assert!(sync_mode_is_active(
            super::sync::coordinator::SyncMode::Enabled
        ));
    }

    #[test]
    fn android_shutdown_choice_maps_retry_and_local_exit_correctly() {
        assert!(retry_shutdown_sync_from_android_choice(false));
        assert!(!retry_shutdown_sync_from_android_choice(true));
    }
}
