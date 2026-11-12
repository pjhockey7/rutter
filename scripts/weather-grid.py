#!/usr/bin/env python3
"""Fetch a GFS wind and temperature forecast and write it as a plain grid.

NOAA's NOMADS serves GRIB2 subset by variable and bounding box, so a
forecast for one stretch of coast is under a kilobyte per hour rather than
the 500 MB of a global field. No key, no token; GFS is a US federal work and
public domain.

What this does *not* do is ship GRIB2 to the boat. GRIB2 is a container with
a dozen template families and a bitmap-packing scheme, and a decoder for it
is a project in its own right — while the only thing the app wants out of it
is "a rectangle of floats per parameter per hour". So GDAL decodes it here,
on a desktop, at fetch time, exactly as it already does for S-57 charts, and
what reaches the app is the rectangle.

The output format is the one `rutter_core::weather::Forecast::parse` reads.
Little-endian throughout:

    0   8 bytes  magic "RUTTERWX"
    8   u16      format version (1)
    10  u16      nx
    12  u16      ny
    14  u16      number of forecast hours
    16  i64      unix time of the model run
    24  f64      north edge, degrees
    32  f64      west edge, degrees
    40  f64      latitude spacing, degrees (positive, rows run south)
    48  f64      longitude spacing, degrees
    56           per hour: i32 hour, then nx*ny f32 each of U, V, temperature

The grid is row-major from the north-west corner, which is GDAL's own
convention, so no flipping happens anywhere.

    python3 scripts/weather-grid.py --box 32.5,-118.7,33.6,-117.0 \\
        --hours 0,3,6,9,12 --out build/charts/weather.bin
"""

import argparse
import json
import os
import struct
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone

FILTER = "https://nomads.ncep.noaa.gov/cgi-bin/filter_gfs_0p25.pl"
MAGIC = b"RUTTERWX"
FORMAT_VERSION = 1

# What to ask NOMADS for. Wind as components at 10 m and temperature at 2 m,
# which is the triple the app's `WindField` and temperature grid expect.
#
# Components, not speed and direction, and not because GFS has no choice:
# averaging directions is wrong. Halfway between 350 and 010 is 0, but the
# arithmetic mean is 180 — pointing exactly backwards. Interpolating u and v
# and converting afterwards has no such discontinuity.
VARIABLES = ["UGRD", "VGRD", "TMP"]
LEVELS = ["lev_10_m_above_ground", "lev_2_m_above_ground"]

KELVIN_OFFSET = 273.15


def latest_run(now):
    """The most recent GFS cycle likely to have been published.

    Cycles run every six hours and take about four to appear, so the newest
    *useful* one is a few hours behind the clock. Asking for one that is not
    there yet gets an empty response rather than an error, which is a
    confusing way to fail, so this errs old deliberately.
    """
    candidate = now - timedelta(hours=5)
    return candidate.replace(hour=candidate.hour // 6 * 6, minute=0, second=0, microsecond=0)


def fetch_hour(run, hour, box, work):
    """Download one forecast hour's GRIB2 subset. Returns a path or None."""
    south, west, north, east = box
    url = (
        "%s?file=gfs.t%02dz.pgrb2.0p25.f%03d"
        "&%s&%s&dir=%%2Fgfs.%s%%2F%02d%%2Fatmos"
        "&subregion=&leftlon=%s&rightlon=%s&toplat=%s&bottomlat=%s"
        % (
            FILTER,
            run.hour,
            hour,
            "&".join("%s=on" % level for level in LEVELS),
            "&".join("var_%s=on" % v for v in VARIABLES),
            run.strftime("%Y%m%d"),
            run.hour,
            west,
            east,
            north,
            south,
        )
    )

    path = os.path.join(work, "f%03d.grb2" % hour)
    request = urllib.request.Request(url, headers={"User-Agent": "rutter weather fetch"})
    try:
        with urllib.request.urlopen(request, timeout=90) as response:
            data = response.read()
    except (urllib.error.HTTPError, urllib.error.URLError) as e:
        print("  hour %+4d: %s" % (hour, e), file=sys.stderr)
        return None

    # An absent cycle answers with an HTML error page rather than a status,
    # so the magic is what actually decides.
    if not data.startswith(b"GRIB"):
        print("  hour %+4d: not GRIB2 (%d bytes)" % (hour, len(data)), file=sys.stderr)
        return None

    with open(path, "wb") as f:
        f.write(data)
    return path


def read_bands(path, work):
    """(geotransform, nx, ny, {element: [values]}) from one GRIB2 file.

    Through GDAL's command line rather than its Python bindings. The
    bindings are a separate package that has to match the library version,
    and `gdalinfo` plus `gdal_translate` are already here for the chart
    pipeline. One fewer thing that can be absent on someone else's machine.
    """
    info = json.loads(
        subprocess.run(
            ["gdalinfo", "-json", path],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )
    gt = info["geoTransform"]
    nx, ny = info["size"]

    # Which band holds which parameter. Band order is not promised by the
    # subsetter, so each is found by what it says it is — a u/v swap here
    # would be invisible until someone sailed by it.
    wanted = {}
    units = {}
    for band in info["bands"]:
        meta = band.get("metadata", {}).get("", {}) or {}
        element = meta.get("GRIB_ELEMENT", "")
        if element in VARIABLES and element not in wanted:
            wanted[element] = band["band"]
            # Read, never assumed. GDAL's GRIB driver converts temperature
            # to Celsius itself and says so in GRIB_UNIT — subtracting 273.15
            # on top of that gives -248 C, which is what the first version of
            # this did. A test asserting the temperature was a southern
            # California one caught it.
            units[element] = meta.get("GRIB_UNIT", "").strip("[]")

    found = {}
    nodes = nx * ny
    for element, index in wanted.items():
        # ENVI is a raw dump plus a text header: float32, row-major from the
        # top-left, which is exactly the layout wanted. No image format to
        # decode on this side.
        raw = os.path.join(work, "band%d.img" % index)
        subprocess.run(
            [
                "gdal_translate",
                "-q",
                "-b", str(index),
                "-ot", "Float32",
                "-of", "ENVI",
                path,
                raw,
            ],
            check=True,
            capture_output=True,
        )
        with open(raw, "rb") as f:
            data = f.read()
        if len(data) != nodes * 4:
            raise RuntimeError(
                "%s band %d: %d bytes for %d nodes" % (path, index, len(data), nodes)
            )
        found[element] = list(struct.unpack("<%df" % nodes, data))

    return gt, nx, ny, found, units


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--box", required=True, help="south,west,north,east")
    parser.add_argument(
        "--hours",
        default="0,3,6,9,12,15,18,21,24",
        help="forecast hours, comma separated",
    )
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    try:
        south, west, north, east = (float(v) for v in args.box.split(","))
    except ValueError:
        sys.exit("expected --box south,west,north,east, got %r" % args.box)
    if south >= north or west >= east:
        sys.exit("--box south,west must be below north,east")

    hours = [int(h) for h in args.hours.split(",") if h.strip()]
    run = latest_run(datetime.now(timezone.utc))
    print("GFS cycle %s, hours %s" % (run.strftime("%Y-%m-%d %HZ"), hours))

    steps = []
    geometry = None

    with tempfile.TemporaryDirectory() as work:
        for hour in hours:
            path = fetch_hour(run, hour, (south, west, north, east), work)
            if path is None:
                continue
            gt, nx, ny, bands, units = read_bands(path, work)
            missing = [v for v in VARIABLES if v not in bands]
            if missing:
                print("  hour %+4d: missing %s" % (hour, missing), file=sys.stderr)
                continue

            if geometry is None:
                geometry = (gt, nx, ny)
            elif (gt, nx, ny) != geometry:
                # Every hour must be the same grid, or the app would sample
                # one hour's wind at another hour's coordinates.
                print("  hour %+4d: grid differs, skipped" % hour, file=sys.stderr)
                continue

            steps.append((hour, bands, units))
            print("  hour %+4d: %dx%d" % (hour, nx, ny))

    if not steps or geometry is None:
        sys.exit("nothing was fetched — is the cycle published yet?")

    gt, nx, ny = geometry
    # GDAL's geotransform: origin is the *corner* of the first pixel, and
    # gt[5] is negative because rows run south. The app wants the same
    # layout with a positive spacing, so the sign is dropped here.
    north_edge, dlon, _, west_is, _, dlat = gt[3], gt[1], gt[2], gt[0], gt[4], gt[5]

    out = bytearray()
    out += MAGIC
    out += struct.pack("<HHHH", FORMAT_VERSION, nx, ny, len(steps))
    out += struct.pack("<q", int(run.timestamp()))
    out += struct.pack("<dddd", north_edge, west_is, abs(dlat), abs(dlon))

    for hour, bands, units in steps:
        out += struct.pack("<i", hour)
        for element in VARIABLES:
            values = bands[element]
            if element == "TMP" and units.get("TMP", "").upper().startswith("K"):
                # Celsius in the file, so that no reader has to remember.
                # Only when GDAL has not already done it — see read_bands.
                values = [v - KELVIN_OFFSET for v in values]
            out += struct.pack("<%df" % len(values), *values)

    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    with open(args.out, "wb") as f:
        f.write(out)

    print()
    print("  %s" % args.out)
    print("  %d x %d nodes, %d hours, %.1f KB" % (nx, ny, len(steps), len(out) / 1024.0))
    print("  area: %.3f..%.3f N, %.3f..%.3f E"
          % (north_edge + dlat * ny, north_edge, west_is, west_is + dlon * nx))


if __name__ == "__main__":
    main()
