//! Asking Android not to kill us while a watch stands.

use rutter_app::keepalive::Keepalive;

use crate::watch;

/// Shown on the lock screen, so the system has to be asked for it from
/// API 33 onwards. Denied, the service still runs and still holds the
/// process open — only the notification is hidden.
const NOTIFY: &str = "android.permission.POST_NOTIFICATIONS";

/// The foreground service.
#[derive(Debug)]
pub struct Service;

impl Keepalive for Service {
    fn hold(&self, reason: &str) {
        // Asked for here rather than at startup, so the dialogue appears
        // at the moment someone arms a watch — which is when the reason
        // for it is obvious — rather than among the launch questions,
        // where it is one more thing to dismiss. Not waited on: a denial
        // costs the notification, not the watch.
        watch::want_notifications(NOTIFY);

        // Called again with a new reason when what is being watched
        // changes, which is the same `startForegroundService` call with
        // different text — Android updates the notification rather than
        // starting a second service.
        watch::begin(reason);
    }

    fn release(&self) {
        watch::end();
    }

    fn describe(&self) -> String {
        "foreground service".to_owned()
    }
}
