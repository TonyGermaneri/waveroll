//! The tempo map: the only thing that knows how to turn a frame into a bar.
//!
//! Two decisions shape all of it.
//!
//! **It is keyed on capture frames, not on song position.** Capture follows the host transport, so
//! a four-bar loop in the DAW fills the window with four passes of the same bars. If the grid were
//! ruled in song position the bar numbers would run backwards in the middle of the screen. What is
//! accumulated here is *elapsed transport time* — monotonic, gapless, and unaware that a locate
//! ever happened. Song position, if it is wanted at all, is display metadata hung off the side.
//!
//! **A tempo change does not rewrite history.** Store one BPM and every bar line drawn over audio
//! captured at a different tempo is wrong. So each observed change appends an entry and the older
//! ones stay exactly as they were; the graticule is drawn from the map, not from "the tempo".
//!
//! Beats are counted in **quarter notes**, because that is what BPM has always meant, and bars are
//! derived from the meter rather than assumed to be four of them. A bar in 6/8 is three quarters,
//! not six, and a map that got that wrong would put its bar lines in musically plausible but
//! incorrect places — the failure mode that never looks like a bug.

/// A time signature. `den` is the note value that gets the beat, as written: the 8 in 6/8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meter {
    pub num: u32,
    pub den: u32,
}

impl Meter {
    pub const FOUR_FOUR: Meter = Meter { num: 4, den: 4 };

    pub fn new(num: u32, den: u32) -> Meter {
        assert!(num > 0 && den > 0, "a meter needs a non-zero numerator and denominator");
        Meter { num, den }
    }

    /// How many quarter notes long one bar is. 4/4 is four; 6/8 is three; 7/8 is three and a half.
    pub fn quarters_per_bar(&self) -> f64 {
        self.num as f64 * 4.0 / self.den as f64
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    frame: u64,
    bpm: f64,
    meter: Meter,
    /// Quarter notes elapsed at `frame`. Precomputed so a lookup is a binary search, not a fold.
    quarters: f64,
    /// Bars elapsed at `frame`, likewise.
    bars: f64,
}

/// Tempo and meter over capture time, as a list of segments.
#[derive(Clone, Debug)]
pub struct TempoMap {
    sample_rate: f64,
    entries: Vec<Entry>,
}

impl TempoMap {
    pub fn new(sample_rate: u32, bpm: f64, meter: Meter) -> TempoMap {
        assert!(sample_rate > 0, "sample rate must be positive");
        assert!(bpm.is_finite() && bpm > 0.0, "bpm must be finite and positive, got {bpm}");
        TempoMap {
            sample_rate: sample_rate as f64,
            entries: vec![Entry { frame: 0, bpm, meter, quarters: 0.0, bars: 0.0 }],
        }
    }

    pub fn sample_rate(&self) -> f64 { self.sample_rate }

    /// Records a tempo or meter change observed at `frame`.
    ///
    /// A change identical to what is already in force is dropped, so a clock that re-reports the
    /// same BPM every beat does not grow the map without bound. A change at a frame the map has
    /// already passed is a bug in the caller and panics rather than corrupting the timeline.
    pub fn push(&mut self, frame: u64, bpm: f64, meter: Meter) {
        assert!(bpm.is_finite() && bpm > 0.0, "bpm must be finite and positive, got {bpm}");
        let last = *self.entries.last().expect("a map always has at least one entry");
        assert!(frame >= last.frame, "tempo changes must be appended in order");
        if (bpm - last.bpm).abs() < 1e-9 && meter == last.meter {
            return;
        }
        if frame == last.frame {
            // Two changes at the same instant: the later one wins outright rather than creating a
            // zero-length segment that every lookup would have to skip past.
            let e = self.entries.last_mut().expect("checked above");
            e.bpm = bpm;
            e.meter = meter;
            return;
        }
        let quarters = last.quarters + self.quarters_between(last, frame);
        let bars = last.bars + self.quarters_between(last, frame) / last.meter.quarters_per_bar();
        self.entries.push(Entry { frame, bpm, meter, quarters, bars });
    }

    fn quarters_between(&self, from: Entry, frame: u64) -> f64 {
        (frame - from.frame) as f64 * from.bpm / (60.0 * self.sample_rate)
    }

    fn segment_at_frame(&self, frame: u64) -> Entry {
        match self.entries.binary_search_by(|e| e.frame.cmp(&frame)) {
            Ok(i) => self.entries[i],
            // `binary_search_by` returns the insertion point; the segment in force is the one
            // before it. Index 0 cannot be the insertion point because the first entry is at
            // frame 0 and frames are unsigned.
            Err(i) => self.entries[i - 1],
        }
    }

    fn segment_at_bar(&self, bars: f64) -> Entry {
        let mut chosen = self.entries[0];
        for e in &self.entries {
            if e.bars <= bars {
                chosen = *e;
            } else {
                break;
            }
        }
        chosen
    }

    pub fn bpm_at(&self, frame: u64) -> f64 { self.segment_at_frame(frame).bpm }
    pub fn meter_at(&self, frame: u64) -> Meter { self.segment_at_frame(frame).meter }

    /// Quarter notes elapsed at `frame`.
    pub fn quarters_at(&self, frame: u64) -> f64 {
        let e = self.segment_at_frame(frame);
        e.quarters + self.quarters_between(e, frame)
    }

    /// Bars elapsed at `frame`. Fractional — 6.25 is a quarter of the way into bar seven.
    pub fn bars_at(&self, frame: u64) -> f64 {
        let e = self.segment_at_frame(frame);
        e.bars + self.quarters_between(e, frame) / e.meter.quarters_per_bar()
    }

    /// The inverse of [`bars_at`], rounded to the nearest frame.
    ///
    /// Rounding rather than truncating matters: snapping computes in bars and comes back here, so
    /// a floor would drift the grid one sample earlier on every conversion.
    pub fn frame_at_bars(&self, bars: f64) -> u64 {
        let bars = bars.max(0.0);
        let e = self.segment_at_bar(bars);
        let quarters = (bars - e.bars) * e.meter.quarters_per_bar();
        let frames = quarters * 60.0 * self.sample_rate / e.bpm;
        e.frame + frames.round().max(0.0) as u64
    }

    /// How many frames one bar lasts, at the tempo in force at `frame`.
    pub fn frames_per_bar_at(&self, frame: u64) -> f64 {
        let e = self.segment_at_frame(frame);
        e.meter.quarters_per_bar() * 60.0 * self.sample_rate / e.bpm
    }

    /// Tempo and meter changes strictly inside `(start, end)`, oldest first.
    ///
    /// Exported clips need these: a MIDI file whose tempo track stops at the first value would play
    /// back at the wrong speed for everything after a change, and unlike audio there is no waveform
    /// to notice it against.
    pub fn changes_in(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, f64, Meter)> + '_ {
        self.entries
            .iter()
            .filter(move |e| e.frame > start && e.frame < end)
            .map(|e| (e.frame, e.bpm, e.meter))
    }

    /// Number of segments. Only useful for asserting that a clock is not appending noise.
    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { false }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    #[test]
    fn a_bar_at_120_is_two_seconds() {
        let m = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
        assert!((m.frames_per_bar_at(0) - 96_000.0).abs() < 1e-6);
        assert!((m.bars_at(96_000) - 1.0).abs() < 1e-12);
        assert_eq!(m.frame_at_bars(1.0), 96_000);
    }

    #[test]
    fn sixteen_bars_at_the_documented_tempi() {
        // The figures the plan quotes, checked against the code rather than against themselves.
        for (bpm, seconds) in [(90.0, 42.666_666), (120.0, 32.0), (174.0, 22.068_965)] {
            let m = TempoMap::new(SR, bpm, Meter::FOUR_FOUR);
            let frames = m.frame_at_bars(16.0) as f64;
            assert!(
                (frames / SR as f64 - seconds).abs() < 1e-3,
                "16 bars at {bpm} should be {seconds}s, got {}s",
                frames / SR as f64
            );
        }
    }

    #[test]
    fn six_eight_is_three_quarters_to_the_bar() {
        assert!((Meter::new(6, 8).quarters_per_bar() - 3.0).abs() < 1e-12);
        assert!((Meter::new(7, 8).quarters_per_bar() - 3.5).abs() < 1e-12);
        let m = TempoMap::new(SR, 120.0, Meter::new(6, 8));
        // Three quarters at 120 bpm is 1.5 s.
        assert!((m.frames_per_bar_at(0) - 72_000.0).abs() < 1e-6);
    }

    #[test]
    fn history_keeps_its_own_tempo() {
        let mut m = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
        // Four bars at 120, then double the tempo.
        let change = m.frame_at_bars(4.0);
        m.push(change, 240.0, Meter::FOUR_FOUR);
        assert_eq!(m.len(), 2);
        // The old region still reads at the old tempo.
        assert!((m.bars_at(96_000) - 1.0).abs() < 1e-9, "bar 1 must not move when bar 5 changes");
        // And the new region runs twice as fast: four more bars in half the frames.
        let eight = m.frame_at_bars(8.0);
        assert_eq!(eight - change, 4 * 48_000);
    }

    #[test]
    fn bars_and_frames_round_trip() {
        let mut m = TempoMap::new(SR, 128.0, Meter::FOUR_FOUR);
        m.push(m.frame_at_bars(3.0), 91.5, Meter::new(3, 4));
        m.push(m.frame_at_bars(9.0), 174.0, Meter::new(7, 8));
        // A crude deterministic sweep beats a random one here: it visits every segment boundary.
        let mut bars = 0.0;
        while bars < 24.0 {
            let frame = m.frame_at_bars(bars);
            let back = m.bars_at(frame);
            let tolerance = 2.0 / m.frames_per_bar_at(frame); // one frame, expressed in bars
            assert!(
                (back - bars).abs() < tolerance,
                "{bars} bars -> frame {frame} -> {back} bars, off by more than a frame"
            );
            bars += 0.125;
        }
    }

    #[test]
    fn bars_never_decrease() {
        let mut m = TempoMap::new(SR, 100.0, Meter::FOUR_FOUR);
        m.push(240_000, 60.0, Meter::new(5, 4));
        m.push(600_000, 200.0, Meter::FOUR_FOUR);
        let mut previous = -1.0;
        for frame in (0..1_200_000).step_by(997) {
            let bars = m.bars_at(frame);
            assert!(bars >= previous, "bars went backwards at frame {frame}");
            previous = bars;
        }
    }

    #[test]
    fn an_unchanged_tempo_does_not_grow_the_map() {
        let mut m = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
        for beat in 1..500u64 {
            m.push(beat * 24_000, 120.0, Meter::FOUR_FOUR);
        }
        assert_eq!(m.len(), 1, "a clock re-reporting the same tempo must not append");
    }

    #[test]
    #[should_panic(expected = "in order")]
    fn a_change_in_the_past_is_refused() {
        let mut m = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
        m.push(100_000, 130.0, Meter::FOUR_FOUR);
        m.push(50_000, 140.0, Meter::FOUR_FOUR);
    }
}
