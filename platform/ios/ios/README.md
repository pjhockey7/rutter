# Building rutter for iOS

**None of this has been run.** The crate is written and type-checked
against `aarch64-apple-ios`; it has never been compiled against the iOS
SDK, linked, signed, or launched. Treat the first build as a debugging
session.

Everything here needs macOS with Xcode. There is no way around that: the
SDK, the linker, the code-signing and the simulator are all Apple's, and
none of them run on Linux.

## What is already done

* `platform/ios/` — the shim: entry point, CoreLocation, AVAudioSession,
  safe-area insets, bundle paths, the time zone, and the background
  location session that stands in for Android's foreground service.
* `ios/Info.plist` — every key the app needs, each one commented with
  what it is for.
* `ios/main.m` — the C `main` that calls into Rust.
* The workspace's Rust toolchain already carries `aarch64-apple-ios` and
  `aarch64-apple-ios-sim`, so `cargo` needs no setup on the Mac beyond
  installing the same toolchain.

## What is not done

* **No Xcode project.** Generating one needs Xcode. The steps below build
  the bundle by hand, which is fewer moving parts and matches how the
  Android APK is built here — no Gradle there either.
* **No icon or launch image.** The app will run with the default white
  icon.
* **Nothing is signed.** A device needs a signing identity; the simulator
  does not.

## Steps

### 1. The static library

```sh
# A device:
cargo build --release -p rutter-ios --target aarch64-apple-ios
# Or the simulator on Apple silicon:
cargo build --release -p rutter-ios --target aarch64-apple-ios-sim
```

The output is `target/<triple>/release/librutter_ios.a`.

### 2. The executable

```sh
SDK=$(xcrun --sdk iphoneos --show-sdk-path)
xcrun clang -arch arm64 -isysroot "$SDK" -mios-version-min=14.0 \
    platform/ios/ios/main.m \
    target/aarch64-apple-ios/release/librutter_ios.a \
    -framework UIKit -framework Foundation -framework CoreLocation \
    -framework AVFoundation -framework Metal -framework QuartzCore \
    -framework Security -framework CoreGraphics \
    -o rutter
```

The framework list is a prediction, not a tested fact. If the linker
complains about an undefined symbol, the missing framework is almost
always named in the symbol: `_OBJC_CLASS_$_AVAudioSession` means
AVFoundation, `_CGRectZero` means CoreGraphics, and so on.

### 3. The bundle

```sh
mkdir -p Payload/rutter.app
cp rutter Payload/rutter.app/
cp platform/ios/ios/Info.plist Payload/rutter.app/
# The data files, which on iOS are read in place — no extraction step,
# unlike the APK.
cp build/charts/noaa.mbtiles Payload/rutter.app/
cp build/charts/tides.sqlite Payload/rutter.app/
cp build/weather.bin Payload/rutter.app/
```

### 4. Run it

On the simulator:

```sh
xcrun simctl boot "iPhone 15"
xcrun simctl install booted Payload/rutter.app
xcrun simctl launch --console booted com.pjhockey7.rutter
```

`--console` puts the app's log on the terminal, which is the iOS
equivalent of `adb logcat` and the thing to watch on the first run.

On a device the bundle has to be signed first, which needs a development
certificate and a provisioning profile — at which point an Xcode project
is less work than doing it by hand.

## How this was verified without a Mac

Worth knowing, because it sets how much to trust what is here.

The workspace toolchain carries `aarch64-apple-ios`, and `cargo check`
does not link — so Rust *can* type-check iOS code on Linux. What it
cannot do is check `rutter-app`, because that reaches `libsqlite3-sys`,
whose build script compiles SQLite's C source and so needs an iOS C
toolchain.

The way round it is a throwaway workspace holding two stub crates named
`rutter-core` and `rutter-app` — just the types the shim touches: `Fix`,
`FixSource`, `Zone`, `Siren`, `Keepalive` — plus one crate that pulls in
the real modules by path:

```rust
#[path = "../../../platform/ios/src/location.rs"]
pub mod location;
```

`cargo check --target aarch64-apple-ios` against that compiles the real
files with real `objc2` bindings. It is how the errors in this shim were
actually found: `NSArray::last` not existing (it is `lastObject`),
`locationServicesEnabled` being an instance method and deprecated,
`AnyClass` having no typed `alloc`, `safeAreaInsets` needing
`firstObject`. None of those would have been caught by reading.

It is not in the repository — it is a few minutes to recreate and a
confusing thing to leave lying about, since the stub crates share names
with the real ones. If the shim is edited before anyone has a Mac, it is
worth recreating.

**What it does not catch:** selectors, linking, signing, the bundle
layout, and every runtime assumption.

## What to expect to go wrong first

In rough order of likelihood:

1. **Linker errors** — a missing framework. See above.
2. **`src/location.rs`** — the most ways to be quietly wrong.
   `CLLocationManager` delivers its callbacks to the run loop of the
   thread it was created on, so if `open()` ever runs off the main
   thread it will simply never call back. No error, no fix, no clue.
3. **Selector typos.** `msg_send!` resolves names at runtime, so
   `setCurrentTime:` being wrong is an "unrecognised selector" crash,
   not a compile error. The crash names the selector, which makes it a
   two-minute fix once seen.
4. **The audio session.** `AVAudioSessionCategoryPlayback` is read as a
   framework symbol rather than written as a literal; if that symbol will
   not link, the fallback is `NSString::from_str("AVAudioSessionCategoryPlayback")`
   — the constant's value does equal its own name.
