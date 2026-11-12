//! The phone's own GPS, through `LocationManager`.
//!
//! Android has no NDK location API. Location is Java, and the only way to
//! it from Rust is JNI — so this is the one file in the project that calls
//! into the Java framework, and it is deliberately the only one.
//!
//! ## The constraint that shapes all of this
//!
//! `requestLocationUpdates` wants a `LocationListener`: an *object*
//! implementing a Java interface. You cannot create one of those without
//! Java bytecode, and this APK has none — `android:hasCode="false"` and
//! zero dex entries, which is the whole reason it builds at all where the
//! OpenCPN attempt did not. `java.lang.reflect.Proxy` does not help: it
//! needs an `InvocationHandler`, which is another interface with the same
//! problem.
//!
//! So position is **pulled**, not pushed:
//!
//! 1. `requestLocationUpdates` is called with the `PendingIntent`
//!    overload, which takes no listener. Nothing receives the broadcast —
//!    the point is the side effect, which is that the provider is switched
//!    on and `LocationManager` keeps its cache current.
//! 2. `getLastKnownLocation` is then polled once a second.
//!
//! The honest caveat: if step 1 does not take effect, step 2 still returns
//! something, but it may be an old cached fix from whatever last used the
//! GPS. That is why [`Fix::taken`] carries the location's *own* timestamp
//! rather than the time it was read — a stale fix then shows as stale on
//! the panel instead of being presented as current. Degrading into an
//! visibly old position is acceptable; degrading into a confidently wrong
//! one is not.
//!
//! The alternative, if this proves not good enough on real hardware, is
//! about forty lines of Java in a `classes.dex` built with `d8`. That is a
//! known, boring fix — but it costs the no-bytecode property, so it is
//! worth finding out whether this suffices first.
//!
//! ## Untested on hardware
//!
//! Every JNI descriptor here is checked at compile time by `jni_sig!`,
//! which catches a mistyped signature against the *shape* it claims — but
//! not against what the framework actually declares. A wrong descriptor is
//! a `NoSuchMethodError` at runtime, so every call is wrapped in a message
//! naming the method it was making. One run of `rutter logcat` will say
//! exactly which line is wrong rather than "no GPS".

// JNI is unsafe by nature, and this crate is the board support — which is
// where the workspace's rule says unsafe belongs.
#![allow(unsafe_code)]

use jni::objects::{JObject, JObjectArray, JString, JValue};
use jni::refs::Reference;
use jni::{jni_sig, jni_str, Env, JavaVM};
use rutter_app::{Fix, FixSource};
use rutter_core::LatLon;

/// Metres per second to knots.
const MS_TO_KNOTS: f64 = 1.943_844;

/// Milliseconds between location updates asked of the system.
///
/// One second: faster than a boat needs and the fastest a phone will
/// usually manage. Asking for zero would be asking for whatever the
/// hardware can do, which is a way to flatten a battery at anchor.
const UPDATE_INTERVAL_MS: i64 = 1_000;

/// Metres of movement before an update is worth sending.
///
/// Zero, deliberately: a boat drifting at anchor is exactly the case where
/// you want to see the position creep, and a distance filter would freeze
/// the marker while the boat moved.
const UPDATE_DISTANCE_M: f32 = 0.0;

/// `PendingIntent.FLAG_IMMUTABLE | FLAG_UPDATE_CURRENT`.
///
/// Immutable is required from API 31 for a `PendingIntent` that is not
/// explicitly mutable, and update-current keeps one request rather than
/// stacking a new one on every reconnect.
const PENDING_INTENT_FLAGS: i32 = (1 << 26) | (1 << 27);

/// The permissions a location needs.
///
/// Both, not just the fine one. From API 31, a request for
/// `ACCESS_FINE_LOCATION` that does not also ask for
/// `ACCESS_COARSE_LOCATION` is *ignored* — the dialogue never appears and
/// nothing is granted, which looks exactly like a user who said no.
const PERMISSIONS: [&str; 2] = [
    "android.permission.ACCESS_FINE_LOCATION",
    "android.permission.ACCESS_COARSE_LOCATION",
];

/// `PackageManager.PERMISSION_GRANTED`.
const PERMISSION_GRANTED: i32 = 0;

/// Providers to ask, best first.
///
/// `gps` is the real thing. `fused` is Android's blend of GPS, WiFi and
/// cell, which indoors is the only one that answers at all. `network` is
/// the old coarse provider, kept as a last resort — a position good to a
/// few hundred metres is useless for pilotage but still tells you which
/// harbour you are in while the GPS gets a fix.
const PROVIDERS: [&str; 3] = ["gps", "fused", "network"];

/// An error with the Java call that produced it.
///
/// Exists so `?` works inside the attach closure — `jni` requires the
/// error type to be `From<jni::errors::Error>`, and a bare `String` is
/// not.
#[derive(Debug)]
struct Trouble(String);

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
fn at<T>(what: &str, got: jni::errors::Result<T>) -> Result<T, Trouble> {
    got.map_err(|e| Trouble(format!("{what}: {e}")))
}

/// Open the device's GPS.
///
/// Asks for the permission if it does not have it, which puts a dialogue
/// in front of the user. That happens on first launch, and deliberately:
/// the first question a chart app exists to answer is "where am I", and
/// hiding the GPS behind a button in a panel means the boat does not
/// appear until you go looking for it. A refusal costs nothing — the
/// chart, the tides, the imagery and a network feed all still work.
pub fn open() -> Result<Box<dyn FixSource>, String> {
    let context = ndk_context::android_context();
    if context.vm().is_null() || context.context().is_null() {
        return Err("no Android context — is the activity running?".to_owned());
    }

    // SAFETY: `ndk_context` is populated by android-activity with the
    // JavaVM and Activity this process was launched from, and both were
    // just checked for null.
    let vm = unsafe { JavaVM::from_raw(context.vm().cast()) };
    let activity_raw = context.context().cast();

    let gps = vm
        .attach_current_thread(|env| -> Result<Gps, Trouble> {
            // SAFETY: the activity pointer comes from ndk_context and
            // lives as long as the process.
            let activity = unsafe { JObject::from_raw(env, activity_raw) };

            ask_for_permission(env, &activity)?;

            let manager = at(
                "getSystemService(location)",
                location_manager(env, &activity),
            )?;

            // Switch the provider on. Without this the cache may be stale
            // or empty: Android does not keep a GPS powered for an app
            // that has not asked for updates. See the note at the top —
            // nothing receives these, the request itself is the point.
            if let Err(e) = request_updates(env, &activity, &manager) {
                // Not fatal. `getLastKnownLocation` may still answer from
                // another app's request, and a position that works badly
                // beats a chart with no boat on it.
                log::warn!("GPS: could not request updates ({e}); falling back to the cache");
            }

            let manager = at(
                "new_global_ref(LocationManager)",
                env.new_global_ref(&manager),
            )?;
            let activity = at("new_global_ref(Activity)", env.new_global_ref(&activity))?;

            Ok(Gps {
                vm: vm.clone(),
                activity,
                manager,
                last_taken: 0,
            })
        })
        .map_err(|e: Trouble| e.0)?;

    Ok(Box::new(gps))
}

/// `activity.getSystemService("location")`, as a `LocationManager`.
fn location_manager<'local>(
    env: &mut Env<'local>,
    activity: &JObject<'_>,
) -> jni::errors::Result<JObject<'local>> {
    let name = env.new_string("location")?;
    env.call_method(
        activity,
        jni_str!("getSystemService"),
        jni_sig!((name: java.lang.String) -> java.lang.Object),
        &[JValue::Object(&name)],
    )?
    .l()
}

/// Ask for the location permission if it is not already held.
///
/// Does not wait for an answer: there is no `onRequestPermissionsResult`
/// to receive without Java, so the dialogue is raised and the next poll
/// finds out. A refusal shows on the panel as a provider that never
/// produces a fix, and granting it later from the system settings is
/// picked up by the reader's own retry loop with no restart.
fn ask_for_permission(env: &mut Env<'_>, activity: &JObject<'_>) -> Result<(), Trouble> {
    let mut missing = Vec::new();
    for permission in PERMISSIONS {
        let name = at("new_string(permission)", env.new_string(permission))?;
        let state = at(
            "checkSelfPermission",
            env.call_method(
                activity,
                jni_str!("checkSelfPermission"),
                jni_sig!((name: java.lang.String) -> jint),
                &[JValue::Object(&name)],
            )
            .and_then(jni::JValueOwned::i),
        )?;
        if state != PERMISSION_GRANTED {
            missing.push(permission);
        }
    }

    if missing.is_empty() {
        return Ok(());
    }
    log::info!("GPS: asking for {missing:?}");

    // Both are always requested, even if only one is missing: the pair has
    // to go in together or API 31 and later ignore the request.
    let blank = at("new_string(blank)", env.new_string(""))?;
    let array = at(
        "new_object_array(String)",
        JObjectArray::<JString<'_>>::new(
            env,
            PERMISSIONS.len(),
            AsRef::<JString<'_>>::as_ref(&blank),
        ),
    )?;
    for (index, permission) in PERMISSIONS.iter().enumerate() {
        let name = at("new_string(permission)", env.new_string(permission))?;
        at(
            "set_element(permissions)",
            array.set_element(env, index, &name),
        )?;
    }

    at(
        "requestPermissions",
        env.call_method(
            activity,
            jni_str!("requestPermissions"),
            jni_sig!((permissions: java.lang.String[], code: jint) -> void),
            &[JValue::Object(&array), JValue::Int(1)],
        ),
    )?;
    Ok(())
}

/// `requestLocationUpdates(provider, interval, distance, PendingIntent)`.
///
/// The `PendingIntent` overload, because the listener overloads all need a
/// Java object this APK cannot create. Nothing is registered to receive
/// the broadcast; asking is what switches the provider on.
fn request_updates(
    env: &mut Env<'_>,
    activity: &JObject<'_>,
    manager: &JObject<'_>,
) -> Result<(), Trouble> {
    let action = at(
        "new_string(action)",
        env.new_string("com.pjhockey7.rutter.LOCATION"),
    )?;
    let intent = at(
        "new Intent(String)",
        env.new_object(
            jni_str!("android/content/Intent"),
            jni_sig!((action: java.lang.String) -> void),
            &[JValue::Object(&action)],
        ),
    )?;
    let pending = at(
        "PendingIntent.getBroadcast",
        env.call_static_method(
            jni_str!("android/app/PendingIntent"),
            jni_str!("getBroadcast"),
            jni_sig!(
                (context: android.content.Context, code: jint,
                 intent: android.content.Intent, flags: jint)
                    -> android.app.PendingIntent
            ),
            &[
                JValue::Object(activity),
                JValue::Int(0),
                JValue::Object(&intent),
                JValue::Int(PENDING_INTENT_FLAGS),
            ],
        )
        .and_then(jni::JValueOwned::l),
    )?;

    // Only the real GPS is asked for updates. Asking `fused` as well would
    // double the battery cost for a provider that is only wanted as a
    // fallback while the GPS is still searching.
    let provider = at("new_string(gps)", env.new_string("gps"))?;
    at(
        "requestLocationUpdates",
        env.call_method(
            manager,
            jni_str!("requestLocationUpdates"),
            jni_sig!(
                (provider: java.lang.String, interval: jlong,
                 distance: jfloat, intent: android.app.PendingIntent)
                    -> void
            ),
            &[
                JValue::Object(&provider),
                JValue::Long(UPDATE_INTERVAL_MS),
                JValue::Float(UPDATE_DISTANCE_M),
                JValue::Object(&pending),
            ],
        ),
    )?;
    Ok(())
}

/// The phone's location service, held open.
struct Gps {
    vm: JavaVM,
    /// Kept because a revoked permission has to be asked for again, and
    /// that is a call on the Activity.
    activity: jni::refs::Global<JObject<'static>>,
    manager: jni::refs::Global<JObject<'static>>,
    /// The timestamp of the last fix handed over, so the same cached
    /// location is not reported again and again as though it were new.
    last_taken: i64,
}

// The Java objects are global references, which are valid on any attached
// thread, and the reader thread is the only one that touches this.
//
// SAFETY: `JavaVM` is already Send and Sync; a global reference is
// explicitly thread-independent, unlike the local references this never
// stores.
unsafe impl Send for Gps {}

impl FixSource for Gps {
    fn describe(&self) -> String {
        "this device's GPS".to_owned()
    }

    fn poll(&mut self) -> Result<Option<Fix>, String> {
        let last_taken = self.last_taken;
        let activity = &self.activity;
        let manager = &self.manager;

        let newest = self
            .vm
            .attach_current_thread(|env| -> Result<Option<Fix>, Trouble> {
                // A permission can be revoked from the settings while the
                // app runs, and the next call would then throw. Checked
                // first so the panel says "permission" rather than
                // "SecurityException".
                let activity = activity.as_ref();
                ask_for_permission(env, activity)?;

                // Every provider, keeping whichever has the newest fix.
                // Not first-wins: a phone that has just started has an
                // hours-old `gps` cache and a current `fused` one, and
                // taking the first non-null would show the wrong harbour.
                let mut best: Option<Fix> = None;
                for name in PROVIDERS {
                    match last_known(env, manager.as_ref(), name)? {
                        Some(fix) if best.is_none_or(|held: Fix| fix.taken > held.taken) => {
                            best = Some(fix);
                        }
                        _ => {}
                    }
                }
                Ok(best)
            })
            .map_err(|e: Trouble| e.0)?;

        match newest {
            // Nothing newer than what was already reported. A phone hands
            // back the same cached location until it gets a new one, and
            // reporting it again would make a frozen fix look live.
            Some(fix) if fix.taken <= last_taken => Ok(None),
            Some(fix) => {
                self.last_taken = fix.taken;
                Ok(Some(fix))
            }
            None => Ok(None),
        }
    }
}

/// `getLastKnownLocation(provider)`, unpacked into a [`Fix`].
///
/// `None` when the provider is unknown to this device or has nothing
/// cached, which is the usual state of `gps` indoors.
fn last_known(
    env: &mut Env<'_>,
    manager: &JObject<'_>,
    provider: &str,
) -> Result<Option<Fix>, Trouble> {
    let name = at("new_string(provider)", env.new_string(provider))?;
    let location = match env.call_method(
        manager,
        jni_str!("getLastKnownLocation"),
        jni_sig!((provider: java.lang.String) -> android.location.Location),
        &[JValue::Object(&name)],
    ) {
        Ok(value) => at("getLastKnownLocation -> Location", value.l())?,
        // An unknown provider throws IllegalArgumentException rather than
        // returning null, and `fused` does not exist below API 31. Not an
        // error: it means this device does not have that one.
        Err(e) => {
            log::debug!("GPS: provider {provider} unavailable ({e})");
            return Ok(None);
        }
    };
    if location.is_null() {
        return Ok(None);
    }

    let number = |env: &mut Env<'_>, method: &'static jni::strings::JNIStr| {
        at(
            method.to_str().as_ref(),
            env.call_method(&location, method, jni_sig!(() -> jdouble), &[])
                .and_then(jni::JValueOwned::d),
        )
    };
    let lat = number(env, jni_str!("getLatitude"))?;
    let lon = number(env, jni_str!("getLongitude"))?;

    // Milliseconds since the epoch, as the *provider* reckoned it. This is
    // the whole reason a phone's fix is trustworthy on the panel: a cached
    // position is visibly old rather than silently current.
    let taken_ms = at(
        "getTime",
        env.call_method(&location, jni_str!("getTime"), jni_sig!(() -> jlong), &[])
            .and_then(jni::JValueOwned::j),
    )?;

    // Each of these is only meaningful if the matching `has` says so.
    // Reading `getSpeed` on a location without one gives 0.0, which would
    // put the boat at a dead stop rather than at an unknown speed — and a
    // stopped boat is drawn pointing where its compass says, so the symbol
    // would quietly stop tracking the course.
    let optional = |env: &mut Env<'_>,
                    has: &'static jni::strings::JNIStr,
                    get: &'static jni::strings::JNIStr|
     -> Result<Option<f64>, Trouble> {
        let present = at(
            has.to_str().as_ref(),
            env.call_method(&location, has, jni_sig!(() -> jboolean), &[])
                .and_then(jni::JValueOwned::z),
        )?;
        if !present {
            return Ok(None);
        }
        let value = at(
            get.to_str().as_ref(),
            env.call_method(&location, get, jni_sig!(() -> jfloat), &[])
                .and_then(jni::JValueOwned::f),
        )?;
        Ok(Some(f64::from(value)))
    };

    // Metres per second from Android, knots on the panel.
    let sog_kn =
        optional(env, jni_str!("hasSpeed"), jni_str!("getSpeed"))?.map(|ms| ms * MS_TO_KNOTS);
    let cog_deg = optional(env, jni_str!("hasBearing"), jni_str!("getBearing"))?;
    let accuracy_m = optional(env, jni_str!("hasAccuracy"), jni_str!("getAccuracy"))?;

    Ok(Some(Fix {
        at: LatLon::new(lat, lon),
        sog_kn,
        cog_deg,
        accuracy_m,
        // Android does not say, and inventing a count would be a
        // confident number with nothing behind it.
        satellites: None,
        taken: taken_ms.div_euclid(1_000),
    }))
}
