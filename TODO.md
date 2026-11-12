# What rutter still needs

A feature audit, ranked. The shape of it: **excellent data layers and no
navigation.** Everything built so far answers "what is around me". Nothing
yet answers "how do I get there" or "tell me when something is wrong".

Home waters for every judgement below are San Diego to Catalina, Android
first.

## Already aboard

NOAA ENC vector charts · USGS aerial imagery z6–16 · harmonic tides, 1.02 cm
RMS against NOAA · tidal currents, 0.011 kn RMS · tide-aware under-keel
clearance, computed on the GPU · GFS wind barbs · air temperature wash · a
time slider driving all of the above · NMEA 0183 over TCP, UDP and serial ·
phone GPS through JNI · own-ship marker with course and heading · follow mode
· Linux and Android builds · a portable glibc 2.17 binary.

354 tests, clippy and fmt clean on host and `aarch64-linux-android`.

## Tier 1 — not a plotter without these

Six of these are small. One is not, and most of the rest of this page waits
behind it.

- [ ] **Routes and waypoints.** Go-to, bearing, distance, cross-track error,
      ETA. Every other feature here serves a decision that currently cannot
      be expressed. Tracks, alarms and ETA all hang off the same geometry
      plumbing, so they get much cheaper once this exists.

- [ ] **Tracks.** Where you have been — a safety feature in its own right,
      retracing your own line out of shallow water in fog, and the basis of a
      log. The fix stream already exists, so this is cheap.

- [ ] **Anchor drag and shallow-water alarms.** `DBT` and `DPT` already
      arrive from the sounder and the position stream is running. The highest
      value-to-effort item on this list.

- [ ] **Feet and fathoms.** Everything is metres. US charts print feet and
      the draft is in feet in your head. Depth, height and clearance all need
      a unit setting.

- [ ] **Local time.** Every time is UTC with a `Z`. Planning an 0600
      departure off UTC tide tables is the one gap here that is actively
      dangerous rather than merely missing. Needs a timezone source, which is
      why it was deferred.

- [ ] **Settings that persist.** Draft, units, layer choices and the NMEA
      address all reset every launch. On Android the address has to arrive
      bundled in the APK because there is nowhere to keep it.

- [x] **Split the layers panel into tabs.** Done: fixed tabs — Boat, Chart,
      Water, Weather, Tools. Every screen has all of them and switches
      freely; only the Chart tab scrolls, which is the one place scrolling
      is the point. Saved presets still to come, and they are a selection of
      layers rather than of tabs.

## Tier 2 — safety, and more work

Each of these is something a paper chart and a pair of binoculars give you
today and the app does not.

- [ ] **AIS targets.** The transport is already there: `!AIVDM` arrives and
      is counted as "unsupported". Needs the 6-bit payload decoder, target
      tracking, and CPA/TCPA. Worth it specifically for these waters — the
      Catalina crossing cuts the Los Angeles and Long Beach traffic
      separation scheme, and closest approach on a ship doing 18 knots is the
      thing you least want to judge by eye.

- [ ] **Tap a feature to identify it.** You can see a Light symbol but not
      read it — period, colour, range, which is *how you identify a light at
      night*. The S-57 attributes survive the whole pipeline; only `DEPTH`
      and `DRVAL1` are ever used.

- [ ] **Night mode.** A white screen destroys night vision for twenty
      minutes. The chart's own colour tables need a night variant rather than
      a filter laid over the top.

- [ ] **Man overboard mark.** One control that stamps the position and starts
      a bearing and distance back to it. Trivial once waypoints exist.

- [ ] **Magnetic bearings.** The steering compass is magnetic and every
      bearing shown is true. The variation already arrives in `HDG`, so this
      is a display choice rather than new data — but it has to be consistent
      everywhere at once.

## Tier 3 — data and coverage

The pipeline runs on a desktop with GDAL and tippecanoe, which is why the
boat cannot fetch its own charts.

- [ ] **Download tool: fetch the marked area in-app.** Half done. The Tools
      tab marks the current view as an area, reports its span in miles and
      estimates the tiles and bytes per zoom range — a 22 × 12 nmi box is
      5,038 imagery tiles at about 125 MB, against 40 chart tiles at 0.1 MB
      — and prints the exact `rutter` commands to run. What is missing is
      the fetching itself: there is no HTTP client in the build at all (no
      TLS stack anywhere in the tree), so that is a real dependency decision
      before imagery can be pulled on the device. Vector charts additionally
      need the S-57 reader below.

      The area is the viewport, marked with a button, rather than a box
      dragged on the chart — it needs no new input plumbing in either
      platform shim and cannot produce a degenerate box. Drag-to-select is
      worth adding later for marking an area you are not looking at.

- [ ] **S-57 reader and MVT encoder in Rust.** The keystone. Drops GDAL and
      tippecanoe from the pipeline, and is the only thing that makes in-app
      chart download possible at all. Also the largest single item here.

- [ ] **Wave height.** WAVEWATCH III, a different model from GFS. For a
      sixty-mile open-water crossing this matters considerably more than the
      air temperature already drawn.

- [ ] **Pressure, gusts, and more forecast hours.** Twenty-four hours at
      three-hourly now; GFS publishes 384. Isobars are how you read a
      forecast rather than a snapshot.

- [ ] **Subordinate current stations.** Twenty-three of the twenty-five
      nearby publish time and speed offsets against a reference station
      rather than harmonic constituents. A different prediction method, and it
      would take two current arrows to twenty-five.

- [ ] **Scale-band tiling for `rutter noaa --box`.** A large box collapses to
      overview cells at one maximum zoom, so harbour detail is lost. The
      current chart is maxzoom 11 across 134°W–115°W because of this.

- [ ] **Sea surface temperature.** Distinct from the 2 m air temperature now
      drawn — a different product entirely, and the one anyone fishing wants.

## Tier 4 — operational

Nothing here changes what the app can do; all of it changes whether it can be
trusted.

- [ ] **Run it on the phone.** The APK has never been installed. There is a
      lot of untested code aimed at a device nothing has ever run on — the
      whole JNI location path above all — and every feature added before that
      first run is one more thing to debug at once. **Do this first.**

- [ ] **Imagery decode off the main thread.** JPEG decoding still happens in
      the frame. The chart worker already shows the pattern to copy.

- [ ] **More than one chart and one imagery file.** One of each is open at a
      time, so coverage cannot be built up in pieces as you cruise.

- [ ] **Search by name.** No way to find a harbour, a buoy or a tide station
      except by panning to it.

- [ ] **iOS, Windows and macOS shims.** Stated targets, and the board-support
      split was built for exactly this. Each is a window, an event loop and a
      set of file paths.

## Design note: many screens on one boat

Two different questions hide inside this, with very different answers.

| Shape | Verdict |
|---|---|
| Several devices, one per screen | **Works today.** This is what `udp://:10110` already buys. Copy the chart files to each device, point them all at the broadcast, and every screen sees the same boat. It is how real boats do it, and position is the only thing that genuinely has to be shared live. |
| One box, several displays | **A contained refactor.** Two splits: `Renderer` into a shared GPU and a per-surface screen, and `App` into a shared boat model and a per-screen view. The chart worker also needs a job per screen rather than coalescing to the newest. None of it is thrown-away work. |
| One server, thin clients | **A different product.** Needs a protocol, a server and tile streaming. Not worth it when the first row already solves the problem. |

The encouraging part is that `App`'s fields already cluster along exactly
that seam:

- **The boat**, shared by every screen: `charts` (the worker), `tides`,
  `stations`, `currents`, `weather`, `imagery`, `nmea`, `gps`, `draft`.
- **The screen**, one per display: `view`, `layers`, `loaded`, `requested`,
  `last`, `imagery_loaded`, every `*_visible` flag, `time_scope`,
  `tide_pinned`, `overlay`, `temperature_patches`, `follow`, `download`,
  `ui`.

Time is already scoped, built while there was still only one screen because
it would have cost a rework later and costs nothing now. A screen is either
`TimeScope::Linked`, following the `shared_offset_secs` that lives on the
boat, or `TimeScope::Own(offset)` with its own. Default linked, so dragging
the slider anywhere moves everything — unlink the screen you steer by and it
stays where you left it. Unlinking never moves the chart; it only stops it
moving with the others. The panel hides the control while there is one
screen, since linked and unlinked are then indistinguishable.

Routes and waypoints should go in a store **outside the process** — a file
plus a change notification, ideally GPX so it interchanges with every other
plotter. That is work the persistence item needs anyway, and it is what makes
"several instances" and "one process, several windows" a deployment choice
rather than an architecture decision. Independent instances are what
commercial MFDs do, and they survive one screen crashing; the single process
earns its keep only where chart geometry and imagery textures have to be
shared, which is the one-box-three-displays case.

Nothing has to move house, only be named. Which makes the tab overhaul the
right moment to do it: a tab and a screen want to be the same object — a
named preset holding a view, a layer selection and a time. Build tabs that way
and "depth and tides on one display, wind and temperature on another" becomes
opening two windows on two presets, not a new architecture.

## Where to start

Install the APK and open it on the water, before anything else. Then routes
and waypoints — the one absence that makes the rest feel like a chart viewer
rather than a plotter, and the thing that makes tracks, alarms and ETA small.

Tabs and the download tool can go in alongside, since both are additive and
neither waits on the navigation work. Keeping the preset shape in mind while
doing the tabs is what keeps the multi-screen version cheap.
