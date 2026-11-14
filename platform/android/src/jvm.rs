//! The small change that makes `?` usable inside a JNI attach closure.
//!
//! `JavaVM::attach_current_thread` requires the closure's error type to
//! be `From<jni::errors::Error>`, which a bare `String` is not. Rather
//! than each caller inventing the same wrapper, it lives here.
//!
//! The permission dance ended up here for the same reason: the GPS wants
//! location and the watch service wants notifications, and the shape of
//! asking is identical.
//!
//! ## Application is not Activity
//!
//! `ndk_context` holds the **`Application`**, not the `Activity` —
//! `android-activity` registers `get_application(..)` at startup. That is
//! fine for nearly everything, because the methods wanted here
//! (`getSystemService`, `checkSelfPermission`, `startForegroundService`,
//! `stopService`) are all declared on `Context`, which `Application` is.
//!
//! `requestPermissions` is the exception: it is declared on `Activity`
//! and nowhere else, so calling it on the `Application` fails with
//! `NoSuchMethodError`, which is exactly what the first run on hardware
//! produced. The `Activity` is reachable through
//! `AndroidApp::activity_as_ptr`, so [`remember_activity`] stashes it at
//! startup and [`ask_for`] uses it.

// JNI is unsafe by nature, and this crate is the board support — which
// is where the workspace's rule says unsafe belongs.
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicPtr, Ordering};

use jni::objects::{JObject, JObjectArray, JString, JValue};
use jni::{jni_sig, jni_str, Env};

/// The `Activity`, as a JNI global reference.
///
/// A raw pointer in a static because the one place it can be obtained —
/// `android_main`'s `AndroidApp` — is nowhere near the code that needs
/// it. `AndroidApp` keeps the global reference alive for as long as it
/// exists, which is the life of the process.
static ACTIVITY: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Stash the `Activity` for later. Called once from `android_main`.
pub fn remember_activity(activity: *mut std::ffi::c_void) {
    ACTIVITY.store(activity, Ordering::Release);
}

/// An error with the Java call that produced it.
#[derive(Debug)]
pub struct Trouble(pub String);

impl From<jni::errors::Error> for Trouble {
    fn from(e: jni::errors::Error) -> Self {
        Self(e.to_string())
    }
}

impl std::fmt::Display for Trouble {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Name the call that failed, so a wrong descriptor is one logcat away
/// from being identified rather than a mystery.
pub fn at<T>(what: &str, got: jni::errors::Result<T>) -> Result<T, Trouble> {
    got.map_err(|e| Trouble(format!("{what}: {e}")))
}

/// `PackageManager.PERMISSION_GRANTED`.
const PERMISSION_GRANTED: i32 = 0;

/// Raise the system's permission dialogue for anything in `wanted` that
/// is not already granted.
///
/// Does not wait for an answer — there is no `onRequestPermissionsResult`
/// to receive without a Java class to receive it in — so this raises the
/// dialogue and the caller finds out later by whether the thing it wanted
/// works. Returns whether everything was already held, which is the one
/// useful answer available synchronously.
///
/// The whole set goes in one request, including the parts already
/// granted. That is not tidiness: from API 31 a request for
/// `ACCESS_FINE_LOCATION` that does not also ask for
/// `ACCESS_COARSE_LOCATION` is *ignored* — no dialogue appears and
/// nothing is granted, which looks exactly like a user who said no.
pub fn ask_for(env: &mut Env<'_>, context: &JObject<'_>, wanted: &[&str]) -> Result<bool, Trouble> {
    let mut missing = Vec::new();
    for permission in wanted {
        let name = at("new_string(permission)", env.new_string(permission))?;
        let state = at(
            "checkSelfPermission",
            env.call_method(
                context,
                jni_str!("checkSelfPermission"),
                jni_sig!((name: java.lang.String) -> jint),
                &[JValue::Object(&name)],
            )
            .and_then(jni::JValueOwned::i),
        )?;
        if state != PERMISSION_GRANTED {
            missing.push(*permission);
        }
    }

    if missing.is_empty() {
        return Ok(true);
    }

    // Only an `Activity` can raise the dialogue. Without one there is
    // nothing to do but say so — and say it once per call rather than
    // silently, because "the GPS never produced a fix" is a much harder
    // thing to diagnose than this line.
    let activity = ACTIVITY.load(Ordering::Acquire);
    if activity.is_null() {
        log::warn!("cannot ask for {missing:?}: no Activity");
        return Ok(false);
    }
    log::info!("asking for {missing:?}");

    // SAFETY: a JNI global reference stored by `remember_activity` from
    // `AndroidApp`, which keeps it alive for the life of the process.
    let activity = unsafe { JObject::from_raw(env, activity.cast()) };

    let blank = at("new_string(blank)", env.new_string(""))?;
    let array = at(
        "new_object_array(String)",
        JObjectArray::<JString<'_>>::new(env, wanted.len(), AsRef::<JString<'_>>::as_ref(&blank)),
    )?;
    for (index, permission) in wanted.iter().enumerate() {
        let name = at("new_string(permission)", env.new_string(permission))?;
        at(
            "set_element(permissions)",
            array.set_element(env, index, &name),
        )?;
    }

    at(
        "requestPermissions",
        env.call_method(
            &activity,
            jni_str!("requestPermissions"),
            jni_sig!((permissions: java.lang.String[], code: jint) -> void),
            &[JValue::Object(&array), JValue::Int(1)],
        ),
    )?;
    Ok(false)
}
