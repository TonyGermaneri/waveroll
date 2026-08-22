//! Quantised selection: the core interaction, and the part most likely to feel wrong if it is
//! specified loosely.
//!
//! Everything here works in **musical time** and converts to frames only on the way out. A cell is
//! a different number of samples on either side of a tempo change, so snapping in frames would put
//! the boundaries in the wrong place for exactly the material where it matters most.
//!
//! The unit ladder is in fractions of a **bar**. It extends below the specified 1/16 for detail
//! work and above one bar because loops are two and four bars far more often than one.

use crate::tempo::TempoMap;

/// Selectable grid units, in bars, coarsest last.
pub const LADDER: [f64; 8] = [
    1.0 / 32.0,
    1.0 / 16.0,
    1.0 / 8.0,
    1.0 / 4.0,
    1.0 / 2.0,
    1.0,
    2.0,
    4.0,
];

/// The on-screen cell width `Unit::Auto` aims for, in CSS pixels.
///
/// One number, deliberately: it is the single knob that decides how fine the default grid feels,
/// and burying that decision in a formula would make it unfindable. At the 16-bar default across a
/// full-screen canvas this lands on one bar.
pub const AUTO_TARGET_PX: f64 = 80.0;

/// What the quantise control is set to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unit {
    /// Derived from the current zoom, re-evaluated whenever the view changes.
    Auto,
    /// A rung of [`LADDER`], in bars.
    Fixed(f64),
}

impl Unit {
    /// Resolves to a concrete number of bars.
    ///
    /// `bars_visible` and `width_px` describe the current view; they are ignored when the setting
    /// is fixed, which is why the caller can pass the same pair unconditionally.
    pub fn bars(self, bars_visible: f64, width_px: f64) -> f64 {
        match self {
            Unit::Auto => auto_unit(bars_visible, width_px, AUTO_TARGET_PX),
            Unit::Fixed(bars) => nearest_rung(bars),
        }
    }
}

/// The rung whose on-screen cell width is closest to `target_px`, measured in `log2` so that
/// "twice too wide" and "half too wide" are the same distance from ideal. A linear comparison
/// would bias every choice towards the coarser rung.
pub fn auto_unit(bars_visible: f64, width_px: f64, target_px: f64) -> f64 {
    let sane = |v: f64| v.is_finite() && v > 0.0;
    if !(sane(bars_visible) && sane(width_px) && sane(target_px)) {
        return 1.0;
    }
    let px_per_bar = width_px / bars_visible;
    let mut best = LADDER[0];
    let mut best_error = f64::INFINITY;
    for rung in LADDER {
        let error = ((rung * px_per_bar) / target_px).log2().abs();
        if error < best_error {
            best_error = error;
            best = rung;
        }
    }
    best
}

/// Snaps an arbitrary bar figure to the nearest rung of the ladder, so a unit that arrives from
/// persisted settings or a MIDI binding cannot be off-ladder.
pub fn nearest_rung(bars: f64) -> f64 {
    if !bars.is_finite() || bars <= 0.0 {
        return 1.0;
    }
    let mut best = LADDER[0];
    let mut best_error = f64::INFINITY;
    for rung in LADDER {
        let error = (rung / bars).log2().abs();
        if error < best_error {
            best_error = error;
            best = rung;
        }
    }
    best
}

/// A selected span of capture time, as absolute frame indices, half-open.
///
/// Frames rather than screen coordinates, so a selection survives the write head sweeping through
/// it and can be asked — of the ring, not of itself — whether it still exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub start: u64,
    pub end: u64,
}

impl Selection {
    pub fn frames(&self) -> u64 { self.end - self.start }
    pub fn is_empty(&self) -> bool { self.end <= self.start }
}

/// A click with no drag: the one cell containing `frame`.
///
/// `[floor(t/u)·u, +u)` — the pointer is always inside the result, which is the property that makes
/// a single click feel like it selected the thing under it rather than the thing beside it.
pub fn cell_at(map: &TempoMap, unit_bars: f64, frame: u64) -> Selection {
    let unit = guard_unit(unit_bars);
    let bars = map.bars_at(frame);
    let index = (bars / unit).floor();
    Selection {
        start: map.frame_at_bars(index * unit),
        end: map.frame_at_bars((index + 1.0) * unit),
    }
}

/// A drag: start snaps down, end snaps up, minimum one cell.
///
/// The endpoints are sorted first, so dragging right-to-left produces the same span as dragging
/// left-to-right rather than an inverted one.
pub fn snap_range(map: &TempoMap, unit_bars: f64, a: u64, b: u64) -> Selection {
    let unit = guard_unit(unit_bars);
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let lo_bars = map.bars_at(lo);
    let hi_bars = map.bars_at(hi);
    let first = (lo_bars / unit).floor();
    let mut last = (hi_bars / unit).ceil();
    // A drag that never left one cell, and a drag whose endpoints landed exactly on the same grid
    // line, both arrive here as a zero-width span. Neither can be what was meant.
    if last <= first {
        last = first + 1.0;
    }
    Selection {
        start: map.frame_at_bars(first * unit),
        end: map.frame_at_bars(last * unit),
    }
}

/// Like [`snap_range`], but never reaching past `limit`.
///
/// Snapping the end *up* is right for a drag — you meant the cell your pointer was in — but at the
/// write head it selects a cell the audio has not reached yet, which cannot be exported. Where the
/// intent is "the material between here and now", the end walks back to the last complete cell
/// instead. Returns `None` when not even one whole cell has finished, which is a real answer:
/// there is nothing there to take yet.
pub fn snap_range_upto(
    map: &TempoMap,
    unit_bars: f64,
    a: u64,
    b: u64,
    limit: u64,
) -> Option<Selection> {
    let unit = guard_unit(unit_bars);
    let lo = a.min(b);
    let hi = a.max(b).min(limit);
    let first = (map.bars_at(lo) / unit).floor();
    let last = (map.bars_at(hi) / unit).floor();
    if last <= first {
        return None;
    }
    Some(Selection {
        start: map.frame_at_bars(first * unit),
        end: map.frame_at_bars(last * unit),
    })
}

/// The number row: the most recent `tenths`/10 of the window, ending at the write head.
///
/// Head to tail: the most recent `tenths`/10 of the window, as a whole number of cells.
///
/// **Both ends snap.** An earlier version ended exactly at the write head, on the reasoning that
/// the head is a fact rather than a preference. That is true and it produced 3.27-bar files, which
/// will not loop — and loops are the entire output of this tool. Ending at the last *complete*
/// cell gives up at most a fraction of a bar of the newest audio and always yields something that
/// can be dropped on a bar line. Hold exists for when that fraction matters.
///
/// `tenths` is 1..=10, with the `0` key meaning ten. Returns `None` when not one whole cell has
/// been captured yet.
pub fn percent_from_head(
    map: &TempoMap,
    unit_bars: f64,
    head: u64,
    window_bars: f64,
    tenths: u32,
) -> Option<Selection> {
    let unit = guard_unit(unit_bars);
    let tenths = tenths.clamp(1, 10) as f64;
    let head_bars = map.bars_at(head);
    let window_start = (head_bars - window_bars).max(0.0);
    let wanted = window_bars * tenths / 10.0;

    let end_bars = (head_bars / unit).floor() * unit;
    // How many whole cells the request comes to, at least one.
    let cells = (wanted / unit).round().max(1.0);
    let start_bars = (end_bars - cells * unit).max(window_start);
    // Re-align the start after clamping, so a request that ran off the front of the window is
    // still a whole number of cells rather than a ragged one.
    let start_bars = (start_bars / unit).ceil() * unit;
    if end_bars <= start_bars {
        return None;
    }
    Some(Selection {
        start: map.frame_at_bars(start_bars),
        end: map.frame_at_bars(end_bars),
    })
}

fn guard_unit(unit_bars: f64) -> f64 {
    if unit_bars.is_finite() && unit_bars > 0.0 { unit_bars } else { 1.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempo::Meter;

    const SR: u32 = 48_000;

    fn flat() -> TempoMap {
        TempoMap::new(SR, 120.0, Meter::FOUR_FOUR)
    }

    /// A dependency-free LCG. Reproducible, and it keeps the crate's dependency list empty.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 11
        }
        fn below(&mut self, n: u64) -> u64 { self.next() % n }
    }

    #[test]
    fn auto_lands_on_one_bar_at_the_default_view() {
        // 16 bars across a full-screen canvas.
        assert_eq!(auto_unit(16.0, 1800.0, AUTO_TARGET_PX), 1.0);
        assert_eq!(auto_unit(16.0, 1280.0, AUTO_TARGET_PX), 1.0);
    }

    #[test]
    fn auto_gets_finer_as_you_zoom_in() {
        let width = 1600.0;
        let mut previous = f64::INFINITY;
        for bars_visible in [64.0, 32.0, 16.0, 8.0, 4.0, 2.0, 1.0, 0.5] {
            let unit = auto_unit(bars_visible, width, AUTO_TARGET_PX);
            assert!(unit <= previous, "zooming in must never coarsen the grid");
            previous = unit;
        }
    }

    #[test]
    fn auto_is_always_on_the_ladder() {
        let mut rng = Lcg(0xA11CE);
        for _ in 0..2000 {
            let bars = 0.25 + (rng.below(10_000) as f64) / 100.0;
            let px = 200.0 + rng.below(3000) as f64;
            let unit = auto_unit(bars, px, AUTO_TARGET_PX);
            assert!(LADDER.contains(&unit), "{unit} is not a rung");
        }
    }

    #[test]
    fn a_click_always_contains_the_click() {
        let map = flat();
        let mut rng = Lcg(0xC0FFEE);
        for _ in 0..5000 {
            let unit = LADDER[rng.below(LADDER.len() as u64) as usize];
            let frame = rng.below(48_000 * 120);
            let sel = cell_at(&map, unit, frame);
            assert!(
                sel.start <= frame && frame < sel.end,
                "unit {unit}: cell [{}, {}) does not contain {frame}",
                sel.start, sel.end
            );
        }
    }

    #[test]
    fn a_click_selects_exactly_one_cell() {
        let map = flat();
        for unit in LADDER {
            let sel = cell_at(&map, unit, 1_234_567);
            let bars = map.bars_at(sel.end) - map.bars_at(sel.start);
            assert!(
                (bars - unit).abs() < 1e-6,
                "unit {unit}: a click selected {bars} bars"
            );
        }
    }

    #[test]
    fn a_drag_is_always_a_whole_number_of_cells_and_never_inverts() {
        let map = flat();
        let mut rng = Lcg(0x5EED);
        for _ in 0..5000 {
            let unit = LADDER[rng.below(LADDER.len() as u64) as usize];
            let a = rng.below(48_000 * 120);
            let b = rng.below(48_000 * 120);
            let sel = snap_range(&map, unit, a, b);
            assert!(sel.end > sel.start, "unit {unit}: {a}..{b} produced an empty selection");
            let cells = (map.bars_at(sel.end) - map.bars_at(sel.start)) / unit;
            assert!(
                (cells - cells.round()).abs() < 1e-6 && cells.round() >= 1.0,
                "unit {unit}: {a}..{b} produced {cells} cells"
            );
            assert!(sel.start <= a.min(b) && sel.end >= a.max(b), "the drag escaped its own span");
        }
    }

    #[test]
    fn a_drag_that_never_moved_still_selects_a_cell() {
        let map = flat();
        for unit in LADDER {
            let sel = snap_range(&map, unit, 500_000, 500_000);
            assert!(!sel.is_empty());
            let cells = (map.bars_at(sel.end) - map.bars_at(sel.start)) / unit;
            assert!((cells - 1.0).abs() < 1e-6);
        }
        // And a drag whose ends land exactly on the same grid line.
        let on_the_line = map.frame_at_bars(4.0);
        let sel = snap_range(&map, 1.0, on_the_line, on_the_line);
        assert_eq!(sel.frames(), 96_000);
    }

    #[test]
    fn dragging_backwards_is_the_same_as_dragging_forwards() {
        let map = flat();
        for unit in LADDER {
            assert_eq!(
                snap_range(&map, unit, 200_000, 900_000),
                snap_range(&map, unit, 900_000, 200_000)
            );
        }
    }

    #[test]
    fn snapping_holds_across_a_tempo_change() {
        let mut map = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
        let change = map.frame_at_bars(8.0);
        map.push(change, 174.0, Meter::FOUR_FOUR);
        // A drag spanning the change: still a whole number of bars, even though the two halves are
        // different numbers of samples.
        let a = map.frame_at_bars(6.5);
        let b = map.frame_at_bars(10.25);
        let sel = snap_range(&map, 1.0, a, b);
        // Tolerance in frames, not in bars. `frame_at_bars` rounds to a whole frame, so at 174 bpm
        // half a frame is already 7.6e-6 of a bar — a bar-domain epsilon would be measuring the
        // sample grid and calling it an error.
        let one_frame_of_a_bar = |f: u64| 1.0 / map.frames_per_bar_at(f);
        assert!((map.bars_at(sel.start) - 6.0).abs() < one_frame_of_a_bar(sel.start));
        assert!((map.bars_at(sel.end) - 11.0).abs() < one_frame_of_a_bar(sel.end));
        // And the two halves really are different lengths, or this test proves nothing.
        assert!(map.frames_per_bar_at(0) > map.frames_per_bar_at(change) * 1.4);
    }

    #[test]
    fn a_range_taken_to_the_head_never_reaches_past_it() {
        let map = flat();
        let head = map.frame_at_bars(9.4); // four tenths of the way into bar 10
        let from = map.frame_at_bars(4.0);
        // The ordinary snap rounds the end up, past audio that does not exist yet. That is correct
        // for a drag and wrong for "everything since the mark", which is why both exist.
        let eager = snap_range(&map, 1.0, from, head);
        assert!(eager.end > head, "this is the behaviour the other function exists to avoid");

        let complete = snap_range_upto(&map, 1.0, from, head, head).expect("five whole bars exist");
        assert!(complete.end <= head, "a completed range may not pass the head");
        assert!((map.bars_at(complete.end) - 9.0).abs() < 1e-6, "it should stop at bar 9");
        assert!((map.bars_at(complete.start) - 4.0).abs() < 1e-6);
    }

    #[test]
    fn a_range_with_no_whole_cell_yet_is_nothing_rather_than_a_guess() {
        let map = flat();
        let from = map.frame_at_bars(4.2);
        let head = map.frame_at_bars(4.6);
        // Not one whole bar has passed since the mark, so at a one-bar grid there is nothing to
        // take. Returning the containing cell would hand back audio from before the mark.
        assert!(snap_range_upto(&map, 1.0, from, head, head).is_none());
        // At a finer grid the same span does contain whole cells.
        assert!(snap_range_upto(&map, 0.125, from, head, head).is_some());
    }

    #[test]
    fn the_number_row_rounds_to_whole_cells() {
        let map = flat();
        let head = map.frame_at_bars(64.0); // on a bar line
        let expected = [2.0, 3.0, 5.0, 6.0, 8.0, 10.0, 11.0, 13.0, 14.0, 16.0];
        for (i, want) in expected.iter().enumerate() {
            let sel = percent_from_head(&map, 1.0, head, 16.0, i as u32 + 1).expect("whole cells");
            let bars = map.bars_at(sel.end) - map.bars_at(sel.start);
            assert!(
                (bars - want).abs() < 1e-6,
                "key {} selected {bars} bars, expected {want}",
                (i + 1) % 10
            );
        }
    }

    #[test]
    fn the_number_row_gives_a_loopable_length_from_anywhere() {
        // The head is almost never on a grid line, and this is the property that matters: whatever
        // it comes out as, it has to be a whole number of cells or it will not loop.
        let map = flat();
        let mut rng = Lcg(0x10AD);
        for _ in 0..2000 {
            let head = map.frame_at_bars(20.0) + rng.below(48_000 * 90);
            let unit = LADDER[rng.below(LADDER.len() as u64) as usize];
            let tenths = 1 + rng.below(10) as u32;
            let Some(sel) = percent_from_head(&map, unit, head, 16.0, tenths) else { continue };
            let cells = (map.bars_at(sel.end) - map.bars_at(sel.start)) / unit;
            assert!(
                (cells - cells.round()).abs() < 1e-6 && cells.round() >= 1.0,
                "unit {unit}, key {tenths}: {cells} cells is not a loop"
            );
            assert!(sel.end <= head, "it may not reach past the head");
        }
    }

    #[test]
    fn the_number_row_never_reaches_past_the_window_or_the_head() {
        let map = flat();
        let mut rng = Lcg(0xBEEF);
        for _ in 0..2000 {
            let head = rng.below(48_000 * 60);
            let window = 4.0 + (rng.below(6000) as f64) / 100.0;
            let unit = LADDER[rng.below(LADDER.len() as u64) as usize];
            let tenths = 1 + rng.below(10) as u32;
            let Some(sel) = percent_from_head(&map, unit, head, window, tenths) else { continue };
            assert!(sel.end <= head, "it may not reach past the head");
            assert!(sel.start <= sel.end);
            let span = map.bars_at(sel.end) - map.bars_at(sel.start);
            assert!(
                span <= window + 1e-6,
                "selected {span} bars of a {window}-bar window"
            );
        }
    }

    #[test]
    fn zero_and_ten_both_mean_the_whole_window() {
        let map = flat();
        let head = map.frame_at_bars(40.0);
        let all = percent_from_head(&map, 1.0, head, 16.0, 10).expect("the whole window");
        assert!((map.bars_at(all.end) - map.bars_at(all.start) - 16.0).abs() < 1e-6);
        // Out-of-range keys clamp rather than producing nonsense.
        assert_eq!(percent_from_head(&map, 1.0, head, 16.0, 0), percent_from_head(&map, 1.0, head, 16.0, 1));
        assert_eq!(percent_from_head(&map, 1.0, head, 16.0, 99), Some(all));
    }
}

#[cfg(test)]
mod phase_tests {
    use super::*;
    use crate::tempo::Meter;

    /// The grid must honour a hand-set downbeat, or the correction exists in the map and nowhere
    /// the user can see it.
    #[test]
    fn snapping_follows_a_hand_set_downbeat() {
        let mut map = TempoMap::new(48_000, 120.0, Meter::FOUR_FOUR);
        let downbeat = map.frame_at_bars(4.375);
        map.set_downbeat(downbeat);

        // A click just after the tapped downbeat must select the cell that starts on it.
        let sel = cell_at(&map, 1.0, downbeat + 1000);
        assert_eq!(
            sel.start, downbeat,
            "the cell should begin on the downbeat the user tapped"
        );
        assert!(sel.end > sel.start);

        // And a drag around it snaps to the same lines.
        let dragged = snap_range(&map, 1.0, downbeat + 1000, downbeat + 200_000);
        assert_eq!(dragged.start, downbeat);
    }
}

// ---------------------------------------------------------------------------------------
// Drawing the grid
// ---------------------------------------------------------------------------------------

use crate::view::Viewport;

/// How prominent a grid line is. The renderer picks colours; this only says what the line *means*.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// A lap boundary — where the display wraps.
    Lap,
    /// A bar line.
    Bar,
    /// A sub-bar cell boundary at the current quantise unit.
    Cell,
}

/// One vertical line, positioned as a fraction of the canvas width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ruling {
    pub fraction: f64,
    pub rule: Rule,
}

/// The most lines that will be emitted before the sub-bar ones are dropped.
///
/// A guard, not a design: auto units keep the count near the canvas width, but a *fixed* small unit
/// at full zoom-out can ask for thousands of lines that would land on top of each other anyway. Bar
/// lines survive the cull, because losing those loses the reading of the picture.
pub const MAX_RULINGS: usize = 2048;

/// Every grid line visible in `viewport`, left to right.
///
/// Positions are computed in bars and converted to canvas fractions, never the reverse: a line has
/// to sit exactly where a snapped selection edge would, and rounding through pixels first would put
/// the two a fraction of a pixel apart at some zoom levels and not others.
pub fn rulings(viewport: &Viewport, unit_bars: f64, out: &mut Vec<Ruling>) {
    out.clear();
    let unit = guard_unit(unit_bars);
    let left = viewport.left_bars;
    let right = left + viewport.span_bars;

    // Sub-bar lines are dropped wholesale rather than thinned, so the grid never shows an
    // irregular subset of a regular thing — which reads as missing lines rather than as a coarser
    // grid, and is worse than showing none.
    let cells = (viewport.span_bars / unit).ceil() as usize + 2;
    let with_cells = unit < 1.0 && cells <= MAX_RULINGS;

    let step = if with_cells { unit } else { 1.0 };
    let first = (left / step).floor() * step;
    let mut at = first;
    // Counted rather than compared, because accumulating `step` in floating point drifts and the
    // loop bound would then depend on how far into the session the view happens to be.
    let count = ((right - first) / step).floor() as i64 + 1;
    for _ in 0..count.max(0) {
        let fraction = viewport.fraction_at(at);
        if (-0.001..=1.001).contains(&fraction) {
            let on_bar = (at - at.round()).abs() < 1e-6;
            let on_lap = (at % viewport.window_bars).abs() < 1e-6
                || (at % viewport.window_bars - viewport.window_bars).abs() < 1e-6;
            out.push(Ruling {
                fraction,
                rule: if on_lap {
                    Rule::Lap
                } else if on_bar {
                    Rule::Bar
                } else {
                    Rule::Cell
                },
            });
        }
        at += step;
    }
}

#[cfg(test)]
mod ruling_tests {
    use super::*;
    use crate::tempo::Meter;
    use crate::view::View;

    fn setup(zoom: f64) -> (TempoMap, Viewport) {
        let m = TempoMap::new(48_000, 120.0, Meter::FOUR_FOUR);
        let mut view = View::new(16.0);
        view.zoom = zoom;
        view.clamp();
        let vp = Viewport::resolve(&view, &m, m.frame_at_bars(40.0), 1600);
        (m, vp)
    }

    #[test]
    fn fitted_at_one_bar_there_are_seventeen_lines() {
        let (_m, vp) = setup(1.0);
        let mut out = Vec::new();
        rulings(&vp, 1.0, &mut out);
        // Sixteen bars, so seventeen boundaries counting both ends.
        assert_eq!(out.len(), 17, "got {:?}", out.iter().map(|r| r.fraction).collect::<Vec<_>>());
        assert!((out[0].fraction - 0.0).abs() < 1e-9);
        assert!((out[16].fraction - 1.0).abs() < 1e-9);
        // Evenly spaced, which is the property a wrapping display exists to provide.
        for pair in out.windows(2) {
            assert!((pair[1].fraction - pair[0].fraction - 1.0 / 16.0).abs() < 1e-9);
        }
    }

    #[test]
    fn the_lap_boundaries_outrank_the_bar_lines() {
        let (_m, vp) = setup(1.0);
        let mut out = Vec::new();
        rulings(&vp, 1.0, &mut out);
        assert_eq!(out[0].rule, Rule::Lap, "the start of the lap is where the display wraps");
        assert_eq!(out[16].rule, Rule::Lap, "and so is the end");
        assert!(out[1..16].iter().all(|r| r.rule == Rule::Bar));
    }

    #[test]
    fn sub_bar_units_add_cells_without_losing_the_bars() {
        let (_m, vp) = setup(1.0);
        let mut out = Vec::new();
        rulings(&vp, 0.25, &mut out);
        assert_eq!(out.len(), 65, "sixteen bars of quarter-bar cells");
        let bars = out.iter().filter(|r| r.rule != Rule::Cell).count();
        assert_eq!(bars, 17, "every bar line survives the finer grid");
    }

    #[test]
    fn an_absurdly_fine_unit_drops_the_cells_rather_than_thinning_them() {
        let (_m, vp) = setup(1.0);
        let mut out = Vec::new();
        // 1/512 of a bar over sixteen bars is 8192 lines, all inside a pixel of each other.
        rulings(&vp, 1.0 / 512.0, &mut out);
        assert_eq!(out.len(), 17, "it should fall back to bar lines, not draw an irregular subset");
        assert!(out.iter().all(|r| r.rule != Rule::Cell));
    }

    #[test]
    fn zoomed_in_the_lines_still_land_on_the_grid() {
        let (m, vp) = setup(8.0);
        let mut out = Vec::new();
        rulings(&vp, 0.25, &mut out);
        assert!(!out.is_empty());
        for ruling in &out {
            assert!((-0.001..=1.001).contains(&ruling.fraction), "off screen: {ruling:?}");
            // The line must sit exactly where a snapped selection edge would, or the two disagree
            // by a fraction of a pixel at some zoom levels and not others.
            let bars = vp.bars_at(ruling.fraction);
            let snapped = cell_at(&m, 0.25, m.frame_at_bars(bars));
            let edge = m.bars_at(snapped.start);
            assert!(
                (bars - edge).abs() < 1e-6,
                "a ruling at {bars} bars is not on a cell boundary ({edge})"
            );
        }
    }

    #[test]
    fn a_tempo_change_does_not_bend_the_grid() {
        let mut m = TempoMap::new(48_000, 120.0, Meter::FOUR_FOUR);
        m.push(m.frame_at_bars(36.0), 174.0, Meter::FOUR_FOUR);
        let view = View::new(16.0);
        let vp = Viewport::resolve(&view, &m, m.frame_at_bars(44.0), 1600);
        let mut out = Vec::new();
        rulings(&vp, 1.0, &mut out);
        // Bars are bars whatever the tempo did: the spacing on screen stays uniform because the
        // axis is musical time, not samples.
        for pair in out.windows(2) {
            assert!(
                (pair[1].fraction - pair[0].fraction - 1.0 / 16.0).abs() < 1e-9,
                "the grid stretched across the tempo change"
            );
        }
    }
}
