/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::{unix_seconds, AuthError, TokenSet};
use crate::sync::progress::SyncProgressDisplay;
use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong};
use jni::{JNIEnv, JavaVM};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const ANDROID_ACCESS_TOKEN_LIFETIME_SECONDS: u64 = 3600;
const AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(300);
const AUTHORIZATION_DISPATCH_STACK_SIZE: usize = 4 * 1024 * 1024;
const JNI_DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);

static NEXT_REQUEST_ID: AtomicI64 = AtomicI64::new(1);
static KEYRING_CONTEXT_INITIALIZED: OnceLock<()> = OnceLock::new();
static ANDROID_JAVA_VM: OnceLock<JavaVM> = OnceLock::new();
static ANDROID_ACTIVITY: OnceLock<Mutex<Option<GlobalRef>>> = OnceLock::new();
static ANDROID_JNI_DISPATCHER: OnceLock<Sender<AndroidJniRequest>> = OnceLock::new();
static PENDING_AUTHORIZATIONS: OnceLock<Mutex<HashMap<i64, Sender<Result<TokenSet, AuthError>>>>> =
    OnceLock::new();
static PENDING_OFFLINE_DECISIONS: OnceLock<Mutex<HashMap<i64, Sender<bool>>>> = OnceLock::new();

enum AndroidJniCommand {
    UpdateSyncEnabled(bool),
    EnqueueFinalSync,
    UpdateStartupSyncOverlay {
        headline: String,
        detail: String,
        active_step: i32,
        track_progress: i32,
        visible: bool,
    },
    RequestOfflineSyncDecision(i64),
    RequestBackgroundAuthorization(i64),
}

struct AndroidJniRequest {
    command: AndroidJniCommand,
    completion: Option<Sender<Result<(), String>>>,
}

pub struct PendingAuthorization {
    receiver: Receiver<Result<TokenSet, AuthError>>,
}

impl PendingAuthorization {
    pub fn try_result(&self) -> Result<Option<Result<TokenSet, AuthError>>, AuthError> {
        match self.receiver.try_recv() {
            Ok(result) => Ok(Some(result)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(AuthError::OAuth(
                "authorization callback stopped unexpectedly".into(),
            )),
        }
    }

    fn wait(self) -> Result<TokenSet, AuthError> {
        self.receiver
            .recv_timeout(AUTHORIZATION_TIMEOUT)
            .map_err(|_| AuthError::OAuth("Google authorization timed out".into()))?
    }
}

pub fn start_authorization(
    _client_id: String,
    _open_browser: impl FnOnce(&str) -> Result<(), String>,
) -> Result<PendingAuthorization, AuthError> {
    begin_authorization(true)
}

pub fn request_access_token(allow_interactive: bool) -> Result<TokenSet, AuthError> {
    begin_authorization(allow_interactive)?.wait()
}

fn begin_authorization(allow_interactive: bool) -> Result<PendingAuthorization, AuthError> {
    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = mpsc::channel();
    PENDING_AUTHORIZATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| AuthError::OAuth("authorization callback is unavailable".into()))?
        .insert(request_id, sender);

    let start_result = if allow_interactive {
        (|| {
            let activity = ANDROID_ACTIVITY
                .get_or_init(|| Mutex::new(None))
                .lock()
                .map_err(|_| AuthError::OAuth("Android activity is unavailable".into()))?
                .as_ref()
                .cloned()
                .ok_or_else(|| AuthError::OAuth("Android activity is not initialized".into()))?;
            if ANDROID_JAVA_VM.get().is_none() {
                return Err(AuthError::OAuth(
                    "Android Java VM is not initialized".into(),
                ));
            }

            log!("Dispatching requestGoogleAuthorization on an attached worker thread");
            std::thread::Builder::new()
                .name("touchHLE-google-auth-dispatch".into())
                .stack_size(AUTHORIZATION_DISPATCH_STACK_SIZE)
                .spawn(move || {
                    let result = (|| {
                        let vm = ANDROID_JAVA_VM.get().ok_or_else(|| {
                            AuthError::OAuth("Android Java VM is not initialized".into())
                        })?;
                        let mut env = vm.attach_current_thread().map_err(|error| {
                            log!("Could not attach authorization worker to Android: {error:?}");
                            AuthError::OAuth("could not attach authorization worker".into())
                        })?;
                        if env.exception_check().unwrap_or(false) {
                            log!("Authorization worker already had a pending Java exception");
                            clear_pending_jni_exception(
                                &mut env,
                                "before requestGoogleAuthorization",
                            );
                            return Err(AuthError::OAuth(
                                "a Java exception was pending before Google authorization".into(),
                            ));
                        }
                        let activity_class =
                            env.get_object_class(activity.as_obj()).map_err(|error| {
                                log!("Could not inspect authorization activity class: {error:?}");
                                AuthError::OAuth("could not inspect Android activity".into())
                            })?;
                        if let Err(error) = env.get_method_id(
                            &activity_class,
                            "requestGoogleAuthorization",
                            "(JZ)V",
                        ) {
                            log!("Could not resolve requestGoogleAuthorization(JZ)V: {error:?}");
                            clear_pending_jni_exception(
                                &mut env,
                                "resolving requestGoogleAuthorization",
                            );
                            return Err(AuthError::OAuth(
                                "Google authorization method is unavailable".into(),
                            ));
                        }
                        let dispatch_result = env.call_method(
                            activity.as_obj(),
                            "requestGoogleAuthorization",
                            "(JZ)V",
                            &[JValue::Long(request_id), JValue::Bool(true.into())],
                        );
                        if let Err(error) = &dispatch_result {
                            log!("JNI authorization dispatch returned {error:?}");
                        }
                        clear_pending_jni_exception(&mut env, "requestGoogleAuthorization");
                        dispatch_result.map(|_| ()).map_err(|_| {
                            AuthError::OAuth("could not start Google authorization".into())
                        })
                    })();
                    if let Err(error) = result {
                        log!("Could not dispatch Google authorization: {error}");
                        report_authorization_dispatch_failure(request_id, error);
                    }
                })
                .map(|_| ())
                .map_err(|error| {
                    log!("Could not create authorization dispatch thread: {error}");
                    AuthError::OAuth("could not start Google authorization".into())
                })
        })()
    } else {
        dispatch_android_jni(
            AndroidJniCommand::RequestBackgroundAuthorization(request_id),
            true,
        )
        .map_err(AuthError::OAuth)
    };

    if let Err(error) = start_result {
        if let Ok(mut pending) = PENDING_AUTHORIZATIONS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
        {
            pending.remove(&request_id);
        }
        return Err(error);
    }

    Ok(PendingAuthorization { receiver })
}

fn report_authorization_dispatch_failure(request_id: i64, error: AuthError) {
    let sender = PENDING_AUTHORIZATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|mut pending| pending.remove(&request_id));
    if let Some(sender) = sender {
        let _ = sender.send(Err(error));
    }
}

fn clear_pending_jni_exception(env: &mut JNIEnv, operation: &str) {
    if env.exception_check().unwrap_or(false) {
        let exception = env.exception_occurred();
        if env.exception_clear().is_err() {
            log!("Could not clear pending Java exception after Android {operation}");
            return;
        }
        let exception_still_pending = env.exception_check().unwrap_or(true);
        log!(
            "JNI exception state after clearing Android {operation}: pending={exception_still_pending}"
        );
        if exception_still_pending {
            log!("Android {operation} exception details unavailable while JNI exception remains pending");
            return;
        }
        let detail = match exception {
            Ok(exception) if exception.is_null() => "exception object unavailable".to_owned(),
            Ok(exception) => {
                let exception = JObject::from(exception);
                classify_java_exception(env, &exception)
                    .unwrap_or("unclassified Java exception")
                    .to_owned()
            }
            Err(error) => format!("exception object lookup failed: {error:?}"),
        };
        log!("Android {operation} threw {detail}; cleared pending JNI exception");
    }
}

fn classify_java_exception(env: &mut JNIEnv, exception: &JObject) -> Option<&'static str> {
    const EXCEPTION_CLASSES: &[&str] = &[
        "java/lang/NoSuchMethodError",
        "java/lang/NoSuchMethodException",
        "java/lang/StackOverflowError",
        "java/lang/OutOfMemoryError",
        "java/lang/NullPointerException",
        "java/lang/IllegalStateException",
        "java/lang/IllegalArgumentException",
        "java/lang/SecurityException",
        "java/lang/AbstractMethodError",
        "java/lang/IncompatibleClassChangeError",
        "java/lang/VerifyError",
        "java/lang/UnsatisfiedLinkError",
        "java/lang/RuntimeException",
        "java/lang/LinkageError",
        "java/lang/Error",
        "java/lang/Throwable",
    ];

    for class_name in EXCEPTION_CLASSES {
        let class = match env.find_class(class_name) {
            Ok(class) => class,
            Err(_) => {
                clear_diagnostic_jni_exception(env);
                continue;
            }
        };
        match env.is_instance_of(exception, class) {
            Ok(true) => return Some(class_name),
            Ok(false) => {}
            Err(_) => clear_diagnostic_jni_exception(env),
        }
    }
    None
}

fn clear_diagnostic_jni_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}

fn parse_authorization_result(
    token: String,
    expires_unix_seconds: u64,
    error: String,
) -> Result<TokenSet, AuthError> {
    if !error.is_empty() {
        return match error.as_str() {
            "cancelled" => Err(AuthError::Cancelled),
            "foreground_required" => Err(AuthError::ForegroundRequired),
            _ => Err(AuthError::OAuth(error)),
        };
    }
    if token.is_empty() {
        return Err(AuthError::InvalidResponse);
    }
    let expires_unix_seconds = if expires_unix_seconds == 0 {
        unix_seconds().saturating_add(ANDROID_ACCESS_TOKEN_LIFETIME_SECONDS)
    } else {
        expires_unix_seconds
    };
    Ok(TokenSet::new_android(token, expires_unix_seconds))
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_touchhle_android_MainActivity_initializeKeyringContext(
    env: JNIEnv,
    class: JClass,
    context: JObject,
) {
    initialize_keyring_context(env, class, context);
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_touchhle_android_NativeSyncBridge_initializeKeyringContext(
    env: JNIEnv,
    class: JClass,
    context: JObject,
) {
    initialize_keyring_context(env, class, context);
}

fn initialize_keyring_context(env: JNIEnv, class: JClass, context: JObject) {
    if let Ok(vm) = env.get_java_vm() {
        let _ = ANDROID_JAVA_VM.set(vm);
    }
    KEYRING_CONTEXT_INITIALIZED.get_or_init(|| {
    android_native_keyring_store::Java_io_crates_keyring_Keyring_00024Companion_initializeNdkContext(
        env,
        class.into(),
        context,
    );
    });
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_touchhle_android_NativeSyncBridge_nativeRunScheduledSync(
    mut env: JNIEnv,
    _class: JClass,
    root_path: JString,
) -> jint {
    let Ok(root_path) = env.get_string(&root_path) else {
        return 2;
    };
    let root_path = std::path::PathBuf::from(String::from(root_path));
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(mut coordinator) =
            crate::sync::coordinator::google_drive_coordinator(&root_path, true)
        else {
            return 2;
        };
        match coordinator.background_reconcile() {
            Ok(
                crate::sync::coordinator::BackgroundSyncResult::Disabled
                | crate::sync::coordinator::BackgroundSyncResult::Completed
                | crate::sync::coordinator::BackgroundSyncResult::Conflicts,
            ) => 0,
            Ok(crate::sync::coordinator::BackgroundSyncResult::GuestActive) => 1,
            Ok(crate::sync::coordinator::BackgroundSyncResult::NeedsAuthorization) => 3,
            Ok(crate::sync::coordinator::BackgroundSyncResult::RetryableFailure) | Err(_) => 2,
        }
    }))
    .unwrap_or(2)
}

pub fn notify_sync_enabled(enabled: bool) {
    if let Err(error) = dispatch_android_jni(AndroidJniCommand::UpdateSyncEnabled(enabled), false) {
        log!("Could not queue Google Drive sync setting update: {error}");
    }
}

pub fn enqueue_final_sync() {
    if let Err(error) = dispatch_android_jni(AndroidJniCommand::EnqueueFinalSync, true) {
        log!("Could not enqueue final Google Drive sync: {error}");
    }
}

pub fn update_sync_progress(display: Option<SyncProgressDisplay>) {
    let (headline, detail, active_step, track_progress, visible) = match display {
        Some(display) => (
            display.headline.to_owned(),
            display.detail,
            display.active_step as i32,
            display.track_progress as i32,
            true,
        ),
        None => (String::new(), String::new(), 0, 0, false),
    };
    if let Err(error) = dispatch_android_jni(
        AndroidJniCommand::UpdateStartupSyncOverlay {
            headline,
            detail,
            active_step,
            track_progress,
            visible,
        },
        false,
    ) {
        log!("Could not queue Android sync progress update: {error}");
    }
}

pub fn request_offline_sync_decision() -> Option<bool> {
    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = mpsc::channel();
    PENDING_OFFLINE_DECISIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()?
        .insert(request_id, sender);

    if dispatch_android_jni(
        AndroidJniCommand::RequestOfflineSyncDecision(request_id),
        true,
    )
    .is_err()
    {
        if let Ok(mut pending) = PENDING_OFFLINE_DECISIONS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
        {
            pending.remove(&request_id);
        }
        return None;
    }

    let decision = receiver.recv_timeout(Duration::from_secs(300)).ok();
    if decision.is_none() {
        if let Ok(mut pending) = PENDING_OFFLINE_DECISIONS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
        {
            pending.remove(&request_id);
        }
    }
    decision
}

fn initialize_android_jni_dispatcher() {
    if ANDROID_JNI_DISPATCHER.get().is_some() {
        return;
    }

    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("touchHLE-android-jni".into())
        .stack_size(AUTHORIZATION_DISPATCH_STACK_SIZE)
        .spawn(move || android_jni_dispatch_loop(receiver));
    match worker {
        Ok(_) => {
            let _ = ANDROID_JNI_DISPATCHER.set(sender);
        }
        Err(error) => {
            log!("Could not create Android JNI dispatch worker: {error}");
        }
    }
}

fn dispatch_android_jni(command: AndroidJniCommand, wait: bool) -> Result<(), String> {
    let dispatcher = ANDROID_JNI_DISPATCHER
        .get()
        .ok_or_else(|| "Android JNI dispatch worker is unavailable".to_owned())?;
    let (completion, receiver) = if wait {
        let (sender, receiver) = mpsc::channel();
        (Some(sender), Some(receiver))
    } else {
        (None, None)
    };
    dispatcher
        .send(AndroidJniRequest {
            command,
            completion,
        })
        .map_err(|_| "Android JNI dispatch worker stopped".to_owned())?;

    if let Some(receiver) = receiver {
        receiver
            .recv_timeout(JNI_DISPATCH_TIMEOUT)
            .map_err(|_| "Android JNI dispatch timed out".to_owned())?
    } else {
        Ok(())
    }
}

fn android_jni_dispatch_loop(receiver: Receiver<AndroidJniRequest>) {
    for request in receiver {
        let AndroidJniRequest {
            command,
            completion,
        } = request;
        let result = run_android_jni_command(command);
        if let Some(completion) = completion {
            let _ = completion.send(result);
        } else if let Err(error) = result {
            log!("Android JNI dispatch failed: {error}");
        }
    }
}

fn run_android_jni_command(command: AndroidJniCommand) -> Result<(), String> {
    let vm = ANDROID_JAVA_VM
        .get()
        .ok_or_else(|| "Android Java VM is not initialized".to_owned())?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|error| format!("could not attach Android JNI worker: {error:?}"))?;
    if env.exception_check().unwrap_or(false) {
        clear_pending_jni_exception(&mut env, "before dispatching Android activity call");
        return Err("a Java exception was pending on the JNI dispatch worker".into());
    }

    match command {
        AndroidJniCommand::UpdateSyncEnabled(enabled) => {
            let activity = current_activity()?;
            call_activity_method(
                &mut env,
                activity.as_obj(),
                "updateCloudSyncSchedule",
                "(Z)V",
                &[JValue::Bool(enabled.into())],
                "updateCloudSyncSchedule",
            )
        }
        AndroidJniCommand::EnqueueFinalSync => {
            let activity = current_activity()?;
            call_activity_method(
                &mut env,
                activity.as_obj(),
                "enqueueCloudSyncNow",
                "()V",
                &[],
                "enqueueCloudSyncNow",
            )
        }
        AndroidJniCommand::UpdateStartupSyncOverlay {
            headline,
            detail,
            active_step,
            track_progress,
            visible,
        } => {
            let activity = current_activity()?;
            let headline = env
                .new_string(headline)
                .map_err(|error| format!("could not create sync progress headline: {error:?}"))?;
            let detail = env
                .new_string(detail)
                .map_err(|error| format!("could not create sync progress detail: {error:?}"))?;
            call_activity_method(
                &mut env,
                activity.as_obj(),
                "updateCloudSyncOverlay",
                "(Ljava/lang/String;Ljava/lang/String;IIZ)V",
                &[
                    JValue::Object(&headline),
                    JValue::Object(&detail),
                    JValue::Int(active_step),
                    JValue::Int(track_progress),
                    JValue::Bool(visible.into()),
                ],
                "updateCloudSyncOverlay",
            )
        }
        AndroidJniCommand::RequestOfflineSyncDecision(request_id) => {
            let activity = current_activity()?;
            call_activity_method(
                &mut env,
                activity.as_obj(),
                "requestOfflineSyncDecision",
                "(J)V",
                &[JValue::Long(request_id)],
                "requestOfflineSyncDecision",
            )
        }
        AndroidJniCommand::RequestBackgroundAuthorization(request_id) => {
            let bridge = match env.find_class("org/touchhle/android/NativeSyncBridge") {
                Ok(bridge) => bridge,
                Err(error) => {
                    clear_pending_jni_exception(
                        &mut env,
                        "resolving NativeSyncBridge.requestGoogleAuthorization",
                    );
                    return Err(format!("could not find Android sync bridge: {error:?}"));
                }
            };
            let result = env.call_static_method(
                bridge,
                "requestGoogleAuthorization",
                "(J)V",
                &[JValue::Long(request_id)],
            );
            if let Err(error) = result {
                clear_pending_jni_exception(
                    &mut env,
                    "NativeSyncBridge.requestGoogleAuthorization",
                );
                Err(format!(
                    "could not start background Google authorization: {error:?}"
                ))
            } else {
                clear_pending_jni_exception(
                    &mut env,
                    "NativeSyncBridge.requestGoogleAuthorization",
                );
                Ok(())
            }
        }
    }
}

fn current_activity() -> Result<GlobalRef, String> {
    ANDROID_ACTIVITY
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "Android activity is unavailable".to_owned())?
        .as_ref()
        .cloned()
        .ok_or_else(|| "Android activity is not initialized".to_owned())
}

fn call_activity_method(
    env: &mut JNIEnv,
    activity: &JObject,
    method: &str,
    signature: &str,
    args: &[JValue],
    operation: &str,
) -> Result<(), String> {
    let result = env.call_method(activity, method, signature, args);
    if let Err(error) = result {
        clear_pending_jni_exception(env, operation);
        Err(format!("Android {operation} failed: {error:?}"))
    } else {
        clear_pending_jni_exception(env, operation);
        Ok(())
    }
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_touchhle_android_MainActivity_setAuthorizationActivity(
    env: JNIEnv,
    _class: JClass,
    activity: JObject,
) {
    let Ok(vm) = env.get_java_vm() else {
        return;
    };
    let Ok(activity) = env.new_global_ref(activity) else {
        return;
    };
    let _ = ANDROID_JAVA_VM.set(vm);
    if let Ok(mut stored_activity) = ANDROID_ACTIVITY.get_or_init(|| Mutex::new(None)).lock() {
        *stored_activity = Some(activity);
    }
    initialize_android_jni_dispatcher();
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_touchhle_android_MainActivity_nativeOAuthResult(
    env: JNIEnv,
    _class: JClass,
    request_id: jlong,
    access_token: JString,
    expires_unix_seconds: jlong,
    error: JString,
) {
    deliver_authorization_result(env, request_id, access_token, expires_unix_seconds, error);
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_touchhle_android_NativeSyncBridge_nativeOAuthResult(
    env: JNIEnv,
    _class: JClass,
    request_id: jlong,
    access_token: JString,
    expires_unix_seconds: jlong,
    error: JString,
) {
    deliver_authorization_result(env, request_id, access_token, expires_unix_seconds, error);
}

fn deliver_authorization_result(
    mut env: JNIEnv,
    request_id: jlong,
    access_token: JString,
    expires_unix_seconds: jlong,
    error: JString,
) {
    let Ok(access_token) = env.get_string(&access_token) else {
        return;
    };
    let Ok(error) = env.get_string(&error) else {
        return;
    };
    let result = parse_authorization_result(
        access_token.into(),
        expires_unix_seconds.max(0) as u64,
        error.into(),
    );
    let sender = PENDING_AUTHORIZATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|mut pending| pending.remove(&request_id));
    log!(
        "Received Android authorization result for request {request_id}; matching request: {}",
        sender.is_some()
    );
    if let Some(sender) = sender {
        let _ = sender.send(result);
    }
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_touchhle_android_MainActivity_nativeOfflineSyncDecision(
    _env: JNIEnv,
    _class: JClass,
    request_id: jlong,
    continue_offline: jboolean,
) {
    let sender = PENDING_OFFLINE_DECISIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|mut pending| pending.remove(&request_id));
    if let Some(sender) = sender {
        let _ = sender.send(continue_offline != 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_android_authorization_creates_access_only_tokens() {
        let tokens = parse_authorization_result("access".into(), 0, String::new()).unwrap();

        assert_eq!(tokens.access_token(), "access");
        assert!(tokens.refresh_token().is_none());
        assert!(tokens.expires_unix_seconds() > unix_seconds());
    }

    #[test]
    fn android_authorization_errors_are_not_treated_as_tokens() {
        assert_eq!(
            parse_authorization_result(String::new(), 0, "cancelled".into()).unwrap_err(),
            AuthError::Cancelled
        );
        assert_eq!(
            parse_authorization_result(String::new(), 0, String::new()).unwrap_err(),
            AuthError::InvalidResponse
        );
        assert_eq!(
            parse_authorization_result(String::new(), 0, "foreground_required".into()).unwrap_err(),
            AuthError::ForegroundRequired
        );
    }
}
