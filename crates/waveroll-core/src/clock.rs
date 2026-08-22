//! Transport, tempo estimation, and the rule that decides whether a block is captured at all.
//!
//! Five things can tell Waveroll what time it is — a plugin host, Ableton Link, MIDI clock, a
//! tapped or typed tempo, and (last resort) onset detection. They differ enormously in what they
//! know: a host hands over an exact sample position and a time signature, while MIDI clock is a
//! metronome that has never heard of bars. [`Transport`] is the shape they all reduce to, and
//! nothing downstream of [`CaptureClock`] knows which one is plugged in.
//!
//! The MIDI-clock estimator is here too, because it is the only source that has to *derive* a
//! tempo rather than being told one.

use crate::tempo::{Meter, TempoMap};

/// What a clock source reports, once per audio block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transport {
    /// Whether the host is rolling. Capture follows this, so `false` means the write head freezes.
    pub playing: bool,
    /// Quarter notes per minute.
    pub bpm: f64,
    pub meter: Meter,
    /// True during an offline bounce.
    ///
    /// This exists because of a failure that is invisible until it has already cost somebody a
    /// take: exporting a track runs the plugin far faster than realtime with the transport
    /// reporting "playing" the whole way, so transport-gated capture would swallow the entire
    /// render and leave the window holding the last sixteen bars of the export. No buffering
    /// scheme helps — an A/B pair fills both halves just as fast. Only refusing to write does.
    pub offline: bool,
}

impl Transport {
    pub fn stopped(bpm: f64, meter: Meter) -> Transport {
        Transport { playing: false, bpm, meter, offline: false }
    }
    pub fn rolling(bpm: f64, meter: Meter) -> Transport {
        Transport { playing: true, bpm, meter, offline: false }
    }
}

/// Anything that can say what time it is. Polled once per block by the capture loop.
pub trait ClockSource {
    fn poll(&mut self) -> Transport;
}

/// Owns the tempo map and decides how much of each block reaches the ring.
///
/// The counter it maintains is **capture frames**: elapsed transport time, monotonic, gapless, and
/// unaware that a locate ever happened. A DAW looping four bars fills the window with four passes
/// of the same bars, and ruling the grid in song position would make the bar numbers run backwards
/// halfway across the screen. Song position, if it is wanted, is display metadata hung off the
/// side — never the axis.
#[derive(Debug)]
pub struct CaptureClock {
    map: TempoMap,
    captured: u64,
    playing: bool,
    /// Blocks refused because the host was rendering offline. Surfaced so the panel can say
    /// "paused during export" rather than appearing to have died.
    offline_blocks: u64,
}

impl CaptureClock {
    pub fn new(sample_rate: u32, bpm: f64, meter: Meter) -> CaptureClock {
        CaptureClock {
            map: TempoMap::new(sample_rate, bpm, meter),
            captured: 0,
            playing: false,
            offline_blocks: 0,
        }
    }

    pub fn map(&self) -> &TempoMap { &self.map }

    /// Mutable access, for the one thing that legitimately reaches in from outside: placing the
    /// downbeat by hand. Everything else about the map is derived from the transport.
    pub fn map_mut(&mut self) -> &mut TempoMap { &mut self.map }
    /// Frames captured so far. This is the ring's absolute frame index, and the grid's x-axis.
    pub fn captured(&self) -> u64 { self.captured }
    pub fn is_playing(&self) -> bool { self.playing }
    pub fn offline_blocks(&self) -> u64 { self.offline_blocks }

    /// Call once per block, before writing to the ring. Returns how many of `frames` to capture:
    /// all of them, or none.
    ///
    /// Partial capture is deliberately not offered. A transport that starts mid-block is out by at
    /// most one buffer — under a millisecond at any sane size — and the calibration offset exists
    /// to absorb exactly that. Splitting a block would buy back a fraction of that error in
    /// exchange for a seam in the ring and a second code path through the hottest loop here.
    pub fn advance(&mut self, transport: &Transport, frames: usize) -> usize {
        if transport.offline {
            self.offline_blocks += 1;
            self.playing = transport.playing;
            return 0;
        }
        self.playing = transport.playing;
        if !transport.playing || frames == 0 {
            return 0;
        }
        // The change is recorded at the frame it takes effect, which is the start of this block —
        // recording it at the end would attribute a block of audio to the wrong tempo.
        self.map.push(self.captured, transport.bpm, transport.meter);
        self.captured += frames as u64;
        frames
    }

    /// Bars elapsed at the write head.
    pub fn head_bars(&self) -> f64 { self.map.bars_at(self.captured) }
}

// ---------------------------------------------------------------------------------------
// MIDI clock
// ---------------------------------------------------------------------------------------

/// The MIDI messages that carry transport. Everything else is somebody else's problem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockMessage {
    /// `0xF8`. Twenty-four of these to the quarter note.
    Tick,
    /// `0xFA`. Rewind to zero and roll.
    Start,
    /// `0xFB`. Roll from wherever the last Song Position put us.
    Continue,
    /// `0xFC`.
    Stop,
    /// `0xF2`, in sixteenth notes from the top of the song.
    ///
    /// This is the only way MIDI clock can express a position, it is sent on a locate rather than
    /// continuously, and plenty of hosts never send it at all — which is why bar one may have to
    /// be established by hand.
    SongPosition(u16),
}

/// Decodes one MIDI packet, or `None` for anything that is not transport.
///
/// System realtime bytes are permitted to appear *inside* other messages, so a decoder that
/// assumed a packet boundary would drop ticks under load — exactly when the timing matters most.
pub fn decode_clock(data: &[u8]) -> Option<ClockMessage> {
    match data.first().copied()? {
        0xF8 => Some(ClockMessage::Tick),
        0xFA => Some(ClockMessage::Start),
        0xFB => Some(ClockMessage::Continue),
        0xFC => Some(ClockMessage::Stop),
        0xF2 if data.len() >= 3 => {
            Some(ClockMessage::SongPosition(u16::from(data[1]) | (u16::from(data[2]) << 7)))
        }
        _ => None,
    }
}

/// Ticks per quarter note in the MIDI clock stream. Fixed by the specification.
pub const CLOCK_PPQ: f64 = 24.0;

/// How many recent tick timestamps the estimator holds. Twenty-four is one quarter note, which is
/// both a natural musical unit and enough lever arm to average the noise down usefully.
const WINDOW: usize = 24;

/// Estimates tempo from a MIDI clock stream.
///
/// **Least squares over a window of timestamps, not an average of intervals.** The obvious design
/// — median-filter the inter-tick intervals to reject jitter, then smooth — has a bias that only
/// shows up at tempi whose tick interval is not a whole number of samples, which is most of them.
/// At 174 BPM a tick is 689.655 samples, so the measured intervals alternate between 689 and 690
/// and the median returns one of them; it never returns 689.655. That is a *systematic* 0.145%
/// error, which is 0.87 seconds of drift over ten minutes, and no amount of smoothing removes it
/// because it is not noise.
///
/// Fitting a line through the last twenty-four timestamps instead gives a slope with sub-sample
/// resolution, reduces zero-mean jitter by the lever arm of the window rather than by luck, and
/// still tracks a real tempo change within half a beat. Gross discontinuities — the stream
/// stopping, a cable going in — are caught before the fit by a plausibility gate, because a gap is
/// not a tempo and folding one into the fit throws the estimate somewhere absurd.
///
/// The estimator works in **samples**, not milliseconds, because everything downstream is indexed
/// in frames and converting once at the boundary beats converting at every use.
#[derive(Clone, Debug)]
pub struct ClockPll {
    sample_rate: f64,
    stamps: [u64; WINDOW],
    /// Which tick each stamp *is*, not merely the order it arrived in. A dropped tick advances
    /// this by two, so the fit stays honest instead of reading the gap as a slower tempo.
    indices: [f64; WINDOW],
    index: f64,
    filled: usize,
    next: usize,
    last_tick: Option<u64>,
    bpm: f64,
    /// Ticks seen since the last Start or Continue. Position, in the only unit MIDI clock has.
    ticks: u64,
    playing: bool,
    /// How many intervals were rejected as impossible. A healthy stream reports zero.
    rejected: u64,
}

impl ClockPll {
    pub fn new(sample_rate: u32, initial_bpm: f64) -> ClockPll {
        ClockPll {
            sample_rate: sample_rate as f64,
            stamps: [0; WINDOW],
            indices: [0.0; WINDOW],
            index: 0.0,
            filled: 0,
            next: 0,
            last_tick: None,
            bpm: initial_bpm,
            ticks: 0,
            playing: false,
            rejected: 0,
        }
    }

    pub fn bpm(&self) -> f64 { self.bpm }
    pub fn is_playing(&self) -> bool { self.playing }
    pub fn rejected(&self) -> u64 { self.rejected }
    /// Quarter notes since the last Start or Continue.
    pub fn quarters(&self) -> f64 { self.ticks as f64 / CLOCK_PPQ }
    /// How much of the fitting window is populated. Below three the estimate is still the initial
    /// guess, which the panel should say rather than presenting as a measurement.
    pub fn settled(&self) -> bool { self.filled >= 3 }

    /// The interval bounds that count as a plausible clock tick, in samples.
    ///
    /// Anything outside 20-999 BPM is not a tempo, it is a gap: the stream stopping and restarting,
    /// a cable being plugged in, or the host being suspended.
    fn plausible(&self) -> (f64, f64) {
        let at = |bpm: f64| 60.0 * self.sample_rate / (bpm * CLOCK_PPQ);
        (at(999.0), at(20.0))
    }

    /// Feeds one message, stamped with the frame it arrived at.
    ///
    /// `at_frame` should come from the audio timeline, not from a wall clock. In a browser that
    /// means pairing `performance.now()` with `AudioContext.getOutputTimestamp()`; a few
    /// milliseconds of constant skew here is the kind of error nobody ever locates.
    pub fn feed(&mut self, message: ClockMessage, at_frame: u64) {
        match message {
            ClockMessage::Tick => {
                let mut steps = 1.0;
                if let Some(previous) = self.last_tick {
                    let interval = at_frame.saturating_sub(previous) as f64;
                    let (lo, hi) = self.plausible();
                    if interval < lo || interval > hi {
                        // Discard the window with the gap. The next tick starts a fresh fit rather
                        // than being regressed against what was true before the stream broke.
                        self.rejected += 1;
                        self.filled = 0;
                        self.next = 0;
                        self.index = 0.0;
                    } else if self.settled() {
                        // A tick can go missing without the stream being broken — a busy sender, a
                        // slipped USB frame. Treating that interval as one tick would read as a
                        // tempo drop of exactly a half, so infer how many ticks it actually spans.
                        //
                        // Two guards, both load-bearing. The inference is only trusted once the fit
                        // is, because it is circular: bootstrapping from a wrong initial guess, a
                        // 60 BPM stream against a 120 BPM prior reads every interval as two ticks
                        // and locks the error in permanently. And a ratio that is not *close* to an
                        // integer is jitter or a tempo change rather than a gap, and one tick is
                        // the safer reading of both.
                        let ratio = interval / self.expected_interval();
                        let nearest = ratio.round().clamp(1.0, 8.0);
                        if nearest < 2.0 || (ratio - nearest).abs() <= 0.15 {
                            steps = nearest;
                        }
                    }
                }
                self.index += steps;
                self.push_stamp(at_frame);
                self.refit();
                self.last_tick = Some(at_frame);
                if self.playing {
                    self.ticks += 1;
                }
            }
            ClockMessage::Start => {
                self.playing = true;
                self.ticks = 0;
                self.last_tick = None;
            }
            ClockMessage::Continue => {
                self.playing = true;
                self.last_tick = None;
            }
            ClockMessage::Stop => {
                self.playing = false;
                self.last_tick = None;
            }
            ClockMessage::SongPosition(sixteenths) => {
                // Six clocks to a sixteenth note.
                self.ticks = u64::from(sixteenths) * 6;
            }
        }
    }

    /// The tick interval implied by the current estimate, in samples.
    fn expected_interval(&self) -> f64 {
        60.0 * self.sample_rate / (self.bpm * CLOCK_PPQ)
    }

    fn push_stamp(&mut self, at: u64) {
        self.stamps[self.next] = at;
        self.indices[self.next] = self.index;
        self.next = (self.next + 1) % WINDOW;
        self.filled = (self.filled + 1).min(WINDOW);
    }

    /// Ordinary least squares on (tick index, timestamp), giving samples per tick.
    ///
    /// The general form rather than the closed one, because the indices are no longer guaranteed
    /// to be 0, 1, 2, ... — a dropped tick leaves a hole in them, which is exactly the information
    /// that keeps the gap from being read as a slower tempo.
    fn refit(&mut self) {
        let n = self.filled;
        if n < 3 {
            return;
        }
        // Oldest first. Before the ring has wrapped, `next` equals `n` and this is just index 0.
        let start = if n < WINDOW { 0 } else { self.next };
        let slot = |k: usize| (start + k) % WINDOW;

        let mut mean_index = 0.0;
        let mut mean_time = 0.0;
        for k in 0..n {
            mean_index += self.indices[slot(k)];
            mean_time += self.stamps[slot(k)] as f64;
        }
        mean_index /= n as f64;
        mean_time /= n as f64;

        let mut covariance = 0.0;
        let mut variance = 0.0;
        for k in 0..n {
            let dx = self.indices[slot(k)] - mean_index;
            covariance += dx * (self.stamps[slot(k)] as f64 - mean_time);
            variance += dx * dx;
        }
        if variance <= 0.0 {
            return;
        }
        let slope = covariance / variance;
        if slope > 0.0 && slope.is_finite() {
            self.bpm = 60.0 * self.sample_rate / (slope * CLOCK_PPQ);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    /// Frames between clock ticks at a given tempo.
    fn tick_interval(bpm: f64) -> f64 {
        60.0 * SR as f64 / (bpm * CLOCK_PPQ)
    }

    fn feed_steady(pll: &mut ClockPll, bpm: f64, ticks: usize, start_at: u64) -> u64 {
        let step = tick_interval(bpm);
        let mut at = start_at as f64;
        for _ in 0..ticks {
            pll.feed(ClockMessage::Tick, at.round() as u64);
            at += step;
        }
        at as u64
    }

    // ---- decoding ----

    #[test]
    fn transport_bytes_decode() {
        assert_eq!(decode_clock(&[0xF8]), Some(ClockMessage::Tick));
        assert_eq!(decode_clock(&[0xFA]), Some(ClockMessage::Start));
        assert_eq!(decode_clock(&[0xFB]), Some(ClockMessage::Continue));
        assert_eq!(decode_clock(&[0xFC]), Some(ClockMessage::Stop));
        assert_eq!(decode_clock(&[0x90, 60, 100]), None);
        assert_eq!(decode_clock(&[]), None);
    }

    #[test]
    fn song_position_is_two_seven_bit_halves() {
        // Bar 5 in 4/4 is 64 sixteenths: 64 = 0x40 -> lsb 0x40, msb 0.
        assert_eq!(decode_clock(&[0xF2, 0x40, 0x00]), Some(ClockMessage::SongPosition(64)));
        // And the msb really is shifted by seven, not eight.
        assert_eq!(decode_clock(&[0xF2, 0x00, 0x01]), Some(ClockMessage::SongPosition(128)));
        assert_eq!(decode_clock(&[0xF2, 0x7F, 0x7F]), Some(ClockMessage::SongPosition(16_383)));
        assert_eq!(decode_clock(&[0xF2, 0x40]), None, "a truncated SPP is not a position");
    }

    // ---- the estimator ----

    #[test]
    fn a_clean_stream_converges_on_its_tempo() {
        for bpm in [60.0, 90.0, 120.0, 128.0, 174.0] {
            let mut pll = ClockPll::new(SR, 120.0);
            feed_steady(&mut pll, bpm, 48, 0);
            assert!(
                (pll.bpm() - bpm).abs() < 0.05,
                "expected {bpm}, estimated {}",
                pll.bpm()
            );
            assert_eq!(pll.rejected(), 0, "a clean stream must reject nothing");
        }
    }

    #[test]
    fn jitter_does_not_reach_the_estimate() {
        // +/- 1 ms of jitter, which is a fair bit worse than a healthy IAC bus.
        let mut pll = ClockPll::new(SR, 120.0);
        let step = tick_interval(128.0);
        let jitter: [f64; 10] = [0.0, 48.0, -48.0, 20.0, -35.0, 12.0, -12.0, 44.0, -44.0, 5.0];
        let mut at = 100_000.0;
        for i in 0..200 {
            let stamp = (at + jitter[i % jitter.len()]).max(0.0);
            pll.feed(ClockMessage::Tick, stamp.round() as u64);
            at += step;
        }
        assert!(
            (pll.bpm() - 128.0).abs() < 0.5,
            "jitter moved the estimate to {}",
            pll.bpm()
        );
    }

    #[test]
    fn one_scheduling_hiccup_does_not_spike_the_tempo() {
        let mut pll = ClockPll::new(SR, 120.0);
        let end = feed_steady(&mut pll, 120.0, 40, 0);
        let settled = pll.bpm();
        // The tick due at `end` arrives 3 ms late; the stream then carries on unshifted.
        pll.feed(ClockMessage::Tick, end + 144);
        feed_steady(&mut pll, 120.0, 20, end + tick_interval(120.0).round() as u64);
        assert!(
            (pll.bpm() - settled).abs() < 1.0,
            "a single late tick moved the tempo from {settled} to {}",
            pll.bpm()
        );
    }

    #[test]
    fn a_dropped_tick_is_not_a_tempo_change() {
        // A tick going missing leaves an interval of exactly two, which a fit that assumed every
        // stamp was consecutive would read as half the tempo.
        let mut pll = ClockPll::new(SR, 120.0);
        let step = tick_interval(128.0);
        let mut at: f64 = 0.0;
        for i in 0..240 {
            if i % 17 != 0 || i == 0 {
                pll.feed(ClockMessage::Tick, at.round() as u64);
            }
            at += step;
        }
        assert!(
            (pll.bpm() - 128.0).abs() < 0.5,
            "one tick in seventeen dropped moved the estimate to {}",
            pll.bpm()
        );
        assert_eq!(pll.rejected(), 0, "a dropped tick is a hiccup, not a broken stream");
    }

    #[test]
    fn a_real_tempo_change_is_tracked() {
        let mut pll = ClockPll::new(SR, 120.0);
        let end = feed_steady(&mut pll, 120.0, 48, 0);
        feed_steady(&mut pll, 140.0, 96, end);
        assert!(
            (pll.bpm() - 140.0).abs() < 1.0,
            "after four beats at the new tempo the estimate is still {}",
            pll.bpm()
        );
    }

    #[test]
    fn a_gap_is_rejected_rather_than_averaged() {
        let mut pll = ClockPll::new(SR, 120.0);
        let end = feed_steady(&mut pll, 120.0, 48, 0);
        // The stream stops for four seconds, then resumes at the same tempo.
        let resumed = end + SR as u64 * 4;
        feed_steady(&mut pll, 120.0, 48, resumed);
        assert_eq!(pll.rejected(), 1, "the four-second gap should have been rejected once");
        assert!(
            (pll.bpm() - 120.0).abs() < 0.05,
            "the gap leaked into the estimate: {}",
            pll.bpm()
        );
    }

    #[test]
    fn start_rewinds_and_stop_freezes_position() {
        let mut pll = ClockPll::new(SR, 120.0);
        pll.feed(ClockMessage::Start, 0);
        assert!(pll.is_playing());
        feed_steady(&mut pll, 120.0, 24, 0);
        assert!((pll.quarters() - 1.0).abs() < 1e-9, "24 ticks is one quarter");
        pll.feed(ClockMessage::Stop, 100_000);
        assert!(!pll.is_playing());
        let held = pll.quarters();
        feed_steady(&mut pll, 120.0, 24, 200_000);
        assert_eq!(pll.quarters(), held, "ticks while stopped must not advance position");
        pll.feed(ClockMessage::Start, 300_000);
        assert_eq!(pll.quarters(), 0.0, "Start means bar one");
    }

    #[test]
    fn continue_resumes_from_a_song_position() {
        let mut pll = ClockPll::new(SR, 120.0);
        pll.feed(ClockMessage::SongPosition(64), 0); // bar 5 in 4/4
        pll.feed(ClockMessage::Continue, 0);
        assert!((pll.quarters() - 16.0).abs() < 1e-9, "64 sixteenths is 16 quarters");
        assert!(pll.is_playing());
    }

    // ---- the capture rule ----

    #[test]
    fn a_stopped_transport_captures_nothing() {
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let stopped = Transport::stopped(120.0, Meter::FOUR_FOUR);
        for _ in 0..100 {
            assert_eq!(clock.advance(&stopped, 128), 0);
        }
        assert_eq!(clock.captured(), 0);
    }

    #[test]
    fn an_offline_render_captures_nothing_even_though_it_is_playing() {
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let rolling = Transport::rolling(120.0, Meter::FOUR_FOUR);
        for _ in 0..10 {
            clock.advance(&rolling, 512);
        }
        let captured_before = clock.captured();

        // Now the host bounces: twenty minutes of song at 200x, transport reporting "playing".
        let bounce = Transport { offline: true, ..rolling };
        for _ in 0..100_000 {
            assert_eq!(clock.advance(&bounce, 512), 0);
        }
        assert_eq!(
            clock.captured(),
            captured_before,
            "the export overwrote the take — this is the bug the offline guard exists for"
        );
        assert_eq!(clock.offline_blocks(), 100_000);

        // And it recovers afterwards.
        assert_eq!(clock.advance(&rolling, 512), 512);
    }

    #[test]
    fn capture_frames_are_monotonic_through_a_scripted_session() {
        // play, stop, locate backwards, loop the same bars, ramp the tempo, bounce, resume.
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let mut previous_bars = -1.0;
        let check = |clock: &CaptureClock, previous: &mut f64| {
            let bars = clock.head_bars();
            assert!(bars >= *previous, "bars went backwards: {bars} after {previous}");
            *previous = bars;
        };

        let rolling = Transport::rolling(120.0, Meter::FOUR_FOUR);
        for _ in 0..400 { clock.advance(&rolling, 512); check(&clock, &mut previous_bars); }
        let stopped = Transport::stopped(120.0, Meter::FOUR_FOUR);
        for _ in 0..50 { clock.advance(&stopped, 512); check(&clock, &mut previous_bars); }
        // A locate backwards changes nothing here, which is the whole point: capture time does not
        // know where the playhead went.
        for _ in 0..400 { clock.advance(&rolling, 512); check(&clock, &mut previous_bars); }
        // Tempo ramp.
        for i in 0..200 {
            let t = Transport::rolling(120.0 + i as f64 * 0.25, Meter::FOUR_FOUR);
            clock.advance(&t, 512);
            check(&clock, &mut previous_bars);
        }
        let bounce = Transport { offline: true, ..rolling };
        for _ in 0..1000 { clock.advance(&bounce, 512); check(&clock, &mut previous_bars); }
        for _ in 0..100 { clock.advance(&rolling, 512); check(&clock, &mut previous_bars); }

        assert!(clock.head_bars() > 0.0);
    }

    #[test]
    fn a_held_tempo_does_not_grow_the_map() {
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let rolling = Transport::rolling(120.0, Meter::FOUR_FOUR);
        for _ in 0..5000 {
            clock.advance(&rolling, 128);
        }
        assert_eq!(clock.map().len(), 1);
    }
}
