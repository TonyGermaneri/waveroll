//! The waveform reduction, end to end: ring, mirror, wrap resolution, GPU reduction.
//!
//! Checked against the same arithmetic done on the CPU from the same column table, which is what
//! makes this a test of the *plumbing* — the incremental upload, the ring masking, the workgroup
//! reduction — rather than of arithmetic already tested elsewhere. Most of what can go wrong here
//! is an index: a column reading one sample early, a mirror segment written at the wrong offset,
//! a wrap resolved on the wrong side of the head.

use waveroll_core::ring::{self, Reader};
use waveroll_core::tempo::{Meter, TempoMap};
use waveroll_core::view::{Column, View, Viewport};
use waveroll_gpu::device::Gpu;
use waveroll_gpu::envelope::{EnvelopePass, RingMirror};

const SR: u32 = 48_000;
const CAPACITY: usize = 1 << 21; // 43.7 s at 48 kHz — comfortably more than a 16-bar lap

fn gpu() -> Option<Gpu> {
    match Gpu::headless() {
        Ok(gpu) => Some(gpu),
        Err(why) => {
            println!("SKIPPED: {why}");
            None
        }
    }
}

/// A signal whose value is a known function of its absolute frame index, so an off-by-one in any
/// index anywhere shows up as a wrong number rather than as a plausible waveform.
fn sample_at(frame: u64) -> f32 {
    let t = frame as f32 / SR as f32;
    (t * 220.0 * std::f32::consts::TAU).sin() * 0.5 + (frame % 977) as f32 * 1e-4
}

/// The reduction the GPU is supposed to be doing.
fn on_cpu(reader: &Reader, column: Column) -> Option<(f32, f32, f32)> {
    if column.count == 0 {
        return None;
    }
    let mut buffer = vec![0.0f32; column.count as usize];
    assert!(reader.read_into(0, column.start, &mut buffer), "the range must still be in the ring");
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    let mut sum = 0.0f64;
    for v in &buffer {
        lo = lo.min(*v);
        hi = hi.max(*v);
        sum += f64::from(*v) * f64::from(*v);
    }
    Some((lo, hi, (sum / f64::from(column.count)).sqrt() as f32))
}

#[test]
fn the_reduced_waveform_matches_the_same_reduction_on_the_cpu() {
    let Some(gpu) = gpu() else { return };
    let map = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
    let (mut producer, reader) = ring::ring(CAPACITY, 1, SR);
    let mut mirror = RingMirror::new(&gpu, CAPACITY, 1);
    let mut pass = EnvelopePass::new(&gpu, 2048);

    // Capture a bit over one lap, in irregular blocks, syncing as we go — which is what the render
    // loop does, and the only way the incremental upload's wrap handling is exercised at all.
    let target = map.frame_at_bars(21.0);
    let mut written = 0u64;
    let mut block = 0usize;
    while written < target {
        let size = [128usize, 512, 64, 1024, 256][block % 5];
        let chunk: Vec<f32> = (0..size).map(|i| sample_at(written + i as u64)).collect();
        producer.write(&[&chunk], size);
        written += size as u64;
        block += 1;
        if block.is_multiple_of(3) {
            mirror.sync(&gpu, &reader);
        }
    }
    mirror.sync(&gpu, &reader);

    let view = View::new(16.0);
    let viewport = Viewport::resolve(&view, &map, written, 800);
    let got = pass.reduce(&gpu, &mirror, &viewport, &map, [1.0, 0.0]);

    let mut columns = Vec::new();
    viewport.columns(&map, &mut columns);
    assert_eq!(got.len(), columns.len());

    let mut compared = 0;
    for (i, (envelope, column)) in got.iter().zip(&columns).enumerate() {
        match on_cpu(&reader, *column) {
            None => assert!(!envelope.written, "column {i} has no samples but reports written"),
            Some((lo, hi, rms)) => {
                assert!(envelope.written, "column {i} has {} samples but reports empty", column.count);
                assert!((envelope.min - lo).abs() < 1e-6, "column {i} min: {} vs {lo}", envelope.min);
                assert!((envelope.max - hi).abs() < 1e-6, "column {i} max: {} vs {hi}", envelope.max);
                // RMS sums thousands of squares, so single precision on the GPU against double on
                // the CPU diverges by more than an exact comparison allows.
                assert!(
                    (envelope.rms - rms).abs() < 1e-4,
                    "column {i} rms: {} vs {rms}",
                    envelope.rms
                );
                compared += 1;
            }
        }
    }
    assert!(compared > 700, "only {compared} of 800 columns had audio; the test proved little");
    println!("{compared} columns reduced and matched, past one wrap");
}

#[test]
fn the_first_lap_leaves_the_far_side_of_the_screen_unwritten() {
    let Some(gpu) = gpu() else { return };
    let map = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
    let (mut producer, reader) = ring::ring(CAPACITY, 1, SR);
    let mut mirror = RingMirror::new(&gpu, CAPACITY, 1);
    let mut pass = EnvelopePass::new(&gpu, 2048);

    // A quarter of the way through the first lap.
    let target = map.frame_at_bars(4.0) as usize;
    let chunk: Vec<f32> = (0..target).map(|i| sample_at(i as u64)).collect();
    for part in chunk.chunks(4096) {
        producer.write(&[part], part.len());
    }
    mirror.sync(&gpu, &reader);

    let view = View::new(16.0);
    let viewport = Viewport::resolve(&view, &map, target as u64, 400);
    let got = pass.reduce(&gpu, &mirror, &viewport, &map, [1.0, 0.0]);

    let written = got.iter().filter(|e| e.written).count();
    assert!(
        (98..=102).contains(&written),
        "a quarter of 400 columns should be written, got {written}"
    );
    assert!(got[0].written, "the start of the lap has audio");
    assert!(!got[399].written, "the far side has never been captured — that is not silence");
    // And the unwritten side really is untouched rather than zeroed-over audio.
    assert_eq!(got[399].min, 0.0);
    assert_eq!(got[399].max, 0.0);
}

#[test]
fn a_column_never_reads_past_the_write_head() {
    let Some(gpu) = gpu() else { return };
    let map = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
    let (mut producer, reader) = ring::ring(CAPACITY, 1, SR);
    let mut mirror = RingMirror::new(&gpu, CAPACITY, 1);
    let mut pass = EnvelopePass::new(&gpu, 2048);

    // Sitting mid-lap on the second time round, where the column under the head has this lap's
    // audio on its left and last lap's on its right.
    let target = map.frame_at_bars(24.0);
    let chunk: Vec<f32> = (0..target).map(sample_at).collect();
    for part in chunk.chunks(8192) {
        producer.write(&[part], part.len());
    }
    mirror.sync(&gpu, &reader);

    let view = View::new(16.0);
    let viewport = Viewport::resolve(&view, &map, target, 256);
    let mut columns = Vec::new();
    viewport.columns(&map, &mut columns);
    for (i, column) in columns.iter().enumerate() {
        assert!(
            column.start + u64::from(column.count) <= target,
            "column {i} reads to {} past a head at {target}",
            column.start + u64::from(column.count)
        );
    }

    // The seam is exactly one column wide: the last one belonging to this lap.
    let got = pass.reduce(&gpu, &mirror, &viewport, &map, [1.0, 0.0]);
    assert!(got.iter().all(|e| e.written), "the second lap has audio everywhere");
    println!("256 columns across the head, none reading past it");
}
