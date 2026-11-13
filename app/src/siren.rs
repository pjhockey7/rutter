//! Making a noise.
//!
//! The alarm's whole value is that it reaches someone who is not looking
//! at the screen. At three in the morning you are asleep, and a banner
//! nobody sees is not an alarm — so the sound is the feature and the
//! drawing is the confirmation.
//!
//! Which makes one property matter more than any other: **a siren that
//! cannot make a noise has to say so**. An alarm that silently fails is
//! worse than no alarm, because the absence of one is at least obvious.
//! So [`Siren::available`] exists and the panel shows it.
//!
//! A trait, like [`crate::nmea::FixSource`] and
//! [`crate::download::TileFetcher`], for the same reason: there is no
//! portable way to make a sound. Android has `ToneGenerator` three JNI
//! calls away; a desktop has whatever its audio stack happens to be. The
//! platform supplies one and nothing above here knows the difference.

/// How urgent the noise should be.
///
/// Two levels rather than a volume, because a siren cannot usefully be
/// asked for a volume — a phone decides that — but it can be asked for a
/// different *sound*, and the difference between "look at this" and "get
/// up" is worth hearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// Something wants attention.
    Warning,
    /// Something is wrong now.
    Alarm,
}

/// Something that can make a noise.
pub trait Siren: Send + Sync + std::fmt::Debug {
    /// Sound once. Called repeatedly while an alarm stands, so it must
    /// return promptly rather than blocking for the length of the tone —
    /// a siren that blocks stops the frame, and a stopped frame is a
    /// chart that has frozen during an emergency.
    fn sound(&self, urgency: Urgency);

    /// Whether a noise can actually be made. False puts a warning on the
    /// panel rather than leaving someone to discover it at anchor.
    fn available(&self) -> bool;

    /// What it sounds through, for the panel.
    fn describe(&self) -> String;
}

/// A sixteen-bit mono WAV of a two-tone alarm.
///
/// Generated rather than shipped: an audio file in the binary is a
/// licence to check and a format to decode, and the sound wanted here is
/// two alternating tones, which is a dozen lines of arithmetic.
///
/// Two tones alternating rather than one steady note, because a steady
/// tone is what every appliance in a galley makes and gets ignored. The
/// alternation is what a real alarm sounds like.
pub fn alarm_wav(urgency: Urgency) -> Vec<u8> {
    const RATE: u32 = 44_100;
    // Long enough to be unmistakable, short enough that the repeat can
    // be driven from the frame loop without overlapping itself.
    let seconds = 1.0_f32;
    // A pair either side of a musical fourth, which beats against itself
    // in a way a single tone does not.
    let (low, high, swaps) = match urgency {
        Urgency::Warning => (660.0_f32, 880.0_f32, 2.0_f32),
        Urgency::Alarm => (750.0_f32, 1_000.0_f32, 6.0_f32),
    };

    let samples = (RATE as f32 * seconds) as usize;
    let mut pcm: Vec<i16> = Vec::with_capacity(samples);
    for index in 0..samples {
        let t = index as f32 / RATE as f32;
        // Which of the two tones is sounding now.
        let frequency = if (t * swaps).fract() < 0.5 { low } else { high };
        // A short fade at each end, or the start and stop click audibly —
        // and a click is what a broken speaker sounds like.
        let fade = (t / 0.01).min((seconds - t) / 0.01).clamp(0.0, 1.0);
        let value = (t * frequency * std::f32::consts::TAU).sin() * fade * 0.6;
        pcm.push((value * f32::from(i16::MAX)) as i16);
    }

    wav(&pcm, RATE)
}

/// Wrap PCM in a WAV header.
fn wav(pcm: &[i16], rate: u32) -> Vec<u8> {
    let data_bytes = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // bytes per second
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in pcm {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// How often to sound while an alarm stands, seconds.
///
/// Often enough to be insistent, and spaced enough that someone can think
/// and reach the Silence button between noises.
pub const REPEAT_SECS: i64 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wav_is_one_a_player_would_accept() {
        for urgency in [Urgency::Warning, Urgency::Alarm] {
            let bytes = alarm_wav(urgency);
            assert_eq!(&bytes[0..4], b"RIFF", "{urgency:?}");
            assert_eq!(&bytes[8..12], b"WAVE");
            assert_eq!(&bytes[12..16], b"fmt ");
            assert_eq!(&bytes[36..40], b"data");

            // The sizes in the header have to match the bytes that follow,
            // or a player reads past the end or stops early.
            let riff = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
            assert_eq!(riff as usize, bytes.len() - 8, "RIFF size");
            let data = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]);
            assert_eq!(data as usize, bytes.len() - 44, "data size");

            // A second of mono 16-bit at 44.1 kHz.
            assert_eq!(data, 44_100 * 2);
        }
    }

    #[test]
    fn it_is_not_silence() {
        // A WAV of zeros is a perfectly valid WAV, plays perfectly, and
        // is exactly as useful as no alarm at all.
        let bytes = alarm_wav(Urgency::Alarm);
        let loudest = bytes[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair).unsigned_abs())
            .max()
            .unwrap_or(0);
        assert!(
            loudest > 10_000,
            "the alarm peaks at {loudest}, which is quiet"
        );
    }

    #[test]
    fn it_starts_and_ends_quietly() {
        // Without the fade the start and stop click, and a click is what
        // a broken speaker sounds like.
        let bytes = alarm_wav(Urgency::Alarm);
        let sample = |index: usize| -> i16 {
            let at = 44 + index * 2;
            i16::from_le_bytes([bytes[at], bytes[at + 1]])
        };
        assert!(sample(0).abs() < 2_000, "it starts with a click");
        let last = (bytes.len() - 44) / 2 - 1;
        assert!(sample(last).abs() < 2_000, "it ends with a click");
    }

    #[test]
    fn the_two_urgencies_sound_different() {
        // Otherwise there is no point in having two.
        assert_ne!(alarm_wav(Urgency::Warning), alarm_wav(Urgency::Alarm));
    }
}

/// Write the tones out, so they can be listened to rather than assumed.
///
/// `cargo test -p rutter-app --lib siren::tests::write_the_tones_out -- --ignored`
/// leaves them in the temporary directory. Ignored by default because it
/// writes files, and kept because "does the alarm sound like an alarm" is
/// not a question any assertion answers.
#[cfg(test)]
mod audition {
    use super::*;

    #[test]
    #[ignore = "writes files; run it on purpose"]
    fn write_the_tones_out() {
        for (name, urgency) in [("warning", Urgency::Warning), ("alarm", Urgency::Alarm)] {
            let path = std::env::temp_dir().join(format!("rutter-{name}.wav"));
            std::fs::write(&path, alarm_wav(urgency)).expect("write the tone");
            println!("{}", path.display());
        }
    }
}
