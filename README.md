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
core/              geography, weather, tides — pure computation, no deps
charts/            MBTiles and vector tile decoding
render/            drawing, via wgpu
platform/linux/    board support: winit window, event loop, input
platform/android/  board support: NativeActivity entry point
```

Dependencies point down that list and never up. The moment `core` needs to
know what it is running on, the split has failed.

**`core` is the part that matters.** Projection, tile selection, harmonic tide
prediction, wind field interpolation, distances and bearings — all of it is
arithmetic over data, with no GPU, no window, no filesystem and no clock. It
is portable by construction. It and `charts` are where the tests are — 119 of
them, running in under a tenth of a second, which means they get run.

One of them earned its keep immediately. `tide::extremes` classified a
turning point from the slope *before* it, which is exactly zero when a peak
lands on a sample — as it does at t=0 for any constituent with zero phase —
so `+0.0 > 0.0` read as falling and labelled a high water a low. The test
that asserts highs and lows alternate caught it on the first run.

**`charts` reads the data.** MBTiles is a SQLite file holding gzipped Mapbox
Vector Tiles, and this crate goes all the way down: SQL, gunzip, protobuf,
geometry. The protobuf decoder is written by hand rather than generated — the
MVT schema is eight messages, frozen since 2016, and the decoder is shorter
than the generated code would be while keeping the module dependency-free and
unit-testable. It also classifies S-57 object classes into the handful of
styles the renderer draws differently.

**`render` is wgpu**, and wgpu is the reason the whole thing is Rust. One
renderer over Vulkan (Android, Linux), Metal (iOS, macOS), D3D12 (Windows) and
GL ES as a floor. The alternative, and what OpenCPN does, is a desktop GL 2.1
path and a mobile GL ES 2 path that drift apart. It depends on `core` and
nothing else of the project's: chart geometry arrives as plain slices of world
coordinates, so linking SQLite stays in `charts` where a renderer need not
care about it.

**The platform crates are deliberately thin** and are the only place a
platform name appears. The Android one exists mostly to handle the one
lifecycle difference that genuinely cannot be abstracted: Android takes the
drawing surface away when the app is backgrounded and hands back a new one on
return, so the renderer is built and dropped repeatedly while the view state
outlives it. That is why `View` lives in `core`.

### Targets

| target | status |
|---|---|
| Linux | **works.** `rutter run --charts <file.mbtiles>` opens a window and draws real chart linework; pan and zoom are live. |
| Android | **installable.** `rutter apk` builds a signed 6.8 MB APK with the chart bundled as an asset. Not yet run on real hardware. |
| Windows | reachable from here via `pkgsCross.mingw32`; not wired up. |
| macOS | this same `default.nix` should work on a mac; untested. |
| iOS | **not buildable from NixOS, at all.** Apple requires Xcode on macOS to compile and sign. `core` and `render` will cross-compile to `aarch64-apple-ios` from a mac; the shell around them has to be built there. The code is ready; the build is not. |

## Chart sources

**Any chart you have.** `rutter charts <source> <out.mbtiles>` takes anything
GDAL reads — an ENC `.000` cell, a Shapefile, a GeoPackage, GeoJSON — and
produces tiles the app loads. That is the front door, and it is the reason the
app itself only reads MBTiles: one conversion step, then a format that opens
instantly on a phone.

**NOAA by default.** `rutter noaa` fetches official ENCs for US waters, free
and without registration. Southern California, San Diego out to Catalina:

```sh
export RUTTER_NOAA_CATALOG=12Region_ENCProdCat_19115.xml   # smaller than national
rutter noaa --box 32.5,-118.7,33.6,-117.0
rutter run --charts build/charts/noaa.mbtiles
```

or by position, or by name:

```sh
rutter noaa --near 32.71,-117.17   # cells covering a position
rutter noaa US5CA72M               # San Diego Bay, 1:12,000
rutter noaa --list                 # everything in the catalogue
```

It downloads the cells, merges their layers into one source, and runs them
through the same pipeline as any other chart. Outside US waters most
hydrographic offices charge for ENCs; the free routes are OpenSeaMap, GSHHG
coastlines and GEBCO or EMODnet bathymetry, all of which `rutter charts`
converts.

**Not yet run against a real NOAA cell.** Downloads are blocked from the
environment this was developed in, so the fetch itself has never executed.
What *is* verified: the catalogue parser, against
`scripts/testdata/catalog-sample.xml` — a cut-down catalogue in NOAA's real
shape, carrying the actual San Diego and Catalina cells. The parser picks the
right cells for a position, the right set for the San Diego–Catalina box, and
correctly excludes a Boston cell.

That last check matters more than it sounds. NOAA records coverage as a
`gml:Polygon` of `gml:pos` values in **latitude-longitude** order, not as the
`EX_GeographicBoundingBox` the schema's name suggests and not in the
lon/lat order most geo tooling uses. The first version of this parser looked
for the wrong element and would have silently found zero cells; reading the
order backwards would have found cells in the Indian Ocean.

## Getting started

```sh
nix-shell          # or: nix-shell --attr shells.default
rutter doctor      # the toolchain, and an honest list of what is missing
rutter test        # the test suite — fast, and the real one

# Linux: draws the bundled test chart
rutter run --charts charts/tests/data/coast.mbtiles

# Android: build, install, launch, watch
rutter apk
rutter install
rutter run-android
rutter logcat
```

Getting a phone connected, if `rutter doctor` says no device: enable
Developer options, then Wireless debugging, then

```sh
rutter adb pair <host:port>      # the pairing dialog shows host:port + code
rutter adb connect <host:port>   # the main screen shows a different port
```

Everything is driven by the one command:

| | |
|---|---|
| `rutter doctor` | toolchain report and what is still missing — **start here** |
| `rutter test` | `cargo test` on the core. Fast; this is the real suite. |
| `rutter test-all` | the whole workspace. Slow — it builds wgpu. |
| `rutter run [--charts <f.mbtiles>]` | open the Linux app |
| `rutter android` | cross-compile the shared library for `$RUTTER_ABI` |
| `rutter apk` | build a signed, installable APK — no Gradle |
| `rutter install` | `adb install` it |
| `rutter run-android` | launch it on the device |
| `rutter logcat` | follow the device log, filtered to what matters |
| `rutter uninstall` | remove it from the device |
| `rutter charts <src> <out.mbtiles>` | turn chart data into tiles |
| `rutter fmt` / `rutter lint` | `cargo fmt` / `cargo clippy` |
| `rutter adb …` | pass through to the SDK's `adb` |
| `rutter clean` | remove `target/` and `build/` |

In the Linux app: drag or arrow keys to pan, scroll or `+`/`-` to zoom, `0` to
go home, `Escape` to quit.

### Nix builds, and why they sit beside the dev shell

```sh
nix-build --attr packages.linux       # bin/rutter, wrapped
nix-build --attr packages.androidLib  # lib/<abi>/librutter.so
nix-build --attr packages.apk         # an unsigned, aligned APK
```

Develop in the shell, not through `nix-build`. A derivation is a pure
function of its inputs, so one line changed in `core/` rebuilds all 297
crates; cargo's incremental build is the right tool for that. The derivations
exist to prove the build does not depend on anything in `$HOME`, and to
produce artifacts worth installing on a real boat's phone.

Cargo and Nix do not fight, because they own different layers: `Cargo.lock`
pins the crates and Nix *reads* it rather than duplicating it. With no git
dependencies in the lockfile — and there are none — this needs no
hand-maintained hashes at all, which is the usual reason people give up on
nixifying Rust.

`packages.apk` is deliberately **unsigned**. Everything in `/nix/store` is
world-readable, so a signing key must never be a derivation input. Nix
produces the reproducible aligned APK; `rutter apk` signs it with a debug key
that stays in `build/` and is gitignored.

### Inputs

Pinned by [nixtamal](https://code.toast.al/toastal/nixtamal) in `nix/tamal`,
flake-free, same as the other repos here. Two inputs: nixpkgs, and
`rust-overlay` — the latter because nixpkgs' rustc ships `std` for the build
host only, and this project cross-compiles to three Android ABIs.

```sh
nix-shell -p nixtamal --run 'nixtamal refresh'
```

## What works, and what it actually draws

The pipeline runs end to end:

```
ENC/S-57, Shapefile, GeoJSON     someone else's data
  → ogr2ogr → tippecanoe         `rutter charts`
  → MBTiles                      SQLite + gzipped protobuf
  → mbtiles::MbTiles             query, gunzip
  → mvt::Tile                    protobuf, geometry
  → ChartTile                    world coordinates
  → rutter-render                lines on a GPU surface
```

Try it on the test fixture, which is 32 KB of made-up Cornish coastline built
by the project's own pipeline:

```sh
rutter run --charts charts/tests/data/coast.mbtiles
```

That opens centred on the data and draws the coastline, the depth-area
boundary and the soundings, over the tile grid, with live pan and zoom.

Polygons are drawn as their ring outlines rather than filled. Filling needs
triangulation, which is real work and not yet done — and depth areas and
coastlines are largely *read* as their boundaries anyway.

### Two details worth recording

Both are the kind of thing that is painful to retrofit.

**Vertices are offsets from a reference point, never absolute world
coordinates.** Absolute Web Mercator at zoom 20 needs more precision than
`f32` has, and the symptom is geometry that jitters as you pan. Each vertex
buffer carries its own reference point and the shader adds it back, so the
numbers reaching `f32` are always small. The subtraction happens in `f64`.

**The chart buffer's reference point is fixed at upload, not tied to the
view.** This is what makes a drag cheap: panning changes one uniform — eight
bytes — while the coastline stays on the GPU untouched. The tile grid, which
is rebuilt per frame anyway, uses the view centre as its reference and so has
an origin of zero. Tying the chart's origin to the view instead would
invalidate every vertex the moment the view moved, which is a megabyte a
frame during a drag.

The app also reloads chart geometry only when the visible *tile set* changes,
not on every pan or frame — most of a drag stays within the same tiles.

## What is not done

Stated plainly, because the whole point of
[`../../opencpn`](../../opencpn) was discovering what someone had not stated.

- **No pinch-to-zoom.** winit hands Android raw touch points rather than a
  scale gesture, so it means tracking two ids and their separation. Left
  until there is a device to tune it on; guessing at gesture thresholds
  without one is how you get a chart that fights you.
- **No filled polygons.** Depth areas are outlines, not shaded bands. Needs a
  triangulator.
- **No labels.** No depth figures, no place names. Text rendering is its own
  project.
- **No real charts vendored.** They are large, often licensed, and never ours
  to redistribute. `rutter charts` turns yours into tiles; the 32 KB fixture
  is invented data.
- **No GRIB decode.** `core::weather` is the part that matters once the bytes
  are decoded: a lat/lon grid, bilinear sampling, and wind kept as U/V
  components. Parsing GRIB2 itself is a separate job.
- **No real tide data.** `core::tide` does the harmonic arithmetic. The
  amplitudes and phases are the asset, and they are per-station, measured
  over years. NOAA publishes them for US waters; other hydrographic offices
  vary.
- **No raster tiles.** `charts` recognises a raster MBTiles and refuses it
  with a clear message rather than drawing nothing. Decoding PNG and drawing
  textured quads is a separate pipeline from vector linework.
- **No nodal corrections** in the tide model, which modulate amplitude and
  phase over the Moon's 18.6-year regression. Costs a few percent. Fine for a
  passage plan; not for clearing a bar at low water on a spring tide.
- **No GPS.** Both platform crates would need it, and on Android it needs a
  runtime permission, which needs an APK.
- **The Android app has no chart picker.** It loads one chart, bundled in the
  APK as an asset and copied to internal storage on first run. Choosing your
  own file needs an intent or a document picker.
- **Nothing has run on real hardware.** The APK is built, signed, aligned and
  structurally verified — 0 dex entries, `librutter.so` stored uncompressed
  and 16 KB-aligned, v2+v3 signatures — but verified is not the same as
  tried, and I have not had a device to try it on.

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
- **No Gradle, and no Java.** The APK is `aapt2 link`, `zip -0`, `zipalign`,
  `apksigner` — four commands. It carries **no `classes.dex` at all**:
  `android:hasCode="false"` plus the framework's `android.app.NativeActivity`
  means a purely native app needs no application bytecode. That also sidesteps
  the entire class of problem that killed the opencpn attempt, where the
  missing piece was a Java activity.
- **`zipalign -p` and `-P 16` are mutually exclusive.** `-p` means the older
  4 KB alignment; `-P 16` names the size. Passing both is an error.
- **Two different alignments have to be right.** The linker flag
  (`-Wl,-z,max-page-size=16384`) controls the ELF's own segments; `zipalign -P
  16` controls the offsets inside the APK's zip. Neither implies the other,
  and `rutter apk` reads both back rather than trusting them.
- **One tile layer per S-57 object class, not one flattened layer.** The
  pipeline used to pass `-l chart` to tippecanoe, which collapsed all ~100
  object classes in an ENC cell into one undifferentiated layer — throwing
  away the only thing a renderer can style on. A coastline and a cable area
  came out identical. The fixture is multi-layer now specifically so this
  cannot regress unnoticed.
- **Nix indented strings strip the *common* leading whitespace.** One line at
  column 0 inside `''…''` stops every other line being dedented. That broke
  the `rutter help` heredoc, whose terminator then sat indented and never
  matched — and the whole driver script stopped parsing. Help is `printf` now,
  and the layer report is SQL rather than a multi-line Python block.
- **MBTiles rows count from the south.** MBTiles inherited TMS numbering,
  where row 0 is the bottom of the world; slippy-map tile y counts from the
  top. The conversion is `row = 2^zoom - 1 - y`, and getting it wrong gives a
  chart that is *plausibly* wrong — correct tiles in the wrong places —
  rather than obviously wrong. There is a test asserting both stored rows of
  the fixture are reachable and that a row the file lacks is not.
- **`tiles` is often a view, not a table.** tippecanoe writes `map` and
  `images` tables with a `tiles` view joining them, so identical tiles are
  stored once. A reader that checks `sqlite_master` for a `tiles` *table*
  rejects real files.
- **Vector tiles are gzipped, and nothing says so.** The MBTiles format is
  silent on compression and writers disagree, so the gzip magic is sniffed
  rather than assumed.
