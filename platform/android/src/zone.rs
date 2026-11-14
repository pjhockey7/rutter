//! The phone's local time rules, through `java.util.TimeZone`.
//!
//! Android carries a full zone database and keeps it updated, so this is
//! two calls: the offset at an instant, and what that offset is called.
//! Both are asked per instant rather than once, because daylight saving
//! moves them — see the note on `rutter_app::zone`.

#![allow(unsafe_code)]

use jni::objects::{JObject, JValue};
use jni::{jni_sig, jni_str, Env, JavaVM};
use rutter_app::zone::Zone;

use crate::jvm::{at, Trouble};

/// The device's zone.
#[derive(Debug)]
pub struct Device;

/// Run something with the VM attached, or give up quietly.
///
/// Quietly, and with a fallback: a time that cannot be localised should
/// come out as UTC, which is honest, rather than as a wrong local time or
/// a crash. A clock is not worth failing a chart over.
fn with_vm<T>(fallback: T, run: impl FnOnce(&mut Env<'_>) -> Result<T, Trouble>) -> T {
    let context = ndk_context::android_context();
    if context.vm().is_null() {
        return fallback;
    }
    // SAFETY: `ndk_context` is populated by android-activity with this
    // process's JavaVM, just checked for null.
    let vm = unsafe { JavaVM::from_raw(context.vm().cast()) };
    vm.attach_current_thread(run).unwrap_or_else(|e: Trouble| {
        log::warn!("zone: {e}");
        fallback
    })
}

/// `TimeZone.getDefault()`.
fn default_zone<'local>(env: &mut Env<'local>) -> Result<JObject<'local>, Trouble> {
    at(
        "TimeZone.getDefault",
        env.call_static_method(
            jni_str!("java/util/TimeZone"),
            jni_str!("getDefault"),
            jni_sig!(() -> java.util.TimeZone),
            &[],
        )
        .and_then(jni::JValueOwned::l),
    )
}

impl Zone for Device {
    fn offset_secs(&self, unix: i64) -> i32 {
        with_vm(0, |env| {
            let zone = default_zone(env)?;
            // `getOffset(long)` takes milliseconds since the epoch and
            // returns milliseconds, daylight saving included. The
            // `getOffset(era, year, month, ...)` overload exists too and
            // is the wrong one — it wants a local date, which is the
            // thing being computed.
            let millis = at(
                "TimeZone.getOffset",
                env.call_method(
                    &zone,
                    jni_str!("getOffset"),
                    jni_sig!((when: jlong) -> jint),
                    &[JValue::Long(unix.saturating_mul(1_000))],
                )
                .and_then(jni::JValueOwned::i),
            )?;
            Ok(millis / 1_000)
        })
    }

    fn abbreviation(&self, unix: i64) -> String {
        with_vm("local".to_owned(), |env| {
            let zone = default_zone(env)?;
            // Whether the short name should be the summer one is a
            // question only this instant's offset answers, so it is
            // asked rather than assumed.
            let in_daylight = self.offset_secs(unix) != raw_offset(env, &zone)?;
            let name = at(
                "TimeZone.getDisplayName",
                env.call_method(
                    &zone,
                    jni_str!("getDisplayName"),
                    jni_sig!((daylight: jboolean, style: jint) -> java.lang.String),
                    // style 0 is SHORT: "PST" rather than "Pacific
                    // Standard Time", which is all a time suffix has
                    // room for.
                    &[JValue::Bool(in_daylight), JValue::Int(0)],
                )
                .and_then(jni::JValueOwned::l),
            )?;
            // `getDisplayName` is declared as returning `String`, but
            // JNI hands back a plain object reference and the cast is
            // checked at runtime rather than assumed — a `Cast` that
            // fails is an error here instead of undefined behaviour
            // somewhere later.
            let name = at(
                "cast(zone name)",
                env.cast_local::<jni::objects::JString<'_>>(name),
            )?;
            let chars = at("zone name", name.mutf8_chars(env))?;
            Ok(chars.to_string())
        })
    }
}

/// `TimeZone.getRawOffset()` — the standard-time offset, with no
/// daylight saving in it.
fn raw_offset(env: &mut Env<'_>, zone: &JObject<'_>) -> Result<i32, Trouble> {
    let millis = at(
        "TimeZone.getRawOffset",
        env.call_method(zone, jni_str!("getRawOffset"), jni_sig!(() -> jint), &[])
            .and_then(jni::JValueOwned::i),
    )?;
    Ok(millis / 1_000)
}
