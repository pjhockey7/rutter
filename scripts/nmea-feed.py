#!/usr/bin/env python3
"""Send a synthetic NMEA 0183 feed, so the GPS path is testable on land.

A boat with no GPS aboard is the usual state of this project's development
machine, and "does the marker appear in the right place" is not a question
a unit test can answer — it needs a feed, a chart and a look. So this is a
boat: it leaves a position, steers a course, and sends the sentences a real
receiver would, at the rate a real receiver would.

    python3 scripts/nmea-feed.py                       # UDP to localhost
    python3 scripts/nmea-feed.py --tcp                 # listen; app connects
    python3 scripts/nmea-feed.py --pty                 # a fake serial dongle
    python3 scripts/nmea-feed.py --from 32.68,-117.23 --course 210 --speed 6

What it sends, once a second:

    GPRMC   position, speed, course, date — the one sentence that carries
            everything needed for a fix, and the one most receivers lead with
    GPGGA   position again, with fix quality and satellite count
    GPVTG   course and speed over ground
    SDDBT   depth below the transducer
    WIMWV   apparent wind at the masthead
    HCHDG   compass heading, with magnetic variation

Deliberately *not* a perfect feed. One sentence in forty is corrupted, with
its checksum left behind, because a plotter that only works on a clean feed
does not work: a real RS-422 run through a bilge drops bytes, and the
failure to catch that is a position jumping a hundred miles. The app's
refusal count should climb slowly and the boat should never jump.
"""

import argparse
import math
import os
import pty
import random
import socket
import sys
import time
from datetime import datetime, timezone

# One nautical mile in degrees of latitude. A minute of latitude, by
# definition — which is the whole reason the unit exists.
DEGREES_PER_NM = 1.0 / 60.0

# How often a sentence is mangled on purpose. See the module docstring:
# the point is that the app's checksum test gets exercised by every run.
CORRUPT_ONE_IN = 40


def checksum(body):
    """The XOR fold NMEA 0183 puts after the asterisk."""
    value = 0
    for byte in body.encode("ascii"):
        value ^= byte
    return "%02X" % value


def sentence(body, corrupt=False):
    """Wrap a sentence body in its `$`, checksum and line ending.

    `corrupt` alters a character *after* the checksum is computed, which is
    exactly what a dropped bit on the wire does: the sentence still looks
    well-formed and the checksum no longer matches it.
    """
    stated = checksum(body)
    if corrupt:
        digits = "0123456789"
        original = next((c for c in body if c in digits), None)
        if original is not None:
            # A *different* digit, always. Picking at random would leave
            # one corruption in ten identical to the original and so still
            # valid — which would make a test for "nothing is believed"
            # pass nine times out of ten and fail on the tenth run.
            replacement = random.choice([d for d in digits if d != original])
            body = body.replace(original, replacement, 1)
    return "$%s*%s\r\n" % (body, stated)


def degrees_minutes(value, positive, negative):
    """`ddmm.mmmm,N` — the form NMEA carries a position in.

    Degrees and minutes, not decimal degrees. Reading these as decimal
    degrees puts 4807.038 at 48.07 instead of 48.12 — thirteen miles out,
    and plausible enough to go unnoticed.
    """
    hemisphere = positive if value >= 0 else negative
    magnitude = abs(value)
    degrees = int(magnitude)
    minutes = (magnitude - degrees) * 60.0
    return "%d%06.3f,%s" % (degrees, minutes, hemisphere)


def cycle(lat, lon, course, speed, depth, wind_angle, wind_speed, heading, variation):
    """One second's worth of sentences, as a receiver would send them."""
    now = datetime.now(timezone.utc)
    hhmmss = now.strftime("%H%M%S.00")
    ddmmyy = now.strftime("%d%m%y")
    position = "%s,%s" % (
        degrees_minutes(lat, "N", "S"),
        degrees_minutes(lon, "E", "W"),
    )
    var = "%05.1f,%s" % (abs(variation), "E" if variation >= 0 else "W")

    return [
        "GPRMC,%s,A,%s,%05.1f,%05.1f,%s,%s"
        % (hhmmss, position, speed, course, ddmmyy, var),
        "GPGGA,%s,%s,2,11,0.9,12.5,M,-33.9,M,," % (hhmmss, position),
        "GPVTG,%05.1f,T,%05.1f,M,%05.1f,N,%05.1f,K"
        % (course, (course - variation) % 360.0, speed, speed * 1.852),
        "SDDBT,%.1f,f,%.1f,M,%.1f,F" % (depth / 0.3048, depth, depth / 1.8288),
        "WIMWV,%05.1f,R,%04.1f,N,A" % (wind_angle, wind_speed),
        "HCHDG,%05.1f,,,%s" % (heading, var),
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--from",
        dest="start",
        default="32.6800,-117.2300",
        help="where to start, as lat,lon (default: San Diego Bay entrance)",
    )
    parser.add_argument("--course", type=float, default=210.0, help="degrees true")
    parser.add_argument("--speed", type=float, default=5.5, help="knots")
    parser.add_argument("--depth", type=float, default=11.0, help="metres")
    parser.add_argument(
        "--variation",
        type=float,
        default=11.5,
        help="magnetic variation, east positive (San Diego is about 11.5E)",
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=10110)
    parser.add_argument(
        "--tcp",
        action="store_true",
        help="listen for a TCP connection instead of sending datagrams",
    )
    parser.add_argument(
        "--pty",
        action="store_true",
        help="pretend to be a serial GPS; prints the device path to use",
    )
    parser.add_argument(
        "--clean",
        action="store_true",
        help="never corrupt a sentence (the default corrupts one in %d)"
        % CORRUPT_ONE_IN,
    )
    parser.add_argument(
        "--corrupt-every",
        type=int,
        default=CORRUPT_ONE_IN,
        metavar="N",
        help="corrupt one sentence in N; 1 corrupts every one, 0 none",
    )
    parser.add_argument(
        "--still",
        action="store_true",
        help="stay put, to check the symbol does not spin on a mooring",
    )
    args = parser.parse_args()

    try:
        lat, lon = (float(v) for v in args.start.split(","))
    except ValueError:
        sys.exit("expected --from lat,lon, got %r" % args.start)

    corrupt_every = 0 if args.clean else max(args.corrupt_every, 0)
    speed = 0.0 if args.still else args.speed
    course = args.course
    sent = 0
    corrupted = 0

    if args.pty:
        # A pseudo-terminal is a serial port as far as termios is
        # concerned, so this exercises the whole dongle path — open,
        # configure, read, resynchronise — with nothing plugged in. The
        # baud rate is the one thing it cannot test: a pty ignores it.
        master, slave = pty.openpty()
        device = os.ttyname(slave)
        print("pretending to be a serial GPS on %s" % device)
        print("  rutter run --nmea %s" % device)
        send = lambda data: os.write(master, data)
    elif args.tcp:
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind((args.host, args.port))
        listener.listen(1)
        print("listening on tcp://%s:%d — start rutter with" % (args.host, args.port))
        print("  rutter run --nmea tcp://%s:%d" % (args.host, args.port))
        stream, peer = listener.accept()
        print("connected: %s:%d" % peer)
        send = stream.sendall
    else:
        datagrams = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        datagrams.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        print("sending to udp://%s:%d — start rutter with" % (args.host, args.port))
        print("  rutter run --nmea udp://:%d" % args.port)
        send = lambda data: datagrams.sendto(data, (args.host, args.port))

    print("ctrl-c to stop")
    try:
        while True:
            # A slow weave either side of the course, so the symbol's
            # heading visibly tracks something rather than sitting still.
            weave = 6.0 * math.sin(sent / 25.0)
            steered = (course + weave) % 360.0
            # Heading differs from course over ground by the set of the
            # tide: a boat crabbing a few degrees is the normal case, and
            # the one that proves the two numbers are not the same number.
            heading = (steered - 4.0) % 360.0

            lines = cycle(
                lat,
                lon,
                steered,
                speed,
                args.depth + 2.0 * math.sin(sent / 17.0),
                (45.0 + weave * 2) % 360.0,
                12.0 + 3.0 * math.sin(sent / 11.0),
                heading,
                args.variation,
            )

            payload = ""
            for body in lines:
                bad = corrupt_every > 0 and random.randrange(corrupt_every) == 0
                corrupted += bad
                payload += sentence(body, corrupt=bad)
            send(payload.encode("ascii"))
            sent += 1

            # Dead reckoning, a second at a time. Good enough for a test
            # feed: over a few minutes the difference from a great circle
            # is centimetres.
            distance_nm = speed / 3600.0
            radians = math.radians(steered)
            lat += distance_nm * math.cos(radians) * DEGREES_PER_NM
            lon += (
                distance_nm
                * math.sin(radians)
                * DEGREES_PER_NM
                / max(math.cos(math.radians(lat)), 1e-6)
            )

            if sent % 10 == 0:
                print(
                    "  %6d cycles  %s  %05.1f°  %.1f kn  (%d corrupted)"
                    % (
                        sent,
                        "%s %s"
                        % (
                            degrees_minutes(lat, "N", "S"),
                            degrees_minutes(lon, "E", "W"),
                        ),
                        steered,
                        speed,
                        corrupted,
                    )
                )
            time.sleep(1.0)
    except KeyboardInterrupt:
        print()
        print("%d cycles sent, %d sentences corrupted on purpose" % (sent, corrupted))
    except (BrokenPipeError, ConnectionResetError, OSError) as e:
        print()
        print("the feed ended after %d cycles: %s" % (sent, e))


if __name__ == "__main__":
    main()
