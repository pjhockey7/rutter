//! Fetching tiles over HTTPS.
//!
//! One crate, one dependency, one job. It exists to keep a TLS stack out of
//! everywhere else: `rutter-core` has no dependencies at all and is the
//! layer the navigation correctness lives in, and `rutter-app` should not
//! grow rustls to satisfy one optional feature.
//!
//! What goes through here is `rutter_app::TileFetcher`, which is a trait
//! precisely so that this is not the only possible answer. A platform that
//! would rather use the operating system's own HTTP stack —
//! `HttpsURLConnection` on Android, `NSURLSession` on iOS — implements the
//! trait itself and never links this crate. There are real reasons to want
//! that eventually: the system stack uses the device's own certificate
//! store rather than a root list baked in at build time, and it honours
//! proxies and captive portals, which is exactly the environment a boat in
//! a marina is in.
//!
//! This one is first because it works on every target from one
//! implementation, and because the Android alternative means more JNI.

use std::time::Duration;

use rutter_app::{FetchOutcome, TileFetcher};

/// How long to wait on one tile before giving up on it.
///
/// Generous: a marina's WiFi is slow rather than absent, and a tile that
/// takes eight seconds is still better than a hole in the imagery. The
/// download as a whole is cancellable, so a slow link costs patience
/// rather than a wedged app.
const TIMEOUT: Duration = Duration::from_secs(20);

/// What to tell the server we are.
///
/// A real identifier rather than a browser's, because a public-domain tile
/// service that starts getting hammered deserves to know who by.
const AGENT: &str = concat!(
    "rutter/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/pjhockey7/rutter)"
);

/// An HTTPS tile fetcher.
///
/// Holds one `ureq` agent, which pools connections — and that pooling is
/// most of why a download of five thousand tiles is minutes rather than an
/// hour. Shared across the fetching threads rather than one agent each,
/// for the same reason.
#[derive(Debug)]
pub struct Https {
    agent: ureq::Agent,
}

impl Https {
    /// Build a fetcher, or say why not.
    ///
    /// Can fail: installing a crypto provider is a process-wide action and
    /// something else may have installed a different one already.
    pub fn new() -> Result<Self, String> {
        // rustls needs a provider chosen before any TLS happens. Done here
        // rather than left to a lazy default so that the choice is this
        // crate's and visible, and so a second call cannot race it.
        //
        // An error means a provider was already installed, which is not a
        // problem — it only means someone got there first.
        if rustls::crypto::ring::default_provider()
            .install_default()
            .is_err()
        {
            log::debug!("a rustls crypto provider was already installed");
        }

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .user_agent(AGENT)
            .build()
            .into();

        Ok(Self { agent })
    }
}

impl TileFetcher for Https {
    fn describe(&self) -> String {
        "HTTPS".to_owned()
    }

    fn fetch(&self, url: &str) -> FetchOutcome {
        match self.agent.get(url).call() {
            Ok(mut response) => {
                // Capped, so a misconfigured URL that returns a web page —
                // or a redirect to one — cannot fill memory. A quarter of a
                // megabyte is ten times the largest real tile measured.
                match response
                    .body_mut()
                    .with_config()
                    .limit(256 * 1024)
                    .read_to_vec()
                {
                    Ok(bytes) if bytes.is_empty() => FetchOutcome::Missing,
                    Ok(bytes) => FetchOutcome::Got(bytes),
                    Err(e) => FetchOutcome::Failed(e.to_string()),
                }
            }
            // A tile the service does not have. Entirely ordinary at the
            // edge of coverage and over open water, so it must not fail
            // the download — it is counted and passed over.
            Err(ureq::Error::StatusCode(404 | 204)) => FetchOutcome::Missing,
            Err(e) => FetchOutcome::Failed(e.to_string()),
        }
    }
}
