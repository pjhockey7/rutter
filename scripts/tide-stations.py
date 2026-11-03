#!/usr/bin/env python3
"""Build a tide-station database from NOAA CO-OPS.

Fetches every harmonic tide station inside a bounding box, together with its
constituents and its datums, and writes them to a SQLite file that
`rutter-tides` reads.

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
import sqlite3
import sys
import urllib.error
import urllib.request

MDAPI = "https://api.tidesandcurrents.noaa.gov/mdapi/prod/webapi/stations"
FEET_TO_M = 0.3048
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

    db.executemany(
        "INSERT OR REPLACE INTO metadata VALUES (?, ?)",
        [
            ("source", "NOAA CO-OPS"),
            ("attribution", "Tides: NOAA CO-OPS (public domain)"),
            ("box", args.box),
            ("stations", str(kept)),
        ],
    )
    db.commit()
    db.close()

    print()
    print("  %s" % args.out)
    print("  stations: %d%s"
          % (kept, ", %d subordinate skipped" % subordinate if subordinate else ""))


if __name__ == "__main__":
    main()
