#!/usr/bin/env python3
"""Build a tide and tidal-current station database from NOAA CO-OPS.

Fetches every harmonic tide station inside a bounding box, together with its
constituents and its datums — and every harmonic *current* station, with its
flood axis and its mean set — and writes them to a SQLite file that
`rutter-tides` reads.

Heights and currents go in one file because they are fetched from one API
for one stretch of coast and are wanted together: the question "can I get
over that bar" and the question "will the ebb carry me onto it" are the
same question asked twice.

Why a local database at all: the constituents are a few hundred bytes per
station and they do not expire. Fetch them once and the tide is predictable
for years with no signal — which is the whole point on a boat. Everything
NOAA serves here is a US federal work and so public domain; no key, no
token, no terms against keeping a copy.

Not every station in the list is usable. NOAA mixes two kinds:

  * harmonic stations, which publish constituents, and
  * subordinate stations, which publish only time and height offsets
    against a nearby harmonic station.

Only the first kind can be predicted from first principles, so the others
are counted and skipped rather than half-stored.

    python3 scripts/tide-stations.py --box 32.5,-118.7,33.7,-117.0 \\
        --out build/charts/tides.sqlite
"""

import argparse
import json
import os
import sqlite3
import sys
import urllib.error
import urllib.request

MDAPI = "https://api.tidesandcurrents.noaa.gov/mdapi/prod/webapi/stations"
DATAGETTER = "https://api.tidesandcurrents.noaa.gov/api/prod/datagetter"
FEET_TO_M = 0.3048
# NOAA serves current amplitudes in cm/s with `units=metric`. Knots is what
# a navigator reads, so the conversion happens once, here.
CMS_TO_KNOTS = 0.0194384449
AGENT = "rutter (https://github.com/pjhockey7/rutter) tide station fetch"

SCHEMA = """
CREATE TABLE station (
    id       TEXT PRIMARY KEY,
    name     TEXT NOT NULL,
    lat      REAL NOT NULL,
    lon      REAL NOT NULL,
    datum_m  REAL NOT NULL
);
CREATE TABLE constituent (
    station      TEXT NOT NULL REFERENCES station(id),
    name         TEXT NOT NULL,
    amplitude_m  REAL NOT NULL,
    phase_deg    REAL NOT NULL,
    PRIMARY KEY (station, name)
);
CREATE TABLE current_station (
    id         TEXT NOT NULL,
    bin        INTEGER NOT NULL,
    name       TEXT NOT NULL,
    lat        REAL NOT NULL,
    lon        REAL NOT NULL,
    flood_deg  REAL NOT NULL,
    ebb_deg    REAL NOT NULL,
    depth_m    REAL,
    mean_kn    REAL NOT NULL,
    PRIMARY KEY (id, bin)
);
CREATE TABLE current_constituent (
    station      TEXT NOT NULL,
    bin          INTEGER NOT NULL,
    name         TEXT NOT NULL,
    amplitude_kn REAL NOT NULL,
    phase_deg    REAL NOT NULL,
    PRIMARY KEY (station, bin, name)
);
CREATE TABLE metadata (name TEXT PRIMARY KEY, value TEXT NOT NULL);
"""


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": AGENT})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def harmonic_station(sid):
    """(datum_m, [(name, amplitude_m, phase_deg)]) or None if unusable."""
    try:
        harcon = fetch("%s/%s/harcon.json" % (MDAPI, sid))
        datums = fetch("%s/%s/datums.json" % (MDAPI, sid))
    except (urllib.error.HTTPError, urllib.error.URLError):
        return None

    rows = harcon.get("HarmonicConstituents") or []
    if not rows:
        return None

    units = (harcon.get("units") or "feet").lower()
    scale = FEET_TO_M if units.startswith(("f", "ft")) else 1.0

    by_name = {d["name"]: d["value"] for d in (datums.get("datums") or [])}
    if "MSL" not in by_name or "MLLW" not in by_name:
        return None
    # The harmonic sum is referenced to mean sea level; a chart's soundings
    # and a navigator's clearance are referenced to lowest water.
    datum_m = (by_name["MSL"] - by_name["MLLW"]) * scale

    constituents = [
        (c["name"], c["amplitude"] * scale, c["phase_GMT"])
        for c in rows
        if c["amplitude"] != 0.0
    ]
    if not constituents:
        return None
    return datum_m, constituents


def current_station(sid, bin_number):
    """Everything needed to predict one current station, or None.

    Two calls, because NOAA splits what is needed across them: `harcon`
    has the constituents, the major-axis azimuth and the mean set, while
    the flood and ebb *directions* appear only on a prediction row. They
    are not reciprocal — San Diego Bay Entrance floods 355 and ebbs 173 —
    so they cannot be derived from the azimuth alone.
    """
    try:
        harcon = fetch("%s/%s/harcon.json?units=metric&bin=%d" % (MDAPI, sid, bin_number))
    except (urllib.error.HTTPError, urllib.error.URLError):
        return None

    rows = harcon.get("HarmonicConstituents") or []
    if not rows:
        return None

    mean_kn = rows[0].get("majorMeanSpeed", 0.0) * CMS_TO_KNOTS
    depth_m = rows[0].get("binDepth")
    constituents = [
        (r["constituentName"], r["majorAmplitude"] * CMS_TO_KNOTS, r["majorPhaseGMT"])
        for r in rows
        if r.get("majorAmplitude")
    ]
    if not constituents:
        return None

    # One hour of prediction, asked for only to read the directions off it.
    url = (
        "%s?product=currents_predictions&application=rutter"
        "&begin_date=20260301&end_date=20260301&station=%s&bin=%d"
        "&time_zone=gmt&units=metric&interval=h&format=json"
        % (DATAGETTER, sid, bin_number)
    )
    try:
        cp = fetch(url).get("current_predictions", {}).get("cp") or []
    except (urllib.error.HTTPError, urllib.error.URLError):
        cp = []
    if cp:
        flood = float(cp[0]["meanFloodDir"])
        ebb = float(cp[0]["meanEbbDir"])
    else:
        # Fall back to the axis. Less accurate in a bend, and said out
        # loud rather than silently assumed.
        flood = float(rows[0].get("azi", 0.0))
        ebb = (flood + 180.0) % 360.0
        print("    (no published directions; using the axis %.0f/%.0f)" % (flood, ebb))

    return flood, ebb, depth_m, mean_kn, constituents


def fetch_currents(db, south, west, north, east):
    """Fill the current tables. Returns (kept, skipped)."""
    try:
        listing = fetch("%s.json?type=currentpredictions&units=metric" % MDAPI)["stations"]
    except (urllib.error.HTTPError, urllib.error.URLError) as e:
        print("current stations unavailable: %s" % e, file=sys.stderr)
        return 0, 0

    inside = [
        s for s in listing
        if south <= s["lat"] <= north and west <= s["lng"] <= east
    ]
    # NOAA lists the same station once per depth bin, so this is bins and
    # not places. Each bin is predicted separately because in a stratified
    # estuary a deep bin can run the other way from the surface.
    print()
    print("%d of %d current-prediction bins inside the box"
          % (len(inside), len(listing)))

    kept = 0
    subordinate = 0
    for s in sorted(inside, key=lambda s: (s["id"], s.get("currbin") or 1)):
        bin_number = s.get("currbin") or 1
        # Type S is subordinate: time and speed offsets against a
        # reference station, with no constituents of its own. There are
        # many more of these than harmonic ones, and predicting from
        # offsets is a different algorithm — so they are counted and
        # skipped rather than half-stored.
        if (s.get("type") or "").upper() != "H":
            subordinate += 1
            continue
        result = current_station(s["id"], bin_number)
        if result is None:
            subordinate += 1
            print("  skip %-9s %s (no harmonic constituents)" % (s["id"], s["name"]))
            continue
        flood, ebb, depth_m, mean_kn, constituents = result
        db.execute(
            "INSERT OR REPLACE INTO current_station VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (s["id"], bin_number, s["name"], s["lat"], s["lng"],
             flood, ebb, depth_m, mean_kn),
        )
        db.executemany(
            "INSERT OR REPLACE INTO current_constituent VALUES (?, ?, ?, ?, ?)",
            [(s["id"], bin_number, n, a, p) for n, a, p in constituents],
        )
        kept += 1
        peak = sum(abs(a) for _, a, _ in constituents)
        print("  %-9s bin %d %-40s %2d constituents, up to ~%.1f kn"
              % (s["id"], bin_number, s["name"][:40], len(constituents), peak))

    return kept, subordinate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--box", required=True,
                        help="south,west,north,east in degrees")
    parser.add_argument("--out", required=True, help="SQLite file to write")
    args = parser.parse_args()

    try:
        south, west, north, east = (float(v) for v in args.box.split(","))
    except ValueError:
        sys.exit("expected --box south,west,north,east, got %r" % args.box)
    if south >= north or west >= east:
        sys.exit("--box south,west must be below north,east")

    listing = fetch("%s.json?type=tidepredictions" % MDAPI)["stations"]
    inside = [
        s for s in listing
        if south <= s["lat"] <= north and west <= s["lng"] <= east
    ]
    print("%d of %d prediction stations inside the box"
          % (len(inside), len(listing)))

    # Rebuilt from scratch rather than added to. A half-updated database
    # — new current tables beside stations from an older, narrower box —
    # would be a file nobody could reason about.
    if os.path.exists(args.out):
        print("replacing %s" % args.out)
        os.remove(args.out)
    db = sqlite3.connect(args.out)
    db.executescript(SCHEMA)

    kept = 0
    subordinate = 0
    for s in sorted(inside, key=lambda s: s["id"]):
        result = harmonic_station(s["id"])
        if result is None:
            subordinate += 1
            print("  skip %-9s %s (no harmonic constituents)"
                  % (s["id"], s["name"]))
            continue
        datum_m, constituents = result
        db.execute(
            "INSERT OR REPLACE INTO station VALUES (?, ?, ?, ?, ?)",
            (s["id"], s["name"], s["lat"], s["lng"], datum_m),
        )
        db.executemany(
            "INSERT OR REPLACE INTO constituent VALUES (?, ?, ?, ?)",
            [(s["id"], n, a, p) for n, a, p in constituents],
        )
        kept += 1
        print("  %-9s %-44s %2d constituents, Z0 %.3f m"
              % (s["id"], s["name"], len(constituents), datum_m))

    if kept == 0:
        db.close()
        sys.exit("no harmonic stations in that box")

    currents, currents_skipped = fetch_currents(db, south, west, north, east)

    db.executemany(
        "INSERT OR REPLACE INTO metadata VALUES (?, ?)",
        [
            ("source", "NOAA CO-OPS"),
            ("attribution", "Tides and currents: NOAA CO-OPS (public domain)"),
            ("box", args.box),
            ("stations", str(kept)),
            ("current_stations", str(currents)),
        ],
    )
    db.commit()
    db.close()

    print()
    print("  %s" % args.out)
    print("  tide stations: %d%s"
          % (kept, ", %d subordinate skipped" % subordinate if subordinate else ""))
    print("  current stations: %d%s"
          % (currents,
             ", %d subordinate skipped" % currents_skipped if currents_skipped else ""))


if __name__ == "__main__":
    main()
