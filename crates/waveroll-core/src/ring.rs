//! Wait-free, single-producer / multi-consumer audio ring.
//!
//! This is waveshape's `audio/ring.ts` with three changes, each of which the language paid for.
//!
//! * **The data is `AtomicU32`, not plain `f32`.** An overwriting ring lets the producer write
//!   a region a consumer is midway through reading; in JavaScript a racing read of a TypedArray
//!   over shared memory is merely *unspecified*, but in Rust it is undefined behaviour, and
//!   "the values are garbage and we throw them away anyway" is not a defence against UB. Relaxed
//!   atomic loads and stores of `u32` lower to the same instructions as plain ones on every
//!   architecture this will run on, so the race becomes defined for free. The only cost is that
//!   the copy is a loop rather than a `memcpy` — 256 stores per 128-frame stereo callback, which
//!   is nothing next to the FFT that follows it.
//!
//! * **The frame counter is a full `u64`.** The TypeScript version publishes `writeIndex mod 2^30`
//!   because JS atomics are `Int32` and a plain counter would go negative after about three hours
//!   at 192 kHz. Rust has `AtomicU64`, which at the same rate wraps after roughly three million
//!   years, so the masking and its careful modular difference arithmetic simply go away.
//!
//! * **Single-producer is enforced by ownership.** `Producer` is not `Clone` and `write` takes
//!   `&mut self`, so a second writer is a compile error rather than a comment asking you not to.
//!
//! What carries over unchanged is the important part: the layout is **planar**, so one channel is
//! a contiguous span that can be uploaded to a GPU storage buffer in a single write; the producer
//! is **overwriting**, never blocking, never allocating, and never branching on consumer state;
//! and consumers hold **private cursors** and detect being lapped, so a slow reader costs the
//! audio thread nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Shared state. Created once, then split into one [`Producer`] and any number of [`Reader`]s.
#[derive(Debug)]
struct Inner {
    /// `channels * capacity` samples, planar: channel `c` occupies `[c * capacity, (c+1) * capacity)`.
    data: Box<[AtomicU32]>,
    /// Total frames ever written. Published with `Release`; read with `Acquire`.
    write: AtomicU64,
    capacity: usize,
    mask: usize,
    channels: usize,
    sample_rate: u32,
}

/// The write half. Lives on the audio thread and is the only thing that may store into the ring.
#[derive(Debug)]
pub struct Producer {
    inner: Arc<Inner>,
}

/// A read half. Each holds its own cursor over the same memory, so the renderer and the exporter
/// can fall behind independently without either one affecting the other or the producer.
#[derive(Debug)]
pub struct Reader {
    inner: Arc<Inner>,
    cursor: u64,
    laps: u64,
}

/// Creates a ring and returns its two halves.
///
/// `capacity` is in **frames** and must be a power of two — the mask is what makes the wrap a
/// single `&` in the inner loop rather than a branch or a division.
///
/// # Panics
/// If `capacity` is not a power of two, or if `channels` is zero.
pub fn ring(capacity: usize, channels: usize, sample_rate: u32) -> (Producer, Reader) {
    assert!(capacity.is_power_of_two(), "ring capacity must be a power of two, got {capacity}");
    assert!(channels > 0, "a ring needs at least one channel");
    let mut data = Vec::with_capacity(capacity * channels);
    data.resize_with(capacity * channels, || AtomicU32::new(0));
    let inner = Arc::new(Inner {
        data: data.into_boxed_slice(),
        write: AtomicU64::new(0),
        capacity,
        mask: capacity - 1,
        channels,
        sample_rate,
    });
    let reader = Reader { inner: Arc::clone(&inner), cursor: 0, laps: 0 };
    (Producer { inner }, reader)
}

impl Producer {
    pub fn capacity(&self) -> usize { self.inner.capacity }
    pub fn channels(&self) -> usize { self.inner.channels }
    pub fn sample_rate(&self) -> u32 { self.inner.sample_rate }

    /// Frames written so far. Monotonic for the life of the ring.
    pub fn written(&self) -> u64 {
        self.inner.write.load(Ordering::Relaxed)
    }

    /// An additional independent consumer, positioned at the newest frame.
    pub fn reader(&self) -> Reader {
        Reader {
            inner: Arc::clone(&self.inner),
            cursor: self.inner.write.load(Ordering::Acquire),
            laps: 0,
        }
    }

    /// Producer entry point. Real-time safe: no allocation, no locking, no unbounded loop, and no
    /// read of any consumer's state.
    ///
    /// `sources` may hold fewer planes than the ring has channels, in which case the last one is
    /// duplicated — that is how a mono input fills a stereo ring without the caller having to
    /// build a second slice to say so.
    ///
    /// # Panics
    /// If `sources` is empty, or if any source plane is shorter than `frames`.
    pub fn write(&mut self, sources: &[&[f32]], frames: usize) {
        assert!(!sources.is_empty(), "write needs at least one source plane");
        if frames == 0 {
            return;
        }
        let inner = &*self.inner;
        // Relaxed is right for our own counter: this thread is the only writer, so it cannot read
        // a stale value of it, and the publication ordering is established by the Release below.
        let start = inner.write.load(Ordering::Relaxed);
        let offset = (start as usize) & inner.mask;
        // A write longer than the ring would lap itself inside this call; only the last
        // `capacity` frames could survive it, so refuse rather than silently keep a suffix.
        assert!(frames <= inner.capacity, "block of {frames} frames exceeds ring capacity");
        let contiguous = frames.min(inner.capacity - offset);

        for c in 0..inner.channels {
            let src = sources[c.min(sources.len() - 1)];
            assert!(src.len() >= frames, "source plane {c} is shorter than {frames} frames");
            let plane = c * inner.capacity;
            let head = &inner.data[plane + offset..plane + offset + contiguous];
            for (slot, &sample) in head.iter().zip(&src[..contiguous]) {
                slot.store(sample.to_bits(), Ordering::Relaxed);
            }
            let wrapped = &inner.data[plane..plane + (frames - contiguous)];
            for (slot, &sample) in wrapped.iter().zip(&src[contiguous..frames]) {
                slot.store(sample.to_bits(), Ordering::Relaxed);
            }
        }

        // Release: everything stored above is visible to any consumer that observes this counter
        // with Acquire. This is the single line that publishes the block.
        inner.write.store(start + frames as u64, Ordering::Release);
    }
}

impl Reader {
    pub fn capacity(&self) -> usize { self.inner.capacity }
    pub fn channels(&self) -> usize { self.inner.channels }
    pub fn sample_rate(&self) -> u32 { self.inner.sample_rate }

    /// Absolute index one past the newest published frame.
    pub fn head(&self) -> u64 {
        self.inner.write.load(Ordering::Acquire)
    }

    /// Absolute index of the next frame this reader has not consumed.
    pub fn position(&self) -> u64 { self.cursor }

    /// How many times this reader has been overtaken and had to skip forward.
    pub fn laps(&self) -> u64 { self.laps }

    /// Frames published but not yet consumed.
    ///
    /// If the producer has lapped this reader the backlog is unrecoverable, so the cursor jumps to
    /// the oldest frame that still exists and the lap is counted. For a visualiser, dropping old
    /// audio is strictly better than stalling the thread that produced it.
    pub fn available(&mut self) -> usize {
        let head = self.head();
        let diff = head - self.cursor;
        if diff > self.inner.capacity as u64 {
            self.laps += 1;
            self.cursor = head - self.inner.capacity as u64;
            return self.inner.capacity;
        }
        diff as usize
    }

    pub fn advance(&mut self, frames: usize) {
        self.cursor += frames as u64;
    }

    /// Discard any backlog and jump to the newest frame.
    pub fn skip_to_head(&mut self) {
        self.cursor = self.head();
    }

    /// The oldest absolute frame index still present in the ring.
    pub fn oldest(&self) -> u64 {
        self.head().saturating_sub(self.inner.capacity as u64)
    }

    /// True when `[start, end)` is entirely inside the window the ring still holds.
    ///
    /// This is what makes a stale selection fail closed. A selection is stored as absolute frame
    /// indices precisely so that the question "is what I picked still here" has an answer, and
    /// export must ask it rather than reading whatever happens to be at those offsets now.
    pub fn holds(&self, start: u64, end: u64) -> bool {
        end >= start && start >= self.oldest() && end <= self.head()
    }

    /// Copies `out.len()` frames of one channel, starting at absolute frame `start`, into `out`.
    ///
    /// Returns `false` without touching `out` when the range is no longer wholly in the ring.
    /// Reading a lapped range would return a seam of old and new audio that looks perfectly
    /// plausible, which is the worst possible failure for something about to be dropped into
    /// somebody's session.
    pub fn read_into(&self, channel: usize, start: u64, out: &mut [f32]) -> bool {
        let end = start + out.len() as u64;
        if !self.holds(start, end) {
            return false;
        }
        let inner = &*self.inner;
        let plane = channel.min(inner.channels - 1) * inner.capacity;
        for (i, slot) in out.iter_mut().enumerate() {
            let at = ((start + i as u64) as usize) & inner.mask;
            *slot = f32::from_bits(inner.data[plane + at].load(Ordering::Relaxed));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random source. A dependency-free LCG keeps the test reproducible and
    /// keeps the crate's dependency list empty, which is the point of writing it this way.
    fn ramp(from: usize, len: usize) -> Vec<f32> {
        (0..len).map(|i| (from + i) as f32).collect()
    }

    #[test]
    fn round_trips_a_block() {
        let (mut p, r) = ring(1024, 2, 48_000);
        let a = ramp(0, 128);
        let b = ramp(1000, 128);
        p.write(&[&a, &b], 128);
        let mut out = vec![0.0; 128];
        assert!(r.read_into(0, 0, &mut out));
        assert_eq!(out, a);
        assert!(r.read_into(1, 0, &mut out));
        assert_eq!(out, b);
    }

    #[test]
    fn a_mono_source_fills_every_channel() {
        let (mut p, r) = ring(64, 2, 48_000);
        let mono = ramp(0, 32);
        p.write(&[&mono], 32);
        let mut out = vec![0.0; 32];
        assert!(r.read_into(1, 0, &mut out));
        assert_eq!(out, mono, "the last source plane should be duplicated into channel 1");
    }

    #[test]
    fn a_block_that_straddles_the_wrap_is_contiguous_on_read() {
        let (mut p, r) = ring(64, 1, 48_000);
        // Land the write head 8 frames from the end, then write 32 across the seam.
        let filler = ramp(0, 56);
        p.write(&[&filler], 56);
        let across = ramp(500, 32);
        p.write(&[&across], 32);
        let mut out = vec![0.0; 32];
        assert!(r.read_into(0, 56, &mut out));
        assert_eq!(out, across);
    }

    #[test]
    fn the_counter_is_monotonic_across_many_wraps() {
        let (mut p, _r) = ring(64, 1, 48_000);
        let block = ramp(0, 16);
        for _ in 0..1000 {
            p.write(&[&block], 16);
        }
        assert_eq!(p.written(), 16_000);
    }

    #[test]
    fn a_lapped_reader_skips_forward_and_counts_it() {
        let (mut p, mut r) = ring(64, 1, 48_000);
        let block = ramp(0, 32);
        for _ in 0..10 {
            p.write(&[&block], 32);
        }
        assert_eq!(r.available(), 64, "a lapped reader sees at most the whole ring");
        assert_eq!(r.laps(), 1);
        assert_eq!(r.position(), 320 - 64);
    }

    #[test]
    fn a_range_the_writer_has_overtaken_fails_closed() {
        let (mut p, r) = ring(64, 1, 48_000);
        let block = ramp(0, 32);
        for _ in 0..4 {
            p.write(&[&block], 32);
        }
        let mut out = vec![7.0; 16];
        assert!(!r.read_into(0, 0, &mut out), "frame 0 is long gone");
        assert_eq!(out, vec![7.0; 16], "a refused read must not touch the output");
        assert!(r.read_into(0, 112, &mut out), "the newest 16 frames are still there");
    }

    #[test]
    fn a_range_running_past_the_head_fails_closed() {
        let (mut p, r) = ring(64, 1, 48_000);
        let block = ramp(0, 32);
        p.write(&[&block], 32);
        let mut out = vec![0.0; 16];
        assert!(!r.read_into(0, 24, &mut out), "8 of those 16 frames have not been written yet");
    }

    #[test]
    fn a_new_reader_starts_at_the_head() {
        let (mut p, _r) = ring(64, 1, 48_000);
        let block = ramp(0, 32);
        p.write(&[&block], 32);
        let mut fresh = p.reader();
        assert_eq!(fresh.position(), 32);
        assert_eq!(fresh.available(), 0);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn a_non_power_of_two_capacity_is_refused() {
        let _ = ring(1000, 2, 48_000);
    }
}
