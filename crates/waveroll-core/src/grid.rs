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

/// The number row: the most recent `tenths`/10 of the window, ending at the write head.
///
/// Head to tail, snapped outward, clamped to the window. `tenths` is 1..=10, with the `0` key
/// meaning ten. At the 16-bar default this yields 2, 4, 5, 7, 8, 10, 12, 13, 15 and 16 bars — the
/// tidy landings are the quarters, and the rest are what falling on a grid actually costs.
pub fn percent_from_head(
    map: &TempoMap,
    unit_bars: f64,
    head: u64,
    window_bars: f64,
    tenths: u32,
) -> Selection {
    let unit = guard_unit(unit_bars);
    let tenths = tenths.clamp(1, 10) as f64;
    let head_bars = map.bars_at(head);
    let window_start = (head_bars - window_bars).max(0.0);
    let wanted = window_bars * tenths / 10.0;

    // Snap the moving edge outward and leave the head where it is: the head is a fact, not a
    // preference, and rounding it would select audio that has not been captured yet.
    let start_bars = ((head_bars - wanted) / unit).floor() * unit;
    let start_bars = start_bars.max(window_start);
    Selection {
        start: map.frame_at_bars(start_bars),
        end: head,
    }
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
    fn the_number_row_at_sixteen_bars_lands_where_the_plan_says() {
        let map = flat();
        let head = map.frame_at_bars(64.0); // well into the session
        let expected = [2.0, 4.0, 5.0, 7.0, 8.0, 10.0, 12.0, 13.0, 15.0, 16.0];
        for (i, want) in expected.iter().enumerate() {
            let sel = percent_from_head(&map, 1.0, head, 16.0, i as u32 + 1);
            let bars = map.bars_at(sel.end) - map.bars_at(sel.start);
            assert!(
                (bars - want).abs() < 1e-6,
                "key {} selected {bars} bars, expected {want}",
                (i + 1) % 10
            );
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
            let sel = percent_from_head(&map, unit, head, window, tenths);
            assert_eq!(sel.end, head, "the head is a fact and must not be rounded");
            assert!(sel.start <= head);
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
        let all = percent_from_head(&map, 1.0, head, 16.0, 10);
        assert!((map.bars_at(all.end) - map.bars_at(all.start) - 16.0).abs() < 1e-6);
        // Out-of-range keys clamp rather than producing nonsense.
        assert_eq!(percent_from_head(&map, 1.0, head, 16.0, 0), percent_from_head(&map, 1.0, head, 16.0, 1));
        assert_eq!(percent_from_head(&map, 1.0, head, 16.0, 99), all);
    }
}
