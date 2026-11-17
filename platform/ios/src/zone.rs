//! The device's local time rules, through libc.
//!
//! Identical to the Linux shim's, and deliberately so: Darwin is a BSD
//! and carries the same `localtime_r` with the same `tm_gmtoff`
//! extension, reading the same compiled zone database. `NSTimeZone` would
//! work too and would be four Objective-C calls where this is one.
//!
//! The one Darwin difference worth knowing is that there is no `TZ`
//! environment variable in a sandboxed app, so this always reports the
//! device's own setting — which is what is wanted.

#![allow(unsafe_code)]

use rutter_app::zone::Zone;

// Not exported by the `libc` crate on any platform, and needed: unlike
// `localtime`, `localtime_r` does not call it, so without this the zone
// state can be whatever it was at process start.
unsafe extern "C" {
    fn tzset();
}

/// Read the device's zone once.
fn read_the_zone_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: `tzset` takes no arguments and initialises libc's own zone
    // state. `Once` guarantees exactly one call, which keeps it off the
    // list of things two threads could race on.
    ONCE.call_once(|| unsafe { tzset() });
}

/// The device's zone.
#[derive(Debug)]
pub struct System;

impl System {
    /// Fill in a `tm` for an instant, or `None` if libc refuses.
    fn broken_down(unix: i64) -> Option<libc::tm> {
        read_the_zone_once();
        let time = unix as libc::time_t;
        let mut out: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: `localtime_r` writes into `out`, which is a valid `tm`
        // we own for the whole call, and reads `time` by value. It is the
        // reentrant form precisely so it does not hand back a pointer to
        // a shared static. Returns null on failure, which is checked.
        let got = unsafe { libc::localtime_r(&time, &mut out) };
        (!got.is_null()).then_some(out)
    }
}

impl Zone for System {
    fn offset_secs(&self, unix: i64) -> i32 {
        Self::broken_down(unix).map_or(0, |tm| tm.tm_gmtoff as i32)
    }

    fn abbreviation(&self, unix: i64) -> String {
        let Some(tm) = Self::broken_down(unix) else {
            return "Z".to_owned();
        };
        if tm.tm_zone.is_null() {
            return "local".to_owned();
        }
        // SAFETY: `tm_zone` points into libc's own static zone-name
        // table, which lives for the life of the process. Copied out
        // immediately rather than borrowed.
        let name = unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) };
        name.to_str().unwrap_or("local").to_owned()
    }
}
