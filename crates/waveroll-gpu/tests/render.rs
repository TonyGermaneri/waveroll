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
use waveroll_gpu::wgpu;

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
    waveform.draw(gpu, &target.view, (W, H), envelope.output(), W, style);
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

// ---------------------------------------------------------------------------------------
// The overlay
// ---------------------------------------------------------------------------------------

use waveroll_core::grid::{self, Ruling};
use waveroll_gpu::overlay::{Overlay, OverlayPass};
use waveroll_gpu::render::OverlayStyle;

/// Draws a plain background plus the overlay, so the assertions are about the overlay alone.
fn paint_overlay(
    gpu: &Gpu,
    build: impl FnOnce(&mut Overlay, &OverlayStyle),
) -> (Vec<[u8; 4]>, OverlayStyle) {
    let style = Style { background: [0.0, 0.0, 0.0, 1.0], ..Style::default() };
    let overlay_style = OverlayStyle::default();
    let target = Target::new(gpu, W, H);
    let waveform = WaveformPass::new(gpu, TARGET_FORMAT);
    let pass = OverlayPass::new(gpu, TARGET_FORMAT, 4096);

    // An empty envelope buffer: the clear is what we want under the overlay.
    let empty = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 16,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    waveform.draw(gpu, &target.view, (W, H), &empty, 0, &style);

    let mut overlay = Overlay::default();
    overlay.begin((W, H));
    build(&mut overlay, &overlay_style);
    pass.draw(gpu, &target.view, (W, H), &overlay);
    (target.read(gpu), overlay_style)
}

#[test]
fn grid_lines_land_on_whole_pixel_columns() {
    let Some(gpu) = gpu() else { return };
    // Sixteen bars across 256 pixels: a bar line every sixteen columns, exactly.
    let rulings: Vec<Ruling> = (0..=16)
        .map(|i| Ruling {
            fraction: f64::from(i) / 16.0,
            rule: if i % 16 == 0 { grid::Rule::Lap } else { grid::Rule::Bar },
        })
        .collect();
    let (pixels, _) = paint_overlay(&gpu, |o, s| o.grid(&rulings, s));

    for i in 0..16 {
        let x = i * 16;
        let on = Target::pixel(&pixels, W, x, 10);
        let off = Target::pixel(&pixels, W, x + 8, 10);
        assert!(on[2] > 20, "expected a grid line at column {x}, got {on:?}");
        assert!(off[2] < 8, "expected background between lines at {}, got {off:?}", x + 8);
    }
    // And a line is exactly one column wide, not smeared over two by a half-pixel position.
    assert!(Target::pixel(&pixels, W, 17, 10)[2] < 8, "the bar line bled into the next column");
}

#[test]
fn the_lap_boundary_reads_stronger_than_a_bar_line() {
    let Some(gpu) = gpu() else { return };
    let rulings = vec![
        Ruling { fraction: 0.25, rule: grid::Rule::Bar },
        Ruling { fraction: 0.5, rule: grid::Rule::Lap },
        Ruling { fraction: 0.75, rule: grid::Rule::Cell },
    ];
    let (pixels, _) = paint_overlay(&gpu, |o, s| o.grid(&rulings, s));
    let brightness = |x: u32| i32::from(Target::pixel(&pixels, W, x, 10)[2]);
    let (cell, bar, lap) = (brightness(192), brightness(64), brightness(128));
    assert!(lap > bar && bar > cell, "expected lap {lap} > bar {bar} > cell {cell}");
}

#[test]
fn the_selection_tints_its_range_and_marks_both_edges() {
    let Some(gpu) = gpu() else { return };
    // Bars 4 to 8 of 16: a quarter to a half of the canvas.
    let (pixels, style) = paint_overlay(&gpu, |o, s| o.selection(0.25, 0.5, s));
    let at = |x: u32| Target::pixel(&pixels, W, x, 32);

    assert!(at(40)[0] < 8, "outside the selection on the left");
    assert!(at(200)[0] < 8, "outside the selection on the right");
    // The fill is translucent: it tints without hiding what is under it.
    let fill = at(96);
    let expected_fill = (style.selection_fill[0] * style.selection_fill[3] * 255.0).round() as i32;
    assert!(
        (i32::from(fill[0]) - expected_fill).abs() <= 2,
        "fill is {fill:?}, expected about {expected_fill} in red over black"
    );
    // Both edges are solid, and at the snapped positions.
    let left = at(64);
    let right = at(127);
    assert!(left[0] > 180, "the left edge should be solid, got {left:?}");
    assert!(right[0] > 180, "the right edge should be solid, got {right:?}");
    assert!(left[0] > fill[0] * 2, "an edge has to read as an edge against its own fill");
}

#[test]
fn the_smallest_possible_selection_is_still_visible() {
    let Some(gpu) = gpu() else { return };
    // A selection narrower than a pixel — which the grid can produce at high zoom — must not
    // vanish, or it is indistinguishable from having selected nothing.
    let (pixels, _) = paint_overlay(&gpu, |o, s| o.selection(0.5, 0.5001, s));
    let lit = (0..W).filter(|x| Target::pixel(&pixels, W, *x, 32)[0] > 100).count();
    assert!(lit >= 1, "a sub-pixel selection disappeared entirely");
    assert!(lit <= 3, "and it should not be smeared across {lit} columns");
}

#[test]
fn the_head_is_drawn_over_everything_else() {
    let Some(gpu) = gpu() else { return };
    let rulings = vec![Ruling { fraction: 0.5, rule: grid::Rule::Lap }];
    let (pixels, style) = paint_overlay(&gpu, |o, s| {
        o.grid(&rulings, s);
        o.selection(0.4, 0.6, s);
        o.head(0.5, s);
    });
    let head = Target::pixel(&pixels, W, 128, 32);
    // The head is red; a lap line and a selection edge share that column and must not win.
    let expected = (style.head[0] * style.head[3] * 255.0).round() as i32;
    assert!(
        i32::from(head[0]) >= expected - 40,
        "the head was buried under the grid: {head:?}"
    );
    assert!(head[0] > head[2], "the head should read red, not as the bluish grid: {head:?}");
}

// ---------------------------------------------------------------------------------------
// Not taking the host down
// ---------------------------------------------------------------------------------------

#[test]
fn a_validation_error_is_recorded_rather_than_fatal() {
    let Some(gpu) = gpu() else { return };
    // This is the shape of the bug that crashed Ableton: a surface configured past the device's
    // texture limit. It used to reach a panicking error handler, and a panic crossing the C
    // boundary aborts the process -- which is somebody's DAW.
    let limit = gpu.max_surface();
    assert!(limit >= 8192, "the requested limits should allow a large editor, got {limit}");

    // Provoke one deliberately, and assert it lands in the record instead of the floor.
    let too_big = limit + 1024;
    gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let _ = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("deliberately too large"),
        size: wgpu::Extent3d { width: too_big, height: 16, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let scoped = pollster_block(gpu.device.pop_error_scope());
    assert!(scoped.is_some(), "the device should have refused a {too_big}px texture");
    println!("device refused {too_big}px and reported it rather than aborting");
}

/// The device's own blocking poll, so the test needs no async runtime.
fn pollster_block<F: std::future::Future>(future: F) -> F::Output {
    waveroll_gpu::device::block_on(future)
}
