//! Renders the rolling buffer to a raw RGBA file, for looking at.
//!
//! `cargo run -p waveroll-gpu --example paint -- <out.raw> [bars-captured]`
//!
//! Capture more than sixteen bars and the wrap becomes visible: the left of the picture is the new
//! lap painting over the right, which is still showing the old one.

use std::f32::consts::TAU;
use std::{env, fs};

use waveroll_core::grid::{self, Ruling, Unit};
use waveroll_core::ring;
use waveroll_core::tempo::{Meter, TempoMap};
use waveroll_core::view::{View, Viewport};
use waveroll_gpu::device::Gpu;
use waveroll_gpu::envelope::{EnvelopePass, RingMirror};
use waveroll_gpu::overlay::{Overlay, OverlayPass};
use waveroll_gpu::render::{OverlayStyle, Style, Target, WaveformPass, TARGET_FORMAT};

const SR: u32 = 48_000;
const W: u32 = 1400;
const H: u32 = 320;

/// Something with the shape of music rather than a test tone: a kick on the beat, hats on the
/// eighths, a sustained bass, and a chorus that arrives eight bars in.
fn material(frame: u64, beat: f64) -> f32 {
    let t = frame as f64 / f64::from(SR);
    let phase = (t / beat).fract();
    let beat_index = (t / beat) as u64;
    let bar = beat_index / 4;

    let mut v = 0.0;
    if beat_index.is_multiple_of(4) || beat_index % 8 == 6 {
        let env = (-phase * 22.0).exp();
        v += (TAU as f64 * 90.0 * phase * beat).sin() * env * 0.9;
    }
    let eighth = (t / (beat / 2.0)).fract();
    if eighth < 0.06 {
        let noise = ((frame.wrapping_mul(6_364_136_223_846_793_005) >> 33) as f64
            / f64::from(u32::MAX))
            * 2.0
            - 1.0;
        v += noise * (-eighth * 90.0).exp() * 0.22;
    }
    let loud = if (8..16).contains(&(bar % 16)) { 1.0 } else { 0.45 };
    v += (TAU as f64 * 55.0 * t).sin() * 0.30 * loud;
    v += (TAU as f64 * 220.0 * t).sin() * 0.10 * loud;
    // Each lap arrives louder than the last, purely so the wrap is visible in the picture: the
    // seam where the new sweep meets what it has not overwritten yet is the whole point of the
    // display, and a demo where both sides happen to match proves nothing.
    let lap_gain = 0.5 + 0.5 * ((bar / 16) as f64).min(1.0);
    (v * 0.62 * lap_gain) as f32
}

fn main() {
    let out = env::args().nth(1).unwrap_or_else(|| "waveroll.raw".into());
    let bars: f64 = env::args().nth(2).and_then(|a| a.parse().ok()).unwrap_or(21.0);

    let gpu = Gpu::headless().expect("a GPU");
    println!("adapter: {}", gpu.describe());

    let map = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
    let capacity = 1 << 21;
    let (mut producer, reader) = ring::ring(capacity, 1, SR);
    let mut mirror = RingMirror::new(&gpu, capacity, 1);
    let mut envelope = EnvelopePass::new(&gpu, W);
    let target = Target::new(&gpu, W, H);
    let waveform = WaveformPass::new(&gpu, TARGET_FORMAT);
    let overlay_pass = OverlayPass::new(&gpu, TARGET_FORMAT, 4096);

    let beat = 60.0 / 120.0;
    let frames = map.frame_at_bars(bars);
    let mut written = 0u64;
    while written < frames {
        let n = 2048.min((frames - written) as usize);
        let chunk: Vec<f32> = (0..n).map(|i| material(written + i as u64, beat)).collect();
        producer.write(&[&chunk], n);
        written += n as u64;
        mirror.sync(&gpu, &reader);
    }

    let view = View::new(16.0);
    let viewport = Viewport::resolve(&view, &map, written, W);
    envelope.reduce(&gpu, &mirror, &viewport, &map, [1.0, 0.0]);
    waveform.draw(&gpu, &target, envelope.output(), W, &Style::default());

    // Auto quantise, at the zoom the view is actually at.
    let unit = Unit::Auto.bars(viewport.span_bars, f64::from(W));
    let mut rulings: Vec<Ruling> = Vec::new();
    grid::rulings(&viewport, unit, &mut rulings);

    // A four-bar selection, snapped, as if the user had dragged across it.
    let dragged_from = map.frame_at_bars(viewport.lap_start_bars + 4.3);
    let dragged_to = map.frame_at_bars(viewport.lap_start_bars + 7.6);
    let selection = grid::snap_range(&map, unit, dragged_from, dragged_to);

    let style = OverlayStyle::default();
    let mut overlay = Overlay::default();
    overlay.begin(&target);
    overlay.grid(&rulings, &style);
    overlay.selection(
        viewport.fraction_at(map.bars_at(selection.start)),
        viewport.fraction_at(map.bars_at(selection.end)),
        &style,
    );
    overlay.head(viewport.head_fraction(), &style);
    overlay_pass.draw(&gpu, &target, &overlay);

    let pixels = target.read(&gpu);
    let mut bytes = Vec::with_capacity(pixels.len() * 4);
    for p in &pixels {
        bytes.extend_from_slice(p);
    }
    fs::write(&out, &bytes).expect("write");
    println!(
        "{out}  {W}x{H}  lap {}  head at {:.1}%  unit {unit} bars  {} rulings  \
         selection {:.2}..{:.2} bars  {:.0} samples per column",
        viewport.lap,
        viewport.head_fraction() * 100.0,
        rulings.len(),
        map.bars_at(selection.start) - viewport.lap_start_bars,
        map.bars_at(selection.end) - viewport.lap_start_bars,
        viewport.frames_per_column(&map)
    );
}
