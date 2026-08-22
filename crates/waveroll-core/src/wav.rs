//! WAV, with the two chunks that decide whether a dropped file lands correctly.
//!
//! A bare RIFF/WAVE file drops into a DAW as audio at whatever tempo the session happens to be at,
//! placed wherever the pointer was, needing to be warped and nudged by hand. Two extra chunks
//! remove both of those steps, and they are the entire reason this module is more than forty lines:
//!
//! * **`acid`** carries the tempo and the beat count, which is what makes Live warp the drop to the
//!   session tempo by itself instead of treating it as a one-shot.
//! * **`bext`** — Broadcast Wave — carries a sample-accurate timestamp, which is what lets Reaper,
//!   Pro Tools and Logic spot a file back to the position it was captured at.
//!
//! Neither is optional in practice, and getting either subtly wrong produces a file that opens
//! perfectly and sits in the wrong place, which is the failure mode worth the most care.

use crate::tempo::Meter;

/// Sample format of the written file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// 32-bit float. The capture format, so this is the only lossless choice.
    F32,
    I24,
    I16,
}

impl Depth {
    pub fn bits(self) -> u16 {
        match self {
            Depth::F32 => 32,
            Depth::I24 => 24,
            Depth::I16 => 16,
        }
    }

    pub fn bytes(self) -> usize {
        (self.bits() / 8) as usize
    }

    /// `WAVE_FORMAT_IEEE_FLOAT` or `WAVE_FORMAT_PCM`.
    fn format_tag(self) -> u16 {
        match self {
            Depth::F32 => 3,
            _ => 1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WavSpec {
    pub sample_rate: u32,
    pub depth: Depth,
    /// Triangular dither, applied only when quantising to an integer depth.
    ///
    /// Truncating to 16 bits without it produces correlated quantisation error — distortion that
    /// tracks the signal rather than noise that does not — and it is audible on a quiet fade.
    /// Meaningless for float, and refused there rather than silently ignored.
    pub dither: bool,
    /// Seeds the dither so a given buffer always produces identical bytes. A writer whose output
    /// changed run to run could not have a golden test at all.
    pub dither_seed: u64,
}

impl WavSpec {
    /// Dither is on for 16-bit and off otherwise — see [`WavSpec::dither`] for why that is the
    /// default rather than a preference.
    pub fn new(sample_rate: u32, depth: Depth) -> WavSpec {
        WavSpec {
            sample_rate,
            depth,
            // On for 16-bit, where it matters; off for 24, where the noise floor is already below
            // anything a converter will resolve.
            dither: depth == Depth::I16,
            dither_seed: 0x5DEECE66D,
        }
    }
}

/// The ACIDized-loop chunk.
///
/// This is a de-facto format rather than a published standard — it was reverse-engineered from
/// Sonic Foundry's files and every implementation since has copied the same 24-byte layout. It is
/// documented here because there is nowhere authoritative to point at.
#[derive(Clone, Copy, Debug)]
pub struct Acid {
    pub tempo: f32,
    /// Length in quarter notes. Together with the tempo this states the loop's duration, and the
    /// two must agree with the actual sample count or a host will warp the file to the wrong length.
    pub quarters: u32,
    pub meter: Meter,
    /// MIDI note number, when the material has a key worth transposing from.
    pub root: Option<u8>,
}

/// Broadcast Wave metadata, version 1.
///
/// Version 1 rather than 2 deliberately: version 2 adds loudness fields whose "unknown" encoding is
/// easy to get wrong, and nothing here measures loudness. Both versions are 602 fixed bytes.
#[derive(Clone, Debug, Default)]
pub struct Bext {
    pub description: String,
    pub originator: String,
    pub originator_reference: String,
    /// `yyyy-mm-dd`.
    pub date: String,
    /// `hh:mm:ss`.
    pub time: String,
    /// Samples since midnight. This is the field a host reads to spot the file back where it came
    /// from, and it is why the capture time has to be threaded all the way down here.
    pub time_reference: u64,
    pub coding_history: String,
}

#[derive(Clone, Debug, Default)]
pub struct WavMeta {
    pub acid: Option<Acid>,
    pub bext: Option<Bext>,
}

impl WavMeta {
    pub fn none() -> WavMeta {
        WavMeta::default()
    }
}

/// Writes a complete WAV file.
///
/// `planes` is one slice per channel, all the same length — the layout the ring already stores, so
/// no de-interleave happens anywhere before this point.
///
/// # Panics
/// If `planes` is empty, if the planes differ in length, or if dither is requested for float.
pub fn write(planes: &[&[f32]], spec: &WavSpec, meta: &WavMeta) -> Vec<u8> {
    assert!(!planes.is_empty(), "a wav file needs at least one channel");
    let frames = planes[0].len();
    assert!(
        planes.iter().all(|p| p.len() == frames),
        "every channel must be the same length"
    );
    assert!(
        !(spec.dither && spec.depth == Depth::F32),
        "dither is quantisation noise and float is not quantised"
    );

    let channels = planes.len() as u16;
    let block_align = channels * spec.depth.bits() / 8;
    let byte_rate = spec.sample_rate * u32::from(block_align);
    let float = spec.depth == Depth::F32;

    let mut out = Vec::with_capacity(64 + frames * usize::from(channels) * spec.depth.bytes());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&0u32.to_le_bytes()); // patched at the end
    out.extend_from_slice(b"WAVE");

    // `fmt `. Non-PCM formats are required to carry the extension size field, and to be followed by
    // a `fact` chunk stating the sample count; a strict reader is entitled to reject a float file
    // without them, and the cost of being correct here is twelve bytes.
    chunk(&mut out, b"fmt ", |body| {
        body.extend_from_slice(&spec.depth.format_tag().to_le_bytes());
        body.extend_from_slice(&channels.to_le_bytes());
        body.extend_from_slice(&spec.sample_rate.to_le_bytes());
        body.extend_from_slice(&byte_rate.to_le_bytes());
        body.extend_from_slice(&block_align.to_le_bytes());
        body.extend_from_slice(&spec.depth.bits().to_le_bytes());
        if float {
            body.extend_from_slice(&0u16.to_le_bytes()); // cbSize
        }
    });
    if float {
        chunk(&mut out, b"fact", |body| {
            body.extend_from_slice(&(frames as u32).to_le_bytes());
        });
    }

    if let Some(bext) = &meta.bext {
        chunk(&mut out, b"bext", |body| write_bext(body, bext));
    }
    if let Some(acid) = &meta.acid {
        chunk(&mut out, b"acid", |body| write_acid(body, acid));
    }

    chunk(&mut out, b"data", |body| {
        let mut dither = Lcg::new(spec.dither_seed);
        for frame in 0..frames {
            for plane in planes {
                write_sample(body, plane[frame], spec, &mut dither);
            }
        }
    });

    let riff_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff_size.to_le_bytes());
    out
}

/// Appends a chunk, writing its size once the body is known and padding to an even length.
///
/// RIFF requires chunks to start on even offsets. An odd-length body followed by no pad byte shifts
/// every later chunk by one, which most readers survive by resynchronising and some do not.
fn chunk(out: &mut Vec<u8>, id: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    out.extend_from_slice(id);
    let size_at = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());
    let body_at = out.len();
    body(out);
    let size = (out.len() - body_at) as u32;
    out[size_at..size_at + 4].copy_from_slice(&size.to_le_bytes());
    if size % 2 == 1 {
        out.push(0);
    }
}

/// Writes `text` as exactly `width` bytes, truncated or zero-padded. Non-ASCII is dropped rather
/// than encoded: these are fixed-width ASCII fields and a multi-byte character would either
/// overflow the field or be cut in half.
fn fixed(out: &mut Vec<u8>, text: &str, width: usize) {
    let start = out.len();
    for byte in text.bytes().filter(|b| b.is_ascii() && *b >= 0x20).take(width) {
        out.push(byte);
    }
    out.resize(start + width, 0);
}

fn write_bext(out: &mut Vec<u8>, bext: &Bext) {
    fixed(out, &bext.description, 256);
    fixed(out, &bext.originator, 32);
    fixed(out, &bext.originator_reference, 32);
    fixed(out, &bext.date, 10);
    fixed(out, &bext.time, 8);
    out.extend_from_slice(&(bext.time_reference as u32).to_le_bytes());
    out.extend_from_slice(&((bext.time_reference >> 32) as u32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // version
    out.resize(out.len() + 64, 0); // UMID, unset
    out.resize(out.len() + 190, 0); // reserved
    out.extend_from_slice(bext.coding_history.as_bytes());
}

fn write_acid(out: &mut Vec<u8>, acid: &Acid) {
    // bit 0 one-shot, bit 1 root note present, bit 2 stretch, bit 3 disk-based.
    // One-shot stays clear: the whole point of this file is that it is a loop with a tempo.
    let mut flags: u32 = 0x04;
    if acid.root.is_some() {
        flags |= 0x02;
    }
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&u16::from(acid.root.unwrap_or(60)).to_le_bytes());
    out.extend_from_slice(&0x8000u16.to_le_bytes()); // fixed in every file anyone has looked at
    out.extend_from_slice(&0f32.to_le_bytes()); // ditto
    out.extend_from_slice(&acid.quarters.to_le_bytes());
    out.extend_from_slice(&(acid.meter.den as u16).to_le_bytes());
    out.extend_from_slice(&(acid.meter.num as u16).to_le_bytes());
    out.extend_from_slice(&acid.tempo.to_le_bytes());
}

fn write_sample(out: &mut Vec<u8>, value: f32, spec: &WavSpec, dither: &mut Lcg) {
    match spec.depth {
        Depth::F32 => out.extend_from_slice(&value.to_le_bytes()),
        Depth::I24 => {
            let v = quantise(value, 8_388_608.0, -8_388_608, 8_388_607, spec.dither, dither);
            out.extend_from_slice(&[v as u8, (v >> 8) as u8, (v >> 16) as u8]);
        }
        Depth::I16 => {
            let v = quantise(value, 32_768.0, -32_768, 32_767, spec.dither, dither);
            out.extend_from_slice(&(v as i16).to_le_bytes());
        }
    }
}

/// Scales, dithers and clamps one sample.
///
/// Scaling by the full negative range and clamping — rather than by `max` to dodge the clip — keeps
/// unity gain exact, so a sample at −1.0 comes back as −1.0 rather than very slightly short of it.
fn quantise(value: f32, scale: f32, min: i32, max: i32, dither: bool, rng: &mut Lcg) -> i32 {
    let mut scaled = f64::from(value) * f64::from(scale);
    if dither {
        // Triangular PDF, one LSB peak to peak: the sum of two independent uniforms. TPDF is the
        // standard choice because it makes the quantisation error independent of the signal, which
        // rectangular dither does not.
        scaled += rng.unit() + rng.unit() - 1.0;
    }
    (scaled.round() as i64).clamp(min as i64, max as i64) as i32
}

/// A small linear congruential generator, so dither needs no dependency and stays reproducible.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Lcg {
        Lcg(seed | 1)
    }

    fn unit(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Finds a chunk's body. A deliberately naive walker: if the writer's padding is wrong this
    /// desynchronises and the test fails, which is the point.
    fn find<'a>(wav: &'a [u8], id: &[u8; 4]) -> Option<&'a [u8]> {
        let mut at = 12;
        while at + 8 <= wav.len() {
            let this = &wav[at..at + 4];
            let size = u32::from_le_bytes(wav[at + 4..at + 8].try_into().ok()?) as usize;
            if this == id {
                return wav.get(at + 8..at + 8 + size);
            }
            at += 8 + size + (size % 2);
        }
        None
    }

    fn u16_at(b: &[u8], at: usize) -> u16 { u16::from_le_bytes(b[at..at + 2].try_into().unwrap()) }
    fn u32_at(b: &[u8], at: usize) -> u32 { u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) }

    #[test]
    fn the_riff_header_describes_the_file_that_follows() {
        let left = [0.0f32, 0.5, -0.5, 1.0];
        let right = [0.25f32, -0.25, 0.0, -1.0];
        let wav = write(&[&left, &right], &WavSpec::new(48_000, Depth::F32), &WavMeta::none());

        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(
            u32_at(&wav, 4) as usize,
            wav.len() - 8,
            "the RIFF size must cover everything after itself"
        );

        let fmt = find(&wav, b"fmt ").expect("fmt is mandatory");
        assert_eq!(fmt.len(), 18, "a non-PCM fmt chunk carries cbSize");
        assert_eq!(u16_at(fmt, 0), 3, "WAVE_FORMAT_IEEE_FLOAT");
        assert_eq!(u16_at(fmt, 2), 2, "channels");
        assert_eq!(u32_at(fmt, 4), 48_000);
        assert_eq!(u32_at(fmt, 8), 48_000 * 2 * 4, "byte rate");
        assert_eq!(u16_at(fmt, 12), 8, "block align");
        assert_eq!(u16_at(fmt, 14), 32, "bits");

        let fact = find(&wav, b"fact").expect("float files need a fact chunk");
        assert_eq!(u32_at(fact, 0), 4, "four frames");
    }

    #[test]
    fn float_samples_round_trip_exactly_and_interleaved() {
        let left = [0.0f32, 0.5, -0.5, 1.0];
        let right = [0.25f32, -0.25, 0.0, -1.0];
        let wav = write(&[&left, &right], &WavSpec::new(48_000, Depth::F32), &WavMeta::none());
        let data = find(&wav, b"data").expect("data is mandatory");
        assert_eq!(data.len(), 4 * 2 * 4);
        let (words, _) = data.as_chunks::<4>();
        let read: Vec<f32> = words.iter().map(|b| f32::from_le_bytes(*b)).collect();
        assert_eq!(read, vec![0.0, 0.25, 0.5, -0.25, -0.5, 0.0, 1.0, -1.0]);
    }

    #[test]
    fn full_scale_survives_every_depth() {
        for depth in [Depth::F32, Depth::I24, Depth::I16] {
            let mut spec = WavSpec::new(48_000, depth);
            spec.dither = false;
            let peaks = [1.0f32, -1.0];
            let wav = write(&[&peaks], &spec, &WavMeta::none());
            let data = find(&wav, b"data").expect("data is mandatory");
            match depth {
                Depth::F32 => {
                    assert_eq!(f32::from_le_bytes(data[0..4].try_into().unwrap()), 1.0);
                    assert_eq!(f32::from_le_bytes(data[4..8].try_into().unwrap()), -1.0);
                }
                Depth::I16 => {
                    assert_eq!(i16::from_le_bytes(data[0..2].try_into().unwrap()), 32_767);
                    assert_eq!(i16::from_le_bytes(data[2..4].try_into().unwrap()), -32_768);
                }
                Depth::I24 => {
                    let first = i32::from_le_bytes([data[0], data[1], data[2], 0]) << 8 >> 8;
                    let second = i32::from_le_bytes([data[3], data[4], data[5], 0]) << 8 >> 8;
                    assert_eq!(first, 8_388_607);
                    assert_eq!(second, -8_388_608);
                }
            }
        }
    }

    #[test]
    fn dither_is_deterministic_and_only_one_lsb() {
        let quiet: Vec<f32> = (0..512).map(|i| (i as f32 / 512.0 - 0.5) * 1e-4).collect();
        let spec = WavSpec::new(48_000, Depth::I16);
        assert!(spec.dither, "16-bit should dither by default");
        let a = write(&[&quiet], &spec, &WavMeta::none());
        let b = write(&[&quiet], &spec, &WavMeta::none());
        assert_eq!(a, b, "a seeded writer must be byte-identical run to run");

        let mut plain = spec;
        plain.dither = false;
        let c = write(&[&quiet], &plain, &WavMeta::none());
        assert_ne!(a, c, "dither that changes nothing is not dither");

        let dithered = find(&a, b"data").unwrap();
        let truncated = find(&c, b"data").unwrap();
        let (dithered, _) = dithered.as_chunks::<2>();
        let (truncated, _) = truncated.as_chunks::<2>();
        for (d, t) in dithered.iter().zip(truncated) {
            let d = i16::from_le_bytes(*d) as i32;
            let t = i16::from_le_bytes(*t) as i32;
            assert!((d - t).abs() <= 1, "dither moved a sample by {} LSB", (d - t).abs());
        }
    }

    #[test]
    fn the_acid_chunk_says_what_the_audio_actually_is() {
        // Four bars of 4/4 at 120 BPM is eight seconds and sixteen quarter notes.
        let frames = 8 * 48_000;
        let silence = vec![0.0f32; frames];
        let acid = Acid { tempo: 120.0, quarters: 16, meter: Meter::FOUR_FOUR, root: None };
        let wav = write(
            &[&silence],
            &WavSpec::new(48_000, Depth::F32),
            &WavMeta { acid: Some(acid), bext: None },
        );
        let body = find(&wav, b"acid").expect("acid chunk missing");
        assert_eq!(body.len(), 24, "the layout is fixed at 24 bytes");
        assert_eq!(u32_at(body, 0) & 0x01, 0, "a loop must not be flagged one-shot");
        assert_eq!(u32_at(body, 0) & 0x04, 0x04, "stretch must be on or the host will not warp it");
        assert_eq!(u32_at(body, 12), 16, "quarters");
        assert_eq!(u16_at(body, 16), 4, "meter denominator");
        assert_eq!(u16_at(body, 18), 4, "meter numerator");
        assert_eq!(f32::from_le_bytes(body[20..24].try_into().unwrap()), 120.0);

        // The chunk and the audio must agree, or the host warps the file to the wrong length.
        let stated = u32_at(body, 12) as f64 * 60.0 / 120.0;
        assert!((stated - frames as f64 / 48_000.0).abs() < 1e-9);
    }

    #[test]
    fn the_bext_chunk_is_602_bytes_before_its_history() {
        let silence = [0.0f32; 8];
        let bext = Bext {
            description: "Waveroll capture".into(),
            originator: "Waveroll".into(),
            date: "2026-08-21".into(),
            time: "14:32:07".into(),
            time_reference: 0x1_0000_0002,
            coding_history: "A=PCM,F=48000,W=32,M=mono\r\n".into(),
            ..Bext::default()
        };
        let wav = write(
            &[&silence],
            &WavSpec::new(48_000, Depth::F32),
            &WavMeta { acid: None, bext: Some(bext.clone()) },
        );
        let body = find(&wav, b"bext").expect("bext chunk missing");
        assert_eq!(body.len(), 602 + bext.coding_history.len());
        assert_eq!(&body[0..16], b"Waveroll capture");
        assert_eq!(&body[256..264], b"Waveroll");
        assert_eq!(&body[320..330], b"2026-08-21");
        assert_eq!(&body[330..338], b"14:32:07");
        // The timestamp is split across two 32-bit words, low first, and a writer that dropped the
        // high one would place anything past 13 hours at 48 kHz in the wrong place.
        assert_eq!(u32_at(body, 338), 2, "TimeReferenceLow");
        assert_eq!(u32_at(body, 342), 1, "TimeReferenceHigh");
        assert_eq!(u16_at(body, 346), 1, "version");
        assert_eq!(&body[602..], b"A=PCM,F=48000,W=32,M=mono\r\n");
    }

    #[test]
    fn an_odd_length_chunk_is_padded_so_later_chunks_stay_aligned() {
        let silence = [0.0f32; 4];
        let bext = Bext { coding_history: "odd".into(), ..Bext::default() };
        let wav = write(
            &[&silence],
            &WavSpec::new(48_000, Depth::F32),
            &WavMeta { acid: Some(Acid {
                tempo: 90.0, quarters: 4, meter: Meter::FOUR_FOUR, root: Some(69),
            }), bext: Some(bext) },
        );
        // acid follows the odd-length bext; finding it at all proves the pad byte is there.
        let body = find(&wav, b"acid").expect("acid was lost behind an unpadded chunk");
        assert_eq!(f32::from_le_bytes(body[20..24].try_into().unwrap()), 90.0);
        assert_eq!(u32_at(body, 0) & 0x02, 0x02, "root note flag");
        assert_eq!(u16_at(body, 4), 69);
        assert!(find(&wav, b"data").is_some(), "data was lost too");
    }

    #[test]
    #[should_panic(expected = "float is not quantised")]
    fn dithering_float_is_refused() {
        let mut spec = WavSpec::new(48_000, Depth::F32);
        spec.dither = true;
        write(&[&[0.0f32; 4]], &spec, &WavMeta::none());
    }
}
