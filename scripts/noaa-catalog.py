#!/usr/bin/env python3
"""Pick NOAA ENC cells out of a product catalogue.

Reads a NOAA ENC product catalogue (ISO 19115 XML) on stdin and prints the
names of cells matching the query, one per line.

    noaa-catalog.py near <lat> <lon>     cells whose coverage contains a point
    noaa-catalog.py box <s> <w> <n> <e>  cells overlapping a bounding box
    noaa-catalog.py list                 every cell, with scale and extent
    noaa-catalog.py urls <name>...       the download URL recorded for a cell

Catalogues:
    https://www.charts.noaa.gov/ENCs/ENCProdCat_19115.xml       all of them
    https://www.charts.noaa.gov/ENCs/12Region_ENCProdCat_19115.xml  one region
    https://www.charts.noaa.gov/ENCs/CA_ENCProdCat_19115.xml        one state

── What the catalogue actually looks like ──────────────────────────────────

Worth writing down, because the obvious guess is wrong. Coverage is *not*
`EX_GeographicBoundingBox`; it is a polygon:

    <extent><EX_Extent><geographicElement><EX_BoundingPolygon><polygon>
      <gml:Polygon gml:id="US1GLBCC_P1"><gml:exterior><gml:LinearRing>
        <gml:pos>15 -149.8837209</gml:pos>
        ...

and `gml:pos` is **latitude first**, which is the opposite of the lon/lat
order most geo tooling uses. Getting that backwards finds cells on the wrong
side of the planet rather than failing visibly.

The cell name is the citation title, in a different subtree from the extent:

    <citation><CI_Citation><title><gco:CharacterString>US5CA72M

Note also that the *per-cell* metadata files (`US5CA72M_19115.xml`) carry no
extent at all — only the catalogue does.

Element matching here ignores namespaces and compares local names, because
NOAA's prefixes have moved between schema revisions and a hard-coded URI is
a silent zero-results failure.
"""

import sys
import xml.etree.ElementTree as ET

# A cell name is eight characters: "US", a usage-band digit, then five more.
CELL_NAME_LENGTH = 8


def local(tag):
    """Element tag without its namespace."""
    return tag.rpartition("}")[2]


def find_all(node, name):
    """Every descendant with this local name, node included."""
    if local(node.tag) == name:
        yield node
    for child in node.iter():
        if child is not node and local(child.tag) == name:
            yield child


def first_text(node, name):
    """Text of the first descendant with this local name."""
    for found in find_all(node, name):
        if found.text and found.text.strip():
            return found.text.strip()
    return None


def looks_like_cell(name):
    return (
        name is not None
        and len(name) == CELL_NAME_LENGTH
        and name[:2].upper() == "US"
        and name.isalnum()
    )


def polygon_extent(node):
    """Bounding box of every gml:pos under this node, as (s, w, n, e).

    `gml:pos` is "latitude longitude". Returns None if there are no usable
    positions.
    """
    lats, lons = [], []
    for pos in find_all(node, "pos"):
        if not pos.text:
            continue
        parts = pos.text.split()
        if len(parts) < 2:
            continue
        try:
            lat, lon = float(parts[0]), float(parts[1])
        except ValueError:
            continue
        lats.append(lat)
        lons.append(lon)

    if not lats:
        return None
    return min(lats), min(lons), max(lats), max(lons)


def cells(root):
    """Yield one record per cell: (name, south, west, north, east, scale, url).

    Pairs each cell name with the nearest enclosing record that also holds an
    extent. Walking up from the name rather than down from a fixed path keeps
    this working across NOAA's schema revisions, and taking the *nearest*
    qualifying ancestor stops a cell inheriting its neighbour's coverage.
    """
    parents = {child: parent for parent in root.iter() for child in parent}

    for title in root.iter():
        if local(title.tag) != "title":
            continue
        name = first_text(title, "CharacterString")
        if not looks_like_cell(name):
            continue

        node = title
        for _ in range(12):
            node = parents.get(node)
            if node is None:
                break

            extent = polygon_extent(node)
            if extent is None:
                continue

            # Guard against climbing so far that we land on a container
            # holding many cells, which would give every one of them the
            # union of all their extents.
            names_here = sum(
                1
                for t in node.iter()
                if local(t.tag) == "title" and looks_like_cell(first_text(t, "CharacterString"))
            )
            if names_here != 1:
                break

            scale = first_text(node, "Integer")
            url = first_text(node, "URL")
            s, w, n, e = extent
            yield name.upper(), s, w, n, e, scale, url
            break


def main():
    argv = sys.argv[1:]
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    mode = argv[0]

    try:
        root = ET.parse(sys.stdin).getroot()
    except ET.ParseError as e:
        print(f"could not parse the catalogue: {e}", file=sys.stderr)
        return 1

    found = list(cells(root))
    if not found:
        print(
            "no cells found in the catalogue. NOAA may have changed its "
            "schema — see the structure notes in scripts/noaa-catalog.py",
            file=sys.stderr,
        )
        return 1

    if mode == "list":
        for name, s, w, n, e, scale, _ in sorted(found):
            band = f"1:{scale}" if scale else "?"
            print(f"{name}  {band:>12}  {s:.3f},{w:.3f} to {n:.3f},{e:.3f}")
        return 0

    if mode == "urls":
        wanted = {a.upper() for a in argv[1:]}
        for name, _s, _w, _n, _e, _scale, url in sorted(found):
            if name in wanted and url:
                print(url)
        return 0

    if mode == "near" and len(argv) == 3:
        lat, lon = float(argv[1]), float(argv[2])
        s0, w0, n0, e0 = lat, lon, lat, lon
    elif mode == "box" and len(argv) == 5:
        s0, w0, n0, e0 = (float(x) for x in argv[1:5])
        if s0 > n0:
            s0, n0 = n0, s0
        if w0 > e0:
            w0, e0 = e0, w0
    else:
        print(__doc__, file=sys.stderr)
        return 2

    matches = []
    for name, s, w, n, e, scale, _ in found:
        if n < s0 or s > n0 or e < w0 or w > e0:
            continue
        # Smallest denominator first: 1:12,000 is a harbour chart and more
        # detailed than a 1:3,500,000 overview. Someone asking for "charts
        # here" wants the detailed ones, and a missing scale sorts last
        # rather than pretending to be the most detailed.
        matches.append((int(scale) if scale and scale.isdigit() else 10**9, name))

    if not matches:
        print("no cells cover that area", file=sys.stderr)
        return 1

    seen = set()
    for _, name in sorted(matches):
        if name not in seen:
            seen.add(name)
            print(name)
    return 0


if __name__ == "__main__":
    sys.exit(main())
