//! The whole picture, end to end: ring, mirror, wrap resolution, reduction, draw, readback.
//!
//! Asserting on pixels is unusual and worth the trouble here, because everything between the ring
//! and the screen is arithmetic that produces *something* plausible when it is wrong. A gain
//! applied twice, a y axis the wrong way up, RMS drawn under peak instead of over it — none of
//! those fail a compile or a shader validation, and all of them are obvious in four pixels.

use std::f32::consts::TAU;

use waveroll_core::ring;
use waveroll_core::tempo::{Meter, TempoMap};
use waveroll_core::view::{View, Viewport};
use waveroll_gpu::device::Gpu;
use waveroll_gpu::envelope::{EnvelopePass, RingMirror};
use waveroll_gpu::render::{Style, Target, WaveformPass, TARGET_FORMAT};

const SR: u32 = 48_000;
const CAPACITY: usize = 1 << 21;
const W: u32 = 256;
const H: u32 = 64;

fn gpu() -> Option<Gpu> {
    match Gpu::headless() {
        Ok(gpu) => Some(gpu),
        Err(why) => {
            println!("SKIPPED: {why}");
            None
        }
    }
}

fn close(got: [u8; 4], want: [f32; 4], what: &str) {
    for i in 0..3 {
        let expected = (want[i] * 255.0).round() as i32;
        assert!(
            (i32::from(got[i]) - expected).abs() <= 2,
            "{what}: channel {i} is {}, expected about {expected} (pixel {got:?})",
            got[i]
        );
    }
}

/// Captures `bars` of a tone and draws it, returning the pixels.
fn paint(gpu: &Gpu, amplitude: f32, bars: f64, style: &Style) -> Vec<[u8; 4]> {
    let map = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
    let (mut producer, reader) = ring::ring(CAPACITY, 1, SR);
    let mut mirror = RingMirror::new(gpu, CAPACITY, 1);
    let mut envelope = EnvelopePass::new(gpu, 1024);
    let target = Target::new(gpu, W, H);
    let waveform = WaveformPass::new(gpu, TARGET_FORMAT);

    let frames = map.frame_at_bars(bars) as usize;
    // 1 kHz: hundreds of cycles per column at this width, so every column sees the full excursion
    // and the reduction is deterministic rather than dependent on where the column boundary fell.
    let signal: Vec<f32> = (0..frames)
        .map(|i| (i as f32 / SR as f32 * 1_000.0 * TAU).sin() * amplitude)
        .collect();
    for chunk in signal.chunks(4096) {
        producer.write(&[chunk], chunk.len());
    }
    mirror.sync(gpu, &reader);

    let viewport = Viewport::resolve(&View::new(16.0), &map, frames as u64, W);
    envelope.reduce(gpu, &mirror, &viewport, &map, [1.0, 0.0]);
    waveform.draw(gpu, &target, envelope.output(), W, style);
    target.read(gpu)
}

#[test]
fn the_trace_spans_the_amplitude_it_was_given() {
    let Some(gpu) = gpu() else { return };
    let style = Style::default();
    let pixels = paint(&gpu, 0.5, 16.0, &style);
    let at = |x: u32, y: u32| Target::pixel(&pixels, W, x, y);
    let x = 40;

    // A ±0.5 sine at unity gain occupies the middle half of the pane: rows 16 to 48. Its RMS is
    // 0.3536, so the inner bar runs from row 21 to row 43.
    close(at(x, 5), style.background, "well above the trace");
    close(at(x, 18), style.peak, "inside peak but outside rms");
    close(at(x, 32), style.rms, "the centre is the rms bar, drawn over peak");
    close(at(x, 45), style.peak, "below rms, still inside peak");
    close(at(x, 58), style.background, "well below the trace");

    // The y axis is not upside down: a positive-going signal reaches the top of the pane, and
    // "up is louder" is the one convention nobody checks and everybody notices.
    let mut top = None;
    let mut bottom = None;
    for y in 0..H {
        if at(x, y) != [
            (style.background[0] * 255.0).round() as u8,
            (style.background[1] * 255.0).round() as u8,
            (style.background[2] * 255.0).round() as u8,
            255,
        ] {
            top.get_or_insert(y);
            bottom = Some(y);
        }
    }
    let (top, bottom) = (top.expect("something was drawn"), bottom.expect("something was drawn"));
    assert!((top as i32 - 16).abs() <= 1, "trace starts at row {top}, expected 16");
    assert!((bottom as i32 - 47).abs() <= 1, "trace ends at row {bottom}, expected 47");
    println!("trace occupies rows {top}..={bottom} of {H} for a +/-0.5 signal");
}

#[test]
fn gain_scales_the_trace_and_clamps_rather_than_wrapping() {
    let Some(gpu) = gpu() else { return };
    let quiet = Style { gain: 1.0, ..Style::default() };
    let loud = Style { gain: 8.0, ..Style::default() };

    let a = paint(&gpu, 0.1, 16.0, &quiet);
    let b = paint(&gpu, 0.1, 16.0, &loud);
    let height = |pixels: &[[u8; 4]]| {
        (0..H).filter(|y| Target::pixel(pixels, W, 40, *y)[2] > 100).count()
    };
    assert!(height(&b) > height(&a) * 3, "gain did not scale the trace");

    // Well past full scale it must fill the pane and stop, not wrap around into a thin bar.
    let clipped = paint(&gpu, 0.9, 16.0, &Style { gain: 20.0, ..Style::default() });
    assert_eq!(height(&clipped), H as usize, "an over-driven trace should fill the pane");
}

#[test]
fn unwritten_columns_are_not_drawn_as_silence() {
    let Some(gpu) = gpu() else { return };
    let style = Style::default();
    // A quarter of the first lap: the right three quarters have never been captured.
    let pixels = paint(&gpu, 0.5, 4.0, &style);
    let at = |x: u32, y: u32| Target::pixel(&pixels, W, x, y);

    close(at(20, 32), style.rms, "the captured quarter has a trace");
    // The rest is a hairline in its own colour, so "nothing yet" reads differently from digital
    // silence — which would be a real measurement and is a different fact.
    close(at(200, 32), style.unwritten, "the uncaptured part has its own mark");
    close(at(200, 20), style.background, "and it really is only a hairline");
    close(at(200, 44), style.background, "on both sides");
}

#[test]
fn silence_leaves_a_line_rather_than_a_hole() {
    let Some(gpu) = gpu() else { return };
    let style = Style::default();
    let pixels = paint(&gpu, 0.0, 16.0, &style);
    let at = |x: u32, y: u32| Target::pixel(&pixels, W, x, y);
    // Minimum bar height is one pixel, at the centre line.
    close(at(100, 32), style.rms, "silence still draws its centre line");
    close(at(100, 20), style.background, "but only a line");
}
