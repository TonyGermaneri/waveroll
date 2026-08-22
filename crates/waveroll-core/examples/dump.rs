//! Writes one of each supported file to a directory, for checking against tools that did not
//! write them. `cargo run -p waveroll-core --example dump -- <dir>`
use std::{env, f32::consts::TAU, fs};
use waveroll_core::grid::Selection;
use waveroll_core::smf::{self, Clip, Note, SmfOptions};
use waveroll_core::tempo::{Meter, TempoMap};
use waveroll_core::wav::{self, Acid, Bext, Depth, WavMeta, WavSpec};

fn main() {
    let dir = env::args().nth(1).unwrap_or_else(|| ".".into());
    let sr = 48_000u32;
    // Four bars of 4/4 at 120 BPM: eight seconds, sixteen quarter notes.
    let frames = sr as usize * 8;
    let left: Vec<f32> = (0..frames).map(|i| (i as f32 / sr as f32 * 440.0 * TAU).sin() * 0.5).collect();
    let right: Vec<f32> = (0..frames).map(|i| (i as f32 / sr as f32 * 660.0 * TAU).sin() * 0.5).collect();

    let meta = WavMeta {
        acid: Some(Acid { tempo: 120.0, quarters: 16, meter: Meter::FOUR_FOUR, root: None }),
        bext: Some(Bext {
            description: "Waveroll capture".into(),
            originator: "Waveroll".into(),
            date: "2026-08-21".into(),
            time: "14:32:07".into(),
            time_reference: 14 * 3600 * u64::from(sr),
            coding_history: "A=PCM,F=48000,W=32,M=stereo\r\n".into(),
            ..Bext::default()
        }),
    };

    for (name, depth) in [("f32", Depth::F32), ("i24", Depth::I24), ("i16", Depth::I16)] {
        let bytes = wav::write(&[&left, &right], &WavSpec::new(sr, depth), &meta);
        let path = format!("{dir}/waveroll-{name}.wav");
        fs::write(&path, &bytes).expect("write");
        println!("{path}  {} bytes", bytes.len());
    }

    let map = TempoMap::new(sr, 120.0, Meter::FOUR_FOUR);
    let bar = |n: f64| map.frame_at_bars(n);
    let notes: Vec<Note> = (0..16)
        .map(|i| Note {
            start: bar(f64::from(i) * 0.25),
            end: Some(bar(f64::from(i) * 0.25 + 0.2)),
            channel: 0,
            key: 60 + (i % 5) as u8 * 2,
            on_velocity: 90,
            off_velocity: 0,
        })
        .collect();
    let clip = Clip { notes: &notes, ..Clip::default() };
    let sel = Selection { start: 0, end: bar(4.0) };
    let bytes = smf::write(sel, &map, &clip, &SmfOptions::default());
    let path = format!("{dir}/waveroll.mid");
    fs::write(&path, &bytes).expect("write");
    println!("{path}  {} bytes", bytes.len());
}
