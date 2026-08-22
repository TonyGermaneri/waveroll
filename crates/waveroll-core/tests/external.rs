//! Checks written files against decoders that did not write them.
//!
//! The unit tests in `wav.rs` assert that the bytes are what this crate meant to produce, which is
//! necessary and not sufficient — a chunk layout can be internally consistent, pass every one of
//! its own assertions, and still be a file a host refuses. `afinfo` is CoreAudio, which is the code
//! Logic actually opens files with, and `ffprobe` is an entirely independent implementation. If
//! both agree with the header, the file is real.
//!
//! Skips loudly when a tool is absent rather than failing, so this is never the reason a checkout
//! goes red on a machine without ffmpeg.

use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

use waveroll_core::tempo::Meter;
use waveroll_core::wav::{self, Acid, Bext, Depth, WavMeta, WavSpec};

const SR: u32 = 48_000;
/// Four bars of 4/4 at 120 BPM.
const SECONDS: usize = 8;

fn have(tool: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `tag` keeps the two tests off each other's files: they run in parallel, and sharing a path
/// means one deletes what the other is still reading.
fn sample_file(depth: Depth, name: &str, tag: &str) -> PathBuf {
    let frames = SR as usize * SECONDS;
    let tone: Vec<f32> = (0..frames)
        .map(|i| (i as f32 / SR as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.5)
        .collect();
    let other: Vec<f32> = tone.iter().map(|v| -v).collect();
    let meta = WavMeta {
        acid: Some(Acid { tempo: 120.0, quarters: 16, meter: Meter::FOUR_FOUR, root: None }),
        bext: Some(Bext {
            description: "Waveroll capture".into(),
            originator: "Waveroll".into(),
            date: "2026-08-21".into(),
            time: "14:32:07".into(),
            time_reference: 14 * 3600 * u64::from(SR),
            // Deliberately odd length, so the pad byte before `acid` is exercised on a real decoder.
            coding_history: "A=PCM,F=48000,W=32,M=stereo\r\n".into(),
            ..Bext::default()
        }),
    };
    let bytes = wav::write(&[&tone, &other], &WavSpec::new(SR, depth), &meta);
    let path = env::temp_dir().join(format!("waveroll-{tag}-{name}.wav"));
    fs::write(&path, &bytes).expect("the temp directory is writable");
    path
}

fn ffprobe(path: &PathBuf, entries: &str) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", entries, "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe runs");
    assert!(out.status.success(), "ffprobe rejected {}", path.display());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn ffprobe_agrees_with_the_header() {
    if !have("ffprobe") {
        println!("SKIPPED: ffprobe not installed");
        return;
    }
    for (name, depth, codec) in [
        ("f32", Depth::F32, "pcm_f32le"),
        ("i24", Depth::I24, "pcm_s24le"),
        ("i16", Depth::I16, "pcm_s16le"),
    ] {
        let path = sample_file(depth, name, "ffprobe");
        let reported = ffprobe(&path, "stream=codec_name,sample_rate,channels,duration");
        let fields: Vec<&str> = reported.split(',').collect();
        assert_eq!(fields[0], codec, "{name}: codec");
        assert_eq!(fields[1], SR.to_string(), "{name}: sample rate");
        assert_eq!(fields[2], "2", "{name}: channels");
        // Exactly eight seconds. A duration that is merely close means the data chunk's size
        // disagrees with the block align, which is how a file ends up a few samples short of the
        // bar it was cut on.
        assert_eq!(fields[3], "8.000000", "{name}: duration");
        let _ = fs::remove_file(&path);
    }
}

#[test]
fn coreaudio_agrees_with_the_header() {
    if !have("afinfo") {
        println!("SKIPPED: afinfo not installed (this is a macOS tool)");
        return;
    }
    // afinfo has short names for the formats CoreAudio treats as canonical and a descriptive
    // fallback for the rest, so the strings are not parallel: Float32 and Int16, but
    // `lpcm (0x0000000C) 24-bit little-endian signed integer`. The rate and the format are
    // therefore checked separately rather than as one expected line.
    for (name, depth, format, bytes_per_frame) in [
        ("f32", Depth::F32, "Float32", 8),
        ("i24", Depth::I24, "24-bit little-endian signed integer", 6),
        ("i16", Depth::I16, "Int16", 4),
    ] {
        let path = sample_file(depth, name, "afinfo");
        let out = Command::new("afinfo").arg(&path).output().expect("afinfo runs");
        assert!(out.status.success(), "{name}: CoreAudio refused the file");
        let text = String::from_utf8_lossy(&out.stdout);

        assert!(text.contains("WAVE"), "{name}: not recognised as WAVE\n{text}");
        assert!(
            text.contains(&format!("2 ch,  {SR} Hz,")),
            "{name}: expected stereo at {SR} Hz in\n{text}"
        );
        assert!(text.contains(format), "{name}: expected `{format}` in\n{text}");
        assert!(
            text.contains("estimated duration: 8.000000 sec"),
            "{name}: wrong duration in\n{text}"
        );
        // Header size is the whole point of the padding rules: if a pad byte were missing the
        // audio would start one byte early and CoreAudio would report a different payload size.
        let expected = SR as usize * SECONDS * bytes_per_frame;
        assert!(
            text.contains(&format!("audio bytes: {expected}")),
            "{name}: expected {expected} audio bytes in\n{text}"
        );
        let _ = fs::remove_file(&path);
    }
}

// ---------------------------------------------------------------------------------------
// MIDI
// ---------------------------------------------------------------------------------------

use waveroll_core::grid::Selection;
use waveroll_core::smf::{self, Clip, Note, SmfOptions};
use waveroll_core::tempo::TempoMap;

/// Renders a four-bar clip through fluidsynth and returns the audio duration.
fn render_four_bars(bpm: f64, tag: &str) -> f64 {
    let map = TempoMap::new(SR, bpm, Meter::FOUR_FOUR);
    let bar = |n: f64| map.frame_at_bars(n);
    let notes: Vec<Note> = (0..16)
        .map(|i| Note {
            start: bar(f64::from(i) * 0.25),
            end: Some(bar(f64::from(i) * 0.25 + 0.2)),
            channel: 0,
            key: 60,
            on_velocity: 90,
            off_velocity: 0,
        })
        .collect();
    let clip = Clip { notes: &notes, ..Clip::default() };
    let bytes = smf::write(Selection { start: 0, end: bar(4.0) }, &map, &clip, &SmfOptions::default());

    let mid = env::temp_dir().join(format!("waveroll-{tag}.mid"));
    let wav = env::temp_dir().join(format!("waveroll-{tag}-rendered.wav"));
    fs::write(&mid, &bytes).expect("temp is writable");

    let out = Command::new("fluidsynth")
        .args(["-n", "-i", "-F"])
        .arg(&wav)
        .arg(&mid)
        .output()
        .expect("fluidsynth runs");
    assert!(
        out.status.success() && wav.exists(),
        "fluidsynth could not render {tag}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seconds: f64 = ffprobe(&wav, "format=duration").parse().expect("a duration");
    let _ = fs::remove_file(&mid);
    let _ = fs::remove_file(&wav);
    seconds
}

#[test]
fn an_independent_sequencer_agrees_about_how_long_the_clip_is() {
    if !have("fluidsynth") || !have("ffprobe") {
        println!("SKIPPED: needs both fluidsynth and ffprobe");
        return;
    }
    // fluidsynth renders a fixed tail past end-of-track so voices can decay, which makes an
    // absolute duration a brittle thing to assert. The *difference* between two tempi cancels the
    // tail exactly, and nothing but a correct reading of PPQ, the tempo meta and the delta times
    // can produce it. Four bars of 4/4 is eight seconds at 120 and sixteen at 60.
    let fast = render_four_bars(120.0, "smf120");
    let slow = render_four_bars(60.0, "smf60");
    println!("fluidsynth rendered {fast:.3}s at 120 BPM and {slow:.3}s at 60 BPM");
    assert!(
        (slow - fast - 8.0).abs() < 0.05,
        "halving the tempo should add exactly eight seconds, but added {:.3}",
        slow - fast
    );
    // And the faster one has to be at least its own musical length.
    assert!(fast >= 8.0, "the clip rendered shorter than four bars: {fast:.3}s");
}
