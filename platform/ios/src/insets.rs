//! How much of the screen the system has already spoken for.
//!
//! The same problem the Android shim solves, for the same reason: an
//! iPhone draws under a notch or a Dynamic Island at the top and a home
//! indicator at the bottom, and a chart drawn to the glass puts the top
//! row of buttons under a camera. iOS calls the usable rectangle the
//! *safe area*, and `UIView` reports it.
//!
//! Asked for per frame rather than cached: the numbers change on
//! rotation, and on iPad they change when the app is resized into a
//! split view.

#![allow(unsafe_code)]

use objc2::MainThreadMarker;
use objc2_foundation::NSSet;
use objc2_ui_kit::{UIApplication, UIScene, UIWindowScene};

/// Screen points claimed by the system on each edge.
///
/// Points, not pixels — UIKit works in points throughout, and the caller
/// multiplies by the scale factor. That is the opposite of Android, where
/// the insets arrive in physical pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Insets {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl Insets {
    /// Whether anything is actually claimed.
    pub fn any(self) -> bool {
        self.left > 0.0 || self.top > 0.0 || self.right > 0.0 || self.bottom > 0.0
    }
}

/// Ask the system what it has taken.
///
/// `None` when there is no window to ask yet, which is a normal state
/// during launch rather than a fault — the caller tries again.
///
/// Must be called on the main thread, which `MainThreadMarker` makes the
/// compiler insist on rather than leaving to a comment. UIKit is not
/// thread-safe and reading a view's geometry from a worker is undefined
/// behaviour, not merely wrong.
pub fn current(mtm: MainThreadMarker) -> Option<Insets> {
    let app = UIApplication::sharedApplication(mtm);
    let scenes: objc2::rc::Retained<NSSet<UIScene>> = app.connectedScenes();

    // `keyWindow` would be one line and is deprecated: it returns nil
    // once an app has more than one scene, which on iPad is routine. So
    // the scenes are walked instead, and the first foreground one with a
    // window wins.
    for scene in &scenes {
        let Ok(window_scene) = scene.downcast::<UIWindowScene>() else {
            continue;
        };
        let windows = window_scene.windows();
        let Some(window) = windows.firstObject() else {
            continue;
        };
        let insets = window.safeAreaInsets();
        return Some(Insets {
            left: insets.left,
            top: insets.top,
            right: insets.right,
            bottom: insets.bottom,
        });
    }
    None
}
