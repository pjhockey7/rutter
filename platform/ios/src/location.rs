//! The device's GPS, through CoreLocation.
//!
//! ## The difference from Android, and why this one is simpler
//!
//! The Android shim polls `getLastKnownLocation` once a second, because
//! `requestLocationUpdates` wants a `LocationListener` — a Java object —
//! and that APK had no bytecode to make one out of. The honest cost is
//! that a stale cached fix can be handed back, which is why `Fix::taken`
//! carries the location's own timestamp.
//!
//! None of that applies here. `objc2` can declare an Objective-C class at
//! runtime, so a real `CLLocationManagerDelegate` is a dozen lines, and
//! CoreLocation *pushes* fixes at us. The newest fix is parked in a
//! mutex; [`FixSource::poll`] takes it if it is newer than the last one
//! handed over. No cache, no staleness question, no polling of a system
//! API once a second.
//!
//! ## Untested
//!
//! Nothing in this file has been run. It cannot be: it needs an iOS SDK
//! to build and a device to execute. It is type-checked against
//! `aarch64-apple-ios`, which catches a wrong selector *shape* but not a
//! wrong selector — `objc2`'s `msg_send!` resolves names at runtime, so a
//! typo is a crash on a Mac rather than an error here.

#![allow(unsafe_code)]

use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_location::{
    CLAuthorizationStatus, CLLocation, CLLocationManager, CLLocationManagerDelegate,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use rutter_app::{Fix, FixSource};
use rutter_core::LatLon;

/// Metres per second to knots.
const MS_TO_KNOTS: f64 = 1.943_844;

/// The newest fix CoreLocation has handed us, shared with the delegate.
///
/// A mutex rather than a channel: only the newest matters. A boat that
/// was backgrounded for ten minutes should come back to where it is now,
/// not replay six hundred positions to get there.
type Latest = Arc<Mutex<Option<Fix>>>;

define_class!(
    // The delegate CoreLocation calls back on.
    //
    // Declared at runtime by objc2. This is the thing the Android shim
    // could not have — and the reason that one polls and this one does
    // not.
    #[unsafe(super(NSObject))]
    #[name = "RutterLocationDelegate"]
    #[ivars = Latest]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl CLLocationManagerDelegate for Delegate {
        #[unsafe(method(locationManager:didUpdateLocations:))]
        fn did_update(&self, _manager: &CLLocationManager, locations: &NSArray<CLLocation>) {
            // Last is newest: CoreLocation documents the array as
            // ordered, and may deliver several at once after a gap.
            let Some(newest) = locations.lastObject() else {
                return;
            };
            if let Some(fix) = unpack(&newest) {
                if let Ok(mut slot) = self.ivars().lock() {
                    *slot = Some(fix);
                }
            }
        }

        #[unsafe(method(locationManager:didFailWithError:))]
        fn did_fail(&self, _manager: &CLLocationManager, error: &NSError) {
            // Logged and nothing else. A failure here is usually
            // "location services are off" or "permission denied", both of
            // which the panel already shows as a feed with no fixes —
            // and neither of which this can do anything about.
            log::warn!("CoreLocation: {}", error.localizedDescription());
        }

        #[unsafe(method(locationManagerDidChangeAuthorization:))]
        fn did_change_authorization(&self, manager: &CLLocationManager) {
            let status = unsafe { manager.authorizationStatus() };
            log::info!("CoreLocation authorization now {status:?}");
            // Granted after the fact — the usual case, since the
            // dialogue is answered a moment after launch. Updates have
            // to be asked for again, because the first request was made
            // while the answer was still "not determined".
            if matches!(
                status,
                CLAuthorizationStatus::AuthorizedWhenInUse
                    | CLAuthorizationStatus::AuthorizedAlways
            ) {
                unsafe { manager.startUpdatingLocation() };
            }
        }
    }
);

impl Delegate {
    fn new(latest: Latest) -> Retained<Self> {
        let this = Self::alloc().set_ivars(latest);
        unsafe { msg_send![super(this), init] }
    }
}

/// A `CLLocation` as one of ours.
///
/// `None` for a position CoreLocation itself calls invalid: a negative
/// horizontal accuracy is its documented way of saying the fix is no
/// good, and it is a great deal better to show no boat than one in the
/// wrong place.
fn unpack(location: &CLLocation) -> Option<Fix> {
    let accuracy_m = unsafe { location.horizontalAccuracy() };
    if accuracy_m < 0.0 {
        return None;
    }
    let coordinate = unsafe { location.coordinate() };

    // Seconds since the epoch. `timeIntervalSince1970` is a double of
    // seconds, which is what `Fix::taken` wants — and it is the fix's
    // *own* time, so a position delivered late still reads as late.
    let taken = unsafe { location.timestamp().timeIntervalSince1970() } as i64;

    // Negative means unavailable for all three of these, which is
    // CoreLocation's convention rather than a sentinel of ours.
    let speed_ms = unsafe { location.speed() };
    let course_deg = unsafe { location.course() };

    Some(Fix {
        at: LatLon::new(coordinate.latitude, coordinate.longitude),
        sog_kn: (speed_ms >= 0.0).then(|| speed_ms * MS_TO_KNOTS),
        cog_deg: (course_deg >= 0.0).then_some(course_deg),
        accuracy_m: Some(accuracy_m),
        // CoreLocation does not expose a satellite count. It is shown
        // for an NMEA feed and simply absent here, which is honest:
        // inventing one would be worse than the blank.
        satellites: None,
        taken,
    })
}

/// CoreLocation, held open.
pub struct Gps {
    /// Kept alive for as long as fixes are wanted, and never read.
    /// Dropping the manager stops updates, and dropping the delegate
    /// while the manager still points at it leaves a dangling pointer —
    /// the manager holds its delegate weakly.
    _manager: Retained<CLLocationManager>,
    _delegate: Retained<Delegate>,
    latest: Latest,
    /// The timestamp of the last fix handed over, so the same one is not
    /// reported twice as though it were new.
    last_taken: i64,
}

impl std::fmt::Debug for Gps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Gps(CoreLocation)")
    }
}

// SAFETY: the Objective-C objects here are used from the thread that
// built them and from CoreLocation's own callbacks, which run on the
// thread the manager was created on. The only cross-thread traffic is
// `latest`, which is behind a mutex.
unsafe impl Send for Gps {}

/// Open the device's GPS.
///
/// Must be called on the main thread: `CLLocationManager` delivers its
/// callbacks to the run loop of the thread it was created on, and a
/// manager built on a worker with no run loop simply never calls back.
/// That is the single most likely way for this to fail silently on
/// hardware, which is why it is said here rather than discovered there.
pub fn open() -> Result<(Box<dyn FixSource>, Handle), String> {
    // Deliberately no `locationServicesEnabled` pre-check. Apple
    // deprecated it in iOS 17 for a good reason — it blocks the calling
    // thread, and the calling thread here has to be the main one — and
    // the answer it gives is a snapshot that can change a second later
    // anyway. Switched off, the delegate's `didFailWithError` says so,
    // and the panel already shows a feed with no fixes.
    let latest: Latest = Arc::new(Mutex::new(None));
    let delegate = Delegate::new(Arc::clone(&latest));
    let manager = unsafe { CLLocationManager::new() };

    unsafe {
        manager.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // Best available. A chart plotter wants the real GPS, not the
        // cell-tower estimate — and on a boat there is nothing else to
        // fall back to anyway.
        manager.setDesiredAccuracy(objc2_core_location::kCLLocationAccuracyBest);
        // No distance filter: a boat creeping at anchor is exactly the
        // case where the position should keep moving, and a filter would
        // freeze the mark while the boat dragged.
        manager.setDistanceFilter(objc2_core_location::kCLDistanceFilterNone);

        // "When in use" rather than "always": the anchor watch runs while
        // the app is the foreground app or while it holds a background
        // location session, and neither needs the always authorisation —
        // which iOS shows a second, more alarming dialogue for and
        // periodically asks the user to reconsider.
        manager.requestWhenInUseAuthorization();
        manager.startUpdatingLocation();
    }

    log::info!("CoreLocation: started");
    // The manager is shared rather than moved: the `FixSource` keeps it
    // alive and reads from it, and the keepalive needs the same object
    // to switch background updates on. `Retained` is a reference count,
    // so this is one manager with two owners, not two managers.
    let handle = Handle {
        manager: manager.clone(),
    };
    Ok((
        Box::new(Gps {
            _manager: manager,
            _delegate: delegate,
            latest,
            last_taken: 0,
        }),
        handle,
    ))
}

impl FixSource for Gps {
    fn describe(&self) -> String {
        "this device's GPS".to_owned()
    }

    fn poll(&mut self) -> Result<Option<Fix>, String> {
        let newest = self
            .latest
            .lock()
            .map_err(|_| "the location mutex was poisoned".to_owned())?
            .take();

        match newest {
            // Nothing newer than what was already reported.
            Some(fix) if fix.taken <= self.last_taken => Ok(None),
            Some(fix) => {
                if self.last_taken == 0 {
                    log::info!(
                        "CoreLocation: first fix {:.5}, {:.5}{}",
                        fix.at.lat,
                        fix.at.lon,
                        fix.accuracy_m
                            .map(|m| format!(", ±{m:.0} m"))
                            .unwrap_or_default()
                    );
                }
                self.last_taken = fix.taken;
                Ok(Some(fix))
            }
            None => Ok(None),
        }
    }
}

/// Where the keepalive finds the manager once the app has opened it.
///
/// A shared slot rather than a value, because `DeviceGps` takes a
/// *closure*: the app opens the GPS when it wants one and can reopen it
/// after a failure, so there is no single moment at startup when a handle
/// could be handed over. The opener below fills this in each time.
pub type Shared = std::sync::Arc<Mutex<Option<Handle>>>;

/// An empty slot, to be given to both the opener and the keepalive.
pub fn shared() -> Shared {
    std::sync::Arc::new(Mutex::new(None))
}

/// The closure `DeviceGps` wants, which records its handle in `slot`.
pub fn opener(slot: Shared) -> impl Fn() -> Result<Box<dyn FixSource>, String> + Send + Sync {
    move |/* no arguments: the app decides when, not what */| {
        let (source, handle) = open()?;
        if let Ok(mut held) = slot.lock() {
            *held = Some(handle);
        }
        Ok(source)
    }
}

/// A second owner of the location manager, for the keepalive.
///
/// Separate from the [`Gps`] because that one is handed to the app as a
/// `Box<dyn FixSource>` and is not reachable afterwards — and switching
/// background updates on is not a thing a fix source should be asked to
/// do.
pub struct Handle {
    manager: Retained<CLLocationManager>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Handle(CLLocationManager)")
    }
}

// SAFETY: `Keepalive` requires `Send + Sync`, and every call to the
// methods below is made from `App`'s frame loop — which on iOS is the
// main thread, because UIKit will not run anywhere else. The marker is
// what the trait needs; the main thread is what UIKit needs; both hold.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    /// Whether the device will keep sending fixes with the app off the
    /// screen, and ask for that if it can.
    ///
    /// iOS has no foreground service. The equivalent is a background
    /// location session: `UIBackgroundModes` must list `location` in
    /// Info.plist *and* `allowsBackgroundLocationUpdates` must be set —
    /// both, or the app is suspended the moment it leaves the screen and
    /// the anchor watch stops with it.
    ///
    /// Returns whether the hold was actually taken, so the panel can say
    /// plainly that nothing is holding the app open rather than leaving
    /// someone to find out at anchor.
    pub fn set_background_updates(&self, wanted: bool) -> bool {
        unsafe {
            self.manager.setAllowsBackgroundLocationUpdates(wanted);
            // The blue "using your location" bar. Left on deliberately:
            // it is the honest signal that something is still watching
            // and still drawing on the battery, which is the same
            // argument as the Android notification. Hiding it would be
            // hiding the cost.
            self.manager.setShowsBackgroundLocationIndicator(wanted);
            self.manager.allowsBackgroundLocationUpdates()
        }
    }
}
