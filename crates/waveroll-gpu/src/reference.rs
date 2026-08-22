//! An independent, deliberately slow spectrum, used as the oracle for the GPU chain.
//!
//! A GPU FFT can be subtly wrong and still look completely plausible: a sign flipped in one
//! butterfly, a twiddle indexed off by a stride, the real-packing split done with the wrong
//! conjugate. Every one of those produces a spectrum with peaks in roughly the right places. The
//! only way to catch them is to compute the same thing a second time by a route that shares no
//! code with the first — so this is an O(N²) discrete Fourier transform in `f64`, written straight
//! from the definition, with no packing, no radix, and no cleverness of any kind.
//!
//! It is far too slow to ship in the render path and that is the point. It runs in tests.

use std::f64::consts::TAU;

/// A periodic Hann window — `0.5 - 0.5·cos(2πn/N)`, dividing by `N` rather than `N-1`.
///
/// Periodic, not symmetric, because this window is used for spectral analysis of a signal assumed
/// to continue past the frame. The symmetric form repeats its endpoint, which puts a discontinuity
/// of one sample into the implied periodic extension and a corresponding error in every bin.
pub fn hann(n: usize) -> Vec<f64> {
    (0..n).map(|i| 0.5 - 0.5 * (TAU * i as f64 / n as f64).cos()).collect()
}

pub fn rectangular(n: usize) -> Vec<f64> {
    vec![1.0; n]
}

/// Discrete Fourier transform of a real signal, returning bins `0..=n/2` as `(re, im)`.
///
/// The forward sign convention is `e^(-2πikn/N)`, matching the twiddle table the GPU chain uses.
/// Getting this backwards conjugates the whole spectrum, which is invisible in a magnitude plot
/// and fatal to anything that reads the phase — which reassignment does.
pub fn real_dft(samples: &[f64]) -> Vec<(f64, f64)> {
    let n = samples.len();
    (0..=n / 2)
        .map(|k| {
            let mut re = 0.0;
            let mut im = 0.0;
            for (i, &x) in samples.iter().enumerate() {
                let angle = -TAU * (k * i % n) as f64 / n as f64;
                re += x * angle.cos();
                im += x * angle.sin();
            }
            (re, im)
        })
        .collect()
}

pub fn magnitudes(bins: &[(f64, f64)]) -> Vec<f64> {
    bins.iter().map(|(re, im)| re.hypot(*im)).collect()
}

/// Windowed spectrum of one frame, which is exactly what the GPU chain produces.
pub fn spectrum(samples: &[f64], window: &[f64]) -> Vec<(f64, f64)> {
    assert_eq!(samples.len(), window.len(), "window and frame must be the same length");
    let windowed: Vec<f64> = samples.iter().zip(window).map(|(x, w)| x * w).collect();
    real_dft(&windowed)
}

/// A sum of sinusoids, in samples. The self-test signal.
pub fn tones(n: usize, sample_rate: f64, tones: &[(f64, f64)]) -> Vec<f64> {
    (0..n)
        .map(|i| {
            tones
                .iter()
                .map(|(freq, amp)| amp * (TAU * freq * i as f64 / sample_rate).sin())
                .sum()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_current_lands_entirely_in_bin_zero() {
        let bins = real_dft(&vec![1.0; 64]);
        assert!((bins[0].0 - 64.0).abs() < 1e-9, "bin 0 should be N");
        assert!(bins[0].1.abs() < 1e-9);
        for (k, (re, im)) in bins.iter().enumerate().skip(1) {
            assert!(re.hypot(*im) < 1e-9, "bin {k} should be empty, is {}", re.hypot(*im));
        }
    }

    #[test]
    fn an_on_bin_tone_lands_in_one_bin_at_half_its_amplitude() {
        // A real sinusoid splits its energy between +f and -f, so a unit-amplitude tone reads N/2.
        let n = 256;
        let k = 7;
        let signal: Vec<f64> = (0..n).map(|i| (TAU * k as f64 * i as f64 / n as f64).sin()).collect();
        let mags = magnitudes(&real_dft(&signal));
        assert!((mags[k] - n as f64 / 2.0).abs() < 1e-9, "bin {k} reads {}", mags[k]);
        for (j, m) in mags.iter().enumerate() {
            if j != k {
                assert!(*m < 1e-9, "bin {j} should be empty, is {m}");
            }
        }
    }

    #[test]
    fn the_sign_convention_is_forward() {
        // For x[n] = sin(2πkn/N), the forward transform puts -iN/2 in bin k. A conjugated
        // convention would put +iN/2 there, which no magnitude test could ever tell apart.
        let n = 64;
        let k = 5;
        let signal: Vec<f64> = (0..n).map(|i| (TAU * k as f64 * i as f64 / n as f64).sin()).collect();
        let bins = real_dft(&signal);
        assert!(bins[k].0.abs() < 1e-9, "the real part should vanish");
        assert!(
            (bins[k].1 + n as f64 / 2.0).abs() < 1e-9,
            "expected -iN/2 in bin {k}, got {:?}",
            bins[k]
        );
    }

    #[test]
    fn parseval_holds() {
        let n = 128;
        let signal = tones(n, 1000.0, &[(50.0, 0.7), (170.0, 0.3), (310.0, 0.11)]);
        let energy: f64 = signal.iter().map(|x| x * x).sum();

        // Summing the half-spectrum needs the interior bins counted twice, since each stands for
        // both itself and its negative-frequency mirror.
        let bins = real_dft(&signal);
        let mut spectral = 0.0;
        for (k, (re, im)) in bins.iter().enumerate() {
            let power = re * re + im * im;
            spectral += if k == 0 || k == n / 2 { power } else { 2.0 * power };
        }
        spectral /= n as f64;
        assert!(
            (energy - spectral).abs() < 1e-9 * energy.max(1.0),
            "Parseval: {energy} in time, {spectral} in frequency"
        );
    }

    #[test]
    fn the_hann_window_is_periodic_not_symmetric() {
        let w = hann(8);
        assert!(w[0].abs() < 1e-12, "a periodic Hann starts at zero");
        // The symmetric form would end at zero too; the periodic form does not, and the difference
        // is a one-sample discontinuity in the implied periodic extension.
        assert!(w[7] > 0.1, "a periodic Hann does not return to zero: {:?}", w);
        // Its sum is exactly N/2, which is the coherent gain every level calculation depends on.
        assert!((w.iter().sum::<f64>() - 4.0).abs() < 1e-12);
    }
}
