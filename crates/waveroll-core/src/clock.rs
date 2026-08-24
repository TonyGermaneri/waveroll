//! Transport, tempo estimation, and the rules that decide whether a block is captured at all and
//! where it lands on the grid when it is.
//!
//! Five things can tell Waveroll what time it is — a plugin host, Ableton Link, MIDI clock, a
//! tapped or typed tempo, and (last resort) onset detection. They differ enormously in what they
//! know: a host hands over an exact sample position and a time signature, while MIDI clock is a
//! metronome that has never heard of bars. [`Transport`] is the shape they all reduce to, and
//! nothing downstream of [`CaptureClock`] knows which one is plugged in.
//!
//! The MIDI-clock estimator is here too, because it is the only source that has to *derive* a
//! tempo rather than being told one.
//!
//! The second rule is the splice: a transport that stops and starts again has almost certainly
//! moved, and audio captured either side of that is adjacent in the ring while being bars apart in
//! the song. [`CaptureClock`] detects it and says how much dead time to lay down so the bar lines
//! carry on landing where the music does.

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
    /// Where the playhead is, in quarter notes from wherever the source counts from. `None` from
    /// a source that has no idea — which is most of them, most of the time.
    ///
    /// Only the *fraction of a bar* is ever used, and only at a splice. That is what makes a
    /// partial answer worth having: MIDI clock counts from the last `Start` rather than from the
    /// top of the song, and a host that reports a position at all reports one good to the sample.
    /// Neither is asked to agree with the other, or with itself across a stop.
    pub position: Option<f64>,
}

impl Transport {
    pub fn stopped(bpm: f64, meter: Meter) -> Transport {
        Transport { playing: false, bpm, meter, offline: false, position: None }
    }
    pub fn rolling(bpm: f64, meter: Meter) -> Transport {
        Transport { playing: true, bpm, meter, offline: false, position: None }
    }
    /// Rolling, and able to say where. `quarters` counts from the top of the song.
    pub fn at(bpm: f64, meter: Meter, quarters: f64) -> Transport {
        Transport { playing: true, bpm, meter, offline: false, position: Some(quarters) }
    }
}

/// Anything that can say what time it is. Polled once per block by the capture loop.
pub trait ClockSource {
    fn poll(&mut self) -> Transport;
}

/// How much of one block [`CaptureClock::advance`] decided to take, and what has to be laid down
/// in front of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Advance {
    /// Frames of silence to write to the ring **before** the block, to bring the grid back into
    /// phase with the song. Zero on every block but the first after a splice, and often zero
    /// there too.
    pub seam: u64,
    /// How many of the block's frames to capture: all of them, or none.
    pub frames: usize,
}

/// Owns the tempo map and decides how much of each block reaches the ring.
///
/// The counter it maintains is **capture frames**: elapsed transport time, monotonic, and unaware
/// that a locate ever happened. A DAW looping four bars fills the window with four passes of the
/// same bars, and ruling the grid in song position would make the bar numbers run backwards
/// halfway across the screen. Song position, if it is wanted, is display metadata hung off the
/// side — never the axis.
///
/// # Splices, and why capture time is no longer gapless
///
/// Capture time being *elapsed transport time* means the frame before a stop and the frame after
/// the next play are neighbours in the ring, however far apart the playhead jumped in between.
/// The grid is ruled from that axis, so the bar lines drawn over the resumed audio sit at whatever
/// phase the stop happened to leave behind. Press stop in almost any DAW and the playhead returns
/// to the arrangement start; press play again and the take now begins some arbitrary fraction of a
/// bar into a cell. Everything downstream inherits it: a "four bar" selection is four bars long
/// but starts in the middle of one, and the exported loop will not line up against anything.
///
/// So the axis is kept, and a **seam** is inserted at each splice: dead frames, fewer than one
/// bar of them, placed so that the first frame of resumed audio lands on the same fraction of a
/// bar the song is on. The alternative — leaving a hole in bar space and no frames to match —
/// gives back a four-bar clip holding three and a half bars of audio, which is the failure that
/// never looks like a bug.
///
/// Three properties fall out of doing it this way and all three are load-bearing:
///
/// * **Bars stay monotonic.** The correction is always forwards, to the *next* aligned position,
///   never back to the nearest one.
/// * **Frames and bars stay in step.** The seam is real frames, so `n` bars of selection is still
///   `n` bars of audio and still loops.
/// * **Pausing costs nothing.** A host that resumes where it stopped is already in phase, so the
///   seam computes to zero. Only a genuine jump spends any ring, which is the only case where
///   something genuinely happened.
///
/// The one thing the clock cannot do is lay the silence down itself — it does not own the ring —
/// so [`Advance::seam`] hands that to the caller, who must write it before the block.
#[derive(Debug)]
pub struct CaptureClock {
    map: TempoMap,
    captured: u64,
    playing: bool,
    /// Whether the previous block reached the ring. A block that captures after one that did not
    /// begins a new run of audio, whatever stopped the last one — a stop, an export, a host that
    /// handed over nothing.
    capturing: bool,
    /// Where the playhead should be at the start of the next block if the transport simply rolls
    /// on. A reported position far from this is a locate rather than the passage of time.
    expected: Option<f64>,
    /// Dead frames inserted at splices. Reported so a test — or a panel — can tell the difference
    /// between a gap in the picture and a fault in the capture.
    seam_frames: u64,
    /// Blocks refused because the host was rendering offline. Surfaced so the panel can say
    /// "paused during export" rather than appearing to have died.
    offline_blocks: u64,
}

/// How far a reported position may miss its prediction before it counts as a locate rather than
/// as jitter, in quarter notes.
///
/// A whole beat is deliberately loose. Under-detecting is nearly free — a loop that wraps on a bar
/// line is already in phase, so the seam it would have inserted is zero — while over-detecting on
/// a host with a noisy position report would spend ring on nothing at all.
const LOCATE_QUARTERS: f64 = 1.0;

impl CaptureClock {
    pub fn new(sample_rate: u32, bpm: f64, meter: Meter) -> CaptureClock {
        CaptureClock {
            map: TempoMap::new(sample_rate, bpm, meter),
            captured: 0,
            playing: false,
            capturing: false,
            expected: None,
            seam_frames: 0,
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
    /// Whether the last block reached the ring. Differs from [`is_playing`](Self::is_playing)
    /// during an export, which reports rolling and captures nothing.
    pub fn is_capturing(&self) -> bool { self.capturing }
    pub fn offline_blocks(&self) -> u64 { self.offline_blocks }
    /// Dead frames laid down at splices, over the life of the clock.
    pub fn seam_frames(&self) -> u64 { self.seam_frames }

    /// Call once per block, before writing to the ring. Says how many of `frames` to capture —
    /// all of them, or none — and how much silence to lay down in front of them.
    ///
    /// Partial capture is deliberately not offered. A transport that starts mid-block is out by at
    /// most one buffer — under a millisecond at any sane size — and the calibration offset exists
    /// to absorb exactly that. Splitting a block would buy back a fraction of that error in
    /// exchange for a discontinuity in the ring and a second code path through the hottest loop
    /// here.
    ///
    /// The caller must write [`Advance::seam`] frames of silence *before* the block's audio, and
    /// must not write it twice: the clock has already counted those frames.
    pub fn advance(&mut self, transport: &Transport, frames: usize) -> Advance {
        if transport.offline {
            self.offline_blocks += 1;
            self.playing = transport.playing;
            // An export is a gap in the take like any other, and the passage after it is somewhere
            // else entirely. Saying so here is what makes the resumption re-align.
            self.capturing = false;
            return Advance::default();
        }
        self.playing = transport.playing;
        if !transport.playing {
            self.capturing = false;
            return Advance::default();
        }
        if frames == 0 {
            // A block with nothing in it is not a stop, it is nothing at all — hosts hand one over
            // for a bypass or a suspended graph. Calling it a splice would spend most of a bar of
            // ring re-phasing a transport that never moved.
            return Advance::default();
        }
        // The change is recorded at the frame it takes effect, which is the start of this block —
        // recording it at the end would attribute a block of audio to the wrong tempo. It goes in
        // ahead of the seam so the dead frames are ruled at the tempo of the passage they
        // introduce rather than the one they follow, which is what makes the arithmetic below a
        // single segment's worth.
        self.map.push(self.captured, transport.bpm, transport.meter);

        let spliced = !self.capturing || self.located(transport.position);
        let seam = if spliced { self.realign(transport.position, frames) } else { 0 };
        self.captured += seam;
        self.seam_frames += seam;
        self.captured += frames as u64;
        self.capturing = true;
        self.expected = transport
            .position
            .map(|q| q + frames as f64 * transport.bpm / (60.0 * self.map.sample_rate()));
        Advance { seam, frames }
    }

    /// Whether the transport jumped rather than rolled on — a locate, a loop wrap, or a host
    /// coming back from somewhere else.
    ///
    /// Only answerable when the source reports a position at all *and* reported one last block.
    /// A source that says nothing is not silently assumed to have stayed put; it simply never
    /// triggers this, and the `capturing` edge catches the stops.
    fn located(&self, position: Option<f64>) -> bool {
        match (position, self.expected) {
            (Some(now), Some(expected)) => (now - expected).abs() > LOCATE_QUARTERS,
            _ => false,
        }
    }

    /// Frames of silence that would put the next captured frame on the same fraction of a bar the
    /// song is on. Never as much as a whole bar, and never negative.
    fn realign(&mut self, position: Option<f64>, frames: usize) -> u64 {
        let quarters_per_bar = self.map.meter_at(self.captured).quarters_per_bar();
        // A source with no position cannot say where the bar line is. The best available reading
        // of "play was pressed" is that it was pressed on one — the same assumption the manual
        // downbeat makes, and the only one MIDI clock's `Start` can support, since it resets the
        // count wherever the playhead happened to be.
        let phase = position.map_or(0.0, |q| (q / quarters_per_bar).rem_euclid(1.0));

        if self.captured == 0 {
            // Nothing captured yet, so no audio is holding the grid in place: move the grid
            // instead of the audio. Exact, and it spends no ring.
            self.map.set_bar_phase(-phase);
            return 0;
        }

        let per_bar = self.map.frames_per_bar_at(self.captured);
        let shift = (phase - self.map.bars_at(self.captured)).rem_euclid(1.0);
        // Nothing finer than a block can be placed at all — this runs once per block, and capture
        // begins on a block boundary — so a seam under one block is noise. A seam within a block
        // of a *whole* bar is the same noise seen from the other side: the head is a hair late
        // rather than most of a bar early, and chasing the next bar line would spend nearly a bar
        // of ring to correct a rounding error. Both round to no seam at all.
        let block = frames as f64;
        if shift * per_bar <= block || (1.0 - shift) * per_bar <= block {
            return 0;
        }
        (shift * per_bar).round() as u64
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
            assert_eq!(clock.advance(&stopped, 128).frames, 0);
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
            assert_eq!(clock.advance(&bounce, 512).frames, 0);
        }
        assert_eq!(
            clock.captured(),
            captured_before,
            "the export overwrote the take — this is the bug the offline guard exists for"
        );
        assert_eq!(clock.offline_blocks(), 100_000);

        // And it recovers afterwards.
        assert_eq!(clock.advance(&rolling, 512).frames, 512);
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

    // ---- splices ----

    const BLOCK: usize = 512;
    /// Frames in one 4/4 bar at 120 BPM.
    const BAR: f64 = 96_000.0;

    /// Rolls `blocks` blocks at 120 BPM, threading the song position along at the same rate.
    /// Returns where the playhead ends up, in quarter notes.
    fn roll(clock: &mut CaptureClock, from_quarters: f64, blocks: usize) -> f64 {
        let mut quarters = from_quarters;
        let per_block = BLOCK as f64 * 120.0 / (60.0 * SR as f64);
        for _ in 0..blocks {
            clock.advance(&Transport::at(120.0, Meter::FOUR_FOUR, quarters), BLOCK);
            quarters += per_block;
        }
        quarters
    }

    fn stop(clock: &mut CaptureClock, blocks: usize) {
        let stopped = Transport::stopped(120.0, Meter::FOUR_FOUR);
        for _ in 0..blocks {
            clock.advance(&stopped, BLOCK);
        }
    }

    /// Where the *first frame of the block just captured* sits within its bar, 0 to 1.
    fn head_phase(clock: &CaptureClock, frames: usize) -> f64 {
        clock.map().bars_at(clock.captured() - frames as u64).rem_euclid(1.0)
    }

    /// Where a song position sits within its bar, in 4/4.
    fn song_phase(quarters: f64) -> f64 {
        (quarters / 4.0).rem_euclid(1.0)
    }

    /// Two phases agree to within a block. Comparing modulo one bar, so 0.999 and 0.001 are close.
    fn in_phase(a: f64, b: f64) -> bool {
        let d = (a - b).rem_euclid(1.0);
        d.min(1.0 - d) * BAR <= BLOCK as f64
    }

    #[test]
    fn a_restart_somewhere_else_puts_the_bar_lines_back_on_the_music() {
        // The bug this exists for: stop in almost any DAW and the playhead returns to the top of
        // the arrangement. Press play again and capture carries on from the frame it stopped at,
        // so the grid rules the new take at whatever fraction of a bar the stop happened to leave.
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        roll(&mut clock, 0.0, 400);
        stop(&mut clock, 50);

        // Play again from bar 10 and a third — nothing to do with where it stopped.
        let restart = 37.3;
        let advance = clock.advance(&Transport::at(120.0, Meter::FOUR_FOUR, restart), BLOCK);

        assert!(advance.seam > 0, "the restart moved and nothing was inserted");
        assert!((advance.seam as f64) < BAR, "a splice may never cost a whole bar");
        assert!(
            in_phase(head_phase(&clock, BLOCK), song_phase(restart)),
            "the take resumes at {} of a bar, but the song is at {}",
            head_phase(&clock, BLOCK),
            song_phase(restart)
        );
    }

    #[test]
    fn a_pause_that_resumes_where_it_stopped_costs_nothing() {
        // The other half of the same rule, and the reason the correction is measured rather than
        // applied blindly: a host that really does pause is already in phase, and spending a
        // half bar of ring to "fix" that would be the same bug wearing a different hat.
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let at = roll(&mut clock, 0.0, 400);
        let before = clock.captured();
        stop(&mut clock, 50);

        let advance = clock.advance(&Transport::at(120.0, Meter::FOUR_FOUR, at), BLOCK);
        assert_eq!(advance.seam, 0, "resuming in place inserted {} frames", advance.seam);
        assert_eq!(clock.captured(), before + BLOCK as u64);
    }

    #[test]
    fn a_loop_that_wraps_on_a_bar_line_inserts_nothing() {
        // A DAW looping four bars sends the playhead backwards every pass. That is a locate, and
        // it is detected as one — but four bars is a whole number of them, so the phase already
        // agrees and the right correction is none at all.
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let mut at = 16.0;
        for _ in 0..4 {
            at = roll(&mut clock, at, 375); // four bars, near enough
            at -= 16.0; // back to the top of the loop
        }
        assert_eq!(clock.seam_frames(), 0, "a bar-aligned loop spent {} frames", clock.seam_frames());
    }

    #[test]
    fn a_loop_that_is_not_a_whole_number_of_bars_is_re_phased() {
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let at = roll(&mut clock, 0.0, 300);
        // Back three and a half bars: every pass lands the take half a bar out from the last.
        let wrapped = at - 14.0;
        let advance = clock.advance(&Transport::at(120.0, Meter::FOUR_FOUR, wrapped), BLOCK);
        assert!(advance.seam > 0, "the half-bar wrap was not corrected");
        assert!(
            in_phase(head_phase(&clock, BLOCK), song_phase(wrapped)),
            "after the wrap the take is at {} of a bar and the song is at {}",
            head_phase(&clock, BLOCK),
            song_phase(wrapped)
        );
    }

    #[test]
    fn the_first_press_moves_the_grid_rather_than_the_audio() {
        // Nothing is captured yet, so nothing is holding the graticule in place. Shifting it is
        // exact and costs no ring; inserting silence in front of an empty buffer would be neither.
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let start = 5.5; // a bar and three eighths in
        let advance = clock.advance(&Transport::at(120.0, Meter::FOUR_FOUR, start), BLOCK);

        assert_eq!(advance.seam, 0, "the first press must not spend any ring");
        assert_eq!(clock.captured(), BLOCK as u64);
        assert!(
            in_phase(head_phase(&clock, BLOCK), song_phase(start)),
            "the take begins at {} of a bar and the song is at {}",
            head_phase(&clock, BLOCK),
            song_phase(start)
        );
    }

    #[test]
    fn a_source_with_no_position_lands_the_restart_on_a_bar_line() {
        // MIDI clock from Live, or the page's own run button: nothing says where the playhead is.
        // The only reading available is that play was pressed on a downbeat, and taking it at
        // least keeps every restart on the same grid as every other.
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let rolling = Transport::rolling(120.0, Meter::FOUR_FOUR);
        for _ in 0..137 {
            clock.advance(&rolling, BLOCK);
        }
        assert!(head_phase(&clock, BLOCK) > 0.05, "the head should be mid-bar to make this a test");
        stop(&mut clock, 10);
        clock.advance(&rolling, BLOCK);
        assert!(
            in_phase(head_phase(&clock, BLOCK), 0.0),
            "the restart landed at {} of a bar rather than on the line",
            head_phase(&clock, BLOCK)
        );
    }

    #[test]
    fn coming_back_from_an_export_is_a_splice_too() {
        // An offline render reports rolling the whole way and captures nothing, so the passage
        // after it is somewhere else entirely — the same discontinuity as a stop, and it has to
        // re-phase for the same reason.
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let rolling = Transport::rolling(120.0, Meter::FOUR_FOUR);
        for _ in 0..137 {
            clock.advance(&rolling, BLOCK);
        }
        let bounce = Transport { offline: true, ..rolling };
        for _ in 0..5000 {
            clock.advance(&bounce, BLOCK);
        }
        clock.advance(&rolling, BLOCK);
        assert!(
            in_phase(head_phase(&clock, BLOCK), 0.0),
            "the take resumed at {} of a bar after the export",
            head_phase(&clock, BLOCK)
        );
    }

    #[test]
    fn a_jittery_position_report_does_not_spend_the_ring() {
        // A host whose reported position wanders by a few milliseconds either side of the truth
        // must not be read as locating every block. Nothing here is a real jump, so nothing may
        // be inserted after the first press.
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let wobble = [0.0, 0.02, -0.03, 0.05, -0.01, 0.04, -0.05, 0.01];
        let per_block = BLOCK as f64 * 120.0 / (60.0 * SR as f64);
        let mut quarters = 0.25; // and not on a bar line, so a spurious splice would show
        for i in 0..2000 {
            let reported = quarters + wobble[i % wobble.len()];
            clock.advance(&Transport::at(120.0, Meter::FOUR_FOUR, reported), BLOCK);
            quarters += per_block;
        }
        assert_eq!(clock.seam_frames(), 0, "jitter cost {} frames", clock.seam_frames());
    }

    #[test]
    fn an_empty_block_is_not_a_splice() {
        // A host handing over a zero-length block — a bypass, a suspended graph — has not said
        // anything about its transport. Reading it as a stop would cost a position-less source
        // most of a bar of ring for nothing.
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let rolling = Transport::rolling(120.0, Meter::FOUR_FOUR);
        for i in 0..400 {
            clock.advance(&rolling, if i % 7 == 0 { 0 } else { BLOCK });
        }
        assert_eq!(clock.seam_frames(), 0, "empty blocks cost {} frames", clock.seam_frames());
    }

    #[test]
    fn splices_never_run_the_bars_backwards() {
        // The correction is always forwards, to the *next* aligned position rather than the
        // nearest one, because everything downstream — erosion, the lap, the wrap — rests on the
        // bar axis never decreasing.
        let mut clock = CaptureClock::new(SR, 120.0, Meter::FOUR_FOUR);
        let mut previous = f64::NEG_INFINITY;
        let mut check = |clock: &CaptureClock| {
            let bars = clock.head_bars();
            assert!(bars >= previous, "bars went backwards: {bars} after {previous}");
            previous = bars;
        };
        // Twenty restarts at arbitrary, deliberately awkward positions, with meter and tempo
        // moving underneath them.
        let mut at = 0.0;
        for i in 0..20 {
            let meter = if i % 3 == 0 { Meter::new(7, 8) } else { Meter::FOUR_FOUR };
            let bpm = 90.0 + (i as f64) * 3.7;
            at = 13.37 * (i as f64) + 0.618;
            let per_block = BLOCK as f64 * bpm / (60.0 * SR as f64);
            for _ in 0..(20 + i * 3) {
                clock.advance(&Transport { position: Some(at), ..Transport::rolling(bpm, meter) }, BLOCK);
                at += per_block;
                check(&clock);
            }
            stop(&mut clock, 3);
            check(&clock);
        }
        assert!(at > 0.0);
        assert!(clock.seam_frames() > 0, "twenty locates should have inserted something");
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
