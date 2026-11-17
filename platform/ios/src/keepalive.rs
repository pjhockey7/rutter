//! Staying alive with the screen off.
//!
//! ## There is no foreground service here
//!
//! Android's answer was a `Service` with an ongoing notification. iOS has
//! nothing of the kind: an app that leaves the screen is suspended within
//! seconds, and no amount of asking changes that. What it has instead is
//! a short list of *background modes*, and exactly one of them fits a
//! chart plotter — `location`.
//!
//! An app that declares `location` in `UIBackgroundModes` **and** sets
//! `allowsBackgroundLocationUpdates` on a live `CLLocationManager` keeps
//! running while location updates continue. That is not a loophole; it is
//! the mechanism every navigation app uses, and it is what the anchor
//! watch needs — the watch runs on the reader thread, and the reader
//! thread needs the process not to be suspended.
//!
//! Two consequences worth stating plainly:
//!
//! * **The blue bar.** iOS shows a persistent indicator while an app uses
//!   location in the background. It is deliberately not suppressed. It is
//!   the honest signal that something is still watching and still drawing
//!   on the battery — the same argument as the Android notification.
//! * **It is tied to location, not to the alarm.** If the GPS stops, the
//!   hold stops with it. A shallow-water alarm on an NMEA feed with no
//!   position would not keep the app alive on its own, which the panel
//!   should say rather than imply otherwise.
//!
//! ## Untested
//!
//! None of this has run.

use rutter_app::keepalive::Keepalive;

/// The background location session.
#[derive(Debug)]
pub struct Background {
    /// The GPS the hold is taken on. `None` where the app is running off
    /// a network feed rather than the device's own location, in which
    /// case there is nothing to hold with and the panel says so.
    gps: crate::location::Shared,
}

impl Background {
    pub fn new(gps: crate::location::Shared) -> Self {
        Self { gps }
    }
}

impl Keepalive for Background {
    fn hold(&self, reason: &str) {
        let Ok(gps) = self.gps.lock() else {
            return;
        };
        match gps.as_ref() {
            Some(handle) => {
                let held = handle.set_background_updates(true);
                log::info!(
                    "background location {}: {reason}",
                    if held { "on" } else { "refused" }
                );
            }
            // Said rather than silently skipped: this is the case where
            // an anchor watch will stop when the screen does, and a
            // watch that stops quietly is the failure the whole alarm
            // design exists to avoid.
            None => log::warn!("no device GPS, so nothing will hold the app open: {reason}"),
        }
    }

    fn release(&self) {
        if let Ok(gps) = self.gps.lock() {
            if let Some(handle) = gps.as_ref() {
                handle.set_background_updates(false);
                log::info!("background location off");
            }
        }
    }

    fn describe(&self) -> String {
        "background location".to_owned()
    }
}
