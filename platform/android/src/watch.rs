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
//! ## Why the intent is built here rather than in Java
//!
//! The obvious shape is a pair of static helpers on `WatchService` and
//! two `CallStaticVoidMethod`s. That was the first attempt, and on
//! hardware it failed:
//!
//! ```text
//! failed to resolve Java class 'com/pjhockey7/rutter/WatchService'
//! (class not found or linkage error)
//! ```
//!
//! on a class that is demonstrably in `classes.dex`. `FindClass` resolves
//! against the class loader of the native method on the call stack, and a
//! thread attached with `AttachCurrentThread` has no Java frame — so it
//! falls back to the *system* class loader, which knows `android.*` and
//! `java.*` and nothing at all about this APK.
//!
//! The usual fix is to go through `Context.getClassLoader().loadClass`.
//! The better one here is not to need our own class at all: a
//! `ComponentName` names a service by *string*, and `ComponentName`,
//! `Intent` and `Context` are all system classes that `FindClass` finds
//! from anywhere. The service is then resolved from the manifest by the
//! system, which is how a component is supposed to be found.
//!
//! So the Java class stays exactly what it should be — a `Service` that
//! knows about notification channels — and nothing calls into it
//! directly.

#![allow(unsafe_code)]

use jni::objects::{JObject, JValue};
use jni::{jni_sig, jni_str, Env, JavaVM};

use crate::jvm::{at, Trouble};

/// The service, as the manifest declares it.
const PACKAGE: &str = "com.pjhockey7.rutter";
const SERVICE: &str = "com.pjhockey7.rutter.WatchService";

/// `WatchService.EXTRA_TEXT`.
const EXTRA_TEXT: &str = "text";

/// Run something with the VM attached and the context to hand.
///
/// The `Context` rather than the `Activity`: `startForegroundService` and
/// `stopService` are declared on `Context`, and `ndk_context` holds the
/// `Application`, which is one. See `jvm.rs`.
fn with_context(what: &str, run: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> Result<(), Trouble>) {
    let context = ndk_context::android_context();
    if context.vm().is_null() || context.context().is_null() {
        log::error!("{what}: no Android context");
        return;
    }
    // SAFETY: `ndk_context` is populated by android-activity with this
    // process's JavaVM and Application, and both were just checked.
    let vm = unsafe { JavaVM::from_raw(context.vm().cast()) };
    let raw = context.context().cast();

    let outcome = vm.attach_current_thread(|env| -> Result<(), Trouble> {
        // SAFETY: a JNI global reference held for the life of the process.
        let object = unsafe { JObject::from_raw(env, raw) };
        run(env, &object)
    });
    if let Err(e) = outcome {
        log::error!("{what}: {e}");
    }
}

/// `new Intent().setComponent(new ComponentName(PACKAGE, SERVICE))`.
fn service_intent<'local>(env: &mut Env<'local>) -> Result<JObject<'local>, Trouble> {
    let package = at("new_string(package)", env.new_string(PACKAGE))?;
    let service = at("new_string(service)", env.new_string(SERVICE))?;
    let component = at(
        "new ComponentName",
        env.new_object(
            jni_str!("android/content/ComponentName"),
            jni_sig!((package: java.lang.String, class: java.lang.String) -> void),
            &[JValue::Object(&package), JValue::Object(&service)],
        ),
    )?;
    let intent = at(
        "new Intent",
        env.new_object(
            jni_str!("android/content/Intent"),
            jni_sig!(() -> void),
            &[],
        ),
    )?;
    at(
        "Intent.setComponent",
        env.call_method(
            &intent,
            jni_str!("setComponent"),
            jni_sig!((component: android.content.ComponentName) -> android.content.Intent),
            &[JValue::Object(&component)],
        ),
    )?;
    Ok(intent)
}

/// Start the watch service, with the line its notification should show.
///
/// Failure is logged and nothing else: the watch itself keeps running
/// either way, and an app that refused to arm an anchor alarm because a
/// notification would not appear would be refusing the useful half over
/// the decorative one. What it costs is only that Android may kill the
/// process while the screen is off — which the panel is the right place
/// to say, not this.
pub fn begin(text: &str) {
    with_context("starting the watch service", |env, context| {
        let intent = service_intent(env)?;
        let key = at("new_string(extra)", env.new_string(EXTRA_TEXT))?;
        let value = at("new_string(text)", env.new_string(text))?;
        at(
            "Intent.putExtra",
            env.call_method(
                &intent,
                jni_str!("putExtra"),
                jni_sig!(
                    (name: java.lang.String, value: java.lang.String)
                        -> android.content.Intent
                ),
                &[JValue::Object(&key), JValue::Object(&value)],
            ),
        )?;

        // `startForegroundService` unconditionally: minSdk is 24 but
        // the call arrived in 26, and below that a plain `startService`
        // is correct. The version check is cheap and the alternative is
        // a `NoSuchMethodError` on an old phone.
        let started = if android_api() >= 26 {
            env.call_method(
                context,
                jni_str!("startForegroundService"),
                jni_sig!((intent: android.content.Intent) -> android.content.ComponentName),
                &[JValue::Object(&intent)],
            )
        } else {
            env.call_method(
                context,
                jni_str!("startService"),
                jni_sig!((intent: android.content.Intent) -> android.content.ComponentName),
                &[JValue::Object(&intent)],
            )
        };
        at("startForegroundService", started)?;

        log::info!("watch service started: {text}");
        Ok(())
    });
}

/// Stop it. Called when the last alarm is disarmed, so the notification
/// goes away and the process stops being held up for nothing.
pub fn end() {
    with_context("stopping the watch service", |env, context| {
        let intent = service_intent(env)?;
        at(
            "stopService",
            env.call_method(
                context,
                jni_str!("stopService"),
                jni_sig!((intent: android.content.Intent) -> jboolean),
                &[JValue::Object(&intent)],
            ),
        )?;
        log::info!("watch service stopped");
        Ok(())
    });
}

/// `Build.VERSION.SDK_INT`.
fn android_api() -> i32 {
    let context = ndk_context::android_context();
    if context.vm().is_null() {
        return 0;
    }
    // SAFETY: as in `with_context`.
    let vm = unsafe { JavaVM::from_raw(context.vm().cast()) };
    vm.attach_current_thread(|env| -> Result<i32, Trouble> {
        let class = at(
            "find_class(Build$VERSION)",
            env.find_class(jni_str!("android/os/Build$VERSION")),
        )?;
        at(
            "SDK_INT",
            env.get_static_field(&class, jni_str!("SDK_INT"), jni_sig!(jint))
                .and_then(jni::JValueOwned::i),
        )
    })
    .unwrap_or_else(|e: Trouble| {
        log::error!("SDK_INT: {e}");
        0
    })
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
        |env, context| match crate::jvm::ask_for(env, context, &[permission])? {
            true => Ok(()),
            false => {
                log::info!("asked for {permission}");
                Ok(())
            }
        },
    );
}
