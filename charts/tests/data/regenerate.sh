#!/usr/bin/env bash
#
# Rebuild coast.mbtiles from the per-layer GeoJSON sources in src/.
#
# Run from the repository root inside `nix-shell`:
#   bash charts/tests/data/regenerate.sh
#
# The sources are named after S-57 object classes — COALNE for coastline,
# DEPARE for depth areas, SOUNDG for soundings, LNDARE for land, BOYSPP for
# buoys — because the point of this fixture is to look like what a real ENC
# cell produces: several named layers, each with its own attributes. A
# single-layer fixture would let a layer-flattening bug pass unnoticed, which
# is exactly the bug this replaced.
#
# Zooms are pinned rather than left to tippecanoe's -zg, so the tests can
# assert exact tile addresses and a zoom range that does not drift when the
# geometry is tweaked.
set -euo pipefail

cd "$(dirname "$0")"

args=()
for f in src/*.geojson; do
  args+=( "--named-layer=$(basename "$f" .geojson):$f" )
done

tippecanoe -o coast.mbtiles -f -Z10 -z12 \
  --drop-densest-as-needed \
  --extend-zooms-if-still-dropping \
  --quiet \
  "${args[@]}"

# VACUUM so the committed file is as small as it can be; it is tracked, and
# SQLite leaves free pages behind otherwise.
sqlite3 coast.mbtiles "VACUUM;"

echo "rebuilt $(pwd)/coast.mbtiles"
ls -l coast.mbtiles
sqlite3 -noheader coast.mbtiles \
  "select '  ' || (je.value ->> 'id') from metadata, json_each(metadata.value -> 'vector_layers') je where metadata.name = 'json' order by 1;"
