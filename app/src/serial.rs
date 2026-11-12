//! Opening a serial port, safely.
//!
//! A GPS dongle is a UART, and configuring a UART means `termios`, which
//! means `unsafe`. This module is the whole of it: forty lines of ioctl
//! wrapped in one function that hands back a `File`, so that everything
//! above it — the NMEA reader, the parser, the app — stays safe code.
//!
//! ## Why this lives here and not in a platform shim
//!
//! The workspace's rule is that unsafe belongs in the board support, and
//! this is a deliberate exception to it. A serial port is `termios` on
//! Linux, on macOS, on iOS and on Android alike — identical code on four
//! of the five targets. Putting it in `platform/linux` would deny it to
//! the other three; copying it into each would be four copies of the same
//! unsafe to review instead of one.
//!
//! So it is here, in one file, behind one safe function, with the unsafe
//! allowed explicitly at the top where a reviewer will see it. Windows is
//! the one target this does not cover, and it is refused in words rather
//! than silently absent.
//!
//! ## What actually goes wrong with a serial port
//!
//! Most of this function is not "open a file" — it is undoing the ways a
//! port can be left configured by whatever used it last, each of which
//! fails in a way that does not look like a configuration problem:
//!
//! * **Cooked mode** translates the CR in every NMEA sentence terminator,
//!   so every sentence fails its checksum. Looks like a wiring fault.
//! * **No `CLOCAL`** makes a read wait on a carrier line that a GPS dongle
//!   does not drive. Looks like a dead GPS.
//! * **No `O_NOCTTY`** makes the port this process's controlling terminal,
//!   so unplugging the dongle sends `SIGHUP` and the app exits. Looks like
//!   a crash.
//! * **The wrong baud rate** gives a port that opens, delivers bytes, and
//!   delivers nothing but rubbish. Looks like a broken GPS — which is why
//!   the panel shows the refusal count beside the rate.

// The exception this module exists to contain. See the note above: the
// alternative is the same unsafe copied into every platform shim.
#![allow(unsafe_code)]

/// The baud rates a port may be opened at.
///
/// Only the ones with a `termios` constant. Anything else needs Linux's
/// `BOTHER` and a different ioctl, which is a lot of machinery for a rate
/// no NMEA device uses. 4800 is the standard; 38400 is what AIS-capable
/// multiplexers run at; 9600 is what cheap dongles ship at.
///
/// Listed on every platform, including those with no `termios` at all, so
/// that a mistyped rate is refused with the same message everywhere.
pub const BAUD_RATES: [u32; 6] = [4800, 9600, 19200, 38400, 57600, 115_200];

/// Whether a rate can be set.
pub fn is_supported_baud(baud: u32) -> bool {
    BAUD_RATES.contains(&baud)
}

/// The rates, for an error message.
pub fn baud_rates_listed() -> String {
    BAUD_RATES
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Open a serial port for reading at `baud`, in raw 8N1.
///
/// The returned file blocks for up to a second per read and then returns
/// whatever arrived, including nothing. A zero-length read is a quiet
/// second, *not* end of file — the one place this differs from a socket,
/// and treating it as a close would reopen the port every second for a GPS
/// that is merely between sentences.
#[cfg(unix)]
pub fn open(path: &str, baud: u32) -> std::io::Result<std::fs::File> {
    use std::io::{Error, ErrorKind};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let speed = speed_of(baud).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("{baud} baud: expected one of {}", baud_rates_listed()),
        )
    })?;

    let c_path = std::ffi::CString::new(std::ffi::OsStr::new(path).as_bytes())
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "the device path contains a NUL"))?;

    // O_NOCTTY so a hangup on the line cannot signal this process.
    // O_NONBLOCK for the open itself, because opening a port whose carrier
    // is low otherwise waits for DCD — which a GPS with no fix may never
    // assert. Cleared again at the end, once termios governs the timeout.
    //
    // SAFETY: a valid NUL-terminated path; the flags are constants.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_NOCTTY | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(Error::last_os_error());
    }

    // Owned before anything else can fail, so every error path below closes
    // it. A leaked descriptor would hold the port against the next reopen —
    // which is to say against every reconnect after the first.
    //
    // SAFETY: `fd` was just returned by `open` and is not owned elsewhere.
    let port = unsafe { std::fs::File::from_raw_fd(fd) };
    let fd = port.as_raw_fd();

    // SAFETY: all-zeroes is a valid `termios` to read into.
    let mut tty: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is open and `tty` is a valid writable termios.
    if unsafe { libc::tcgetattr(fd, &mut tty) } != 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{path} is not a serial port: {}", Error::last_os_error()),
        ));
    }

    // Raw: no echo, no signal characters, no CR/LF translation.
    //
    // SAFETY: `tty` is a valid termios.
    unsafe { libc::cfmakeraw(&mut tty) };

    // 8N1 with the modem lines ignored, which is NMEA 0183's framing.
    tty.c_cflag |= libc::CREAD | libc::CLOCAL;
    tty.c_cflag &= !(libc::PARENB | libc::CSTOPB | libc::CRTSCTS);

    // Return whatever has arrived after at most a second, rather than
    // waiting for a byte count that may never come. VTIME is in tenths.
    // This is the serial equivalent of the socket read timeout: it exists
    // so the reader thread gets to notice the app has gone away.
    tty.c_cc[libc::VMIN] = 0;
    tty.c_cc[libc::VTIME] = 10;

    // SAFETY: `tty` is a valid termios and `speed` a termios constant.
    if unsafe { libc::cfsetispeed(&mut tty, speed) } != 0
        || unsafe { libc::cfsetospeed(&mut tty, speed) } != 0
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("could not set {baud} baud"),
        ));
    }

    // TCSANOW rather than TCSADRAIN: nothing is being written, so there is
    // nothing to drain.
    //
    // SAFETY: `fd` is open and `tty` is a valid termios.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tty) } != 0 {
        return Err(Error::last_os_error());
    }

    // Whatever arrived while the port was in the previous mode is
    // unreadable by definition. Discarded rather than counted as refused
    // sentences, which would make every open look like a wiring fault.
    //
    // SAFETY: `fd` is open.
    unsafe { libc::tcflush(fd, libc::TCIFLUSH) };

    // Blocking from here, so VMIN and VTIME govern reads instead of the
    // open-time O_NONBLOCK, which would spin at full speed.
    //
    // SAFETY: `fd` is open; F_GETFL and F_SETFL take no pointer arguments.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(Error::last_os_error());
    }

    Ok(port)
}

/// The `termios` constant for a rate.
#[cfg(unix)]
fn speed_of(baud: u32) -> Option<libc::speed_t> {
    Some(match baud {
        4800 => libc::B4800,
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115_200 => libc::B115200,
        _ => return None,
    })
}

/// Serial, where there is no `termios`.
///
/// A refusal in words rather than a silent absence: someone on Windows
/// passing `COM3` should be told it is not wired up yet, not left
/// wondering why nothing arrives. `SetCommState` is the equivalent, and is
/// the obvious thing to write when this project grows a Windows shim.
#[cfg(not(unix))]
pub fn open(path: &str, _baud: u32) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("{path}: serial ports are not supported on this platform yet"),
    ))
}

/// A pseudo-terminal pair, for testing the serial path without hardware.
///
/// A pty is a serial port as far as `termios` is concerned: it accepts
/// every ioctl this module issues, and bytes written to the master arrive
/// at the slave. So the dongle path — open, configure, read, cut into
/// lines, parse — can be exercised end to end on a machine with no GPS
/// plugged into it, which is every machine this is developed on.
///
/// Test-only, and in this module because this is where unsafe is allowed.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod pty {
    use std::fs::File;
    use std::os::unix::io::FromRawFd;

    /// A master end, and the path of its slave.
    ///
    /// Writing to the master is "the GPS is sending"; the reader under test
    /// opens the slave by name, exactly as it would open `/dev/ttyUSB0`.
    pub(crate) fn pair() -> Option<(File, String)> {
        // SAFETY: flags are constants; the call returns a descriptor or -1.
        let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if master < 0 {
            return None;
        }
        // SAFETY: `master` was just returned by posix_openpt.
        let file = unsafe { File::from_raw_fd(master) };

        // SAFETY: `master` is an open pty master.
        if unsafe { libc::grantpt(master) } != 0 || unsafe { libc::unlockpt(master) } != 0 {
            return None;
        }

        let mut name = [0 as libc::c_char; 128];
        // SAFETY: `master` is an open pty master and the buffer is 128
        // bytes, which is past every real /dev/pts path.
        if unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) } != 0 {
            return None;
        }
        // SAFETY: ptsname_r wrote a NUL-terminated path into `name`.
        let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .ok()?
            .to_owned();

        Some((file, path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_nmea_rate_is_supported() {
        // 4800 is what NMEA 0183 specifies and what a dongle ships at. If
        // this list ever loses it, every default-configured GPS stops
        // working with a message about baud rates.
        assert!(is_supported_baud(4800));
        assert!(is_supported_baud(38400), "AIS multiplexers run at 38400");
    }

    #[test]
    fn an_invented_rate_is_refused_with_the_alternatives() {
        assert!(!is_supported_baud(4801));
        assert!(!is_supported_baud(0));
        let listed = baud_rates_listed();
        assert!(listed.contains("4800"), "{listed}");
        assert!(listed.contains("115200"), "{listed}");
    }

    #[test]
    fn a_path_that_is_not_a_serial_port_is_refused_rather_than_read() {
        // /dev/null opens and reads happily, but it is not a tty, so
        // tcgetattr fails — and the message has to say so rather than
        // leaving the reader waiting forever on a device that will never
        // produce a sentence.
        let got = open("/dev/null", 4800);
        let error = got.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            error.contains("not a serial port"),
            "unexpected result: {error:?}"
        );
    }

    #[test]
    fn a_missing_device_is_refused_with_the_reason() {
        let got = open("/dev/rutter-no-such-device", 4800);
        assert!(got.is_err(), "a device that is not there opened");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_pty_opens_and_delivers_what_is_written_to_it() {
        use std::io::{Read, Write};

        let Some((mut master, path)) = pty::pair() else {
            eprintln!("no pty available; skipping");
            return;
        };
        let mut port = open(&path, 4800).expect("the pty should open as a port");

        master.write_all(b"$GPRMC,hello\r\n").expect("write");
        master.flush().expect("flush");

        let mut buffer = [0u8; 64];
        let read = port.read(&mut buffer).expect("read");
        assert_eq!(
            &buffer[..read],
            b"$GPRMC,hello\r\n",
            "raw mode must not translate the CR — a mangled terminator \
             fails every checksum and looks like a wiring fault"
        );
    }

    #[test]
    fn an_unsupported_rate_never_touches_the_device() {
        // Checked before the open, so a mistyped rate does not hold a port
        // that something else may be using.
        let error = open("/dev/null", 1234)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains("1234 baud"), "unexpected: {error:?}");
    }
}
