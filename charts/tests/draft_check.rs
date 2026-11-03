//! That the real chart carries depths the under-keel check can use.
//!
//! The check is only as good as the attributes that survive the pipeline,
//! and there are three places they can be lost silently: the S-57 reader
//! can drop them, tippecanoe can drop them, and `value_attribute` can be
//! looking for the wrong spelling. In every one of those cases the feature
//! does not fail — it just finds nothing to judge, paints no warnings, and
//! looks exactly like a chart where everything is safe.
//!
//! That is the worst possible failure for this particular feature, so it
//! gets a test against a real chart rather than a fixture.
//!
//! Needs `build/charts/noaa.mbtiles`, which is not committed. Build it with:
//!
//!     rutter noaa --near 32.68,-117.23

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use rutter_charts::ChartSource;
use rutter_core::chart::ChartStyle;
use rutter_core::depth::{assess, Clearance, Draft, Tally, TideWindow};
use rutter_core::{LatLon, View};

fn chart_path() -> Option<PathBuf> {
    let path = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../build/charts/noaa.mbtiles"
    ));
    path.exists().then_some(path)
}

/// (style, features, features carrying a depth), in first-seen order.
fn count_by_style(
    features: impl Iterator<Item = (ChartStyle, Option<f64>)>,
) -> Vec<(ChartStyle, usize, usize)> {
    let mut out: Vec<(ChartStyle, usize, usize)> = Vec::new();
    for (style, value) in features {
        match out.iter_mut().find(|row| row.0 == style) {
            Some(row) => {
                row.1 += 1;
                row.2 += usize::from(value.is_some());
            }
            None => out.push((style, 1, usize::from(value.is_some()))),
        }
    }
    out
}

/// (features, features with a depth) for one style.
fn find(rows: &[(ChartStyle, usize, usize)], style: ChartStyle) -> Option<(usize, usize)> {
    rows.iter()
        .find(|row| row.0 == style)
        .map(|row| (row.1, row.2))
}

/// The San Diego Bay entrance, zoomed in enough to have real detail. This
/// is where the shallow water is, so it is where the check matters.
fn bay_entrance() -> View {
    View::new(LatLon::new(32.6836, -117.2206), 15.0, 1280, 800)
}

#[test]
fn depth_attributes_survive_the_whole_pipeline() {
    let Some(path) = chart_path() else {
        eprintln!("skipping: no build/charts/noaa.mbtiles — see the note above");
        return;
    };

    let mut source = ChartSource::open(&path, &[]).expect("open");
    let collected = source.collect(&bay_entrance());

    // Counted into a list rather than a map: `ChartStyle` is deliberately
    // not `Ord`, because `depth_order` is the order that means anything and
    // a derived one would quietly compete with it.
    let lines = count_by_style(collected.lines.iter().map(|l| (l.style, l.value)));
    let points = count_by_style(collected.points.iter().map(|p| (p.style, p.value)));

    for (style, total, valued) in &lines {
        println!("line  {style:?}: {total} features, {valued} with a depth");
    }
    for (style, total, valued) in &points {
        println!("point {style:?}: {total} features, {valued} with a depth");
    }

    // Soundings are the chart's own depth measurements. If these lose their
    // DEPTH attribute there is nothing left to check at all.
    let (total, valued) =
        find(&points, ChartStyle::Sounding).expect("the bay entrance has soundings");
    assert!(total > 500, "only {total} soundings");
    assert_eq!(
        valued,
        total,
        "{} of {total} soundings lost their depth",
        total - valued
    );

    // Depth areas are what make the check a picture rather than a scatter
    // of flagged dots, so DRVAL1 has to come through too.
    //
    // Almost all, not all. S-57 permits DRVAL1 to be absent — an area whose
    // depth was never surveyed — and a chart covering open water carries a
    // handful. This asserted 100% at first, which held while the chart was
    // one harbour and broke the moment it covered more sea. The ones with
    // no depth must go unjudged rather than guessed at, which
    // `every_judged_feature_had_a_depth` pins down.
    let (total, valued) =
        find(&lines, ChartStyle::DepthArea).expect("the bay entrance has depth areas");
    assert!(total > 100, "only {total} depth areas");
    assert!(
        valued * 100 >= total * 99,
        "{} of {total} depth areas have no DRVAL1, which is more than the \
         occasional unsurveyed one",
        total - valued
    );

    // Contours carry VALDCO, and those really are all or nothing: a contour
    // with no value is not a contour.
    let (total, valued) =
        find(&lines, ChartStyle::DepthContour).expect("the bay entrance has depth contours");
    assert_eq!(valued, total, "depth contours lost VALDCO");
}

#[test]
fn drying_heights_come_through_negative() {
    // S-57 records ground that uncovers as a negative DRVAL1 — above chart
    // datum. The clearance arithmetic depends on that sign surviving: if
    // something took an absolute value along the way, a drying bank would
    // read as a metre and a half of water.
    let Some(path) = chart_path() else {
        eprintln!("skipping: no chart");
        return;
    };

    let mut source = ChartSource::open(&path, &[]).expect("open");
    let collected = source.collect(&bay_entrance());

    let shallowest = collected
        .lines
        .iter()
        .filter(|l| l.style == ChartStyle::DepthArea)
        .filter_map(|l| l.value)
        .fold(f64::MAX, f64::min);

    assert!(
        shallowest < 0.0,
        "no drying depth area found; shallowest DRVAL1 is {shallowest} — \
         has the sign been lost?"
    );
    // Sanity: the bay does not dry to twenty metres.
    assert!(shallowest > -5.0, "shallowest DRVAL1 is {shallowest}");
}

#[test]
fn a_height_is_never_judged_as_a_depth() {
    // Lights carry HEIGHT, which is metres *above* datum. Running one
    // through the depth check would call a 26 m light tower deep water.
    // The renderer filters by style for exactly this reason; this test
    // pins the premise — that lights really do carry values.
    let Some(path) = chart_path() else {
        eprintln!("skipping: no chart");
        return;
    };

    let mut source = ChartSource::open(&path, &[]).expect("open");
    let collected = source.collect(&bay_entrance());

    let light_values: Vec<f64> = collected
        .points
        .iter()
        .filter(|p| p.style == ChartStyle::Light)
        .filter_map(|p| p.value)
        .collect();

    assert!(
        !light_values.is_empty(),
        "no lights with a height, so this test proves nothing"
    );
    // Heights are positive and can be tens of metres — numbers that would
    // look like gloriously deep water to a check that did not filter.
    assert!(
        light_values.iter().copied().fold(f64::MIN, f64::max) > 10.0,
        "expected a tall light among {light_values:?}"
    );
}

#[test]
fn todays_tide_produces_a_meaningful_verdict() {
    // End to end: real chart, real tide numbers, a real boat. The point is
    // not the exact counts — they move with the chart — but that all three
    // verdicts occur. A result that is all-clear or all-foul would mean the
    // thresholds or the units are wrong.
    let Some(path) = chart_path() else {
        eprintln!("skipping: no chart");
        return;
    };

    let mut source = ChartSource::open(&path, &[]).expect("open");
    let collected = source.collect(&bay_entrance());

    // San Diego, 2026-11-03: +0.21 m now, and the week's spring low at
    // -0.11 m. Hard-coded so the test does not depend on the clock.
    let tide = TideWindow {
        now_m: 0.209,
        lowest_m: -0.112,
    };
    let boat = Draft {
        draft_m: 1.5,
        safety_m: 0.5,
    };

    let mut tally = Tally::default();
    for line in &collected.lines {
        if matches!(line.style, ChartStyle::DepthArea | ChartStyle::DepthContour) {
            if let Some(charted) = line.value {
                tally.count(assess(&boat, charted, &tide));
            }
        }
    }
    for point in &collected.points {
        if matches!(point.style, ChartStyle::Sounding | ChartStyle::Danger) {
            if let Some(charted) = point.value {
                tally.count(assess(&boat, charted, &tide));
            }
        }
    }

    println!(
        "clear {} / tight at low {} / foul {} of {}",
        tally.clear,
        tally.tight_at_low,
        tally.foul,
        tally.total()
    );

    assert!(tally.total() > 1000, "only {} judged", tally.total());
    assert!(tally.clear > 0, "nothing clears — units or sign wrong?");
    assert!(
        tally.foul > 0,
        "nothing is foul in a harbour with 0.1 m soundings"
    );
    assert!(
        tally.tight_at_low > 0,
        "nothing goes dry within the window, which a 0.32 m tide fall \
         over a harbour this shallow should certainly do"
    );

    // And the specific case from the panel's own explanation: a 1.2 m
    // sounding does not take a 2.0 m boat, whatever the chart looks like.
    assert_eq!(assess(&boat, 1.2, &tide), Clearance::Foul);
}

#[test]
fn every_judged_feature_had_a_depth() {
    // The contract the renderer relies on: a feature with no charted depth
    // gets no verdict and no shading. Guessing a depth for an unsurveyed
    // area would be the worst failure available here — confident green over
    // water nobody has measured.
    let Some(path) = chart_path() else {
        eprintln!("skipping: no chart");
        return;
    };

    let mut source = ChartSource::open(&path, &[]).expect("open");
    let collected = source.collect(&bay_entrance());

    let tide = TideWindow {
        now_m: 0.209,
        lowest_m: -0.112,
    };
    let boat = Draft {
        draft_m: 1.5,
        safety_m: 0.5,
    };
    let judged_styles = [
        ChartStyle::Sounding,
        ChartStyle::Danger,
        ChartStyle::DepthArea,
        ChartStyle::DepthContour,
    ];

    let mut with_depth = 0usize;
    let mut without_depth = 0usize;
    let mut tally = Tally::default();

    for (style, value) in collected
        .lines
        .iter()
        .map(|l| (l.style, l.value))
        .chain(collected.points.iter().map(|p| (p.style, p.value)))
    {
        if !judged_styles.contains(&style) {
            continue;
        }
        match value {
            Some(charted) => {
                with_depth += 1;
                tally.count(assess(&boat, charted, &tide));
            }
            None => without_depth += 1,
        }
    }

    println!("{with_depth} judged, {without_depth} left alone for want of a depth");
    assert_eq!(
        tally.total(),
        with_depth,
        "the verdict count must match the number of features that had a depth"
    );
    assert!(
        without_depth > 0,
        "no unsurveyed features in view, so this proves nothing"
    );
}
