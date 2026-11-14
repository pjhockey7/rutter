//! How much of the screen the system has already spoken for.
//!
//! From Android 15 (API 35) an app that targets 35 or later is *forced*
//! edge-to-edge: the window is the whole display, status bar and gesture
//! bar float over it, and the camera cutout and the rounded corners are
//! the app's problem. Drawing to the edge anyway puts the top row of
//! buttons under a camera hole and clips the leftmost one on the corner
//! radius, which is exactly how this first looked on a Pixel.
//!
//! There is an opt-out — `windowOptOutEdgeToEdgeEnforcement` — and it is
//! deliberately not used. It stops working at targetSdk 36, so taking it
//! would mean doing this work later anyway, and it would hand the top of
//! the chart to a status bar on every device rather than only where
//! something is actually in the way.
//!
//! So the numbers are asked for instead. They are *not* constants: a
//! cutout is a different size on every phone, the gesture bar differs
//! between gesture and three-button navigation, and all four change when
//! the device is rotated. Hence a fresh query on every resize rather than
//! a value cached at startup.

#![allow(unsafe_code)]

use jni::objects::{JObject, JValue};
use jni::{jni_sig, jni_str, Env, JavaVM};

use crate::jvm::{at, Trouble};

/// Screen pixels claimed by the system on each edge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Insets {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl Insets {
    /// Whether anything is actually claimed.
    pub fn any(self) -> bool {
        self.left | self.top | self.right | self.bottom != 0
    }
}

/// Ask the system what it has taken.
///
/// `None` rather than zeroes when it cannot be determined, so a failure
/// reads as "unknown" and leaves the previous value standing rather than
/// snapping the chart to the edge for one frame.
pub fn current() -> Option<Insets> {
    let context = ndk_context::android_context();
    if context.vm().is_null() {
        return None;
    }
    let activity = crate::jvm::activity()?;
    // SAFETY: `ndk_context` is populated by android-activity with this
    // process's JavaVM, just checked for null.
    let vm = unsafe { JavaVM::from_raw(context.vm().cast()) };

    vm.attach_current_thread(|env| -> Result<Option<Insets>, Trouble> {
        // SAFETY: a JNI global reference held for the life of the process.
        let activity = unsafe { JObject::from_raw(env, activity.cast()) };
        read(env, &activity)
    })
    .unwrap_or_else(|e: Trouble| {
        log::warn!("insets: {e}");
        None
    })
}

fn read(env: &mut Env<'_>, activity: &JObject<'_>) -> Result<Option<Insets>, Trouble> {
    let window = at(
        "Activity.getWindow",
        env.call_method(
            activity,
            jni_str!("getWindow"),
            jni_sig!(() -> android.view.Window),
            &[],
        )
        .and_then(jni::JValueOwned::l),
    )?;
    let decor = at(
        "Window.getDecorView",
        env.call_method(
            &window,
            jni_str!("getDecorView"),
            jni_sig!(() -> android.view.View),
            &[],
        )
        .and_then(jni::JValueOwned::l),
    )?;
    let insets = at(
        "View.getRootWindowInsets",
        env.call_method(
            &decor,
            jni_str!("getRootWindowInsets"),
            jni_sig!(() -> android.view.WindowInsets),
            &[],
        )
        .and_then(jni::JValueOwned::l),
    )?;
    // Null until the view is attached, which is a normal state during
    // startup rather than a fault. The caller tries again on the next
    // resize.
    if insets.is_null() {
        return Ok(None);
    }

    // `WindowInsets.Type` and `getInsets` arrived in API 30. Below that
    // the deprecated per-edge getters are the only route, and they do not
    // account for a cutout — which is acceptable, because a phone old
    // enough to lack `Type` is old enough to have a rectangular screen.
    if crate::watch::android_api() < 30 {
        return Ok(Some(Insets {
            left: edge(env, &insets, jni_str!("getSystemWindowInsetLeft"))?,
            top: edge(env, &insets, jni_str!("getSystemWindowInsetTop"))?,
            right: edge(env, &insets, jni_str!("getSystemWindowInsetRight"))?,
            bottom: edge(env, &insets, jni_str!("getSystemWindowInsetBottom"))?,
        }));
    }

    // Both, combined: the status and gesture bars are what overlap the
    // interface, and the cutout is what overlaps the top corner. Asking
    // for either alone leaves one of the two problems unsolved.
    let bars = type_bits(env, jni_str!("systemBars"))?;
    let cutout = type_bits(env, jni_str!("displayCutout"))?;
    let got = at(
        "WindowInsets.getInsets",
        env.call_method(
            &insets,
            jni_str!("getInsets"),
            jni_sig!((types: jint) -> android.graphics.Insets),
            &[JValue::Int(bars | cutout)],
        )
        .and_then(jni::JValueOwned::l),
    )?;

    Ok(Some(Insets {
        left: field(env, &got, jni_str!("left"))?,
        top: field(env, &got, jni_str!("top"))?,
        right: field(env, &got, jni_str!("right"))?,
        bottom: field(env, &got, jni_str!("bottom"))?,
    }))
}

/// One of the `WindowInsets.Type` static methods, each returning a bit.
fn type_bits(env: &mut Env<'_>, name: &jni::strings::JNIStr) -> Result<i32, Trouble> {
    at(
        "WindowInsets.Type",
        env.call_static_method(
            jni_str!("android/view/WindowInsets$Type"),
            name,
            jni_sig!(() -> jint),
            &[],
        )
        .and_then(jni::JValueOwned::i),
    )
}

/// A public `int` field on `android.graphics.Insets`.
fn field(
    env: &mut Env<'_>,
    insets: &JObject<'_>,
    name: &jni::strings::JNIStr,
) -> Result<u32, Trouble> {
    let value = at(
        "Insets field",
        env.get_field(insets, name, jni_sig!(jint))
            .and_then(jni::JValueOwned::i),
    )?;
    Ok(value.max(0).unsigned_abs())
}

/// One of the deprecated per-edge getters, for API 29 and below.
fn edge(
    env: &mut Env<'_>,
    insets: &JObject<'_>,
    name: &jni::strings::JNIStr,
) -> Result<u32, Trouble> {
    let value = at(
        "getSystemWindowInset",
        env.call_method(insets, name, jni_sig!(() -> jint), &[])
            .and_then(jni::JValueOwned::i),
    )?;
    Ok(value.max(0).unsigned_abs())
}
