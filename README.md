# rutter

Charts, weather and tides. Android first, Linux alongside it, the rest when
they earn it.

Named for the rutter — the book of written sailing directions a pilot kept
before printed charts: bearings, soundings, landmarks and tides.

## Why this exists

The obvious answer to "I want an open-source chartplotter on my phone" is
OpenCPN, and it does not work. OpenCPN is GPLv2 and the only Android build is
the paid Play Store one, so the reasonable move is to compile it yourself.
[`../../opencpn`](../../opencpn) is that attempt, carried to a definite
conclusion: a 34.5 MB arm64 APK that installs on a Pixel 10a, loads every
library, unpacks its assets and initialises Qt — and cannot become a usable
app, because `android/androidUTIL.cpp` makes 58 calls into its Java activity
and 37 of those methods exist nowhere in the repository.

| tag | native calls into Java | missing from the Java tree |
|---|---|---|
| v5.14_Beta4 | 58 | 37 |
| v5.11.3-beta3 | 58 | 36 |
| v5.9.4-beta4 | — | no Java files at all |
| v5.2.4 | 43 | 24 |

The Play Store build comes from a tree that was never published. That is a
fact about the upstream project, not a build problem to be solved, so this
repo stops trying.

What was actually wanted was three things — charts, wind and temperature,
tides — and none of them are hard in the way OpenCPN's Android build is hard.
They are hard in the ordinary way: a lot of careful code. So they get written
here, on a structure meant to be ported.

## Structure

Laid out like firmware, because the goal is the same: one body of application
code landing on several boards.

```
core/              charts, weather, tides — pure computation, no platform
render/            drawing, via wgpu
platform/linux/    board support: winit window, event loop, input
platform/android/  board support: NativeActivity entry point
```

Dependencies point down that list and never up. The moment `core` needs to
know what it is running on, the split has failed.

**`core` is the part that matters.** Projection, tile selection, harmonic tide
prediction, wind field interpolation, distances and bearings — all of it is
arithmetic over data, with no GPU, no window, no filesystem and no clock. It
is portable by construction and it is where the tests are. 48 of them, and
they run in under a hundredth of a second, which means they get run.

One of them earned its keep immediately. `tide::extremes` classified a
turning point from the slope *before* it, which is exactly zero when a peak
lands on a sample — as it does at t=0 for any constituent with zero phase —
so `+0.0 > 0.0` read as falling and labelled a high water a low. The test
that asserts highs and lows alternate caught it on the first run.

**`render` is wgpu**, and wgpu is the reason the whole thing is Rust. One
renderer over Vulkan (Android, Linux), Metal (iOS, macOS), D3D12 (Windows) and
GL ES as a floor. The alternative, and what OpenCPN does, is a desktop GL 2.1
path and a mobile GL ES 2 path that drift apart.

**The platform crates are deliberately thin** and are the only place a
platform name appears. The Android one exists mostly to handle the one
lifecycle difference that genuinely cannot be abstracted: Android takes the
drawing surface away when the app is backgrounded and hands back a new one on
return, so the renderer is built and dropped repeatedly while the view state
outlives it. That is why `View` lives in `core`.

### Targets

| target | status |
|---|---|
| Linux | **works.** `rutter run` opens a window and draws the chart grid; pan and zoom are live. |
| Android | **cross-compiles.** `rutter android` builds `librutter.so` for arm64-v8a and verifies the ELF arch and page size. No APK yet. |
| Windows | reachable from here via `pkgsCross.mingw32`; not wired up. |
| macOS | this same `default.nix` should work on a mac; untested. |
| iOS | **not buildable from NixOS, at all.** Apple requires Xcode on macOS to compile and sign. `core` and `render` will cross-compile to `aarch64-apple-ios` from a mac; the shell around them has to be built there. The code is ready; the build is not. |

## Getting started

```sh
nix-shell          # or: nix-shell --attr shells.default
rutter doctor      # the toolchain, and an honest list of what is missing
rutter test        # the core test suite — fast, and the real one
rutter run         # the Linux app
```

Everything is driven by the one command:

| | |
|---|---|
| `rutter doctor` | toolchain report and what is still missing — **start here** |
| `rutter test` | `cargo test` on the core. Fast; this is the real suite. |
| `rutter test-all` | the whole workspace. Slow — it builds wgpu. |
| `rutter run` | open the Linux app |
| `rutter android` | cross-compile the shared library for `$RUTTER_ABI` |
| `rutter charts <src> <out.mbtiles>` | turn chart data into tiles |
| `rutter fmt` / `rutter lint` | `cargo fmt` / `cargo clippy` |
| `rutter adb …` | pass through to the SDK's `adb` |
| `rutter clean` | remove `target/` and `build/` |

In the Linux app: drag or arrow keys to pan, scroll or `+`/`-` to zoom, `0` to
go home, `Escape` to quit.

### Inputs

Pinned by [nixtamal](https://code.toast.al/toastal/nixtamal) in `nix/tamal`,
flake-free, same as the other repos here. Two inputs: nixpkgs, and
`rust-overlay` — the latter because nixpkgs' rustc ships `std` for the build
host only, and this project cross-compiles to three Android ABIs.

```sh
nix-shell -p nixtamal --run 'nixtamal refresh'
```

## What works, and what it actually draws

`rutter run` renders the boundaries of the tiles the current view needs, plus
a mark at the centre. That is not a chart. It is the whole pipeline, end to
end: `core` computes which tiles the viewport covers, `render` puts the
geometry on a GPU surface, and panning and zooming are visibly right or
visibly wrong. Tile imagery goes on top of this without changing its shape.

One detail worth recording, because it is the kind of thing that is painful to
retrofit: vertices reach the GPU as an **offset from the view centre** in
normalised world units, not as absolute world coordinates. Absolute Web
Mercator coordinates at zoom 20 need more precision than `f32` has, and the
symptom is geometry that jitters as you pan. An offset from the middle of the
screen is always a small number, so `f32` is plenty.

## What is not done

Stated plainly, because the whole point of
[`../../opencpn`](../../opencpn) was discovering what someone had not stated.

- **No APK.** `rutter android` produces the shared library an APK would
  carry, and stops. Packaging needs either Gradle (painful under Nix — see
  opencpn's `GRADLE_OPTS` comment for why) or a direct
  `aapt2`/`d8`/`apksigner` path, which is the better answer and is not
  written. Nothing has run on a device.
- **No pinch-to-zoom.** winit hands Android raw touch points rather than a
  scale gesture, so it means tracking two ids and their separation. Left
  until there is a device to tune it on; guessing at gesture thresholds
  without one is how you get a chart that fights you.
- **No chart imagery.** `rutter charts` is the pipeline — GDAL reads ENC/S-57
  and most else, tippecanoe tiles it, the result is MBTiles — but nothing
  reads those tiles back yet, and no charts are vendored. They are large,
  often licensed, and never ours to redistribute.
- **No GRIB decode.** `core::weather` is the part that matters once the bytes
  are decoded: a lat/lon grid, bilinear sampling, and wind kept as U/V
  components. Parsing GRIB2 itself is a separate job.
- **No real tide data.** `core::tide` does the harmonic arithmetic. The
  amplitudes and phases are the asset, and they are per-station, measured
  over years. NOAA publishes them for US waters; other hydrographic offices
  vary.
- **No nodal corrections** in the tide model, which modulate amplitude and
  phase over the Moon's 18.6-year regression. Costs a few percent. Fine for a
  passage plan; not for clearing a bar at low water on a spring tide.
- **No GPS.** Both platform crates would need it, and on Android it needs a
  runtime permission, which needs an APK.

## Notes kept because they cost something to learn

- **Wind must be interpolated as components, never as a bearing.** Halfway
  between 350° and 010° is due north; the arithmetic mean of the two numbers
  is 180°, pointing exactly backwards. `core::weather` keeps U and V separate
  until the moment it is asked for a direction, and there is a test for the
  350°/010° case specifically.
- **Meteorological convention is "blows from".** A northerly has a *negative*
  V component. Getting this backwards is the classic GRIB bug, so all four
  cardinals are asserted.
- **API 24 is the floor, chosen rather than inherited.** 21 and 22 make
  `fseeko`/`ftello` unavailable under `_FILE_OFFSET_BITS=64`, and 23 and below
  hide `nl_langinfo` — each of which cost a day in opencpn. 24 is also where
  Vulkan became dependable, which wgpu cares about.
- **16 KB pages.** Android 15+ wants `-Wl,-z,max-page-size=16384`; it is set
  per-target in `default.nix` and `rutter android` reads it back out of the
  ELF rather than trusting that it was set.
- **wgpu's limits default to the desktop's.** `required_limits` is
  `downlevel_defaults()` — what GL ES 3.0 can promise — because asking for
  more is how a renderer works on the laptop and fails on the phone.
- **The NDK linker paths contain store hashes**, so a checked-in
  `.cargo/config.toml` would be either wrong or untrackable. They go into the
  shell's environment instead.
