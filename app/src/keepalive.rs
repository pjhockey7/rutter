//! Staying alive with the screen off.
//!
//! An anchor watch exists for exactly one situation: nobody is looking.
//! You set it, you put the phone down, and eight hours later it either
//! woke you or it didn't. So the alarms were moved off the frame loop and
//! onto the reader thread — see the note on [`crate::App`]'s `watch` —
//! and that fixes the half of the problem that is ours.
//!
//! The other half is not ours. A phone decides what runs. Backgrounded,
//! an app is a candidate for being killed whenever the system wants
//! memory, and no amount of correctness in a reader thread survives the
//! process being shot. The operating system has to be *told* that this
//! one is doing something on the user's behalf, and the telling is
//! different everywhere: Android wants a foreground service with an
//! ongoing notification, iOS wants a background mode, a laptop wants the
//! power manager told not to suspend, and a Raspberry Pi wired to the
//! boat's panel wants nothing at all because it never sleeps.
//!
//! Hence a trait, as with [`crate::siren::Siren`] and
//! [`crate::nmea::FixSource`]. The app says "an alarm is armed" and
//! "it isn't any more" and nothing above here knows what that costs.
//!
//! Two properties worth keeping:
//!
//! * **It is held only while armed.** A notification that sits there
//!   forever is one people swipe away and then learn to ignore, and a
//!   process held up for nothing is a battery complaint.
//! * **Failing to hold is not failing to watch.** If the hold cannot be
//!   taken — permission refused, no service, whatever — the watch still
//!   runs and the siren still sounds. What is lost is only the promise
//!   that the system won't kill the process, which the panel is the place
//!   to say, not the watch.

/// Something that can ask the system to keep this process running.
pub trait Keepalive: Send + Sync + std::fmt::Debug {
    /// Take the hold. `reason` is shown to the user where the platform
    /// shows anything — an Android notification, say — so it should read
    /// as a sentence about the boat and not about the software.
    ///
    /// Called only on a change, never repeatedly, and expected to be
    /// idempotent anyway.
    fn hold(&self, reason: &str);

    /// Let it go.
    fn release(&self);

    /// What the hold is, for the panel. Something like
    /// `"foreground service"` or `"not needed"`.
    fn describe(&self) -> String;
}

/// The line to show while a watch is standing.
///
/// Built here rather than at the call site so every platform's
/// notification says the same thing, and so it can be tested.
pub fn reason(anchored: bool, shallow_m: Option<f64>) -> String {
    match (anchored, shallow_m) {
        (true, Some(m)) => format!("Anchor watch and shallow alarm at {m:.1} m"),
        (true, None) => "Anchor watch standing".to_owned(),
        (false, Some(m)) => format!("Shallow alarm at {m:.1} m"),
        // Not reachable through `App`, which only holds while something
        // is armed, but a trait is a public thing and this beats a panic.
        (false, None) => "Watching".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notification_says_what_is_being_watched() {
        // Someone who sees this on a lock screen at 3 a.m. should be able
        // to tell what woke them without unlocking anything.
        assert_eq!(reason(true, None), "Anchor watch standing");
        assert_eq!(reason(false, Some(3.5)), "Shallow alarm at 3.5 m");
        assert_eq!(
            reason(true, Some(3.5)),
            "Anchor watch and shallow alarm at 3.5 m"
        );
    }
}
