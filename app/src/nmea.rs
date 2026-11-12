//! An NMEA 0183 feed over the network, read on a thread of its own.
//!
//! The parsing lives in `rutter_core::nmea`, which has no I/O in it at all.
//! This is the half that talks to a socket: connect, read bytes, cut them
//! into lines, hand each line to the parser, and publish what the boat
//! currently is.
//!
//! ## Why a network socket and not the phone's GPS
//!
//! Both, eventually. But this one comes first, because it is the one that
//! works everywhere and carries everything.
//!
//! A phone's own GPS means Android's `LocationManager`, which means Java,
//! which means JNI and a `classes.dex` in the APK and a runtime permission
//! dialogue — platform code, in the one place this project tries hardest to
//! keep thin, and useless on the laptop. An NMEA socket is `std::net` and
//! nothing else. It is identical on all five targets.
//!
//! And it is strictly more capable. A chartplotter, a VHF with AIS, a
//! masthead unit or a depth sounder on a boat's network all speak this, so
//! one reader gets position *and* depth *and* wind *and* heading from the
//! instruments actually on the boat. Phone GPS gets position alone. A relay
//! app on the phone — GPS2IP, Share GPS — turns the handset into exactly
//! this kind of source, so supporting the socket subsumes supporting the
//! handset rather than duplicating it.
//!
//! ## Why a thread
//!
//! Same reason as the chart worker: a read blocks. A TCP connect to a phone
//! that has gone to sleep blocks for the whole connect timeout, and doing
//! that between two frames stops the window. So the socket lives here and
//! the main thread reads a snapshot.
//!
//! ## What it does when the boat's network misbehaves, which it will
//!
//! * **Reconnects, forever.** A WiFi link on a boat drops when you walk
//!   past the mast. The thread retries on a fixed delay and says so in the
//!   panel; it never gives up and it never needs restarting.
//! * **Survives a silent socket.** A TCP connection that stays open while
//!   the far end has died reads nothing and returns no error. After
//!   [`SILENCE_LIMIT`] of that it is treated as dead and reconnected.
//! * **Resynchronises on rubbish.** Half a sentence, a wrong baud rate or a
//!   binary NMEA 2000 gateway on the wrong port all produce bytes with no
//!   newline in them. A line that grows past [`MAX_LINE`] is discarded and
//!   reading continues from the next newline.
//! * **Keeps the last fix.** Losing the feed ages the boat's position; it
//!   does not delete it. A marker shown as stale is more use than a marker
//!   that vanishes.

use std::io::Read;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rutter_core::nmea::{self, Fix, Vessel};

/// The default port for NMEA 0183 over IP.
///
/// Not a standard in the sense of being written down by the NMEA, but
/// universal in practice: it is what the open-source world settled on and
/// what multiplexers, relay apps and plotters all default to.
pub const DEFAULT_PORT: u16 = 10110;

/// Longest line accepted before giving up and resynchronising.
///
/// NMEA 0183 caps a sentence at 82 characters. This is deliberately well
/// past that rather than exact, because a few devices overrun it slightly
/// and the point of the limit is to notice *binary* data, not to enforce
/// the spec.
const MAX_LINE: usize = 512;

/// How long to wait before reconnecting after a failure.
const RETRY_DELAY: Duration = Duration::from_secs(3);

/// How long to wait on a connect before calling it failed.
///
/// Short, because the common case for a failure is a phone that is asleep
/// or an address typed wrong, and the operating system's own timeout is
/// over a minute of the panel saying nothing.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// How long a read waits before coming back empty-handed.
///
/// Not a failure when it expires: a quiet feed is normal. It only exists so
/// the thread gets to check whether anyone is still listening to it.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an open socket may say nothing before it is treated as dead.
///
/// A real feed sends at least a GGA or an RMC every second or two. Half a
/// minute of silence on a socket that is still nominally open means the far
/// end is gone and TCP has not noticed yet.
const SILENCE_LIMIT: Duration = Duration::from_secs(30);

/// How long a serial read waits before returning what it has.
///
/// Set by `VTIME` in [`crate::serial`]; named here because the silence
/// deadline counts in these.
const SERIAL_READ_WINDOW: Duration = Duration::from_secs(1);

/// A position source that is not a feed of sentences — the device's own
/// GPS, reached through whatever its operating system offers.
///
/// A trait, and implemented by the platform shim, because this is the one
/// part of getting a position that cannot be portable: on Android it is
/// `LocationManager` through JNI, on iOS it will be CoreLocation, and on a
/// laptop there is usually no such thing at all. What the portable layer
/// needs from all of them is the same four lines, so that is what the
/// trait says and nothing more.
///
/// Polled rather than callback-driven deliberately. Every one of these
/// APIs delivers on a thread of its own choosing, and a callback would
/// have to hand the fix across to this crate's reader thread anyway — so
/// the handing across is the interface, and the reader stays the one place
/// that owns the boat.
pub trait FixSource: Send {
    /// A line for the panel: what this source is.
    fn describe(&self) -> String;

    /// The newest fix.
    ///
    /// `Ok(None)` means nothing new — no fix yet, or the same one as last
    /// time. `Err` means the source has failed in a way worth showing;
    /// the reader will report it and try again.
    fn poll(&mut self) -> Result<Option<Fix>, String>;
}

/// How to open the device's GPS, held so it can be opened more than once.
///
/// A factory rather than a `Box<dyn FixSource>` because switching to a
/// network feed and back again is a reasonable thing to do on a boat — the
/// phone's own GPS while the instruments are off, the instruments once
/// they are on — and a source that was moved into a thread cannot be moved
/// back out of it.
/// Behind an `Arc` so the app can keep a handle while the reader thread
/// holds another — which is what makes switching to a network feed and
/// back again possible rather than one-way.
#[derive(Clone)]
pub struct DeviceGps(std::sync::Arc<dyn Fn() -> Result<Box<dyn FixSource>, String> + Send + Sync>);

impl DeviceGps {
    pub fn new(
        open: impl Fn() -> Result<Box<dyn FixSource>, String> + Send + Sync + 'static,
    ) -> Self {
        Self(std::sync::Arc::new(open))
    }

    fn open(&self) -> Result<Box<dyn FixSource>, String> {
        (self.0)()
    }
}

// A closure has no Debug, and the workspace wants one on everything so
// that an `App` can be printed when something is wrong with it.
impl std::fmt::Debug for DeviceGps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceGps")
    }
}

/// How often the device's GPS is asked for a fix.
///
/// A phone's location service updates about once a second at best, so
/// asking faster only costs JNI calls. Asking much slower would make the
/// boat lurch between positions instead of moving.
const DEVICE_POLL: Duration = Duration::from_millis(1000);

/// Where to get sentences from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// Connect out to a device that is listening. What a phone relay app
    /// and most multiplexers offer.
    Tcp(String),
    /// Bind and listen for datagrams. What a device that broadcasts to the
    /// whole boat network sends.
    Udp(String),
    /// The device's own GPS, through [`FixSource`]. Not spellable as a
    /// command-line address, because there is nothing to address — it is
    /// whatever the platform offers, or nothing.
    Device,
    /// A serial port: a USB GPS dongle, or an actual NMEA 0183 wire
    /// through a level converter.
    ///
    /// The oldest and still the most common way to get a position onto a
    /// laptop, and the only one that needs no network at all — which on a
    /// boat, where the network is a phone hotspot that drops when you walk
    /// past the mast, is a real advantage.
    Serial { path: String, baud: u32 },
}

/// The rate NMEA 0183 specifies, and what nearly every dongle ships at.
///
/// A wrong rate does not fail cleanly: the port opens, bytes arrive, and
/// every one of them is rubbish. That reads as a broken GPS rather than a
/// mistyped number, which is why the refusal count is on the panel.
pub const DEFAULT_BAUD: u32 = 4800;

impl Endpoint {
    /// Parse a command-line spec.
    ///
    /// ```text
    /// tcp://192.168.1.50:10110    connect out
    /// udp://0.0.0.0:10110         listen for broadcasts
    /// 192.168.1.50:10110          TCP, the common case
    /// 192.168.1.50                TCP on the default port
    /// udp://:10110                listen on every interface
    /// /dev/ttyUSB0                a serial GPS at 4800 baud
    /// /dev/ttyUSB0@38400          the same, at a stated rate
    /// serial:///dev/ttyACM0       spelled out, if you prefer
    /// ```
    ///
    /// TCP is the default scheme because connecting out is what a phone
    /// relay wants, and because a wrong guess there fails loudly — nothing
    /// arrives — whereas a UDP socket bound to the wrong thing sits there
    /// looking healthy and silent.
    ///
    /// A leading `/` means a serial port with no scheme needed, because a
    /// host and port never start with one and `/dev/ttyUSB0` is what a
    /// person already has written down.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("no address given".to_owned());
        }

        let (udp, rest) = match spec.split_once("://") {
            Some(("udp", rest)) => (true, rest),
            Some(("tcp", rest)) => (false, rest),
            Some(("serial", rest)) => return Self::serial(rest),
            Some((scheme, _)) => {
                return Err(format!("{scheme}: expected tcp://, udp:// or serial://"));
            }
            None if spec.starts_with('/') => return Self::serial(spec),
            None => (false, spec),
        };

        // A bare host, or `udp://:port` for every interface. Spelled out so
        // the address handed to the socket is always host:port and the
        // defaults are visible here rather than buried in a socket call.
        let address = if let Some(port) = rest.strip_prefix(':') {
            format!("0.0.0.0:{port}")
        } else if rest.contains(':') {
            rest.to_owned()
        } else {
            format!("{rest}:{DEFAULT_PORT}")
        };

        if address.starts_with(':') || address.ends_with(':') {
            return Err(format!("{spec}: expected host:port"));
        }

        Ok(if udp {
            Self::Udp(address)
        } else {
            Self::Tcp(address)
        })
    }

    /// A serial spec: a device path, optionally `@baud`.
    ///
    /// `@` rather than `:` separates the rate, because a device path may
    /// contain a colon on some systems and `:4800` would then be
    /// ambiguous with part of the name.
    fn serial(rest: &str) -> Result<Self, String> {
        let (path, baud) = match rest.rsplit_once('@') {
            Some((path, rate)) => {
                let baud: u32 = rate
                    .parse()
                    .map_err(|_| format!("{rate}: expected a baud rate"))?;
                (path, baud)
            }
            None => (rest, DEFAULT_BAUD),
        };
        if path.is_empty() {
            return Err("no device given".to_owned());
        }
        // Refused here rather than at open, so a mistyped rate is caught
        // before the port is held and before the panel starts counting
        // refusals at a device that is working perfectly.
        if !crate::serial::is_supported_baud(baud) {
            return Err(format!(
                "{baud} baud: expected one of {}",
                crate::serial::baud_rates_listed()
            ));
        }
        Ok(Self::Serial {
            path: path.to_owned(),
            baud,
        })
    }

    /// How to describe this in the panel.
    pub fn address(&self) -> &str {
        match self {
            Self::Tcp(a) | Self::Udp(a) => a,
            Self::Serial { path, .. } => path,
            Self::Device => "this device",
        }
    }

    /// Which transport this is, for a log line or a usage message.
    pub fn protocol_name(&self) -> &'static str {
        match self {
            Self::Tcp(_) => "TCP",
            Self::Udp(_) => "UDP",
            Self::Serial { .. } => "serial",
            Self::Device => "built-in",
        }
    }
}

/// What is at the other end of an open feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// The peer for TCP, the bound local address for UDP — the useful one
    /// in each case. Carried as an address rather than a string so a test
    /// can discover which port it got when it asked for port 0.
    Socket(SocketAddr),
    /// A serial port, and the rate it was opened at. The rate belongs here
    /// because it is the first thing to doubt when a port is open and
    /// every sentence is being refused.
    Port { path: String, baud: u32 },
    /// The device's own GPS, as it describes itself.
    Device(String),
}

/// What the feed is doing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Link {
    /// Opening, or waiting to retry after a failure.
    #[default]
    Connecting,
    /// Open and readable.
    Open(Opened),
    /// The last attempt failed, with the reason. Kept rather than logged
    /// and dropped: "connection refused" is the whole diagnosis, and a
    /// panel that only says "no fix" sends you looking in the wrong place.
    Down(String),
}

impl Link {
    /// The address of an open socket, if this is one.
    ///
    /// Exists for the tests, which ask for port 0 and need to know what
    /// they were given before they can send anything to it.
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        match self {
            Self::Open(Opened::Socket(at)) => Some(*at),
            _ => None,
        }
    }

    /// Whether the feed is open, whatever it is open on.
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open(_))
    }
}

/// A snapshot of the feed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub vessel: Vessel,
    pub link: Link,
    /// Sentences understood.
    pub accepted: u64,
    /// Sentences parsed but of a kind not handled — GSV, AIS, a maker's
    /// private sentence. Normal, and counted separately from rubbish so
    /// that a feed of nothing but AIS is diagnosable.
    pub unsupported: u64,
    /// Lines refused: bad checksum, truncated, or not a sentence at all.
    /// A non-zero and climbing count means the wrong port or a noisy link.
    pub rejected: u64,
}

/// An NMEA feed being read somewhere else.
#[derive(Debug)]
pub struct Receiver {
    from_reader: mpsc::Receiver<Report>,
    /// The newest snapshot. Held on this side so every caller in a frame
    /// sees the same boat, and so the state survives a frame with no
    /// traffic in it.
    last: Report,
    endpoint: Endpoint,
}

impl Receiver {
    /// Start reading. `None` if the thread could not be started — the same
    /// rule as the chart worker: an app with no GPS is still a chart.
    pub fn spawn(endpoint: Endpoint) -> Option<Self> {
        let (to_app, from_reader) = mpsc::channel::<Report>();
        let moved = endpoint.clone();

        if let Err(e) = thread::Builder::new()
            .name("rutter-nmea".to_owned())
            .spawn(move || reader(&moved, &to_app))
        {
            log::error!("could not start the NMEA reader: {e}");
            return None;
        }

        log::info!(
            "NMEA: reading {} from {}",
            endpoint.protocol_name(),
            endpoint.address()
        );
        Some(Self {
            from_reader,
            last: Report::default(),
            endpoint,
        })
    }

    /// Take whatever has arrived. Never blocks. True if anything changed.
    ///
    /// Drains to the newest rather than taking one per call: at ten
    /// sentences a second and sixty frames a second this is usually zero or
    /// one, but a frame that took 200 ms must not leave the boat two
    /// seconds behind.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(report) = self.from_reader.try_recv() {
            self.last = report;
            changed = true;
        }
        changed
    }

    /// Start reading the device's own GPS.
    ///
    /// Shares everything above the transport with the NMEA reader: the
    /// same thread pattern, the same [`Report`] over the same channel, so
    /// the panel, the boat marker and the follow mode cannot tell the
    /// difference and did not need changing.
    pub fn spawn_device(gps: DeviceGps) -> Option<Self> {
        let (to_app, from_reader) = mpsc::channel::<Report>();

        if let Err(e) = thread::Builder::new()
            .name("rutter-gps".to_owned())
            .spawn(move || device_reader(&gps, &to_app))
        {
            log::error!("could not start the GPS reader: {e}");
            return None;
        }

        Some(Self {
            from_reader,
            last: Report::default(),
            endpoint: Endpoint::Device,
        })
    }

    pub fn report(&self) -> &Report {
        &self.last
    }

    pub fn vessel(&self) -> &Vessel {
        &self.last.vessel
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// How to describe the link in one line.
    pub fn status(&self) -> String {
        match &self.last.link {
            Link::Connecting => format!(
                "connecting to {} {}",
                self.endpoint.protocol_name(),
                self.endpoint.address()
            ),
            Link::Open(Opened::Socket(at)) => match self.endpoint {
                Endpoint::Tcp(_) => format!("connected to {at}"),
                _ => format!("listening on {at}"),
            },
            Link::Open(Opened::Port { path, baud }) => {
                format!("reading {path} at {baud} baud")
            }
            Link::Open(Opened::Device(what)) => what.clone(),
            Link::Down(why) => format!("{}: {why}", self.endpoint.address()),
        }
    }

    /// Whether the panel should show the link as a problem.
    pub fn troubled(&self) -> bool {
        matches!(self.last.link, Link::Down(_))
    }
}

/// Sentences accumulating into a boat, and the channel they are published
/// on.
///
/// Together in one struct because every read path needs all of it, and
/// because publishing has to happen from inside the read loops — a report
/// that waited for the loop to end would never be sent at all.
struct Feed<'a> {
    report: Report,
    to_app: &'a mpsc::Sender<Report>,
    /// Bytes seen since the last newline.
    partial: Vec<u8>,
    /// Whether the current line is being skipped because it overran.
    resyncing: bool,
}

impl<'a> Feed<'a> {
    fn new(to_app: &'a mpsc::Sender<Report>) -> Self {
        Self {
            report: Report::default(),
            to_app,
            partial: Vec::new(),
            resyncing: false,
        }
    }

    /// Publish the current state. False if the app has gone away, which is
    /// the thread's signal to stop.
    fn publish(&self) -> bool {
        self.to_app.send(self.report.clone()).is_ok()
    }

    fn set_link(&mut self, link: Link) -> bool {
        if self.report.link != link {
            self.report.link = link;
            return self.publish();
        }
        true
    }

    /// Feed raw bytes in. Returns false if the app has gone away.
    ///
    /// Lines are cut here rather than with `BufRead::read_line` because a
    /// read timeout mid-sentence makes that method's buffer contents
    /// unspecified, and a timeout mid-sentence is routine on a feed that
    /// sends one burst a second.
    ///
    /// One report per call, not per sentence: a burst is six sentences and
    /// the main thread only ever looks at the newest. And a report for
    /// *any* line, not only one that moved the boat — the first version
    /// published only on a change, which meant a feed of pure rubbish
    /// reported "0 refused" while refusing all of it. The counts exist to
    /// tell "no data" from "data, all bad", and that only works if they
    /// arrive.
    fn bytes(&mut self, bytes: &[u8]) -> bool {
        let mut lines = 0usize;

        for &byte in bytes {
            if byte == b'\n' {
                if !self.resyncing {
                    // `from_utf8_lossy` and not a hard refusal: a stray
                    // high byte in one field should not cost the fix in
                    // the others, and the checksum has the final say
                    // anyway.
                    let line = String::from_utf8_lossy(&self.partial).trim().to_owned();
                    if !line.is_empty() {
                        self.line(&line);
                        lines += 1;
                    }
                }
                self.partial.clear();
                self.resyncing = false;
                continue;
            }
            if byte == b'\r' {
                continue;
            }
            if self.partial.len() >= MAX_LINE {
                // Too long to be a sentence. Drop the rest of it and wait
                // for a newline to get back in step.
                if !self.resyncing {
                    self.report.rejected += 1;
                    log::warn!("NMEA: a line ran past {MAX_LINE} bytes; resynchronising");
                }
                self.partial.clear();
                self.resyncing = true;
                continue;
            }
            self.partial.push(byte);
        }

        if lines > 0 {
            return self.publish();
        }
        true
    }

    /// Account for one line: understood, ignored, or refused.
    fn line(&mut self, line: &str) {
        // The arrival time, not the time in the sentence. A GPS states its
        // own clock to the second with no date in most sentences, and a
        // receiver with no fix states nothing at all — while what staleness
        // needs is simply "how long ago did this reach us".
        let now = unix_now();

        match nmea::parse(line) {
            Ok(sentence) => {
                self.report.accepted += 1;
                self.report.vessel.apply(sentence, now);
            }
            Err(nmea::Invalid::Unsupported) => self.report.unsupported += 1,
            Err(e) => {
                self.report.rejected += 1;
                // Only the first few, and only at debug: a wrong-port feed
                // produces thousands a second and filling the log with
                // them loses the one line that said what went wrong.
                if self.report.rejected <= 5 {
                    log::debug!("NMEA: refused {line:?}: {e:?}");
                }
            }
        }
    }
}

/// The device GPS's loop: open it, poll it, publish what it says.
///
/// The same shape as [`reader`] — open, read until it fails, wait, open
/// again — because the failure modes are the same ones. A location
/// permission can be refused and then granted from the system settings
/// while the app is running, and a provider can be switched off and on,
/// both of which this recovers from without a restart.
fn device_reader(gps: &DeviceGps, to_app: &mpsc::Sender<Report>) {
    let mut feed = Feed::new(to_app);

    loop {
        if !feed.set_link(Link::Connecting) {
            break;
        }

        match gps.open() {
            Ok(mut source) => {
                let what = source.describe();
                if !feed.set_link(Link::Open(Opened::Device(what.clone()))) {
                    break;
                }
                log::info!("GPS: {what}");

                // Polled until it fails. A source that merely has no fix
                // yet returns Ok(None) and stays open, which is the normal
                // state of a phone indoors.
                loop {
                    match source.poll() {
                        Ok(Some(fix)) => {
                            feed.report.accepted += 1;
                            feed.report.vessel.apply_fix(fix);
                            if !feed.publish() {
                                return;
                            }
                        }
                        Ok(None) => {
                            // Still a publish: it is how the thread
                            // notices the app has gone, and how a feed
                            // with no fix keeps its counts on the panel.
                            if !feed.publish() {
                                return;
                            }
                        }
                        Err(why) => {
                            log::warn!("GPS: {why}");
                            if !feed.set_link(Link::Down(why)) {
                                return;
                            }
                            break;
                        }
                    }
                    thread::sleep(DEVICE_POLL);
                }
            }
            Err(why) => {
                log::warn!("GPS: {why}");
                if !feed.set_link(Link::Down(why)) {
                    break;
                }
            }
        }

        thread::sleep(RETRY_DELAY);
    }

    log::debug!("GPS reader finished");
}

/// The reader's loop: open, read until it fails, wait, open again.
fn reader(endpoint: &Endpoint, to_app: &mpsc::Sender<Report>) {
    let mut feed = Feed::new(to_app);

    loop {
        if !feed.set_link(Link::Connecting) {
            break;
        }

        let outcome = match endpoint {
            Endpoint::Tcp(address) => read_tcp(address, &mut feed),
            Endpoint::Udp(address) => read_udp(address, &mut feed),
            Endpoint::Serial { path, baud } => read_serial(path, *baud, &mut feed),
            // Handled by `device_reader`, which has its own loop: there
            // are no bytes and no lines, so none of this applies.
            Endpoint::Device => Outcome::Failed("not a sentence feed".to_owned()),
        };

        match outcome {
            Outcome::AppGone => break,
            Outcome::Failed(why) => {
                log::warn!(
                    "NMEA: {} {}: {why}",
                    endpoint.protocol_name(),
                    endpoint.address()
                );
                if !feed.set_link(Link::Down(why)) {
                    break;
                }
            }
        }

        thread::sleep(RETRY_DELAY);
    }

    log::debug!("NMEA reader finished");
}

/// Why a read loop ended. There is no success case: a feed is read until
/// something goes wrong with it or until nobody wants it any more.
enum Outcome {
    /// The app dropped the receiver. Stop entirely.
    AppGone,
    /// The socket failed, with a reason worth showing. Retry.
    Failed(String),
}

/// Resolve `host:port`, so a connect can have a timeout of our choosing.
fn resolve(address: &str) -> Result<SocketAddr, String> {
    address
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "no address found".to_owned())
}

fn read_tcp(address: &str, feed: &mut Feed) -> Outcome {
    let peer = match resolve(address) {
        Ok(peer) => peer,
        Err(why) => return Outcome::Failed(why),
    };

    let socket = match TcpStream::connect_timeout(&peer, CONNECT_TIMEOUT) {
        Ok(socket) => socket,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    if let Err(e) = socket.set_read_timeout(Some(READ_TIMEOUT)) {
        return Outcome::Failed(e.to_string());
    }
    // Sentences are tiny and arrive in bursts; waiting to fill a segment
    // would add latency to a position for no gain in a feed this small.
    let _ = socket.set_nodelay(true);

    if !feed.set_link(Link::Open(Opened::Socket(peer))) {
        return Outcome::AppGone;
    }
    log::info!("NMEA: connected to {peer}");

    let mut socket = socket;
    let mut buffer = [0u8; 4096];
    let mut silent_for = Duration::ZERO;

    loop {
        match socket.read(&mut buffer) {
            Ok(0) => return Outcome::Failed("the far end closed the connection".to_owned()),
            Ok(n) => {
                silent_for = Duration::ZERO;
                if !feed.bytes(&buffer[..n]) {
                    return Outcome::AppGone;
                }
            }
            Err(e) if timed_out(&e) => {
                // Nothing to read, which is not an error. But a socket
                // that stays open and silent is the one failure TCP will
                // not report on its own, so it is given a deadline.
                silent_for += READ_TIMEOUT;
                if silent_for >= SILENCE_LIMIT {
                    return Outcome::Failed(format!("silent for {} s", SILENCE_LIMIT.as_secs()));
                }
                // A publish doubles as a check that anyone is still
                // listening, so a quiet feed still notices the app going.
                if !feed.publish() {
                    return Outcome::AppGone;
                }
            }
            Err(e) => return Outcome::Failed(e.to_string()),
        }
    }
}

fn read_udp(address: &str, feed: &mut Feed) -> Outcome {
    let socket = match UdpSocket::bind(address) {
        Ok(socket) => socket,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    if let Err(e) = socket.set_read_timeout(Some(READ_TIMEOUT)) {
        return Outcome::Failed(e.to_string());
    }
    // Broadcast feeds are the usual UDP case, and some devices send to the
    // subnet address rather than to anyone in particular.
    let _ = socket.set_broadcast(true);

    let local = match socket.local_addr() {
        Ok(local) => local,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    if !feed.set_link(Link::Open(Opened::Socket(local))) {
        return Outcome::AppGone;
    }
    log::info!("NMEA: listening on {local}");

    let mut buffer = [0u8; 4096];

    loop {
        match socket.recv_from(&mut buffer) {
            Ok((n, _from)) => {
                // Each datagram is a whole number of sentences in practice,
                // but it is fed through the same line splitter as TCP so
                // that a sender which splits one across two packets still
                // works.
                if !feed.bytes(&buffer[..n]) {
                    return Outcome::AppGone;
                }
                // A datagram with no newline at the end would otherwise sit
                // in the partial buffer until the next one arrived.
                if buffer[n.saturating_sub(1)] != b'\n' && !feed.bytes(b"\n") {
                    return Outcome::AppGone;
                }
            }
            // No deadline here, unlike TCP: there is nothing to reconnect
            // to and nothing that could have gone stale. A bound socket
            // with no traffic is indistinguishable from a boat with its
            // instruments off, and rebinding would not help either.
            Err(e) if timed_out(&e) => {
                if !feed.publish() {
                    return Outcome::AppGone;
                }
            }
            Err(e) => return Outcome::Failed(e.to_string()),
        }
    }
}

/// Read NMEA off a serial port.
///
/// The bytes on the wire are the same sentences a socket carries, so
/// everything below [`Feed::bytes`] is shared with TCP and UDP. What is
/// different is the opening, and that lives in [`crate::serial`] — the one
/// place in this crate with `unsafe` in it.
fn read_serial(path: &str, baud: u32, feed: &mut Feed) -> Outcome {
    let port = match crate::serial::open(path, baud) {
        Ok(port) => port,
        Err(e) => return Outcome::Failed(e.to_string()),
    };

    if !feed.set_link(Link::Open(Opened::Port {
        path: path.to_owned(),
        baud,
    })) {
        return Outcome::AppGone;
    }
    log::info!("NMEA: reading {path} at {baud} baud");

    let mut port = port;
    let mut buffer = [0u8; 4096];
    let mut silent_for = Duration::ZERO;

    loop {
        match port.read(&mut buffer) {
            // Zero means the read window expired with nothing in it, not
            // end of file — the one place a serial port and a socket read
            // differ. Treating it as a close would reopen the port every
            // second for a GPS that is merely between sentences.
            Ok(0) => {
                silent_for += SERIAL_READ_WINDOW;
                if silent_for >= SILENCE_LIMIT {
                    return Outcome::Failed(format!(
                        "nothing on {path} for {} s — wrong device, wrong rate, or a cable",
                        SILENCE_LIMIT.as_secs()
                    ));
                }
                if !feed.publish() {
                    return Outcome::AppGone;
                }
            }
            Ok(n) => {
                silent_for = Duration::ZERO;
                if !feed.bytes(&buffer[..n]) {
                    return Outcome::AppGone;
                }
            }
            Err(e) if timed_out(&e) => {
                if !feed.publish() {
                    return Outcome::AppGone;
                }
            }
            // An unplugged dongle lands here, and the retry loop reopens
            // the path when it comes back — so unplugging and replugging
            // recovers on its own, with no restart.
            Err(e) => return Outcome::Failed(e.to_string()),
        }
    }
}

/// Whether a read error is just the timeout expiring.
///
/// Both kinds have to be checked: a timed-out read reports `WouldBlock` on
/// Unix and `TimedOut` on Windows, and this crate is meant to compile for
/// both.
fn timed_out(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Seconds since the Unix epoch, now.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Instant;

    const RMC: &str = "$GPRMC,123519,A,3240.800,N,11713.800,W,005.2,184.4,230394,013.0,E*6A";

    /// Wait for a condition, polling the receiver.
    ///
    /// A fixed sleep would be either flaky or slow; this is neither, and it
    /// fails with the receiver's own status rather than a bare timeout.
    fn wait_for(nmea: &mut Receiver, what: &str, mut done: impl FnMut(&Receiver) -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            nmea.poll();
            if done(nmea) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {what}; link is {}", nmea.status());
    }

    fn opened(nmea: &mut Receiver) -> SocketAddr {
        wait_for(nmea, "the socket to open", |n| n.report().link.is_open());
        match nmea.report().link.socket_addr() {
            Some(at) => at,
            None => panic!("not a socket: {:?}", nmea.report().link),
        }
    }

    #[test]
    fn specs_parse_the_way_they_are_written() {
        assert_eq!(
            Endpoint::parse("tcp://192.168.1.50:10110"),
            Ok(Endpoint::Tcp("192.168.1.50:10110".to_owned()))
        );
        assert_eq!(
            Endpoint::parse("udp://0.0.0.0:10110"),
            Ok(Endpoint::Udp("0.0.0.0:10110".to_owned()))
        );
        // No scheme means TCP, which is what a phone relay offers.
        assert_eq!(
            Endpoint::parse("192.168.1.50:2000"),
            Ok(Endpoint::Tcp("192.168.1.50:2000".to_owned()))
        );
        // No port means the conventional one.
        assert_eq!(
            Endpoint::parse("192.168.1.50"),
            Ok(Endpoint::Tcp(format!("192.168.1.50:{DEFAULT_PORT}")))
        );
        // No host means every interface, which only makes sense for UDP but
        // is accepted either way rather than guessed at.
        assert_eq!(
            Endpoint::parse("udp://:10110"),
            Ok(Endpoint::Udp("0.0.0.0:10110".to_owned()))
        );
        assert_eq!(
            Endpoint::parse("  tcp://localhost:10110  "),
            Ok(Endpoint::Tcp("localhost:10110".to_owned())),
            "surrounding space is a typo, not an address"
        );
    }

    #[test]
    fn a_device_path_is_a_serial_port_without_being_told() {
        // What someone already has written down, typed straight in. A host
        // and port never begin with a slash, so there is nothing to guess.
        assert_eq!(
            Endpoint::parse("/dev/ttyUSB0"),
            Ok(Endpoint::Serial {
                path: "/dev/ttyUSB0".to_owned(),
                baud: DEFAULT_BAUD
            })
        );
        assert_eq!(
            Endpoint::parse("/dev/ttyACM0@38400"),
            Ok(Endpoint::Serial {
                path: "/dev/ttyACM0".to_owned(),
                baud: 38400
            })
        );
        assert_eq!(
            Endpoint::parse("serial:///dev/ttyS0@9600"),
            Ok(Endpoint::Serial {
                path: "/dev/ttyS0".to_owned(),
                baud: 9600
            })
        );
        // The standard NMEA rate, so a bare path needs no number.
        assert_eq!(
            Endpoint::parse("/dev/ttyUSB0").map(|e| match e {
                Endpoint::Serial { baud, .. } => baud,
                _ => 0,
            }),
            Ok(4800)
        );
    }

    #[test]
    fn a_mistyped_baud_rate_is_refused_before_the_port_is_opened() {
        // A rate with no termios constant would otherwise be caught at
        // open, after the device had been held — and the message would be
        // about an ioctl rather than about the number that was wrong.
        let error = Endpoint::parse("/dev/ttyUSB0@4801")
            .err()
            .unwrap_or_default();
        assert!(error.contains("4801 baud"), "unexpected: {error:?}");
        assert!(error.contains("4800"), "the alternatives: {error:?}");

        assert!(Endpoint::parse("/dev/ttyUSB0@slow").is_err());
        assert!(Endpoint::parse("serial://").is_err(), "no device named");
    }

    #[test]
    fn nonsense_specs_are_refused_with_a_reason() {
        for spec in ["", "   ", "http://boat:80", "ws://x:1"] {
            let got = Endpoint::parse(spec);
            assert!(got.is_err(), "{spec:?} should not parse, got {got:?}");
        }
    }

    #[test]
    fn a_udp_feed_moves_the_boat() {
        // Port 0 so the test never collides with a real feed or with
        // another test; the bound port comes back in the report, which is
        // why `Link::Open` carries an address at all.
        let mut nmea =
            Receiver::spawn(Endpoint::parse("udp://127.0.0.1:0").expect("spec")).expect("thread");
        let to = opened(&mut nmea);

        let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");
        sender
            .send_to(format!("{RMC}\r\n").as_bytes(), to)
            .expect("send");

        wait_for(&mut nmea, "a fix", |n| n.vessel().position.is_some());

        let at = nmea.vessel().position.as_ref().expect("a fix").value;
        assert!((at.lat - 32.680_0).abs() < 1e-4, "lat was {}", at.lat);
        assert!((at.lon + 117.230_0).abs() < 1e-4, "lon was {}", at.lon);
        assert_eq!(nmea.report().accepted, 1);
        assert_eq!(nmea.report().rejected, 0);
    }

    #[test]
    fn several_sentences_in_one_datagram_all_arrive() {
        // A multiplexer sends a whole cycle in one packet, and some send it
        // without a trailing newline.
        let mut nmea =
            Receiver::spawn(Endpoint::parse("udp://127.0.0.1:0").expect("spec")).expect("thread");
        let to = opened(&mut nmea);

        let burst = format!(
            "{RMC}\r\n\
             $SDDBT,32.8,f,10.0,M,5.5,F\r\n\
             $WIMWV,045.0,R,12.0,N,A"
        );
        let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");
        sender.send_to(burst.as_bytes(), to).expect("send");

        wait_for(&mut nmea, "all three sentences", |n| {
            n.report().accepted >= 3
        });

        let vessel = nmea.vessel();
        assert!(vessel.position.is_some(), "no position");
        assert!(vessel.depth_m.is_some(), "no depth");
        assert!(vessel.apparent_wind.is_some(), "no wind");
    }

    #[test]
    fn a_tcp_feed_moves_the_boat() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("addr").port();

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            // Split across two writes, deliberately: TCP has no idea where
            // a sentence ends and a reader that assumes one read is one
            // line works on a loopback and fails on a boat.
            let line = format!("{RMC}\r\n");
            let (head, tail) = line.split_at(20);
            socket.write_all(head.as_bytes()).expect("head");
            socket.flush().expect("flush");
            thread::sleep(Duration::from_millis(30));
            socket.write_all(tail.as_bytes()).expect("tail");
            socket.flush().expect("flush");
            // Held open: closing would end the read loop, and this test is
            // about the data rather than the teardown.
            thread::sleep(Duration::from_millis(500));
        });

        let mut nmea =
            Receiver::spawn(Endpoint::parse(&format!("tcp://127.0.0.1:{port}")).expect("spec"))
                .expect("thread");
        wait_for(&mut nmea, "a fix", |n| n.vessel().position.is_some());
        assert_eq!(nmea.report().accepted, 1);

        server.join().expect("server");
    }

    #[test]
    fn a_refused_connection_is_reported_rather_than_retried_silently() {
        // Bound and then dropped, so the port is almost certainly closed.
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").expect("probe");
            probe.local_addr().expect("addr").port()
        };

        let mut nmea =
            Receiver::spawn(Endpoint::parse(&format!("tcp://127.0.0.1:{port}")).expect("spec"))
                .expect("thread");

        wait_for(&mut nmea, "a failure", |n| n.troubled());
        assert!(
            nmea.status().contains(&port.to_string()),
            "the status should name the address: {}",
            nmea.status()
        );
    }

    #[test]
    fn rubbish_does_not_stop_the_feed() {
        // The failure this guards: a binary gateway or the wrong baud rate
        // produces megabytes with no newline. The reader has to throw it
        // away and still find the sentence afterwards.
        let mut nmea =
            Receiver::spawn(Endpoint::parse("udp://127.0.0.1:0").expect("spec")).expect("thread");
        let to = opened(&mut nmea);
        let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");

        let mut noise = vec![b'\xff'; MAX_LINE * 2];
        noise.push(b'\n');
        sender.send_to(&noise, to).expect("noise");
        sender
            .send_to(format!("{RMC}\r\n").as_bytes(), to)
            .expect("sentence");

        wait_for(&mut nmea, "a fix after the noise", |n| {
            n.vessel().position.is_some()
        });
        assert!(nmea.report().rejected >= 1, "the noise was not counted");
    }

    #[test]
    fn a_bad_checksum_is_counted_and_not_believed() {
        let mut nmea =
            Receiver::spawn(Endpoint::parse("udp://127.0.0.1:0").expect("spec")).expect("thread");
        let to = opened(&mut nmea);
        let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");

        // The same sentence with one digit of latitude altered and the old
        // checksum left behind — the exact shape of a corrupted serial byte.
        let corrupt = RMC.replace("3240.800", "9240.800");
        sender
            .send_to(format!("{corrupt}\r\n").as_bytes(), to)
            .expect("send");

        wait_for(&mut nmea, "the rejection", |n| n.report().rejected >= 1);
        assert!(
            nmea.vessel().position.is_none(),
            "a corrupt sentence moved the boat"
        );
        assert_eq!(nmea.report().accepted, 0);
    }

    #[test]
    fn a_feed_of_pure_rubbish_still_reports_its_counts() {
        // The failure this guards: reports were published only when the
        // boat changed, so a feed where nothing parses published nothing
        // and the panel showed "0 refused" while refusing every line. "No
        // data" and "data, all bad" are different problems with different
        // fixes, and the counts are the only thing that tells them apart.
        let mut nmea =
            Receiver::spawn(Endpoint::parse("udp://127.0.0.1:0").expect("spec")).expect("thread");
        let to = opened(&mut nmea);
        let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");

        // Well-formed sentences with the wrong checksum, sent steadily, so
        // the read timeout never fires and publishing has to be driven by
        // the data rather than by the silence between it.
        for _ in 0..4 {
            sender
                .send_to(
                    b"$GPRMC,123519,A,3240.800,N,11713.800,W,,,,,*00\r\n".as_ref(),
                    to,
                )
                .expect("send");
            thread::sleep(Duration::from_millis(10));
        }

        wait_for(&mut nmea, "the refusals to be reported", |n| {
            n.report().rejected >= 4
        });
        assert!(nmea.vessel().position.is_none());
    }

    #[test]
    fn unsupported_sentences_are_separated_from_rubbish() {
        // A feed of nothing but satellite status and AIS is working
        // perfectly and giving no position, which is a different problem
        // from a feed of noise. The counts have to tell them apart.
        let mut nmea =
            Receiver::spawn(Endpoint::parse("udp://127.0.0.1:0").expect("spec")).expect("thread");
        let to = opened(&mut nmea);
        let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");

        sender
            .send_to(
                b"$GPGSV,3,1,11,03,03,111,00,04,15,270,00*7F\r\n\
                  !AIVDM,1,1,,B,177KQJ5000G?tO`K>RA1wUbN0TKH,0*5C\r\n"
                    .as_ref(),
                to,
            )
            .expect("send");

        wait_for(&mut nmea, "both to be classified", |n| {
            n.report().unsupported >= 2
        });
        assert_eq!(nmea.report().rejected, 0, "these are not rubbish");
        assert_eq!(nmea.report().accepted, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_serial_gps_moves_the_boat() {
        // The dongle path, end to end, against a pseudo-terminal: open,
        // configure, read, cut into lines, parse, apply. The only thing
        // this does not exercise that a real dongle would is the baud
        // rate, which a pty ignores.
        use std::io::Write;

        let Some((mut gps, path)) = crate::serial::pty::pair() else {
            eprintln!("no pty available; skipping");
            return;
        };

        let mut nmea = Receiver::spawn(Endpoint::parse(&path).expect("spec")).expect("thread");
        wait_for(&mut nmea, "the port to open", |n| n.report().link.is_open());

        // Written in two pieces across the sentence boundary, because a
        // UART delivers whatever has arrived rather than whole lines — and
        // a reader that assumes one read is one sentence works against a
        // test harness and fails against hardware.
        let feed = format!("{RMC}\r\n$SDDBT,32.8,f,10.0,M,5.5,F\r\n");
        let (head, tail) = feed.split_at(30);
        gps.write_all(head.as_bytes()).expect("head");
        gps.flush().expect("flush");
        thread::sleep(Duration::from_millis(30));
        gps.write_all(tail.as_bytes()).expect("tail");
        gps.flush().expect("flush");

        wait_for(&mut nmea, "a fix", |n| n.vessel().position.is_some());

        let at = nmea.vessel().position.expect("a fix").value;
        assert!((at.lat - 32.680_0).abs() < 1e-4, "lat was {}", at.lat);
        assert!((at.lon + 117.230_0).abs() < 1e-4, "lon was {}", at.lon);

        wait_for(&mut nmea, "the depth", |n| n.vessel().depth_m.is_some());
        assert!(nmea.report().rejected == 0, "a clean feed had refusals");

        // And the panel's line names the port and the rate, which is what
        // you check first when a port is open and nothing parses.
        let status = nmea.status();
        assert!(status.contains(&path), "status reads {status:?}");
        assert!(status.contains("4800"), "status reads {status:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_serial_port_that_goes_quiet_is_not_mistaken_for_a_closed_one() {
        // A serial read returns zero bytes when its window expires, which
        // on a socket means the far end hung up. Treating the two the same
        // would reopen the port every second for a GPS that is merely
        // between sentences — and reopening flushes, so it would lose data
        // forever while looking like a flaky dongle.
        let Some((_gps, path)) = crate::serial::pty::pair() else {
            eprintln!("no pty available; skipping");
            return;
        };

        let mut nmea = Receiver::spawn(Endpoint::parse(&path).expect("spec")).expect("thread");
        wait_for(&mut nmea, "the port to open", |n| n.report().link.is_open());

        // Well past one read window with nothing written, and well short
        // of the silence deadline.
        thread::sleep(SERIAL_READ_WINDOW * 3);
        nmea.poll();
        assert!(
            nmea.report().link.is_open(),
            "a quiet port was treated as closed: {:?}",
            nmea.report().link
        );
        assert!(!nmea.troubled(), "a quiet port reported trouble");
    }

    #[test]
    fn dropping_the_receiver_closes_the_socket_and_ends_the_thread() {
        // Otherwise every receiver ever opened leaks a thread and a socket,
        // which on a phone is eventually fatal — and a reader still holding
        // a boat's TCP slot stops the app reconnecting to it.
        //
        // Observable over TCP: the thread exiting drops the stream, which
        // the far end sees as end-of-file. It takes until the reader's next
        // publish to notice, which on a silent feed is one read timeout.
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("addr").port();

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            socket
                .set_read_timeout(Some(READ_TIMEOUT * 3))
                .expect("timeout");
            let mut sink = [0u8; 64];
            socket.read(&mut sink)
        });

        let mut nmea =
            Receiver::spawn(Endpoint::parse(&format!("tcp://127.0.0.1:{port}")).expect("spec"))
                .expect("thread");
        opened(&mut nmea);
        drop(nmea);

        let read = server.join().expect("server").expect("read");
        assert_eq!(read, 0, "the reader did not close its socket");
    }

    #[test]
    fn polling_an_idle_feed_is_cheap_and_says_nothing_changed() {
        let mut nmea =
            Receiver::spawn(Endpoint::parse("udp://127.0.0.1:0").expect("spec")).expect("thread");
        opened(&mut nmea);
        // Drain whatever the open produced.
        while nmea.poll() {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!nmea.poll(), "an idle feed reported a change");
    }
}
