//! What the canvas is looking at.
//!
//! The display wraps rather than scrolls: the x-axis is the whole window, the write head sweeps
//! left to right and starts again at the beginning, and new audio paints over old. That choice is
//! what makes the grid a *static graticule you can aim at* — a scrolling display is a moving target
//! under the cursor, and this is a selection interface before it is a picture.
//!
//! It costs one constraint, which is why the window is measured in **bars** rather than seconds: if
//! a lap were not a whole number of bars the grid would shift under the pointer every time round.
//! Defining it in bars satisfies that by construction, and leaves only the arithmetic of converting
//! a lap to samples when the tempo moves.
//!
//! Everything here is in bars until the last moment. Zoom, scroll, laps and hit-testing all happen
//! in musical time, and `TempoMap` converts once at the boundary.

use crate::tempo::TempoMap;

/// How far in the view can be zoomed. The practical limit is one sample per column, which
/// [`Viewport::frames_per_column`] reports; this is only a guard against a runaway wheel event.
pub const MAX_ZOOM: f64 = 65_536.0;

/// The persistent part of the view: what the user has done to it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    /// Length of one lap, in bars. 16 by default.
    pub window_bars: f64,
    /// 1.0 shows the whole window. Larger shows less of it.
    pub zoom: f64,
    /// Left edge, in bars from the start of the lap.
    pub scroll_bars: f64,
}

impl View {
    pub fn new(window_bars: f64) -> View {
        View { window_bars: window_bars.max(0.25), zoom: 1.0, scroll_bars: 0.0 }
    }

    /// Fit to width, which is home.
    ///
    /// First load and any change to the quantise setting come back here. Zoom is an excursion,
    /// not a mode — the whole window is one keystroke away, always.
    pub fn home(&mut self) {
        self.zoom = 1.0;
        self.scroll_bars = 0.0;
    }

    pub fn is_home(&self) -> bool {
        self.zoom <= 1.0 && self.scroll_bars == 0.0
    }

    /// Visible span, in bars.
    pub fn span_bars(&self) -> f64 {
        self.window_bars / self.zoom.clamp(1.0, MAX_ZOOM)
    }

    /// Zooms by `factor` about a fixed point given as a fraction of the canvas width.
    ///
    /// Anchoring on the pointer rather than on the write head is deliberate: anchor it to the head
    /// and the view chases, and you can never hold still on the thing you were looking at.
    pub fn zoom_about(&mut self, factor: f64, anchor: f64) {
        if !(factor.is_finite() && factor > 0.0) {
            return;
        }
        let anchor = anchor.clamp(0.0, 1.0);
        let at = self.scroll_bars + anchor * self.span_bars();
        self.zoom = (self.zoom * factor).clamp(1.0, MAX_ZOOM);
        self.scroll_bars = at - anchor * self.span_bars();
        self.clamp();
    }

    /// Scrolls by a fraction of the visible span.
    pub fn scroll_by(&mut self, fraction: f64) {
        self.scroll_bars += fraction * self.span_bars();
        self.clamp();
    }

    /// Keeps the visible span inside the window. Zooming out past fit is meaningless — there is
    /// nothing outside the window to see — so the span is clamped rather than the zoom alone.
    pub fn clamp(&mut self) {
        self.zoom = self.zoom.clamp(1.0, MAX_ZOOM);
        let slack = (self.window_bars - self.span_bars()).max(0.0);
        self.scroll_bars = self.scroll_bars.clamp(0.0, slack);
    }
}

/// The view resolved against the clock, once per painted frame.
///
/// The renderer uploads these four numbers and the pointer reads them back; nothing downstream
/// needs the `View` itself, which is what keeps hit-testing and drawing in exact agreement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    /// Absolute bar position of the start of the current lap.
    pub lap_start_bars: f64,
    /// Absolute bar position of the left edge of the canvas.
    pub left_bars: f64,
    pub span_bars: f64,
    pub window_bars: f64,
    /// Absolute bar position of the write head.
    pub head_bars: f64,
    /// The write head as a frame index. Carried alongside `head_bars` rather than re-derived,
    /// because a column that straddles the head has to stop at exactly the frame the ring stops
    /// at, and converting back through the map would land a sample or two either side of it.
    pub head_frame: u64,
    /// Which lap the head is on. Zero before the first wrap.
    pub lap: u64,
    /// Canvas width in pixels, used only to report the reduction ratio.
    pub columns: u32,
}

impl Viewport {
    pub fn resolve(view: &View, map: &TempoMap, captured: u64, columns: u32) -> Viewport {
        let mut view = *view;
        view.clamp();
        let head_bars = map.bars_at(captured).max(0.0);
        let lap = (head_bars / view.window_bars).floor().max(0.0);
        let lap_start_bars = lap * view.window_bars;
        Viewport {
            lap_start_bars,
            left_bars: lap_start_bars + view.scroll_bars,
            span_bars: view.span_bars(),
            window_bars: view.window_bars,
            head_bars,
            head_frame: captured,
            lap: lap as u64,
            columns: columns.max(1),
        }
    }

    /// Bar position shown at canvas fraction `u`, before the wrap is resolved.
    pub fn bars_at(&self, u: f64) -> f64 {
        self.left_bars + u * self.span_bars
    }

    /// Canvas fraction showing `bars`. Outside `0..1` when off screen, which the caller wants to
    /// know rather than have clamped away.
    pub fn fraction_at(&self, bars: f64) -> f64 {
        (bars - self.left_bars) / self.span_bars
    }

    /// Where the write head sits, as a canvas fraction.
    pub fn head_fraction(&self) -> f64 {
        self.fraction_at(self.head_bars)
    }

    /// The frame shown at canvas fraction `u`, or `None` where nothing has been captured yet.
    ///
    /// This is where wrapping actually happens. A column to the right of the head has not been
    /// written *this* lap, so it still shows the previous one — which is exactly the look the
    /// display is for, new sweeping over old. On the very first lap there is no previous one, and
    /// those columns are genuinely empty rather than showing something older.
    pub fn frame_at(&self, map: &TempoMap, u: f64) -> Option<u64> {
        let bars = self.bars_at(u);
        if bars <= self.head_bars {
            return Some(map.frame_at_bars(bars));
        }
        let previous = bars - self.window_bars;
        if previous < 0.0 || self.lap == 0 {
            return None;
        }
        Some(map.frame_at_bars(previous))
    }

    /// The frames a column spanning `u0..u1` draws from, or `None` where nothing is captured yet.
    ///
    /// Three cases, and the third is the one that is easy to miss: a column can *straddle* the
    /// write head, with its left edge in this lap and its right edge in the last one. Resolving
    /// the two edges independently would return a range running backwards. It stops at the head
    /// instead, which is also what makes the leading edge of the sweep look like a hard edge
    /// rather than a seam.
    pub fn frame_span(&self, map: &TempoMap, u0: f64, u1: f64) -> Option<(u64, u64)> {
        let b0 = self.bars_at(u0);
        let b1 = self.bars_at(u1);
        match (b0 > self.head_bars, b1 > self.head_bars) {
            (false, false) => Some((map.frame_at_bars(b0), map.frame_at_bars(b1))),
            (true, true) => {
                if self.lap == 0 || b0 - self.window_bars < 0.0 {
                    return None;
                }
                Some((
                    map.frame_at_bars(b0 - self.window_bars),
                    map.frame_at_bars(b1 - self.window_bars),
                ))
            }
            (false, true) => {
                let start = map.frame_at_bars(b0);
                if start < self.head_frame {
                    return Some((start, self.head_frame));
                }
                // The column sits exactly on the head and its current-lap share is zero samples
                // wide. Left alone that is a one-pixel gap in the leading edge of the sweep, which
                // reads as a rendering fault rather than as a playhead, so it shows the previous
                // lap like the columns to its right.
                if self.lap == 0 || b0 - self.window_bars < 0.0 {
                    return None;
                }
                Some((
                    map.frame_at_bars(b0 - self.window_bars),
                    map.frame_at_bars(b1 - self.window_bars),
                ))
            }
            (true, false) => unreachable!("u1 > u0, so b1 cannot be behind b0"),
        }
    }

    /// Fills `out` with one entry per column: the ring range each pixel reduces.
    ///
    /// This is the whole musical-time half of drawing the waveform, done on the CPU where it can
    /// be tested, so the shader receives nothing but "reduce these samples into this pixel" and
    /// needs to know about neither tempo nor wrapping.
    pub fn columns(&self, map: &TempoMap, out: &mut Vec<Column>) {
        out.clear();
        out.reserve(self.columns as usize);
        let width = f64::from(self.columns);
        for c in 0..self.columns {
            let span = self.frame_span(map, f64::from(c) / width, f64::from(c + 1) / width);
            out.push(match span {
                None => Column { start: 0, count: 0 },
                Some((start, end)) => {
                    // Nothing may read past the head: the frame at `head_frame` has not been
                    // written yet, and a column that reached it would reduce whatever the ring
                    // happens to hold from a lap ago into the leading edge of the sweep.
                    let end = end.min(self.head_frame);
                    if end > start {
                        Column { start, count: (end - start).min(u64::from(u32::MAX)) as u32 }
                    } else if start < self.head_frame {
                        // Narrower than one sample. It still has to name the sample under it, or a
                        // zoomed-in trace goes blank between samples instead of interpolating.
                        Column { start, count: 1 }
                    } else {
                        Column { start: 0, count: 0 }
                    }
                }
            });
        }
    }

    /// Samples reduced into one pixel column.
    ///
    /// Below one, the columns are closer together than the samples and the trace should be drawn
    /// by band-limited reconstruction rather than as a min/max envelope: straight lines between
    /// samples show a shape the signal never had.
    pub fn frames_per_column(&self, map: &TempoMap) -> f64 {
        let left = map.frame_at_bars(self.left_bars) as f64;
        let right = map.frame_at_bars(self.left_bars + self.span_bars) as f64;
        (right - left) / f64::from(self.columns)
    }
}

/// One pixel column's source range in the ring.
///
/// `count == 0` means nothing has been captured there — the first lap, ahead of the head — which
/// is different from silence and has to be drawn differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Column {
    pub start: u64,
    pub count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempo::Meter;

    const SR: u32 = 48_000;
    const W: u32 = 1600;

    fn map() -> TempoMap {
        TempoMap::new(SR, 120.0, Meter::FOUR_FOUR)
    }

    fn at_bars(m: &TempoMap, bars: f64) -> u64 {
        m.frame_at_bars(bars)
    }

    #[test]
    fn fitted_the_canvas_is_exactly_one_lap() {
        let m = map();
        let view = View::new(16.0);
        let vp = Viewport::resolve(&view, &m, at_bars(&m, 5.0), W);
        assert_eq!(vp.lap, 0);
        assert!((vp.left_bars - 0.0).abs() < 1e-9);
        assert!((vp.span_bars - 16.0).abs() < 1e-9);
        assert!((vp.head_fraction() - 5.0 / 16.0).abs() < 1e-9);
    }

    #[test]
    fn the_head_sweeps_and_the_lap_advances() {
        let m = map();
        let view = View::new(16.0);
        for (captured_bars, lap, fraction) in
            [(0.0, 0u64, 0.0), (8.0, 0, 0.5), (15.9, 0, 15.9 / 16.0), (16.0, 1, 0.0), (20.0, 1, 0.25)]
        {
            let vp = Viewport::resolve(&view, &m, at_bars(&m, captured_bars), W);
            assert_eq!(vp.lap, lap, "at {captured_bars} bars");
            assert!(
                (vp.head_fraction() - fraction).abs() < 1e-6,
                "at {captured_bars} bars the head is at {}, expected {fraction}",
                vp.head_fraction()
            );
        }
    }

    #[test]
    fn columns_ahead_of_the_head_show_the_previous_lap() {
        let m = map();
        let view = View::new(16.0);
        // Four bars into the second lap.
        let captured = at_bars(&m, 20.0);
        let vp = Viewport::resolve(&view, &m, captured, W);

        // A column behind the head is this lap's audio, and recent. Bars are absolute, so two
        // bars into lap 1 is bar 18, not bar 2 — the lap index is not subtracted anywhere.
        let behind = vp.frame_at(&m, 0.125).expect("behind the head");
        assert!((m.bars_at(behind) - 18.0).abs() < 1e-6, "expected bar 18, got {}", m.bars_at(behind));
        assert!((m.bars_at(behind) - vp.lap_start_bars - 2.0).abs() < 1e-6, "two bars into the lap");

        // A column ahead of the head is last lap's, and therefore older than the head but newer
        // than one window back — which is the whole definition of what is still on screen.
        let ahead = vp.frame_at(&m, 0.5).expect("the previous lap exists").min(captured);
        let ahead_bars = m.bars_at(ahead);
        assert!(
            (ahead_bars - 8.0).abs() < 1e-6,
            "a column at half width on lap 1 should show bar 8, got {ahead_bars}"
        );
        assert!(ahead < captured, "it must be older than the head");
        assert!(ahead > captured - (at_bars(&m, 16.0)), "and still inside the window");
    }

    #[test]
    fn on_the_first_lap_there_is_nothing_ahead_of_the_head() {
        let m = map();
        let view = View::new(16.0);
        let vp = Viewport::resolve(&view, &m, at_bars(&m, 5.0), W);
        assert!(vp.frame_at(&m, 0.1).is_some(), "behind the head is written");
        assert!(
            vp.frame_at(&m, 0.9).is_none(),
            "ahead of the head on lap zero is empty, not old"
        );
    }

    #[test]
    fn zoom_holds_the_anchor_still() {
        let m = map();
        let mut view = View::new(16.0);
        let captured = at_bars(&m, 40.0);
        for anchor in [0.0, 0.25, 0.5, 0.75, 1.0] {
            view.home();
            let before = Viewport::resolve(&view, &m, captured, W).bars_at(anchor);
            view.zoom_about(4.0, anchor);
            let after = Viewport::resolve(&view, &m, captured, W).bars_at(anchor);
            // The anchor can only move if clamping pushed the view back inside the window, which
            // is exactly what happens at the two extremes.
            let slack = view.window_bars - view.span_bars();
            let pinned = view.scroll_bars > 1e-9 && view.scroll_bars < slack - 1e-9;
            if pinned {
                assert!(
                    (before - after).abs() < 1e-9,
                    "anchor {anchor} moved from {before} to {after}"
                );
            }
            assert!((view.span_bars() - 4.0).abs() < 1e-9, "zooming 4x should show 4 bars");
        }
    }

    #[test]
    fn the_view_can_never_leave_its_window() {
        let m = map();
        let mut view = View::new(16.0);
        // Drive it hard in both directions; it must stay inside the lap at every step.
        for i in 0..200 {
            view.zoom_about(if i % 3 == 0 { 0.5 } else { 1.7 }, (i % 5) as f64 / 4.0);
            view.scroll_by(if i % 2 == 0 { -3.0 } else { 2.0 });
            assert!(view.zoom >= 1.0 && view.zoom <= MAX_ZOOM, "zoom escaped: {}", view.zoom);
            assert!(view.scroll_bars >= -1e-9, "scrolled before the lap: {}", view.scroll_bars);
            assert!(
                view.scroll_bars + view.span_bars() <= view.window_bars + 1e-9,
                "scrolled past the lap: {} + {} > {}",
                view.scroll_bars,
                view.span_bars(),
                view.window_bars
            );
        }
        let vp = Viewport::resolve(&view, &m, at_bars(&m, 33.0), W);
        assert!(vp.left_bars >= vp.lap_start_bars - 1e-9);
        assert!(vp.left_bars + vp.span_bars <= vp.lap_start_bars + vp.window_bars + 1e-9);
    }

    #[test]
    fn home_is_reachable_from_anywhere() {
        let mut view = View::new(16.0);
        view.zoom_about(300.0, 0.7);
        view.scroll_by(0.9);
        assert!(!view.is_home());
        view.home();
        assert!(view.is_home());
        assert!((view.span_bars() - 16.0).abs() < 1e-9);
    }

    #[test]
    fn fractions_and_bars_are_inverses() {
        let m = map();
        let mut view = View::new(16.0);
        view.zoom_about(8.0, 0.5);
        let vp = Viewport::resolve(&view, &m, at_bars(&m, 25.0), W);
        for i in 0..=20 {
            let u = i as f64 / 20.0;
            assert!((vp.fraction_at(vp.bars_at(u)) - u).abs() < 1e-12);
        }
    }

    #[test]
    fn the_reduction_ratio_reports_when_to_stop_drawing_an_envelope() {
        let m = map();
        let mut view = View::new(16.0);
        // Fitted, 16 bars at 120 BPM is 32 s: 1.536 M samples over 1600 columns.
        let fitted = Viewport::resolve(&view, &m, at_bars(&m, 40.0), W).frames_per_column(&m);
        assert!((fitted - 1_536_000.0 / 1600.0).abs() < 1.0, "got {fitted}");
        // Zoomed all the way in there is less than one sample per column, which is the signal to
        // switch from a min/max envelope to band-limited reconstruction.
        view.zoom = MAX_ZOOM;
        let zoomed = Viewport::resolve(&view, &m, at_bars(&m, 40.0), W).frames_per_column(&m);
        assert!(zoomed < 1.0, "at maximum zoom there should be under a sample per column: {zoomed}");
    }

    #[test]
    fn every_column_is_covered_exactly_once_and_in_order() {
        let m = map();
        let view = View::new(16.0);
        let vp = Viewport::resolve(&view, &m, at_bars(&m, 40.0), 640);
        let mut cols = Vec::new();
        vp.columns(&m, &mut cols);
        assert_eq!(cols.len(), 640);

        // Behind the head the ranges tile forwards without gaps; ahead of it they tile forwards
        // too, one lap back. The seam between the two is the head, and it is the only place the
        // sequence is allowed to jump.
        let mut seams = 0;
        for pair in cols.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(a.count > 0 && b.count > 0, "lap 2 has no empty columns");
            if b.start != a.start + u64::from(a.count) {
                seams += 1;
            }
        }
        assert_eq!(seams, 1, "there should be exactly one discontinuity, at the write head");
    }

    #[test]
    fn a_column_straddling_the_head_stops_at_it() {
        let m = map();
        let view = View::new(16.0);
        let captured = at_bars(&m, 24.0); // half way through lap 1
        let vp = Viewport::resolve(&view, &m, captured, 64);
        let mut cols = Vec::new();
        vp.columns(&m, &mut cols);
        // Column 32 spans the head at exactly half width.
        let straddling = cols[31];
        assert!(
            straddling.start + u64::from(straddling.count) <= captured,
            "a column may never read past the write head: {} + {} > {captured}",
            straddling.start,
            straddling.count
        );
        for (i, col) in cols.iter().enumerate() {
            assert!(
                col.start + u64::from(col.count) <= captured,
                "column {i} reads past the head"
            );
        }
    }

    #[test]
    fn the_first_lap_has_empty_columns_ahead_of_the_head() {
        let m = map();
        let view = View::new(16.0);
        let vp = Viewport::resolve(&view, &m, at_bars(&m, 4.0), 64);
        let mut cols = Vec::new();
        vp.columns(&m, &mut cols);
        let written = cols.iter().filter(|c| c.count > 0).count();
        // A quarter of a 16-bar lap.
        assert!((15..=17).contains(&written), "expected about 16 written columns, got {written}");
        assert!(cols[0].count > 0);
        assert_eq!(cols[63].count, 0, "nothing has ever been captured there");
    }

    #[test]
    fn zoomed_in_past_one_sample_a_column_still_names_a_sample() {
        let m = map();
        let mut view = View::new(16.0);
        view.zoom = MAX_ZOOM;
        let vp = Viewport::resolve(&view, &m, at_bars(&m, 40.0), 1600);
        assert!(vp.frames_per_column(&m) < 1.0);
        let mut cols = Vec::new();
        vp.columns(&m, &mut cols);
        assert!(
            cols.iter().all(|c| c.count >= 1),
            "a column narrower than a sample must still name one, or the trace goes blank"
        );
    }

    #[test]
    fn a_tempo_change_does_not_move_the_lap_lines() {
        // The lap is a whole number of bars whatever the tempo does, which is the property the
        // whole wrap-mode display rests on.
        let mut m = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
        m.push(m.frame_at_bars(20.0), 174.0, Meter::FOUR_FOUR);
        let view = View::new(16.0);
        for bars in [4.0, 15.99, 16.0, 24.0, 32.0, 47.5] {
            let vp = Viewport::resolve(&view, &m, m.frame_at_bars(bars), W);
            let within = vp.head_bars - vp.lap_start_bars;
            assert!(
                (0.0..16.0 + 1e-6).contains(&within),
                "at {bars} bars the head is {within} into a 16-bar lap"
            );
            assert!((vp.lap_start_bars % 16.0).abs() < 1e-6, "lap start is not on a bar line");
        }
    }
}
