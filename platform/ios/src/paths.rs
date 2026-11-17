//! Where the app's files are.
//!
//! Two directories, and the distinction matters:
//!
//! * The **bundle** holds what shipped — the chart, the tide
//!   constituents, the forecast. Read-only, and already real files on
//!   disk, which is the one place iOS is easier than Android: an APK is a
//!   zip and every asset has to be copied out before SQLite can seek
//!   within it. Here they are simply opened where they lie.
//! * **Documents** is the app's own writable directory, where settings,
//!   the route, the track and any downloaded imagery go. Backed up, and
//!   visible in the Files app if `UIFileSharingEnabled` is set — which it
//!   is, so a chart downloaded on the boat can be copied off it.

use std::path::PathBuf;

/// The read-only resources that shipped with the app.
///
/// `None` would mean the bundle has no resource path, which does not
/// happen for a real app — but it is an `Option` in the API, so it is an
/// `Option` here rather than an unwrap waiting for a Mac to find.
pub fn bundle() -> Option<PathBuf> {
    let bundle = objc2_foundation::NSBundle::mainBundle();
    let path = bundle.resourcePath()?;
    Some(PathBuf::from(path.to_string()))
}

/// The app's writable directory.
///
/// `$HOME/Documents`: inside the sandbox `HOME` is the app's container,
/// so this is both correct and one call rather than four into
/// `NSSearchPathForDirectoriesInDomains`.
pub fn documents() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let documents = PathBuf::from(home).join("Documents");
    // Created rather than assumed: it exists in a real app container,
    // and not creating it would turn a missing directory into a
    // confusing write failure later.
    if let Err(e) = std::fs::create_dir_all(&documents) {
        log::warn!("could not make {}: {e}", documents.display());
        return None;
    }
    Some(documents)
}

/// One shipped data file, if it is in the bundle.
pub fn resource(name: &str) -> Option<PathBuf> {
    let path = bundle()?.join(name);
    path.exists().then_some(path)
}
