//! Making a noise on a desktop.
//!
//! Through whatever audio player the system has, rather than through an
//! audio library. The reasoning is narrow and worth stating, because the
//! obvious answer is `cpal`:
//!
//! * `cpal` needs ALSA headers at build time, which this project's shell
//!   does not carry — so taking it means a build-environment change as
//!   well as a dependency, to play one tone.
//! * A player is already there. PipeWire, PulseAudio and ALSA each ship
//!   one, and on a machine with none of them there is no audio stack to
//!   play through either.
//!
//! The cost is that this *can* fail to find a player, which is exactly
//! why [`Siren::available`] exists and why the panel shows it. A siren
//! that silently cannot sound is worse than no siren, because the absence
//! of one is at least obvious.

use std::io::Write;
use std::process::{Command, Stdio};

use rutter_app::siren::{alarm_wav, Siren, Urgency};

/// Players to try, best first.
///
/// PipeWire's own first because it is what a current desktop runs;
/// `paplay` next for PulseAudio; `aplay` last because bare ALSA is the
/// fallback everywhere. Each reads a WAV from standard input, which is
/// what lets this avoid a temporary file.
const PLAYERS: [&[&str]; 3] = [
    &["pw-play", "-"],
    &["paplay", "--raw=false", "-"],
    &["aplay", "-q", "-"],
];

#[derive(Debug)]
pub struct Audio {
    /// The player that was found, if any.
    player: Option<&'static [&'static str]>,
    /// The tones, rendered once at startup. A second of 44.1 kHz mono is
    /// 88 KB, and generating it per sounding would be arithmetic done
    /// over and over for a sound that never changes.
    warning: Vec<u8>,
    alarm: Vec<u8>,
}

impl Audio {
    /// Find a player and render the tones.
    pub fn new() -> Self {
        let player = PLAYERS.into_iter().find(|command| {
            command.first().is_some_and(|name| {
                // `--version` rather than running it: asking a player to
                // play nothing is a reliable way to get a noise or a
                // hang, depending on the player.
                Command::new(name)
                    .arg("--version")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            })
        });

        match player.and_then(|p| p.first()) {
            Some(name) => log::info!("alarms will sound through {name}"),
            None => log::warn!(
                "no audio player found — alarms will be silent. \
                 Install pipewire, pulseaudio or alsa-utils."
            ),
        }

        Self {
            player,
            warning: alarm_wav(Urgency::Warning),
            alarm: alarm_wav(Urgency::Alarm),
        }
    }
}

impl Siren for Audio {
    fn sound(&self, urgency: Urgency) {
        let Some(command) = self.player else {
            return;
        };
        let Some((name, arguments)) = command.split_first() else {
            return;
        };

        let wav = match urgency {
            Urgency::Warning => &self.warning,
            Urgency::Alarm => &self.alarm,
        };

        // Spawned and left to run. Waiting would block the frame for the
        // length of the tone, and a chart that freezes for a second every
        // three is a chart that has stopped working during an emergency.
        let child = Command::new(name)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match child {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    // A broken pipe here means the player exited early,
                    // which is its business — the alarm has still been
                    // attempted and the banner is still on screen.
                    let _ = stdin.write_all(wav);
                }
                // Reaped on a thread of its own rather than waited for
                // here: without this every sounding leaves a zombie, and
                // a night at anchor would leave a thousand.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => log::warn!("could not sound the alarm: {e}"),
        }
    }

    fn available(&self) -> bool {
        self.player.is_some()
    }

    fn describe(&self) -> String {
        match self.player.and_then(|p| p.first()) {
            Some(name) => (*name).to_owned(),
            None => "no audio player found".to_owned(),
        }
    }
}
