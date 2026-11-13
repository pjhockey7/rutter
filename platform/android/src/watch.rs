//! Starting and stopping the foreground service.
//!
//! The service itself is `android/java/.../WatchService.java` and runs no
//! navigation — the anchor watch, the depth watch, the NMEA reader and the
//! siren are all Rust threads in this same process. Its only job is to
//! make "this process" mean something when the screen is off: a
//! backgrounded Android process is killable at any moment, and a
//! foreground service with an ongoing notification is the only sanctioned
//! way to say "not this one".
//!
//! So this is two JNI calls, each a static method on that class.

#![allow(unsafe_code)]

use jni::objects::{JObject, JValue};
use jni::{jni_sig, jni_str, JavaVM};

/// Run something with the VM attached and the activity to hand.
fn with_context<T>(
    what: &str,
    run: impl FnOnce(&mut jni::Env, &JObject) -> Option<T>,
) -> Option<T> {
    let context = ndk_context::android_context();
    if context.vm().is_null() || context.context().is_null() {
        log::error!("{what}: no Android context");
        return None;
    }
    // SAFETY: `ndk_context` is populated by android-activity with this
    // process's JavaVM and Activity, and both were just checked.
    let vm = unsafe { JavaVM::from_raw(context.vm().cast()) };
    let activity_raw = context.context().cast();

    vm.attach_current_thread(|env| -> Result<Option<T>, jni::errors::Error> {
        // SAFETY: the activity pointer lives as long as the process.
        let activity = unsafe { JObject::from_raw(env, activity_raw) };
        Ok(run(env, &activity))
    })
    .unwrap_or_else(|e: jni::errors::Error| {
        log::error!("{what}: {e}");
        None
    })
}

/// Start the watch service, with the line its notification should show.
///
/// Failure is logged and nothing else: the watch itself keeps running
/// either way, and an app that refused to arm an anchor alarm because a
/// notification would not appear would be refusing the useful half over
/// the decorative one. What it costs is only that Android may kill the
/// process while the screen is off — which the panel is the right place
/// to say, not this.
pub fn begin(text: &str) -> bool {
    with_context("starting the watch service", |env, activity| {
        let message = env.new_string(text).ok()?;
        env.call_static_method(
            jni_str!("com/pjhockey7/rutter/WatchService"),
            jni_str!("begin"),
            jni_sig!((context: android.content.Context, text: java.lang.String) -> void),
            &[JValue::Object(activity), JValue::Object(&message)],
        )
        .inspect_err(|e| log::error!("WatchService.begin: {e}"))
        .ok()?;
        log::info!("watch service started: {text}");
        Some(true)
    })
    .unwrap_or(false)
}

/// Stop it. Called when the last alarm is disarmed, so the notification
/// goes away and the process stops being held up for nothing.
pub fn end() -> bool {
    with_context("stopping the watch service", |env, activity| {
        env.call_static_method(
            jni_str!("com/pjhockey7/rutter/WatchService"),
            jni_str!("end"),
            jni_sig!((context: android.content.Context) -> void),
            &[JValue::Object(activity)],
        )
        .inspect_err(|e| log::error!("WatchService.end: {e}"))
        .ok()?;
        log::info!("watch service stopped");
        Some(true)
    })
    .unwrap_or(false)
}

/// Raise the notification permission dialogue if it is not already held.
///
/// Separate from [`begin`] because it is a different kind of failure: a
/// service that will not start means the process is killable, while a
/// notification that will not show means only that nobody can see the
/// watch is standing. Both are logged; neither stops anything.
pub fn want_notifications(permission: &str) {
    with_context(
        "asking for notifications",
        |env, activity| match crate::jvm::ask_for(env, activity, &[permission]) {
            Ok(true) => Some(true),
            Ok(false) => {
                log::info!("asked for {permission}");
                Some(false)
            }
            Err(e) => {
                log::error!("{permission}: {e}");
                None
            }
        },
    );
}
