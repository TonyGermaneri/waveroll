/**
 * Wait-free, single-producer / single-consumer audio ring, in JavaScript.
 *
 * The mirror image of `waveroll_core::ring`, and it exists for one reason: the AudioWorklet writes
 * on the audio render thread, and Rust cannot be put there without a nightly toolchain and shared
 * wasm memory. Forty lines of JavaScript that never allocate is a better trade than that, so the
 * worklet produces into this and the main thread drains it into wasm once per painted frame.
 *
 * Planar, overwriting, and never blocking. Dropping old audio is strictly better than stalling the
 * thread that produced it.
 */

const CONTROL_WORDS = 16;
const IDX_WRITE = 0;

/** Frame counters are published modulo 2^30: a plain Int32 would go negative after ~3 h at 192 kHz. */
export const COUNTER_MASK = 0x3fffffff;

export class AudioRing {
  constructor(buffer, capacity, channels, sampleRate) {
    this.control = new Int32Array(buffer, 0, CONTROL_WORDS);
    this.data = new Float32Array(buffer, CONTROL_WORDS * 4, capacity * channels);
    this.capacity = capacity;
    this.mask = capacity - 1;
    this.channels = channels;
    this.sampleRate = sampleRate;
    this.planes = [];
    for (let c = 0; c < channels; c++) {
      this.planes.push(this.data.subarray(c * capacity, (c + 1) * capacity));
    }
  }

  static create(capacity, channels, sampleRate) {
    if (capacity <= 0 || (capacity & (capacity - 1)) !== 0) {
      throw new Error(`ring capacity must be a power of two, got ${capacity}`);
    }
    const bytes = CONTROL_WORDS * 4 + capacity * channels * 4;
    const shared = typeof SharedArrayBuffer !== 'undefined' && globalThis.crossOriginIsolated;
    const buffer = shared ? new SharedArrayBuffer(bytes) : new ArrayBuffer(bytes);
    const ring = new AudioRing(buffer, capacity, channels, sampleRate);
    Atomics.store(ring.control, IDX_WRITE, 0);
    return ring;
  }

  static attach(layout) {
    return new AudioRing(layout.buffer, layout.capacity, layout.channels, layout.sampleRate);
  }

  get layout() {
    return {
      buffer: this.data.buffer,
      capacity: this.capacity,
      channels: this.channels,
      sampleRate: this.sampleRate,
    };
  }

  get shared() {
    return typeof SharedArrayBuffer !== 'undefined' && this.data.buffer instanceof SharedArrayBuffer;
  }

  get writeIndex() {
    return Atomics.load(this.control, IDX_WRITE);
  }

  /**
   * Producer entry point. Real-time safe: no allocation, no locking, no branch on consumer state.
   * A mono source into a stereo ring duplicates its last plane rather than making the caller do it.
   */
  write(sources, count) {
    const start = Atomics.load(this.control, IDX_WRITE);
    const offset = start & this.mask;
    const contiguous = Math.min(count, this.capacity - offset);
    for (let c = 0; c < this.channels; c++) {
      const src = sources[Math.min(c, sources.length - 1)];
      if (!src) continue;
      const dst = this.planes[c];
      dst.set(src.subarray(0, contiguous), offset);
      if (contiguous < count) dst.set(src.subarray(contiguous, count), 0);
    }
    Atomics.store(this.control, IDX_WRITE, (start + count) & COUNTER_MASK);
  }
}

/** A consumer with its own cursor, which detects being lapped rather than reading a seam. */
export class RingReader {
  constructor(ring) {
    this.ring = ring;
    this.cursor = ring.writeIndex;
    this.laps = 0;
  }

  available() {
    const write = this.ring.writeIndex;
    const diff = (write - this.cursor) & COUNTER_MASK;
    if (diff > this.ring.capacity) {
      this.laps++;
      this.cursor = (write - this.ring.capacity) & COUNTER_MASK;
      return this.ring.capacity;
    }
    return diff;
  }

  /**
   * Copies up to `out.length / channels` frames into `out`, interleaved, and advances.
   * Returns the frame count taken. Interleaved because that is the shape wasm wants, and doing it
   * here saves a second pass over the samples on the other side of the boundary.
   */
  drainInterleaved(out) {
    const channels = this.ring.channels;
    const frames = Math.min(this.available(), Math.floor(out.length / channels));
    for (let i = 0; i < frames; i++) {
      const at = (this.cursor + i) & this.ring.mask;
      for (let c = 0; c < channels; c++) out[i * channels + c] = this.ring.planes[c][at];
    }
    this.cursor = (this.cursor + frames) & COUNTER_MASK;
    return frames;
  }
}
