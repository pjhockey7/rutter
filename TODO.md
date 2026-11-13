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

426 tests, clippy and fmt clean on host and `aarch64-linux-android`.

## Tier 1 — not a plotter without these

Six of these are small. One is not, and most of the rest of this page waits
behind it.

- [x] **Routes and waypoints.** Done. Waypoints placed by hand (`w` key or
      the Route tab), straight legs between them, and the three numbers that
      make a route worth having: course and distance to the next mark,
      signed cross-track error *with which way to steer back*, and speed
      made good with an arrival time. Legs are drawn in world space so they
      stretch with the zoom; the active waypoint is ringed.

      Choosing a waypoint by hand sticks, because a plotter that reselects
      the nearest leg every second hops between legs where a passage
      doubles back. Editing the route releases that choice, since an index
      names a waypoint and deleting one renumbers them.

      Rhumb-line bearings, because a rhumb line is the only track a helm or
      an autopilot can hold; cross-track measured against the great circle,
      with a test that measures the disagreement rather than assuming it.

      Still to come: **tap to place** a waypoint on the chart rather than
      the `w` key, **saving** routes (GPX, so they interchange with other
      plotters), and **arrival and off-track alarms** — which are small now.

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

- [x] **Settings that persist.** Done. Two files in a platform-chosen
      directory — `$XDG_DATA_HOME/rutter` on Linux, the app's own data
      directory on Android:

      * `settings.conf`, `key = value`, hand-rolled rather than TOML or
        JSON. A settings file that will not load is a thing someone has to
        repair on a boat with no internet, and a text editor is the only
        tool they are guaranteed to have. Unknown keys are kept and written
        back, so running an older build does not reset what a newer one
        was keeping.
      * `route.gpx`, which is **GPX** rather than a format of our own, so a
        passage can be handed to someone with a Garmin and one off their
        Garmin can be sailed here. Read with a tolerant scanner that
        handles namespace prefixes, self-closing tags and files holding
        only a recorded track; its limits are documented rather than
        hoped for.

      Written only when something changed — saving runs every frame, and
      writing unconditionally would be sixty writes a second on a phone —
      and written via a temporary file and a rename, so a power cut cannot
      leave half a settings file.

      Precedence is flag, then saved, then default. Worth stating because
      the first version applied the settings *after* the command line and
      silently discarded `--draft`.

      Still to come: the NMEA address is saved but **units** are not, since
      there are none yet.

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

- [x] **Download tool: fetch the marked area in-app.** Done for imagery.
      The Tools tab marks the current view, reports its span in miles,
      estimates tiles and bytes per zoom range — a 22 × 12 nmi box is 5,038
      imagery tiles at about 125 MB, against 40 chart tiles at 0.1 MB — then
      fetches it with a progress bar, a Stop button, and reopens the result
      so the new imagery appears without a restart.

      Fetching goes through `TileFetcher`, a trait in `rutter-app`, with the
      HTTPS implementation isolated in its own `rutter-fetch` crate: `ureq`
      with rustls pinned to the `ring` provider, which cross-compiles for
      Android as it stands. Swapping Android to `HttpsURLConnection` over
      JNI later — for the device's own certificate store, and to survive a
      marina's captive portal — is one trait impl and touches nothing else.

      The area is the viewport, marked with a button or the `m` key, rather
      than a box dragged on the chart: no new input plumbing in either
      platform shim and no way to produce a degenerate box. Drag-to-select
      is still worth adding for marking an area you are not looking at.

      Downloads **add** to the imagery you have rather than replacing it:
      the fetch writes a `.part` file and merges it in, widening the zoom
      range and bounds. The first version wrote straight to the open file
      and destroyed 35 MB of harbour detail the first time it ran.

      Still missing: **vector charts**, which need the S-57 reader below —
      the Tools tab prints the `rutter noaa` command for those. And
      **resuming**: a download creates its part file fresh, so a cancelled
      one re-fetches rather than filling in its gaps.

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

- [ ] **Trim downloads from inside the app.** `rutter tiles` does it from
      the command line — report by zoom, trim to a box, drop blanks, with a
      `--dry-run` — but a download marks a rectangle on a screen and a
      rectangle reaches inland, so the place to undo that is next to the
      button that did it.

- [ ] **More than one chart and one imagery file.** One of each is open at a
      time, so coverage cannot be built up in pieces as you cruise — and an
      in-app download now has to write a whole new file rather than adding
      to what is there, which makes this matter more than it did.

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
