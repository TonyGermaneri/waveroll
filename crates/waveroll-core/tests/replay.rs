//! Replays real MIDI clock captured from a DAW through the estimator.
//!
//! The synthetic tests in `clock.rs` prove the estimator does the right thing with the jitter I
//! thought to write down. This one proves it against jitter nobody designed — which is the only
//! kind that has ever broken a clock. Traces are captured with `tools/clock-trace.html` and checked
//! in under `tests/traces/`, so a regression here is reproducible years later without a DAW.
//!
//! With no traces present the test passes and says so. That is deliberate: this must not be the
//! reason a fresh checkout fails, and a skipped check that announces itself is honest in a way that
//! a silently absent one is not.

use std::fs;
use std::path::Path;

use waveroll_core::clock::{ClockMessage, ClockPll, decode_clock};

const SR: u32 = 48_000;

struct Event {
    frame: u64,
    message: ClockMessage,
}

/// One event per line: milliseconds, then raw bytes in hex. `#` starts a comment.
fn parse(text: &str) -> Vec<Event> {
    let mut events = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(ms) = fields.next().and_then(|f| f.parse::<f64>().ok()) else { continue };
        let bytes: Vec<u8> = fields.filter_map(|f| u8::from_str_radix(f, 16).ok()).collect();
        if let Some(message) = decode_clock(&bytes) {
            events.push(Event { frame: (ms * SR as f64 / 1000.0).round() as u64, message });
        }
    }
    events
}

#[test]
fn recorded_clock_streams_hold_their_tempo() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/traces");
    let mut traces: Vec<_> = fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "txt"))
                .collect()
        })
        .unwrap_or_default();
    traces.sort();

    if traces.is_empty() {
        println!(
            "SKIPPED: no traces in {}. Capture one with tools/clock-trace.html — see the README.",
            dir.display()
        );
        return;
    }

    for path in traces {
        let text = fs::read_to_string(&path).expect("trace is readable");
        let events = parse(&text);
        let name = path.file_name().expect("a file has a name").to_string_lossy().into_owned();
        assert!(events.len() > 100, "{name}: only {} usable events", events.len());

        let mut pll = ClockPll::new(SR, 120.0);
        let mut estimates = Vec::new();
        for event in &events {
            pll.feed(event.message, event.frame);
            if pll.settled() && pll.is_playing() {
                estimates.push(pll.bpm());
            }
        }
        assert!(!estimates.is_empty(), "{name}: the stream never settled while rolling");

        // Judge the second half: the first is the estimator finding the tempo, which is not what
        // this is measuring.
        let tail = &estimates[estimates.len() / 2..];
        let mean = tail.iter().sum::<f64>() / tail.len() as f64;
        let deviation = tail
            .iter()
            .map(|b| (b - mean).abs())
            .fold(0.0_f64, f64::max);

        println!(
            "{name}: {} events, {:.3} BPM mean, worst excursion {:.3} BPM, {} intervals rejected",
            events.len(),
            mean,
            deviation,
            pll.rejected()
        );

        assert!(
            mean.is_finite() && (20.0..=999.0).contains(&mean),
            "{name}: implausible mean tempo {mean}"
        );
        // A tenth of a BPM is about 8 ms of drift over a 16-bar window at 120 — under a 32nd note,
        // and comfortably inside what the calibration offset absorbs.
        assert!(
            deviation < 0.5,
            "{name}: tempo wandered by {deviation:.3} BPM on a stream that should be steady. \
             If the DAW really did ramp the tempo in this trace, split it or rename it *-ramp.txt \
             and exclude it here."
        );
    }
}
