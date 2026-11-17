// The app's entry point.
//
// Apple's, not ours: an iOS binary must have a C `main`, and winit's iOS
// backend calls `UIApplicationMain` from inside `run_app`. So this hands
// straight over to Rust and never comes back.
//
// Objective-C rather than C only because the toolchain expects a `.m`
// here and because a future version of this file may need UIKit for
// something; today it needs nothing.

extern void rutter_main(void);

int main(int argc, char *argv[]) {
    (void)argc;
    (void)argv;
    rutter_main();
    // Not reached: `rutter_main` runs the event loop, which owns the
    // thread for the life of the app.
    return 0;
}
