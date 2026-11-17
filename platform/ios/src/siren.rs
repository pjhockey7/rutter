//! Making a noise on an iPhone, through `AVAudioSession` and
//! `AVAudioPlayer`.
//!
//! ## Why the session and not just a sound
//!
//! `AudioServicesPlayAlertSound` is three lines and would have done,
//! except for the one property that matters: it respects the silent
//! switch. An anchor alarm that goes quiet because the phone was silenced
//! at dinner is not an anchor alarm. The only way to sound regardless is
//! an `AVAudioSession` in the **playback** category, which is what this
//! configures — the same decision as the Android shim's choice of
//! `STREAM_ALARM`.
//!
//! The WAV is the app's own two-tone alarm from `rutter_app::siren`, so
//! this platform and the desktop make the same noise.
//!
//! ## Why these classes are reached by hand
//!
//! `objc2-av-foundation` 0.3 ships *stubs* for `AVAudioSession` and
//! `AVAudioPlayer` — the generated files are two lines of header comment
//! and nothing else. Checked, rather than assumed, before writing
//! against them. So the two classes are looked up in the Objective-C
//! runtime by name and messaged directly, which is what a binding would
//! have done anyway.
//!
//! The cost is honest and worth stating: a selector typo is a crash on a
//! Mac rather than an error here. `msg_send!` resolves names at runtime
//! whichever route is taken, so this is no worse than the bindings — but
//! it is no better either.
//!
//! ## Untested
//!
//! None of this has run. It cannot, off a Mac.

#![allow(unsafe_code)]

use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSData, NSString};
use rutter_app::siren::{alarm_wav, Siren, Urgency};

// The classes below live in AVFoundation, which has to be linked for the
// runtime to know them. Nothing is called through this block; it exists
// for the linker.
#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    /// `AVAudioSessionCategoryPlayback` — "sound is the point of this
    /// app", which is what earns the right to ignore the silent switch.
    ///
    /// Taken as the framework's own symbol rather than written out as a
    /// literal. The value happens to equal its own name, and relying on
    /// that would be relying on a coincidence.
    static AVAudioSessionCategoryPlayback: &'static NSString;
}

/// Two players, kept open.
///
/// Built once rather than per noise: `AVAudioPlayer` decodes on
/// construction, and an alarm that takes a moment to start sounds
/// hesitant at exactly the wrong time.
pub struct Alarm {
    warning: Retained<AnyObject>,
    alarm: Retained<AnyObject>,
}

impl std::fmt::Debug for Alarm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Alarm(AVAudioPlayer)")
    }
}

// SAFETY: `AVAudioPlayer` is documented as safe to message from any
// thread, and the only calls made on it here are `play` and `stop`. The
// reader thread is the one that sounds alarms.
unsafe impl Send for Alarm {}
unsafe impl Sync for Alarm {}

/// Put the audio session into the category that ignores the ringer
/// switch.
///
/// Failure is returned rather than swallowed: an alarm that cannot be
/// heard has to say so, and the panel is where it says it.
fn configure_session() -> Result<(), String> {
    let class = AnyClass::get(c"AVAudioSession").ok_or("no AVAudioSession class")?;
    // SAFETY: `sharedInstance` is a class method returning the singleton.
    let session: Retained<AnyObject> = unsafe { msg_send![class, sharedInstance] };

    // SAFETY: `setCategory:error:` takes the category string and an out
    // pointer for an error; null for the error means "do not tell me
    // why", and the boolean return is the same information.
    let set: bool = unsafe {
        msg_send![&*session, setCategory: &*AVAudioSessionCategoryPlayback, error: std::ptr::null_mut::<*mut AnyObject>()]
    };
    if !set {
        return Err("AVAudioSession would not take the playback category".to_owned());
    }

    // SAFETY: as above. Activating is what actually applies the category.
    let active: bool = unsafe {
        msg_send![&*session, setActive: true, error: std::ptr::null_mut::<*mut AnyObject>()]
    };
    if !active {
        return Err("AVAudioSession would not activate".to_owned());
    }
    Ok(())
}

/// One player over a WAV held in memory.
fn player(wav: &[u8]) -> Result<Retained<AnyObject>, String> {
    let class = AnyClass::get(c"AVAudioPlayer").ok_or("no AVAudioPlayer class")?;
    let data = NSData::with_bytes(wav);

    // SAFETY: `initWithData:error:` takes the audio and an out pointer
    // for an error. It returns nil if the data will not decode, which is
    // checked — a nil `Retained` is the one thing that must not be
    // messaged later.
    let player: Option<Retained<AnyObject>> = unsafe {
        // `alloc` then `initWithData:` — the two halves of Objective-C
        // construction. `AnyClass` has no typed `alloc`, since the type
        // is exactly what is not known here, so the message is sent by
        // hand.
        let allocated: Allocated<AnyObject> = msg_send![class, alloc];
        msg_send![allocated, initWithData: &*data, error: std::ptr::null_mut::<*mut AnyObject>()]
    };
    let player = player.ok_or("AVAudioPlayer would not decode the alarm")?;

    // SAFETY: both are plain setters on a live player.
    unsafe {
        // Full volume. The system alarm volume is the knob the user
        // actually has; this is the app's share of it.
        let _: () = msg_send![&*player, setVolume: 1.0_f32];
        // Decode and buffer now, so `play` is immediate.
        let _: bool = msg_send![&*player, prepareToPlay];
    }
    Ok(player)
}

/// Open the two players, or explain why not.
pub fn open() -> Result<Alarm, String> {
    configure_session()?;
    let warning = player(&alarm_wav(Urgency::Warning))?;
    let alarm = player(&alarm_wav(Urgency::Alarm))?;
    log::info!("siren: AVAudioPlayer ready on the playback category");
    Ok(Alarm { warning, alarm })
}

impl Siren for Alarm {
    fn sound(&self, urgency: Urgency) {
        let player = match urgency {
            Urgency::Warning => &self.warning,
            Urgency::Alarm => &self.alarm,
        };
        // SAFETY: both players were built in `open` and live as long as
        // this does. `play` returns at once and sounds asynchronously,
        // which is what the trait requires — a siren that blocked for the
        // length of the tone would block the thread that notices the next
        // fix.
        unsafe {
            // Rewound first. A player asked to play while it is already
            // playing does nothing at all, so a re-sound three seconds
            // into a two-second tone would be silently dropped.
            let _: () = msg_send![&**player, setCurrentTime: 0.0_f64];
            let started: bool = msg_send![&**player, play];
            if !started {
                log::error!("siren: AVAudioPlayer refused to play");
            }
        }
    }

    fn available(&self) -> bool {
        // It opened, which means the session took the playback category
        // and both WAVs decoded. Whether anything is connected to listen
        // is not a thing iOS will say.
        true
    }

    fn describe(&self) -> String {
        "the playback category".to_owned()
    }
}
