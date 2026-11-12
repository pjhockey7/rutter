#!/usr/bin/env python3
"""Fetch one current station's constituents and NOAA's own predictions.

The companion to `tide-fixture.py`, and it exists for the same reason: a
tidal predictor with a mismatched epoch produces a perfectly well-formed
current at the wrong time, and self-consistency tests cannot tell. The only
thing that settles it is predicting from the published constituents and
comparing against the numbers NOAA publishes for the same instants.

Currents are worth checking separately from heights even though they share
the arithmetic, because three things differ and each is a way to be wrong:

  * the amplitudes are cm/s along a channel, not metres of rise;
  * there is a *mean* term, `majorMeanSpeed`, which is a net set that never
    reverses — drop it and slack water lands at the wrong time;
  * the flood and ebb directions are published separately and are not
    reciprocal. At San Diego Bay Entrance the flood sets 355 and the ebb
    173, which is 178 degrees apart, not 180.

    python3 scripts/current-fixture.py PCT0031 \\
        > core/tests/data/current/PCT0031.txt

Everything here is a US federal work and so public domain.
"""

import argparse
import json
import sys
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone

MDAPI = "https://api.tidesandcurrents.noaa.gov/mdapi/prod/webapi/stations"
DATAGETTER = "https://api.tidesandcurrents.noaa.gov/api/prod/datagetter"
AGENT = "rutter (https://github.com/pjhockey7/rutter) current fixture fetch"

# NOAA serves current predictions in cm/s with `units=metric`. Knots is what
# a navigator reads and what the app carries, so the conversion happens here
# rather than in four places later.
CMS_TO_KNOTS = 0.0194384449

# Epochs spread across the Moon's 18.6-year nodal cycle. The node factors
# are the part of the arithmetic most easily got wrong in a way that is
# invisible for years at a time, so one window is not enough.
EPOCHS = ["20180315", "20220615", "20260301", "20300901", "20341201"]

# How long a window at each epoch. Two days of hourly values is 48 points,
# which covers four full semidiurnal cycles and both diurnal inequalities.
WINDOW_DAYS = 2


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": AGENT})
    with urllib.request.urlopen(request, timeout=90) as response:
        return json.load(response)


def constituents(sid, bin_number):
    """(azi, mean_cms, [(name, amplitude_cms, phase_deg)], bin_depth)."""
    harcon = fetch("%s/%s/harcon.json?units=metric&bin=%d" % (MDAPI, sid, bin_number))
    rows = harcon.get("HarmonicConstituents") or []
    if not rows:
        sys.exit("%s has no harmonic constituents — a subordinate station?" % sid)

    # Every row repeats the station-wide values, so the first is as good as
    # any. Read rather than assumed: a station whose rows disagreed would be
    # a reason to stop, not to pick one.
    azi = rows[0]["azi"]
    mean = rows[0]["majorMeanSpeed"]
    depth = rows[0].get("binDepth")
    for row in rows:
        if row["azi"] != azi or row["majorMeanSpeed"] != mean:
            sys.exit("%s: rows disagree about the axis or the mean set" % sid)

    kept = [
        (row["constituentName"], row["majorAmplitude"], row["majorPhaseGMT"])
        for row in rows
        if row["majorAmplitude"] != 0.0
    ]
    return azi, mean, kept, depth


def directions(sid, bin_number):
    """(flood_deg, ebb_deg), from a prediction row.

    The predictions carry them and the metadata does not, which is why this
    asks for a single hour of prediction it then throws away.
    """
    url = (
        "%s?product=currents_predictions&application=rutter"
        "&begin_date=20260301&end_date=20260301&station=%s&bin=%d"
        "&time_zone=gmt&units=metric&interval=h&format=json"
        % (DATAGETTER, sid, bin_number)
    )
    rows = fetch(url).get("current_predictions", {}).get("cp") or []
    if not rows:
        sys.exit("%s published no current predictions" % sid)
    return rows[0]["meanFloodDir"], rows[0]["meanEbbDir"]


def predictions(sid, bin_number, start):
    """[(unix, velocity_cms)] over WINDOW_DAYS from `start` (YYYYMMDD)."""
    begin = datetime.strptime(start, "%Y%m%d").replace(tzinfo=timezone.utc)
    end = begin + timedelta(days=WINDOW_DAYS - 1)
    url = (
        "%s?product=currents_predictions&application=rutter"
        "&begin_date=%s&end_date=%s&station=%s&bin=%d"
        "&time_zone=gmt&units=metric&interval=h&format=json"
        % (DATAGETTER, begin.strftime("%Y%m%d"), end.strftime("%Y%m%d"), sid, bin_number)
    )
    out = []
    for row in fetch(url).get("current_predictions", {}).get("cp") or []:
        when = datetime.strptime(row["Time"], "%Y-%m-%d %H:%M").replace(
            tzinfo=timezone.utc
        )
        out.append((int(when.timestamp()), row["Velocity_Major"]))
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("station", help="NOAA current station id, e.g. PCT0031")
    parser.add_argument("--bin", type=int, default=1, help="current bin (default 1)")
    args = parser.parse_args()

    sid = args.station
    try:
        azi, mean_cms, rows, depth = constituents(sid, args.bin)
        flood, ebb = directions(sid, args.bin)
    except (urllib.error.HTTPError, urllib.error.URLError) as e:
        sys.exit("%s: %s" % (sid, e))

    listing = fetch("%s/%s.json?type=currentpredictions&units=metric" % (MDAPI, sid))
    station = (listing.get("stations") or [{}])[0]

    print("# NOAA CO-OPS current station %s, bin %d, from the public API."
          % (sid, args.bin))
    print("# Regenerate with scripts/current-fixture.py %s" % sid)
    print("# Public domain (US federal work).")
    print("#")
    print("# Amplitudes and the mean set are knots along the flood axis;")
    print("# NOAA publishes cm/s and the conversion is %.10f." % CMS_TO_KNOTS)
    print("station %s %.6f %.6f %.1f %.1f %.4f %s"
          % (sid,
             station.get("lat", 0.0),
             station.get("lng", 0.0),
             flood,
             ebb,
             mean_cms * CMS_TO_KNOTS,
             station.get("name", sid)))
    if depth is not None:
        print("depth %.2f" % depth)
    print("# The major-axis azimuth from harcon, for comparison with the")
    print("# flood direction above: they should agree.")
    print("# azi %.1f" % azi)

    for name, amplitude_cms, phase in sorted(rows):
        print("constituent %s %.6f %.3f" % (name, amplitude_cms * CMS_TO_KNOTS, phase))

    total = 0
    for epoch in EPOCHS:
        try:
            series = predictions(sid, args.bin, epoch)
        except (urllib.error.HTTPError, urllib.error.URLError) as e:
            print("# %s: %s" % (epoch, e), file=sys.stderr)
            continue
        print("# epoch %s, %d hourly values" % (epoch, len(series)))
        for unix, cms in series:
            print("velocity %d %.6f" % (unix, cms * CMS_TO_KNOTS))
        total += len(series)

    print("# %d reference values in total" % total, file=sys.stderr)


if __name__ == "__main__":
    main()
