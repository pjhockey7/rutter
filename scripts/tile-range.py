#!/usr/bin/env python3
"""Enumerate slippy-map tiles covering a geographic box.

    tile-range.py <south> <west> <north> <east> <zoom>

Prints one `z x y row` line per tile, where `x`/`y` are slippy-map
coordinates and `row` is the TMS row MBTiles stores (`2^z - 1 - y`). Having
both on the line keeps the caller from repeating the flip, which is the
single most common way to get a tile store subtly wrong.

    tile-range.py --count <south> <west> <north> <east> <zoom>

Prints just the tile count, for deciding whether a download is reasonable
before starting it.

The projection is the same Web Mercator as `rutter-core`'s chart module —
deliberately duplicated here rather than shelled out to, because this runs
before anything is built.
"""

import math
import sys

# Web Mercator is truncated here; see rutter_core::chart::MAX_LATITUDE.
MAX_LATITUDE = 85.051_128_779_806_59


def project(lat, lon):
    """Degrees to normalised world coordinates, 0..1, north-west origin."""
    lat = max(-MAX_LATITUDE, min(MAX_LATITUDE, lat))
    x = (lon + 180.0) / 360.0
    rad = math.radians(lat)
    y = 0.5 - math.log(math.tan(math.pi / 4.0 + rad / 2.0)) / (2.0 * math.pi)
    return x, y


def tile_of(lat, lon, zoom):
    n = 1 << zoom
    x, y = project(lat, lon)
    return (
        max(0, min(n - 1, int(x * n))),
        max(0, min(n - 1, int(y * n))),
    )


def tile_range(south, west, north, east, zoom):
    """Inclusive (x0, y0, x1, y1) covering the box."""
    if south > north:
        south, north = north, south
    if west > east:
        west, east = east, west

    # North edge gives the smaller y, since y grows southward.
    x0, y0 = tile_of(north, west, zoom)
    x1, y1 = tile_of(south, east, zoom)
    return min(x0, x1), min(y0, y1), max(x0, x1), max(y0, y1)


def main():
    argv = sys.argv[1:]
    count_only = False
    if argv and argv[0] == "--count":
        count_only = True
        argv = argv[1:]

    if len(argv) != 5:
        print(__doc__, file=sys.stderr)
        return 2

    try:
        south, west, north, east = (float(v) for v in argv[:4])
        zoom = int(argv[4])
    except ValueError as e:
        print(f"bad arguments: {e}", file=sys.stderr)
        return 2

    if not 0 <= zoom <= 24:
        print(f"zoom {zoom} is outside 0..24", file=sys.stderr)
        return 2

    x0, y0, x1, y1 = tile_range(south, west, north, east, zoom)
    total = (x1 - x0 + 1) * (y1 - y0 + 1)

    if count_only:
        print(total)
        return 0

    n = 1 << zoom
    for y in range(y0, y1 + 1):
        for x in range(x0, x1 + 1):
            # TMS row, which is what MBTiles stores.
            print(f"{zoom} {x} {y} {n - 1 - y}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
