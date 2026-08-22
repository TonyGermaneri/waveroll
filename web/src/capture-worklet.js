/**
 * Capture processor. Runs on the audio render thread at the render-quantum rate — 128 frames,
 * which is 375 Hz at 48 kHz.
 *
 * The only rule that matters here: never allocate, never lock, never do unbounded work.
 */

import { AudioRing } from './ring.js';

class CaptureProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super(options);
    const opts = options?.processorOptions ?? {};
    this.ring = opts.ring ? AudioRing.attach(opts.ring) : null;
    this.silent = 0;
    this.port.postMessage({ type: 'ready', sampleRate });
  }

  process(inputs) {
    const input = inputs[0];
    if (!input || input.length === 0 || !input[0]) {
      // Not producing yet, or disconnected. Stay alive; a processor that returns false is gone.
      if (++this.silent === 400) this.port.postMessage({ type: 'no-input' });
      return true;
    }
    this.silent = 0;
    if (this.ring) this.ring.write(input, input[0].length);
    return true;
  }
}

registerProcessor('waveroll-capture', CaptureProcessor);
