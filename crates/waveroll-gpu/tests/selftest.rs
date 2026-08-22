//! The GPU chain against an independent CPU transform.
//!
//! This is the check that makes the rest of the renderer trustworthy. A GPU FFT can be wrong in
//! ways that still look like a spectrum — a sign flipped in one butterfly, a twiddle indexed off by
//! a stride, the real-packing split taking the wrong conjugate — and every one of those produces
//! peaks in roughly the right places. Comparing against an O(N²) transform written straight from
//! the definition, sharing no code with the shaders, is the only thing that catches them.
//!
//! Skips loudly with no adapter, so a machine without a usable GPU does not fail the suite.

use waveroll_gpu::device::Gpu;
use waveroll_gpu::fft::Analyzer;
use waveroll_gpu::reference;

const SR: f64 = 48_000.0;

fn gpu() -> Option<Gpu> {
    match Gpu::headless() {
        Ok(gpu) => Some(gpu),
        Err(why) => {
            println!("SKIPPED: {why}");
            None
        }
    }
}

/// Max and RMS deviation between the GPU spectrum and the reference, as a percentage of the
/// reference's peak magnitude. Relative to the peak rather than bin by bin, because a bin whose
/// true value is 1e-9 can be off by 300% and mean nothing at all.
fn compare(got: &[(f32, f32)], want: &[(f64, f64)]) -> (f64, f64) {
    assert_eq!(got.len(), want.len(), "bin count");
    let peak = want.iter().map(|(re, im)| re.hypot(*im)).fold(0.0_f64, f64::max).max(1e-30);
    let mut worst = 0.0_f64;
    let mut sum = 0.0_f64;
    for ((gr, gi), (wr, wi)) in got.iter().zip(want) {
        let error = (f64::from(*gr) - wr).hypot(f64::from(*gi) - wi) / peak;
        worst = worst.max(error);
        sum += error * error;
    }
    (worst * 100.0, (sum / got.len() as f64).sqrt() * 100.0)
}

#[test]
fn the_gpu_chain_matches_an_independent_transform() {
    let Some(gpu) = gpu() else { return };
    println!("adapter: {}", gpu.describe());

    // Both parities of log2(N/2) are covered on purpose: an odd one runs a radix-2 stage before
    // the radix-4 chain, and that stage is only exercised at those sizes.
    for size in [512usize, 1024, 2048, 4096, 8192] {
        let analyzer = Analyzer::new(&gpu, size);
        let signal = reference::tones(size, SR, &[(997.0, 0.5), (5_000.0, 0.25), (11_713.0, 0.125)]);
        let window = reference::hann(size);

        let want = reference::spectrum(&signal, &window);
        let samples: Vec<f32> = signal.iter().map(|v| *v as f32).collect();
        let window32: Vec<f32> = window.iter().map(|v| *v as f32).collect();
        let got = analyzer.spectrum(&gpu, &samples, &window32);

        let (worst, rms) = compare(&got, &want);
        println!(
            "N = {size:>5}  {:>5} bins  max error {worst:.3e} %   rms {rms:.3e} %  {}",
            got.len(),
            if (size / 2).trailing_zeros() % 2 == 1 { "(radix-2 stage first)" } else { "" }
        );
        // waveshape measures 3.2e-6 % max against the same reference. Single precision through a
        // different driver will not land on the same figure, so the bar is set where it still
        // catches every structural error while leaving room for the arithmetic.
        assert!(worst < 1e-3, "N = {size}: max error {worst:.3e} % is too large to be rounding");
    }
}

#[test]
fn an_impulse_has_a_flat_spectrum() {
    let Some(gpu) = gpu() else { return };
    let size = 1024;
    let analyzer = Analyzer::new(&gpu, size);
    // An impulse at sample zero transforms to 1 in every bin. It is the sharpest test of the
    // packing and the stage schedule there is: any misplaced sample shows up as a ripple.
    let mut samples = vec![0.0f32; size];
    samples[0] = 1.0;
    let window = vec![1.0f32; size];
    let bins = analyzer.spectrum(&gpu, &samples, &window);
    for (k, (re, im)) in bins.iter().enumerate() {
        assert!(
            (f64::from(*re) - 1.0).abs() < 1e-5 && f64::from(*im).abs() < 1e-5,
            "bin {k} should be 1 + 0i, is {re} + {im}i"
        );
    }
}

#[test]
fn a_pure_tone_on_a_bin_centre_lands_in_that_bin_alone() {
    let Some(gpu) = gpu() else { return };
    let size = 2048;
    let analyzer = Analyzer::new(&gpu, size);
    let k = 64;
    // Exactly on a bin centre with a rectangular window: no leakage is possible, so every other
    // bin must be zero. A twiddle indexed off by one puts the peak in the wrong bin, and a
    // conjugation error puts energy in its mirror.
    let samples: Vec<f32> = (0..size)
        .map(|i| (std::f64::consts::TAU * k as f64 * i as f64 / size as f64).sin() as f32)
        .collect();
    let bins = analyzer.spectrum(&gpu, &samples, &vec![1.0f32; size]);

    let peak = f64::from(bins[k].0).hypot(f64::from(bins[k].1));
    assert!((peak - size as f64 / 2.0).abs() < 0.05, "bin {k} reads {peak}, expected {}", size / 2);
    // The forward sign convention puts -iN/2 there. A conjugated table would put +iN/2, which no
    // magnitude check could ever distinguish.
    assert!(
        f64::from(bins[k].1) < 0.0,
        "the transform is conjugated: bin {k} imaginary part is {}",
        bins[k].1
    );
    for (j, (re, im)) in bins.iter().enumerate() {
        if j != k {
            let m = f64::from(*re).hypot(f64::from(*im));
            assert!(m < 0.05, "bin {j} should be empty, reads {m}");
        }
    }
}
