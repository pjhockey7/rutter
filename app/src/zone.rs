//! What time it is where the boat is.
//!
//! Every time in this app is a Unix timestamp, and every time shown has
//! been UTC with a `Z` on it. That is the right thing to *store* and a
//! poor thing to read: a tide table you have to add seven hours to in
//! your head, at six in the morning, is a tide table you will eventually
//! get wrong.
//!
//! ## Why this is a trait and not a number
//!
//! The obvious shape is an offset in seconds, read once at startup. It is
//! wrong twice a year. San Diego is −8 in winter and −7 in summer, and a
//! tide curve or a forecast drawn across the changeover would put every
//! time on one side of it out by an hour — silently, and most
//! confusingly on exactly the weekend when the clocks moved.
//!
//! So the question is asked per timestamp, not once. And it has to be
//! asked of the platform, because Rust's standard library has no
//! time-zone database and this workspace is not about to carry one:
//! Android has `java.util.TimeZone`, a desktop has the system's own
//! rules, and both already know the answer including every historical
//! oddity.
//!
//! A platform that supplies nothing leaves the app in UTC, which is
//! correct, legible and never wrong — just inconvenient.

/// The local time rules, as the system knows them.
pub trait Zone: Send + Sync + std::fmt::Debug {
    /// Seconds to add to UTC to get local time *at that instant*.
    ///
    /// Per instant, not per zone, because of daylight saving. Positive
    /// east of Greenwich.
    fn offset_secs(&self, unix: i64) -> i32;

    /// What to put after the time — `"PST"`, `"PDT"`, `"CEST"`.
    ///
    /// Also per instant, and for the same reason: the abbreviation is
    /// half of what tells you which side of the changeover you are on.
    fn abbreviation(&self, unix: i64) -> String;
}

/// How times are shown: as stored, or as read locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shown {
    /// UTC, suffixed `Z`. The default, and what every almanac and every
    /// weather product is published in.
    #[default]
    Utc,
    /// The system's local time.
    Local,
}

/// A clock to format times against.
///
/// Resolved once per frame from the setting and the platform's zone, so
/// that every time on a panel is in the same scale as every other — a
/// panel mixing UTC and local would be worse than either.
#[derive(Debug, Clone, Default)]
pub struct Clock {
    offset_secs: i32,
    suffix: String,
}

impl Clock {
    /// UTC.
    pub fn utc() -> Self {
        Self {
            offset_secs: 0,
            suffix: "Z".to_owned(),
        }
    }

    /// Local, as the zone describes it around `unix`.
    ///
    /// Taken at one instant rather than per formatted time: a tide table
    /// whose rows were each in their own offset would not be a table. The
    /// instant to use is the one the panel is about — "now", or wherever
    /// the time slider is — so a day being planned across a changeover
    /// reads in the offset that applies to that day.
    pub fn local(zone: &dyn Zone, unix: i64) -> Self {
        Self {
            offset_secs: zone.offset_secs(unix),
            suffix: zone.abbreviation(unix),
        }
    }

    /// Pick one from the setting, falling back to UTC where the platform
    /// has no zone to ask.
    pub fn resolve(shown: Shown, zone: Option<&dyn Zone>, unix: i64) -> Self {
        match (shown, zone) {
            (Shown::Local, Some(zone)) => Self::local(zone, unix),
            _ => Self::utc(),
        }
    }

    /// Whether this is UTC, for the panel to say so.
    pub fn is_utc(&self) -> bool {
        self.offset_secs == 0 && self.suffix == "Z"
    }

    /// What follows a time.
    pub fn suffix(&self) -> &str {
        &self.suffix
    }

    /// `HH:MM` in this clock's scale, with its suffix.
    pub fn hhmm(&self, unix: i64) -> String {
        let (hour, minute) = self.wall(unix);
        format!("{hour:02}:{minute:02}{}", self.suffix)
    }

    /// The same without the suffix, for a place that says it once for a
    /// whole column.
    pub fn hhmm_bare(&self, unix: i64) -> String {
        let (hour, minute) = self.wall(unix);
        format!("{hour:02}:{minute:02}")
    }

    fn wall(&self, unix: i64) -> (i64, i64) {
        let shifted = unix + i64::from(self.offset_secs);
        // `rem_euclid`, not `%`: a timestamp before 1970 is negative, and
        // `%` would give "-1:-1" for one second before the epoch.
        let seconds = shifted.rem_euclid(86_400);
        (seconds / 3_600, (seconds % 3_600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zone that moves by an hour partway through, like a real one.
    #[derive(Debug)]
    struct Pacific;

    /// 2026-11-01T09:00:00Z — when the US clocks went back that year.
    const CHANGEOVER: i64 = 1_793_955_600;

    impl Zone for Pacific {
        fn offset_secs(&self, unix: i64) -> i32 {
            if unix < CHANGEOVER {
                -7 * 3_600
            } else {
                -8 * 3_600
            }
        }
        fn abbreviation(&self, unix: i64) -> String {
            if unix < CHANGEOVER { "PDT" } else { "PST" }.to_owned()
        }
    }

    #[test]
    fn utc_is_what_it_always_was() {
        let clock = Clock::utc();
        // 2026-03-01T15:25:00Z — the high water NOAA publishes for San
        // Diego that afternoon, so the arithmetic is checked against a
        // real instant rather than a round number.
        assert_eq!(clock.hhmm(1_772_378_700), "15:25Z");
        assert_eq!(clock.hhmm(1_772_323_200), "00:00Z");
        assert_eq!(clock.hhmm(1_772_323_199), "23:59Z");
        assert!(clock.is_utc());
    }

    #[test]
    fn before_the_epoch_does_not_print_a_negative_clock() {
        // One second before 1970. `%` would give "-1:-1Z"; `rem_euclid`
        // is what makes this right.
        assert_eq!(Clock::utc().hhmm(-1), "23:59Z");
    }

    #[test]
    fn local_shifts_the_clock_and_says_which_one_it_is() {
        // Noon UTC is 05:00 Pacific daylight time.
        let summer = CHANGEOVER - 86_400;
        let noon = summer - summer.rem_euclid(86_400) + 12 * 3_600;
        let clock = Clock::local(&Pacific, noon);
        assert_eq!(clock.hhmm(noon), "05:00PDT");
        assert!(!clock.is_utc());
    }

    #[test]
    fn the_offset_follows_daylight_saving() {
        // The reason this is a trait and not a number. An offset read
        // once would put one side of this out by an hour.
        let before = Clock::local(&Pacific, CHANGEOVER - 3_600);
        let after = Clock::local(&Pacific, CHANGEOVER + 3_600);
        assert_eq!(before.suffix(), "PDT");
        assert_eq!(after.suffix(), "PST");
        assert_eq!(before.hhmm(CHANGEOVER), "02:00PDT");
        assert_eq!(after.hhmm(CHANGEOVER), "01:00PST");
    }

    #[test]
    fn a_platform_with_no_zone_stays_in_utc() {
        // Asking for local time on a platform that cannot answer must
        // give UTC rather than a silent zero offset labelled as local —
        // a time that claims to be local and is not is worse than one
        // that is honestly UTC.
        let clock = Clock::resolve(Shown::Local, None, 0);
        assert!(clock.is_utc());
        assert_eq!(clock.suffix(), "Z");
    }
}
