//! The ring, with a producer and a consumer on different threads.
//!
//! Everything else about the ring is tested on one thread, which is exactly the condition under
//! which its interesting failures cannot happen. The producer runs on the audio thread in every
//! real build and the consumers do not, so the properties worth checking are the cross-thread
//! ones: that published audio is whole, and that a read the writer overtook is *refused* rather
//! than returned as a plausible-looking seam of old and new.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use waveroll_core::ring;

/// A sample whose value states which frame it is.
///
/// Modulo a million so it stays exactly representable in `f32` — beyond 2^24 consecutive integers
/// stop being distinct, and a test that cannot tell two frames apart cannot detect a tear.
fn sample_for(frame: u64) -> f32 {
    (frame % 1_000_000) as f32
}

/// A hot writer and a reader working where the real one works.
///
/// The regime matters, and getting it wrong makes this test meaningless in either direction. The
/// plugin allocates a ring of 87 seconds and exports at most a 32-second selection, so the export
/// path sits 55 seconds of writing clear of the oldest end. A test that instead reads right at the
/// tail of a tiny ring is asking a question no ring can answer: nothing published to a buffer that
/// turns over every fraction of a millisecond is safe to read, and no amount of checking changes
/// that. This one keeps the same *ratio* the plugin has and hammers it.
#[test]
fn a_reader_working_where_the_real_one_works_never_sees_a_seam() {
    const CAPACITY: usize = 1 << 20;
    const WINDOW: usize = 1 << 14;
    // The same proportion of the ring the plugin's largest export occupies.
    const BEHIND: u64 = (CAPACITY as u64 * 3) / 8;

    let (mut producer, reader) = ring::ring(CAPACITY, 1, 48_000);
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut frame = 0u64;
            let mut block = vec![0.0f32; 256];
            while !stop.load(Ordering::Relaxed) {
                for (i, slot) in block.iter_mut().enumerate() {
                    *slot = sample_for(frame + i as u64);
                }
                producer.write(&[&block], block.len());
                frame += block.len() as u64;
            }
            frame
        })
    };

    while reader.head() < CAPACITY as u64 {
        std::hint::spin_loop();
    }

    let mut buffer = vec![0.0f32; WINDOW];
    let mut accepted = 0u64;
    let mut refused = 0u64;
    let mut torn = Vec::new();

    for _ in 0..5_000 {
        let start = reader.head() - BEHIND;
        if !reader.read_into(0, start, &mut buffer) {
            refused += 1;
            continue;
        }
        accepted += 1;
        for (i, got) in buffer.iter().enumerate() {
            let want = sample_for(start + i as u64);
            if *got != want {
                torn.push((start + i as u64, *got, want));
                break;
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    let written = writer.join().expect("the writer finished");

    println!("{written} frames written, {accepted} accepted, {refused} refused");
    assert!(written > CAPACITY as u64, "the writer must have lapped the ring many times");
    assert!(accepted > 4_000, "only {accepted} of 5,000 reads were accepted");
    assert!(
        torn.is_empty(),
        "{} accepted reads were seams of old and new audio; first at {:?}",
        torn.len(),
        torn.first()
    );
}

/// The tail is refused, which is the half of the promise that can be kept.
///
/// A range the writer has already reached cannot be recovered by any means, so the only useful
/// behaviour is to say so. This checks the saying-so, not the recovering.
#[test]
fn a_range_the_writer_has_reached_is_refused() {
    const CAPACITY: usize = 1 << 16;
    let (mut producer, reader) = ring::ring(CAPACITY, 1, 48_000);
    let block = vec![1.0f32; 1024];
    for _ in 0..(CAPACITY / 1024) * 3 {
        producer.write(&[&block], block.len());
    }

    let head = reader.head();
    let mut out = vec![0.0f32; 32];
    assert!(!reader.read_into(0, head - CAPACITY as u64 - 1, &mut out), "older than the ring");
    assert!(!reader.read_into(0, head - 16, &mut out), "half of it is not written yet");
    assert!(reader.read_into(0, head - 32, &mut out), "exactly the newest frames are there");
    assert!(reader.read_into(0, head - CAPACITY as u64, &mut out), "and the oldest still held");
}

#[test]
fn a_reader_that_falls_behind_is_told_so_rather_than_reading_rubbish() {
    const CAPACITY: usize = 1 << 12;
    let (mut producer, mut reader) = ring::ring(CAPACITY, 1, 48_000);
    let block = vec![1.0f32; 512];

    // Lap it several times without the consumer looking.
    for _ in 0..40 {
        producer.write(&[&block], block.len());
    }
    assert_eq!(reader.available(), CAPACITY, "a lapped reader sees at most the whole ring");
    assert!(reader.laps() > 0, "and is told that it was lapped");

    // The oldest frames are genuinely gone, and asking for them is refused.
    let mut out = vec![0.0f32; 16];
    assert!(!reader.read_into(0, 0, &mut out), "frame zero is long overwritten");
    assert!(reader.read_into(0, reader.head() - 16, &mut out), "the newest frames are there");
}

#[test]
fn two_consumers_do_not_disturb_each_other() {
    // The render path and the exporter each hold their own cursor over the same memory. One
    // falling behind must not move the other.
    let (mut producer, mut first) = ring::ring(1 << 12, 1, 48_000);

    let block = vec![0.5f32; 256];
    for _ in 0..4 {
        producer.write(&[&block], block.len());
    }
    // Made after some audio exists, so it starts at the head rather than at zero.
    let mut second = producer.reader();

    assert_eq!(first.available(), 1024, "the first reader started at zero");
    first.advance(1024);
    assert_eq!(first.available(), 0);
    // The second was created after some audio existed, so it starts at the head and sees only
    // what arrived afterwards — independently of what the first has done.
    assert_eq!(second.available(), 0, "a reader made at the head has no backlog");

    for _ in 0..2 {
        producer.write(&[&block], block.len());
    }
    assert_eq!(first.available(), 512);
    assert_eq!(second.available(), 512);
}
