#!/usr/bin/env python3
"""Build the tide test fixture from NOAA's public API.

Writes one line-oriented file holding a station's harmonic constituents and
a set of NOAA's own published predictions to check them against. The test
in core/tests/tide_noaa.rs predicts from the constituents and compares.

The reference windows deliberately span the Moon's 18.6-year nodal cycle.
A quadrant mistake in the nodal correction is correct near a node longitude
of zero and wrong in between, so a fixture taken from a single year would
pass while half the cycle was broken. That is not hypothetical: it happened,
and sampling the cycle is what caught it.

    python3 scripts/tide-fixture.py 9410170 > core/tests/data/tide/9410170.txt

No key is needed. NOAA CO-OPS data is public domain.
"""

import argparse
import json
import sys
import urllib.request
from datetime import datetime, timedelta, timezone

MDAPI = "https://api.tidesandcurrents.noaa.gov/mdapi/prod/webapi/stations"
DATAGETTER = "https://api.tidesandcurrents.noaa.gov/api/prod/datagetter"
FEET_TO_M = 0.3048

# Ten days each, spread across the nodal cycle so every quadrant of the
# node's longitude is represented. The node regresses about 19.3 deg/year,
# so these are roughly a quarter-cycle apart.
WINDOWS = ["20170308", "20210612", "20260301", "20300905", "20350120"]
WINDOW_DAYS = 10


def fetch(url):
    request = urllib.request.Request(
        url, headers={"User-Agent": "rutter tide fixture (public domain data)"}
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("station", help="NOAA CO-OPS station id, e.g. 9410170")
    args = parser.parse_args()
    sid = args.station

    meta = fetch("%s/%s.json" % (MDAPI, sid))["stations"][0]
    harcon = fetch("%s/%s/harcon.json" % (MDAPI, sid))
    datums = fetch("%s/%s/datums.json" % (MDAPI, sid))

    units = (harcon.get("units") or "feet").lower()
    if units not in ("feet", "ft", "metres", "meters", "m"):
        sys.exit("unexpected amplitude units %r" % units)
    scale = FEET_TO_M if units.startswith(("f", "ft")) else 1.0

    by_name = {d["name"]: d["value"] for d in datums["datums"]}
    for needed in ("MSL", "MLLW"):
        if needed not in by_name:
            sys.exit("station %s publishes no %s datum" % (sid, needed))
    # The harmonic sum is about mean sea level; soundings and clearances are
    # about lowest water. This offset is what reconciles them.
    z0 = (by_name["MSL"] - by_name["MLLW"]) * scale

    out = sys.stdout
    out.write("# NOAA CO-OPS station %s, fetched from the public API.\n" % sid)
    out.write("# Regenerate with scripts/tide-fixture.py %s\n" % sid)
    out.write("# Public domain (US federal work).\n")
    out.write("station %s %.6f %.6f %.6f %s\n"
              % (sid, meta["lat"], meta["lng"], z0, meta["name"]))

    kept = 0
    for c in harcon["HarmonicConstituents"]:
        amplitude = c["amplitude"] * scale
        # NOAA lists every constituent it models, including ones it has no
        # amplitude for at this station. A zero term contributes nothing and
        # only makes the fixture bigger.
        if amplitude == 0.0:
            continue
        out.write("constituent %s %.6f %.3f\n"
                  % (c["name"], amplitude, c["phase_GMT"]))
        kept += 1

    total = 0
    for start in WINDOWS:
        begin = datetime.strptime(start, "%Y%m%d").replace(tzinfo=timezone.utc)
        end = begin + timedelta(days=WINDOW_DAYS)
        # The interval goes on per request: datagetter honours the first
        # `interval` it sees, so a base URL that already names one cannot be
        # extended with another.
        base = ("%s?station=%s&product=predictions&datum=MLLW&time_zone=gmt"
                "&units=metric&format=json&begin_date=%s&end_date=%s"
                % (DATAGETTER, sid, start, end.strftime("%Y%m%d")))
        points = fetch(base + "&interval=h")["predictions"]
        out.write("# window %s, %d hourly predictions\n" % (start, len(points)))
        for p in points:
            when = datetime.strptime(p["t"], "%Y-%m-%d %H:%M").replace(
                tzinfo=timezone.utc)
            out.write("prediction %d %s\n" % (int(when.timestamp()), p["v"]))
            total += 1

        # The published high and low waters for the same window. Hourly
        # samples put a turning point only within half an hour, so testing
        # the extremes finder against them would be testing the sampling;
        # these are NOAA's own answers to the minute.
        hilo = fetch(base + "&interval=hilo")["predictions"]
        out.write("# window %s, %d published high/low waters\n"
                  % (start, len(hilo)))
        for p in hilo:
            when = datetime.strptime(p["t"], "%Y-%m-%d %H:%M").replace(
                tzinfo=timezone.utc)
            out.write("extreme %d %s %s\n"
                      % (int(when.timestamp()), p["type"], p["v"]))

    sys.stderr.write("%s: %d constituents, %d reference predictions\n"
                     % (sid, kept, total))


if __name__ == "__main__":
    main()
