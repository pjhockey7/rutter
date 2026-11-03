//! Turning chart polygons into triangles, so areas can be filled.
//!
//! Everything in this project was drawn as outlines until now, because
//! filling needs this and this was not written. It is the piece that makes a
//! chart look like a chart — land buff, shallow water pale, deep water dark
//! — and the piece that lets the under-keel check shade a region instead of
//! reddening a scatter of sounding crosses.
//!
//! The algorithm is ear clipping with holes spliced into the exterior
//! (earcut). Chosen over the GPU alternative — stencil-invert a triangle fan
//! per ring and fill by parity — because it is pure arithmetic and so can be
//! tested without a screen, which is the rule this crate exists to keep. The
//! stencil version needs a depth-stencil attachment and one draw pair per
//! polygon, and could only be checked by looking at it.
//!
//! Chart polygons are not well-behaved. They arrive from tippecanoe after
//! simplification and tile clipping, which produces repeated vertices,
//! zero-area spurs and the occasional ring that touches itself. So the
//! failure modes matter as much as the algorithm:
//!
//! * A polygon that cannot be triangulated is **dropped entirely**, never
//!   partially filled. For a depth overlay a missing area reads as "no
//!   information"; an area filled over its own hole reads as "this deep
//!   basin is a shoal", which is worse than silence.
//! * A hole that cannot be bridged drops its whole polygon, for the same
//!   reason.

use crate::chart::WorldXY;

/// Twice the signed area of a ring, by the shoelace formula.
///
/// World coordinates run y-south, so a positive value here means clockwise
/// on screen. Only the sign and the magnitude relative to other rings
/// matter, so the factor of two is left in.
fn signed_area_x2(ring: &[WorldXY]) -> f64 {
    let mut sum = 0.0;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        sum += (a.x - b.x) * (a.y + b.y);
    }
    sum
}

/// Area of a ring, unsigned, in square world units.
pub fn ring_area(ring: &[WorldXY]) -> f64 {
    signed_area_x2(ring).abs() / 2.0
}

/// A ring with its closing duplicate removed and its winding normalised.
///
/// Ear clipping needs one consistent orientation, and chart rings cannot be
/// trusted to have it: MVT uses winding to mark holes, but a ring that
/// survived simplification may be degenerate enough to have lost it.
fn normalise(ring: &[WorldXY], want_positive: bool) -> Vec<WorldXY> {
    let mut out: Vec<WorldXY> = Vec::with_capacity(ring.len());
    for &p in ring {
        // Drop exact repeats, including the closing point. A repeated
        // vertex is a zero-length edge, and the ear test divides by edge
        // lengths' cross products.
        if out.last().is_some_and(|last: &WorldXY| *last == p) {
            continue;
        }
        out.push(p);
    }
    while out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    if (signed_area_x2(&out) > 0.0) != want_positive {
        out.reverse();
    }
    out
}

/// Twice the signed area of a triangle. Positive in the same sense as
/// [`signed_area_x2`].
fn triangle_area_x2(a: WorldXY, b: WorldXY, c: WorldXY) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

/// Whether `p` is inside or on the triangle `a, b, c`.
///
/// Inclusive of the boundary on purpose: a vertex lying exactly on an edge
/// of a candidate ear must block that ear, or the clip produces overlapping
/// triangles.
fn in_triangle(a: WorldXY, b: WorldXY, c: WorldXY, p: WorldXY) -> bool {
    let d1 = triangle_area_x2(a, b, p);
    let d2 = triangle_area_x2(b, c, p);
    let d3 = triangle_area_x2(c, a, p);
    let has_negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_negative && has_positive)
}

/// Whether two segments properly cross.
///
/// Shared endpoints do not count: a bridge from a hole to the exterior
/// necessarily touches both rings at its ends.
fn segments_cross(p1: WorldXY, p2: WorldXY, q1: WorldXY, q2: WorldXY) -> bool {
    if p1 == q1 || p1 == q2 || p2 == q1 || p2 == q2 {
        return false;
    }
    let d1 = triangle_area_x2(q1, q2, p1);
    let d2 = triangle_area_x2(q1, q2, p2);
    let d3 = triangle_area_x2(p1, p2, q1);
    let d4 = triangle_area_x2(p1, p2, q2);
    (d1 > 0.0) != (d2 > 0.0) && (d3 > 0.0) != (d4 > 0.0)
}

/// Splice every hole into the exterior ring, producing one simple polygon.
///
/// Each hole is joined by a "bridge": a segment from one of its vertices to
/// a visible vertex of the ring built so far, walked in both directions so
/// the ring stays closed. The result traverses the hole backwards, which is
/// what makes the interior come out on the correct side.
fn bridge_holes(exterior: &[WorldXY], holes: &[Vec<WorldXY>]) -> Option<Vec<WorldXY>> {
    let mut ring = exterior.to_vec();

    // Leftmost-first. A hole further left can always be bridged through
    // territory no later hole has consumed, which keeps the easy cases easy.
    let mut ordered: Vec<&Vec<WorldXY>> = holes.iter().collect();
    ordered.sort_by(|a, b| {
        let ax = a.iter().map(|p| p.x).fold(f64::MAX, f64::min);
        let bx = b.iter().map(|p| p.x).fold(f64::MAX, f64::min);
        ax.partial_cmp(&bx).unwrap_or(core::cmp::Ordering::Equal)
    });

    for hole in ordered {
        if hole.len() < 3 {
            // A degenerate hole encloses nothing, so ignoring it cannot
            // fill real water.
            continue;
        }
        let (ring_at, hole_at) = find_bridge(&ring, hole)?;

        // Walk the ring to the bridge point, cross into the hole, all the
        // way round it, back across the bridge, then finish the ring.
        let mut spliced = Vec::with_capacity(ring.len() + hole.len() + 2);
        spliced.extend_from_slice(&ring[..=ring_at]);
        for k in 0..=hole.len() {
            spliced.push(hole[(hole_at + k) % hole.len()]);
        }
        spliced.extend_from_slice(&ring[ring_at..]);
        ring = spliced;
    }

    Some(ring)
}

/// Pick a bridge between a ring and a hole: `(ring index, hole index)`.
///
/// Tried in order of how short the bridge is, because a short bridge is
/// least likely to cross anything. The first candidate that crosses no edge
/// of either ring wins. That is O(ring × hole) comparisons in the worst
/// case, which for chart polygons — a few hundred vertices against a few
/// dozen — is cheaper than it looks, and it happens once per tile load
/// rather than per frame.
fn find_bridge(ring: &[WorldXY], hole: &[WorldXY]) -> Option<(usize, usize)> {
    let mut candidates: Vec<(f64, usize, usize)> = Vec::new();
    for (hi, &h) in hole.iter().enumerate() {
        for (ri, &r) in ring.iter().enumerate() {
            let dx = r.x - h.x;
            let dy = r.y - h.y;
            candidates.push((dx * dx + dy * dy, ri, hi));
        }
    }
    candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));

    for (_, ri, hi) in candidates {
        if bridge_is_clear(ring, hole, ri, hi) {
            return Some((ri, hi));
        }
    }
    None
}

/// Whether the segment from `ring[ri]` to `hole[hi]` crosses either ring.
fn bridge_is_clear(ring: &[WorldXY], hole: &[WorldXY], ri: usize, hi: usize) -> bool {
    let a = ring[ri];
    let b = hole[hi];
    if a == b {
        return false;
    }
    let crosses = |poly: &[WorldXY]| {
        (0..poly.len()).any(|i| segments_cross(a, b, poly[i], poly[(i + 1) % poly.len()]))
    };
    !crosses(ring) && !crosses(hole)
}

/// Triangulate one polygon.
///
/// `exterior` is the outer ring and `holes` the inner ones, each closed or
/// not — the closing duplicate is removed either way. Returns the triangles,
/// or an empty vector if the polygon cannot be triangulated, which the
/// caller should treat as "draw no fill here".
pub fn triangulate(exterior: &[WorldXY], holes: &[Vec<WorldXY>]) -> Vec<[WorldXY; 3]> {
    let outer = normalise(exterior, true);
    if outer.len() < 3 {
        return Vec::new();
    }

    // Holes wound opposite to the exterior, so that traversing the spliced
    // ring keeps the interior consistently on one side.
    let inner: Vec<Vec<WorldXY>> = holes
        .iter()
        .map(|h| normalise(h, false))
        .filter(|h| h.len() >= 3)
        .collect();

    // What the triangles must add up to. A malformed polygon — holes
    // bigger than the exterior, or overlapping each other — gives a
    // negative target, and there is nothing sensible to draw for it.
    let expected = ring_area(&outer) - inner.iter().map(|h| ring_area(h)).sum::<f64>();
    if expected <= 0.0 {
        return Vec::new();
    }

    let Some(ring) = bridge_holes(&outer, &inner) else {
        // Refusing the whole polygon rather than filling over an
        // unbridgeable hole. See the module note on failure modes.
        return Vec::new();
    };

    let triangles = ear_clip(&ring);
    if triangles.is_empty() {
        return triangles;
    }

    // Check the work before handing it over.
    //
    // Area conservation is the whole correctness condition: triangles
    // summing to the polygon's own area can only be tiling it, while a
    // surplus means they overlap or have covered a hole and a deficit
    // means a gap. It is O(triangles) to verify and it turns the property
    // the tests assert into one the caller is actually given — which
    // matters because the splitting pass below can resolve a malformed
    // ring into something plausible-looking but wrong.
    let got: f64 = triangles
        .iter()
        .map(|t| triangle_area_x2(t[0], t[1], t[2]).abs() / 2.0)
        .sum();
    if (got - expected).abs() > expected * 1e-6 {
        return Vec::new();
    }

    triangles
}

/// How many times a stalled ring may be split before giving up.
///
/// Each split halves the problem, so eight levels covers rings far larger
/// than a chart tile holds. The bound exists to make non-termination
/// impossible rather than because deep recursion is expected.
const MAX_SPLIT_DEPTH: u32 = 8;

/// Ear clipping over an already-simple polygon.
///
/// All or nothing: a ring that cannot be fully tiled yields no triangles.
/// Keeping the ears clipped so far would leave a hole in the fill, and an
/// unshaded patch inside a shaded shoal reads as clear water — the one
/// direction this must not fail in.
fn ear_clip(ring: &[WorldXY]) -> Vec<[WorldXY; 3]> {
    let mut out = Vec::new();
    if clip_into(ring, &mut out, 0) {
        out
    } else {
        Vec::new()
    }
}

/// Find a diagonal that splits a stalled ring into two simpler ones.
///
/// Two cases, in order of how common they are on chart data:
///
/// 1. The ring touches itself — the same point appears twice, which tile
///    clipping produces freely. Splitting there separates two rings that
///    merely share a vertex, and each clips cleanly.
/// 2. Otherwise, any proper diagonal: a segment between two non-adjacent
///    vertices that crosses no edge and whose midpoint is inside.
fn find_split(ring: &[WorldXY]) -> Option<(usize, usize)> {
    let n = ring.len();

    // A pinch point. Checked first because it is both the likely cause and
    // the cheaper test.
    for i in 0..n {
        for j in (i + 2)..n {
            if i == 0 && j == n - 1 {
                continue; // adjacent round the back
            }
            if ring[i] == ring[j] {
                return Some((i, j));
            }
        }
    }

    for i in 0..n {
        for j in (i + 2)..n {
            if i == 0 && j == n - 1 {
                continue;
            }
            let (a, b) = (ring[i], ring[j]);
            if a == b {
                continue;
            }
            let crosses = (0..n).any(|k| segments_cross(a, b, ring[k], ring[(k + 1) % n]));
            if crosses {
                continue;
            }
            // Inside, not merely non-crossing: a diagonal across a concave
            // notch crosses nothing and still lies outside the polygon.
            let middle = WorldXY {
                x: (a.x + b.x) / 2.0,
                y: (a.y + b.y) / 2.0,
            };
            if point_in_ring(ring, middle) {
                return Some((i, j));
            }
        }
    }
    None
}

/// Ray casting: whether a point is inside a ring.
fn point_in_ring(ring: &[WorldXY], q: WorldXY) -> bool {
    let mut inside = false;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        if (a.y > q.y) != (b.y > q.y) {
            let t = (q.y - a.y) / (b.y - a.y);
            if a.x + t * (b.x - a.x) > q.x {
                inside = !inside;
            }
        }
    }
    inside
}

/// Triangulate one ring into `out`. Returns false if it could not be done.
fn clip_into(ring: &[WorldXY], out: &mut Vec<[WorldXY; 3]>, depth: u32) -> bool {
    if ring.len() < 3 {
        return true;
    }

    let mut remaining: Vec<usize> = (0..ring.len()).collect();
    let mut clipped: Vec<[WorldXY; 3]> = Vec::with_capacity(ring.len());

    // Each pass round the ring must clip at least one ear or the polygon is
    // not making progress. Counting failures since the last success is what
    // turns a potential infinite loop into a bounded give-up.
    let mut failures = 0usize;
    let mut cursor = 0usize;

    while remaining.len() > 3 {
        if failures > remaining.len() {
            // Stalled, which on chart data means the ring touches itself
            // somewhere. Split what is left at a diagonal and tile the two
            // halves independently; everything clipped so far is valid and
            // is kept only if both halves succeed.
            if depth >= MAX_SPLIT_DEPTH {
                return false;
            }
            let rest: Vec<WorldXY> = remaining.iter().map(|&i| ring[i]).collect();
            let Some((i, j)) = find_split(&rest) else {
                return false;
            };
            let first: Vec<WorldXY> = rest[i..=j].to_vec();
            let second: Vec<WorldXY> = rest[j..].iter().chain(rest[..=i].iter()).copied().collect();

            let mut halves = Vec::new();
            if !clip_into(&first, &mut halves, depth + 1)
                || !clip_into(&second, &mut halves, depth + 1)
            {
                return false;
            }
            out.extend(clipped);
            out.extend(halves);
            return true;
        }

        let n = remaining.len();
        let (ia, ib, ic) = (
            remaining[(cursor + n - 1) % n],
            remaining[cursor % n],
            remaining[(cursor + 1) % n],
        );
        let (a, b, c) = (ring[ia], ring[ib], ring[ic]);

        if is_ear(ring, &remaining, cursor, a, b, c) {
            clipped.push([a, b, c]);
            remaining.remove(cursor % n);
            if cursor >= remaining.len() {
                cursor = 0;
            }
            failures = 0;
        } else {
            cursor = (cursor + 1) % n;
            failures += 1;
        }
    }

    if remaining.len() == 3 {
        let t = [ring[remaining[0]], ring[remaining[1]], ring[remaining[2]]];
        // A final degenerate sliver contributes nothing and would be a
        // zero-area triangle in the buffer.
        if triangle_area_x2(t[0], t[1], t[2]).abs() > 0.0 {
            clipped.push(t);
        }
    }

    out.extend(clipped);
    true
}

/// Whether the vertex at `cursor` forms an ear: convex, and with no other
/// remaining vertex inside the triangle it would cut.
fn is_ear(
    ring: &[WorldXY],
    remaining: &[usize],
    cursor: usize,
    a: WorldXY,
    b: WorldXY,
    c: WorldXY,
) -> bool {
    // Reflex or collinear vertices are not ears. Collinear is excluded as
    // well as reflex: clipping a zero-area ear makes no progress but does
    // add a degenerate triangle.
    if triangle_area_x2(a, b, c) <= 0.0 {
        return false;
    }

    let n = remaining.len();
    for offset in 2..n - 1 {
        let index = remaining[(cursor + offset) % n];
        let p = ring[index];
        // The ear's own corners are allowed to coincide with other
        // vertices — chart rings touch themselves after simplification —
        // so only a *different* vertex falling inside disqualifies it.
        if p == a || p == b || p == c {
            continue;
        }
        if in_triangle(a, b, c, p) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> WorldXY {
        WorldXY { x, y }
    }

    fn total_area(triangles: &[[WorldXY; 3]]) -> f64 {
        triangles
            .iter()
            .map(|t| triangle_area_x2(t[0], t[1], t[2]).abs() / 2.0)
            .sum()
    }

    /// Whether a point is strictly inside the polygon, holes excluded.
    /// Ray casting, written here rather than in the module because only
    /// the tests need it.
    fn inside(exterior: &[WorldXY], holes: &[Vec<WorldXY>], q: WorldXY) -> bool {
        let crossings = |ring: &[WorldXY]| {
            let mut count = 0;
            for i in 0..ring.len() {
                let a = ring[i];
                let b = ring[(i + 1) % ring.len()];
                if (a.y > q.y) != (b.y > q.y) {
                    let t = (q.y - a.y) / (b.y - a.y);
                    if a.x + t * (b.x - a.x) > q.x {
                        count += 1;
                    }
                }
            }
            count
        };
        crossings(exterior) % 2 == 1 && holes.iter().all(|h| crossings(h) % 2 == 0)
    }

    /// Every triangle must lie inside the polygon and none may overlap.
    /// Overlap is checked by area: the triangles' areas summing to the
    /// polygon's is only possible if they tile it.
    fn check(exterior: &[WorldXY], holes: &[Vec<WorldXY>], triangles: &[[WorldXY; 3]]) {
        assert!(!triangles.is_empty(), "no triangles produced");

        let expected = ring_area(exterior) - holes.iter().map(|h| ring_area(h)).sum::<f64>();
        let got = total_area(triangles);
        assert!(
            (got - expected).abs() < expected * 1e-9 + 1e-18,
            "area {got} should be {expected}"
        );

        for t in triangles {
            assert!(
                triangle_area_x2(t[0], t[1], t[2]).abs() > 0.0,
                "degenerate triangle {t:?}"
            );
            let centroid = p(
                (t[0].x + t[1].x + t[2].x) / 3.0,
                (t[0].y + t[1].y + t[2].y) / 3.0,
            );
            assert!(
                inside(exterior, holes, centroid),
                "triangle {t:?} is outside the polygon"
            );
        }
    }

    #[test]
    fn a_square_becomes_two_triangles() {
        let square = vec![p(0.0, 0.0), p(1.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)];
        let t = triangulate(&square, &[]);
        assert_eq!(t.len(), 2);
        check(&square, &[], &t);
    }

    #[test]
    fn winding_does_not_matter() {
        // MVT marks holes by winding, but a simplified ring may have lost
        // it. Both directions must give the same area.
        let cw = vec![p(0.0, 0.0), p(1.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)];
        let mut ccw = cw.clone();
        ccw.reverse();
        assert!((total_area(&triangulate(&cw, &[])) - 1.0).abs() < 1e-12);
        assert!((total_area(&triangulate(&ccw, &[])) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_closed_ring_is_the_same_as_an_open_one() {
        let open = vec![p(0.0, 0.0), p(1.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)];
        let mut closed = open.clone();
        closed.push(open[0]);
        assert_eq!(
            triangulate(&open, &[]).len(),
            triangulate(&closed, &[]).len()
        );
    }

    #[test]
    fn a_concave_polygon_is_tiled_without_spilling_outside() {
        // An L. A naive fan from vertex zero would cover the notch, which
        // the centroid check in `check` catches.
        let l = vec![
            p(0.0, 0.0),
            p(2.0, 0.0),
            p(2.0, 1.0),
            p(1.0, 1.0),
            p(1.0, 2.0),
            p(0.0, 2.0),
        ];
        let t = triangulate(&l, &[]);
        assert_eq!(t.len(), 4, "an L has six vertices, so four ears");
        check(&l, &[], &t);
    }

    #[test]
    fn a_deeply_concave_comb_is_handled() {
        // Many reflex vertices in a row, which is where a careless ear test
        // starts clipping across the gaps.
        let mut ring = vec![p(0.0, 0.0)];
        for i in 0..6 {
            let x = f64::from(i) * 2.0;
            ring.push(p(x + 1.0, 0.0));
            ring.push(p(x + 1.0, 3.0));
            ring.push(p(x + 2.0, 3.0));
            ring.push(p(x + 2.0, 0.0));
        }
        ring.push(p(13.0, 0.0));
        ring.push(p(13.0, -1.0));
        ring.push(p(0.0, -1.0));
        let t = triangulate(&ring, &[]);
        check(&ring, &[], &t);
    }

    #[test]
    fn a_square_hole_is_cut_out() {
        // The case that matters on a chart: a deep basin inside a shoal.
        let outer = vec![p(0.0, 0.0), p(4.0, 0.0), p(4.0, 4.0), p(0.0, 4.0)];
        let hole = vec![p(1.0, 1.0), p(1.0, 2.0), p(2.0, 2.0), p(2.0, 1.0)];
        let holes = vec![hole];
        let t = triangulate(&outer, &holes);
        check(&outer, &holes, &t);
        // 16 outside minus 1 inside.
        assert!((total_area(&t) - 15.0).abs() < 1e-9);
    }

    #[test]
    fn several_holes_are_all_cut_out() {
        // Real depth areas carry up to six.
        let outer = vec![p(0.0, 0.0), p(10.0, 0.0), p(10.0, 10.0), p(0.0, 10.0)];
        let mut holes = Vec::new();
        for i in 0..6 {
            let x = 1.0 + f64::from(i) * 1.5;
            holes.push(vec![p(x, 4.0), p(x, 5.0), p(x + 1.0, 5.0), p(x + 1.0, 4.0)]);
        }
        let t = triangulate(&outer, &holes);
        check(&outer, &holes, &t);
        assert!((total_area(&t) - (100.0 - 6.0)).abs() < 1e-9);
    }

    #[test]
    fn a_hole_touching_the_outer_ring_does_not_produce_overlap() {
        // Tile clipping makes these: a hole whose vertex sits exactly on
        // the boundary.
        let outer = vec![p(0.0, 0.0), p(4.0, 0.0), p(4.0, 4.0), p(0.0, 4.0)];
        let hole = vec![p(0.0, 1.0), p(1.0, 2.0), p(1.0, 1.0)];
        let holes = vec![hole];
        let t = triangulate(&outer, &holes);
        if !t.is_empty() {
            check(&outer, &holes, &t);
        }
        // Dropping it is acceptable; filling over it is not.
        let expected = 16.0 - 0.5;
        if !t.is_empty() {
            assert!((total_area(&t) - expected).abs() < 1e-9);
        }
    }

    #[test]
    fn repeated_vertices_are_tolerated() {
        // Simplification emits these freely.
        let ring = vec![
            p(0.0, 0.0),
            p(0.0, 0.0),
            p(1.0, 0.0),
            p(1.0, 0.0),
            p(1.0, 1.0),
            p(0.0, 1.0),
            p(0.0, 1.0),
        ];
        let t = triangulate(&ring, &[]);
        assert!((total_area(&t) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn collinear_runs_do_not_make_degenerate_triangles() {
        let ring = vec![
            p(0.0, 0.0),
            p(0.5, 0.0),
            p(1.0, 0.0),
            p(1.0, 1.0),
            p(0.5, 1.0),
            p(0.0, 1.0),
        ];
        let t = triangulate(&ring, &[]);
        assert!((total_area(&t) - 1.0).abs() < 1e-12);
        for tri in &t {
            assert!(triangle_area_x2(tri[0], tri[1], tri[2]).abs() > 0.0);
        }
    }

    #[test]
    fn degenerate_input_produces_nothing_rather_than_garbage() {
        assert!(triangulate(&[], &[]).is_empty());
        assert!(triangulate(&[p(0.0, 0.0)], &[]).is_empty());
        assert!(triangulate(&[p(0.0, 0.0), p(1.0, 1.0)], &[]).is_empty());
        // Three collinear points enclose nothing.
        assert!(triangulate(&[p(0.0, 0.0), p(1.0, 0.0), p(2.0, 0.0)], &[]).is_empty());
        // All the same point.
        assert!(triangulate(&[p(1.0, 1.0), p(1.0, 1.0), p(1.0, 1.0)], &[]).is_empty());
    }

    #[test]
    fn a_hole_bigger_than_its_polygon_is_refused_not_inverted() {
        // Malformed, but it must not come out as negative area or as a fill
        // over the whole tile.
        let outer = vec![p(1.0, 1.0), p(2.0, 1.0), p(2.0, 2.0), p(1.0, 2.0)];
        let holes = vec![vec![p(0.0, 0.0), p(3.0, 0.0), p(3.0, 3.0), p(0.0, 3.0)]];
        let t = triangulate(&outer, &holes);
        // Either nothing, or at most the outer ring's own area.
        assert!(total_area(&t) <= ring_area(&outer) + 1e-12);
    }

    #[test]
    fn a_many_vertex_ring_terminates_and_tiles() {
        // Real exterior rings run to 520 points. This is a circle, which is
        // all-convex and so the easy case for termination, but it checks the
        // loop bookkeeping at a realistic size.
        let n = 520;
        let ring: Vec<WorldXY> = (0..n)
            .map(|i| {
                let a = f64::from(i) / f64::from(n) * core::f64::consts::TAU;
                p(a.cos(), a.sin())
            })
            .collect();
        let t = triangulate(&ring, &[]);
        assert_eq!(t.len(), n as usize - 2);
        check(&ring, &[], &t);
    }

    #[test]
    fn a_star_terminates_and_tiles() {
        // Alternating convex and reflex at every vertex — the shape that
        // breaks a fan and exercises the stall counter.
        let points = 12;
        let ring: Vec<WorldXY> = (0..points * 2)
            .map(|i| {
                let a = f64::from(i) / f64::from(points * 2) * core::f64::consts::TAU;
                let r = if i % 2 == 0 { 1.0 } else { 0.4 };
                p(r * a.cos(), r * a.sin())
            })
            .collect();
        let t = triangulate(&ring, &[]);
        check(&ring, &[], &t);
    }

    #[test]
    fn world_scale_coordinates_keep_their_precision() {
        // World coordinates are 0..1 across the planet, so a harbour-sized
        // polygon has coordinates around 0.17 differing in the seventh
        // decimal. The area tolerance has to hold there, not just on a
        // unit square.
        let base = p(0.174387, 0.371_55);
        let d = 1.0e-6;
        let ring = vec![
            base,
            p(base.x + d, base.y),
            p(base.x + d, base.y + d),
            p(base.x, base.y + d),
        ];
        let t = triangulate(&ring, &[]);
        assert_eq!(t.len(), 2);
        check(&ring, &[], &t);
    }
}
