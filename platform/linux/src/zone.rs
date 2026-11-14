//! The system's local time rules, through libc.
//!
//! `localtime_r` is the only portable way to the host's time-zone
//! database, and it does the whole job: historical offsets, daylight
//! saving, the lot. It reads `TZ` or `/etc/localtime`, so it also honours
//! someone running the app with `TZ=America/Los_Angeles` to plan a
//! passage from a different continent.
//!
//! The unsafe is here, in the board support, which is where the
//! workspace's rule says it belongs.

#![allow(unsafe_code)]

use rutter_app::zone::Zone;

// `tzset` is not in the `libc` crate's Linux surface, and it is needed:
// unlike `localtime`, glibc's `localtime_r` does **not** call it, so
// without this the zone state can be whatever it was at process start —
// which for a process that has never touched a time function is nothing
// at all.
unsafe extern "C" {
    fn tzset();
}

/// Read `TZ` / `/etc/localtime` once.
fn read_the_zone_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: `tzset` takes no arguments and initialises libc's own
    // zone state. `Once` guarantees exactly one call, which is what
    // keeps it off the list of things two threads could race on.
    ONCE.call_once(|| unsafe { tzset() });
}

/// The host's zone.
#[derive(Debug)]
pub struct System;

impl System {
    /// Fill in a `tm` for an instant, or `None` if libc refuses.
    fn broken_down(unix: i64) -> Option<libc::tm> {
        read_the_zone_once();
        let time = unix as libc::time_t;
        let mut out: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: `localtime_r` writes into `out`, which is a valid
        // `tm` we own for the whole call, and reads `time` by value. It
        // is the reentrant form precisely so it does not hand back a
        // pointer to a shared static, so there is nothing here to
        // outlive the call. Returns null on failure, which is checked.
        let got = unsafe { libc::localtime_r(&time, &mut out) };
        (!got.is_null()).then_some(out)
    }
}

impl Zone for System {
    fn offset_secs(&self, unix: i64) -> i32 {
        // `tm_gmtoff` is a GNU/BSD extension rather than POSIX, and is
        // the whole reason this is simple: without it the offset has to
        // be recovered by differencing `gmtime` and `localtime`, which
        // goes wrong across a changeover. Both Linux and macOS have it.
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
        // table, which lives for the life of the process and is not
        // written by anything here. Copied out immediately rather than
        // borrowed, so nothing holds the pointer.
        let name = unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) };
        name.to_str().unwrap_or("local").to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_answers_for_an_instant_without_crashing() {
        // Whatever the build host's zone is, the contract is that an
        // answer comes back and is a plausible offset. Real zones run
        // from -12 to +14 hours.
        let zone = System;
        for unix in [0_i64, 1_772_378_700, 1_793_955_600, -1] {
            let offset = zone.offset_secs(unix);
            assert!(
                (-12 * 3600..=14 * 3600).contains(&offset),
                "{unix} gave {offset}"
            );
            assert!(!zone.abbreviation(unix).is_empty());
        }
    }

    #[test]
    fn utc_is_utc() {
        // Under TZ=UTC the offset must be exactly zero — if this drifts,
        // every local time in the app is wrong by the same amount and
        // nothing else would notice.
        //
        // `set_var` is unsafe from Rust 2024 because another thread
        // reading the environment at the same moment is a data race.
        // This test is single-threaded by construction and sets the
        // variable before the first call that reads it.
        unsafe { std::env::set_var("TZ", "UTC") };
        // Directly rather than through `read_the_zone_once`, which may
        // already have run in this process for another test.
        unsafe { tzset() };
        assert_eq!(System.offset_secs(1_772_378_700), 0);
    }
}
