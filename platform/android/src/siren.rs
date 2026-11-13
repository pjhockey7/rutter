//! Making a noise on a phone, through `ToneGenerator`.
//!
//! The app generates its own two-tone WAV (see `rutter_app::siren`) and a
//! desktop pipes it into whatever audio player is installed. Android has
//! no such pipe — playing a buffer means `AudioTrack`, which means
//! threads, buffer callbacks and a lifecycle, all through JNI.
//!
//! `ToneGenerator` is four calls and does the one thing wanted: a loud
//! tone on the **alarm** stream, which is the stream that still sounds
//! when the phone is on silent. That last part is the entire point. An
//! anchor alarm that respects the ringer switch is an anchor alarm that
//! does not go off at three in the morning, because the phone was
//! silenced at dinner.
//!
//! ## Reading the constants rather than hard-coding them
//!
//! `TONE_SUP_RINGTONE` and friends are `public static final int`, so
//! their values are baked into any app that compiles against them — but
//! this app does not compile against android.jar, it looks everything up
//! at runtime. Writing `33` here would be writing down a number from
//! memory and hoping. So the fields are read by name, once, and a wrong
//! name shows up in logcat as the name rather than as silence.
//!
//! ## Untested on hardware
//!
//! As with `gps.rs`: the signatures are checked at compile time against
//! the shape they claim, not against what the framework declares. If
//! something here is wrong it is a `NoSuchMethodError` naming the call.

#![allow(unsafe_code)]

use jni::objects::{JObject, JValue};
use jni::refs::Global;
use jni::strings::JNIStr;
use jni::{jni_sig, jni_str, Env, JavaVM};
use rutter_app::siren::{Siren, Urgency};

use crate::jvm::{at, Trouble};

/// How loud, 0–100. Full: this is an alarm, and the system volume for the
/// alarm stream is the knob the user actually has.
const VOLUME: i32 = 100;

/// A `ToneGenerator`, kept open.
///
/// Kept rather than built per noise because constructing one allocates an
/// audio track, and an alarm that takes a moment to start is an alarm
/// that sounds hesitant.
#[derive(Debug)]
pub struct Tones {
    generator: Global<JObject<'static>>,
    /// Which tone to play for each urgency, resolved at construction.
    warning: i32,
    alarm: i32,
}

impl Tones {
    /// Open the tone generator, or explain why not.
    ///
    /// Failure is a `String` rather than a panic because the panel shows
    /// it: "no sound available" with the reason on hover is a great deal
    /// more useful than a crash, and far more useful than silence.
    pub fn open() -> Result<Self, String> {
        let context = ndk_context::android_context();
        if context.vm().is_null() {
            return Err("no Android context".to_owned());
        }
        // SAFETY: `ndk_context` is populated by android-activity with
        // this process's JavaVM, just checked for null.
        let vm = unsafe { JavaVM::from_raw(context.vm().cast()) };

        vm.attach_current_thread(|env| -> Result<Self, Trouble> {
            // `AudioManager.STREAM_ALARM` — the stream that ignores the
            // ringer switch, read by name rather than written down as a 4.
            let stream = static_int(
                env,
                jni_str!("android/media/AudioManager"),
                jni_str!("STREAM_ALARM"),
            )?;

            // Two tones, so "look at this" and "get up" are audibly
            // different. A ringtone cadence for the real alarm because it
            // repeats on its own between our three-second re-sounds, and
            // a double beep for a warning.
            let alarm = static_int(
                env,
                jni_str!("android/media/ToneGenerator"),
                jni_str!("TONE_SUP_RINGTONE"),
            )?;
            let warning = static_int(
                env,
                jni_str!("android/media/ToneGenerator"),
                jni_str!("TONE_PROP_BEEP2"),
            )
            // Not worth failing over: a warning that sounds like the
            // alarm still reaches someone.
            .unwrap_or(alarm);

            let generator = at(
                "new ToneGenerator",
                env.new_object(
                    jni_str!("android/media/ToneGenerator"),
                    jni_sig!((stream: jint, volume: jint) -> void),
                    &[JValue::Int(stream), JValue::Int(VOLUME)],
                ),
            )?;
            let generator = at(
                "new_global_ref(ToneGenerator)",
                env.new_global_ref(&generator),
            )?;

            log::info!("siren: ToneGenerator open on stream {stream}");
            Ok(Self {
                generator,
                warning,
                alarm,
            })
        })
        .map_err(|e: Trouble| e.0)
    }
}

/// Read a `public static final int` by name.
fn static_int(env: &mut Env<'_>, class: &JNIStr, field: &JNIStr) -> Result<i32, Trouble> {
    let name = field.to_str();
    let found = at(
        &format!("find_class({})", class.to_str()),
        env.find_class(class),
    )?;
    at(
        &format!("get_static_field({name})"),
        env.get_static_field(&found, field, jni_sig!(jint))
            .and_then(|value| value.i()),
    )
}

impl Siren for Tones {
    fn sound(&self, urgency: Urgency) {
        let (tone, millis) = match urgency {
            // Long enough to wake someone, short enough to finish before
            // the three-second repeat comes round again — overlapping
            // tones on one generator cut each other off and the result
            // sounds broken rather than urgent.
            Urgency::Alarm => (self.alarm, 2_000),
            Urgency::Warning => (self.warning, 500),
        };

        let context = ndk_context::android_context();
        if context.vm().is_null() {
            return;
        }
        // SAFETY: as in `open`.
        let vm = unsafe { JavaVM::from_raw(context.vm().cast()) };

        // Called from the NMEA reader thread, which is not a Java thread,
        // so it attaches each time. `startTone` returns as soon as the
        // tone is queued rather than when it finishes, which is what the
        // trait requires — a siren that blocked for two seconds would
        // block the thread that notices the next fix.
        let _ = vm.attach_current_thread(|env| -> Result<(), Trouble> {
            let generator = self.generator.as_ref();
            if let Err(e) = env.call_method(
                generator,
                jni_str!("startTone"),
                jni_sig!((tone: jint, duration: jint) -> jboolean),
                &[JValue::Int(tone), JValue::Int(millis)],
            ) {
                log::error!("startTone: {e}");
            }
            Ok(())
        });
    }

    fn available(&self) -> bool {
        // It opened, which means the audio track was allocated. Whether a
        // speaker is connected is not a thing Android will say.
        true
    }

    fn describe(&self) -> String {
        "the alarm stream".to_owned()
    }
}
