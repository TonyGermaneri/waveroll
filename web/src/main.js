/**
 * The page. It contributes devices, events and a canvas; it contributes no behaviour.
 *
 * Every decision — what a click selects, where the bar lines fall, which side of the write head a
 * column reads from, whether a block is captured at all — is in Rust, tested without a browser, and
 * shared with the builds that will not have a browser. Anything that leaked into this file would be
 * something to write a second time in C++ later and then keep in step.
 */

import init, { Waveroll } from '../pkg/waveroll_wasm.js';
import { AudioRing, RingReader } from './ring.js';

const $ = (id) => document.getElementById(id);
const canvas = $('canvas');

/** Frames of ring, as a power of two. 2^21 is 43.7 s at 48 kHz — comfortably over a 16-bar lap. */
const CAPACITY_LOG2 = 21;

const state = {
  wr: null,
  ctx: null,
  ring: null,
  reader: null,
  scratch: null,
  channels: 1,
  running: false,
  midiOn: false,
  clockSource: 'internal',
  lastClockAt: 0,
  drag: null,
  unitBars: 0, // 0 = auto
  demo: null,
  staged: null,
};

// ---------------------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------------------

/**
 * Lists inputs on the boot screen, once their labels are readable.
 *
 * A browser hides device labels until the page already holds a microphone permission, so an empty
 * list here is not "no devices" — it is "not permitted yet", and the picker fills in after the
 * first grant. Taking the default is a coin flip on a machine with a loopback driver, an
 * interface and a webcam all offering audio.
 */
async function listDevices() {
  const select = $('device');
  if (!select) return;
  const devices = await navigator.mediaDevices.enumerateDevices();
  const inputs = devices.filter((d) => d.kind === 'audioinput' && d.label);
  for (const device of inputs) {
    const option = document.createElement('option');
    option.value = device.deviceId;
    option.textContent = device.label;
    select.append(option);
  }
}

async function openCapture() {
  // Every browser defence is off. Chrome turns all three on by default, and automatic gain control
  // alone makes a captured level meaningless — which for a sampler means the file you drop is not
  // the sound you heard.
  const wanted = $('device')?.value;
  const stream = await navigator.mediaDevices.getUserMedia({
    audio: {
      echoCancellation: false,
      noiseSuppression: false,
      autoGainControl: false,
      channelCount: { ideal: 2 },
      // `exact` rather than `ideal`: a named device that has gone away should fail loudly rather
      // than silently capture something else, which is how you end up recording the wrong thing
      // for twenty minutes without noticing.
      ...(wanted ? { deviceId: { exact: wanted } } : {}),
    },
  });
  const settings = stream.getAudioTracks()[0].getSettings();
  const rate = settings.sampleRate ?? 48000;
  state.channels = Math.min(2, settings.channelCount ?? 1);

  // Opened at the device's own rate, so no resampler nobody chose sits in the path.
  const ctx = new AudioContext({ sampleRate: rate, latencyHint: 'interactive' });
  await ctx.audioWorklet.addModule(new URL('./capture-worklet.js', import.meta.url));

  const ring = AudioRing.create(1 << CAPACITY_LOG2, state.channels, rate);
  const node = new AudioWorkletNode(ctx, 'waveroll-capture', {
    numberOfInputs: 1,
    numberOfOutputs: 1,
    outputChannelCount: [state.channels],
    processorOptions: { ring: ring.layout },
  });
  const source = ctx.createMediaStreamSource(stream);
  source.connect(node);

  // A worklet whose output reaches nothing is not guaranteed to be pulled, so the chain runs into
  // a zero-gain node and on to the destination. This is not monitoring: the program makes no sound
  // by design, and removing this as "we output nothing" silently stops capture.
  const sink = ctx.createGain();
  sink.gain.value = 0;
  node.connect(sink).connect(ctx.destination);

  await ctx.resume();
  state.deviceLabel = stream.getAudioTracks()[0].label || 'default input';
  state.ctx = ctx;
  state.ring = ring;
  state.reader = new RingReader(ring);
  state.scratch = new Float32Array(65536 * state.channels);
  return { rate, shared: ring.shared };
}

/**
 * A MIDI event's arrival time, in frames of the audio clock.
 *
 * `getOutputTimestamp` is the only API that pairs a context time with a performance time, and that
 * pairing is the whole point: stamping clock ticks in frames of the device we are capturing from
 * cancels any rate difference between it and the browser's clock. Measured at 2.5 ppm against Live
 * over IAC — small, but it accumulates rather than averaging out.
 */
function frameStamp(eventTimeMs) {
  const ctx = state.ctx;
  if (!ctx) return 0;
  const ts = ctx.getOutputTimestamp();
  const base = ts.contextTime ?? ctx.currentTime;
  const perf = ts.performanceTime ?? performance.now();
  return Math.max(0, (base + (eventTimeMs - perf) / 1000) * ctx.sampleRate);
}

/**
 * A source that needs no device.
 *
 * Not only a convenience for testing: a microphone prompt is a modal the page cannot dismiss, CI
 * has no input at all, and someone opening this to see what it is should not have to grant access
 * to their microphone first. It generates the same material the native `paint` example does, so a
 * picture from the browser and a picture from the test suite are comparable.
 */
async function openDemo() {
  const rate = 48000;
  state.channels = 1;
  state.ctx = new AudioContext({ sampleRate: rate, latencyHint: 'interactive' });
  await state.ctx.resume();
  state.demo = { frame: 0, last: performance.now() };
  state.scratch = new Float32Array(1 << 16);
  return { rate, shared: false };
}

const TAU = Math.PI * 2;

/** Kick on the beat, hats on the eighths, a sustained bass, and a chorus eight bars in. */
function material(frame, rate, beat) {
  const t = frame / rate;
  const phase = (t / beat) % 1;
  const beatIndex = Math.floor(t / beat);
  const bar = Math.floor(beatIndex / 4);

  let v = 0;
  if (beatIndex % 4 === 0 || beatIndex % 8 === 6) {
    v += Math.sin(TAU * 90 * phase * beat) * Math.exp(-phase * 22) * 0.9;
  }
  const eighth = (t / (beat / 2)) % 1;
  if (eighth < 0.06) {
    // Deterministic, so the same moment always looks the same.
    const n = Math.sin(frame * 12.9898) * 43758.5453;
    v += (n - Math.floor(n)) * 2 - 1 > 0 ? 0.22 * Math.exp(-eighth * 90) : -0.22 * Math.exp(-eighth * 90);
  }
  const loud = bar % 16 >= 8 ? 1.0 : 0.45;
  v += Math.sin(TAU * 55 * t) * 0.3 * loud;
  v += Math.sin(TAU * 220 * t) * 0.1 * loud;
  return v * 0.62;
}

/** Generates however many frames real time has passed, so the demo rolls at the right speed. */
function pumpDemo(wr) {
  const demo = state.demo;
  const rate = state.ctx.sampleRate;
  const now = performance.now();
  // Clamped: a backgrounded tab can be gone for minutes, and catching all of it up in one block
  // would lap the ring in a single frame.
  const elapsed = Math.min(now - demo.last, 250);
  demo.last = now;
  const frames = Math.min(Math.round((elapsed / 1000) * rate), state.scratch.length);
  if (frames <= 0) return;
  const beat = 60 / (Number(document.getElementById('bpmIn')?.value) || 120);
  for (let i = 0; i < frames; i++) {
    state.scratch[i] = material(demo.frame + i, rate, beat);
  }
  demo.frame += frames;
  wr.push(state.scratch.subarray(0, frames));
}

// ---------------------------------------------------------------------------------------
// MIDI
// ---------------------------------------------------------------------------------------

async function enableMidi() {
  if (!navigator.requestMIDIAccess) {
    note('This browser has no Web MIDI. Chrome and Edge do.');
    return;
  }
  // No sysex: nothing here needs it, and asking turns a mild permission into one that can
  // reprogram the hardware on the other end.
  const access = await navigator.requestMIDIAccess({ sysex: false });
  const bind = () => {
    for (const input of access.inputs.values()) {
      input.onmidimessage = (event) => {
        const status = event.data[0];
        if (status < 0xf0) return; // channel traffic is not our business
        state.wr?.midi(event.data, frameStamp(event.timeStamp));
        state.lastClockAt = performance.now();
        state.clockSource = 'midi';
      };
    }
  };
  access.onstatechange = bind;
  bind();
  state.midiOn = true;
  $('midi').dataset.on = '1';
  $('midi').textContent = 'MIDI on';
}

// ---------------------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------------------

function resize() {
  // Capped at 2: past that the extra pixels cost real fill rate and buy nothing on a waveform,
  // which is mostly one-pixel features that are already sharp.
  const dpr = Math.min(window.devicePixelRatio || 1, 2);
  const w = Math.max(1, Math.round(canvas.clientWidth * dpr));
  const h = Math.max(1, Math.round(canvas.clientHeight * dpr));
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }
  // Called every frame rather than only on a change, because the pixel ratio travels with it and
  // a canvas that happened to be the right size already would otherwise never report one — which
  // left the grid choosing its unit against device pixels on exactly the paths where the layout
  // did not shift. The Rust side returns immediately when the size is unchanged.
  state.wr?.resize(w, h, dpr);
}

function frame() {
  requestAnimationFrame(frame);
  const wr = state.wr;
  if (!wr) return;
  resize();

  // The clock has gone quiet: fall back to the internal transport rather than freezing on a tempo
  // nothing is sending any more.
  if (state.clockSource === 'midi' && performance.now() - state.lastClockAt > 500) {
    state.clockSource = 'internal';
  }
  if (state.clockSource === 'internal') {
    wr.transport(state.running, Number($('bpmIn')?.value) || 120, 4, 4, false);
  }

  if (state.demo) {
    pumpDemo(wr);
  } else if (state.reader) {
    const frames = state.reader.drainInterleaved(state.scratch);
    if (frames > 0) wr.push(state.scratch.subarray(0, frames * state.channels));
  }

  wr.frame();
  paintStatus(JSON.parse(wr.status()));
}

function paintStatus(s) {
  $('cap').textContent = s.held ? 'frozen' : s.playing ? 'capturing' : 'stopped';
  $('marks').textContent = s.markers;
  $('cap-dot').dataset.on = s.playing ? '1' : '0';
  $('clock').textContent = state.clockSource;
  $('bpm').textContent = s.bpm.toFixed(2);
  $('lap').textContent = s.lap;
  $('window').textContent = s.windowBars;
  $('unit').textContent = state.unitBars === 0 ? `auto · ${fmtUnit(s.unit)}` : fmtUnit(s.unit);
  $('pos').textContent = (s.head * s.windowBars + 1).toFixed(2);
  if (!s.selection) {
    $('sel').textContent = '—';
  } else {
    // Position as a percentage across the window, not as a bar number: a selection older than the
    // window has no bar number in the lap on screen, and reporting one produces a negative.
    const where = `${(s.selection.from * 100).toFixed(0)}%`;
    // `pending` and `overwritten` are opposite problems and must not read the same.
    const flag =
      s.state === 'pending' ? ' not captured yet'
      : s.state === 'overwritten' ? ' overwritten'
      : !s.selectionInView ? ' out of view'
      : '';
    $('sel').textContent = `${s.selection.bars.toFixed(2)} bars @ ${where}${flag}`;
    $('sel').style.color = flag ? '#f0b342' : '';
  }
}

const fmtUnit = (bars) => {
  if (bars >= 1) return `${bars}`;
  const denominator = Math.round(1 / bars);
  return `1/${denominator}`;
};

// ---------------------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------------------

const fractionOf = (event) => {
  const rect = canvas.getBoundingClientRect();
  return Math.min(1, Math.max(0, (event.clientX - rect.left) / rect.width));
};

canvas.addEventListener('pointerdown', (event) => {
  canvas.setPointerCapture(event.pointerId);
  state.drag = { from: fractionOf(event), moved: false };
});
canvas.addEventListener('pointermove', (event) => {
  if (!state.drag) return;
  const to = fractionOf(event);
  // A pointer that has not travelled a pixel is a click, not a drag. Without this every click
  // becomes a one-pixel drag, and at high zoom those select different things.
  if (Math.abs(to - state.drag.from) * canvas.clientWidth > 2) state.drag.moved = true;
  if (state.drag.moved) state.wr?.drag(state.drag.from, to);
});
canvas.addEventListener('pointerup', (event) => {
  if (!state.drag) return;
  if (!state.drag.moved) state.wr?.click(state.drag.from);
  state.drag = null;
});

canvas.addEventListener(
  'wheel',
  (event) => {
    event.preventDefault();
    // Anchored on the pointer, never on the write head: anchor it to the head and the view chases,
    // and you can never hold still on the thing you were looking at.
    state.wr?.zoom(Math.exp(-event.deltaY * 0.002), fractionOf(event));
  },
  { passive: false },
);

const UNITS = [0, 1 / 32, 1 / 16, 1 / 8, 1 / 4, 1 / 2, 1, 2, 4];

function flash(key) {
  const el = document.querySelector(`#bindables span[data-key="${key}"]`);
  if (!el) return;
  el.classList.add('hit');
  setTimeout(() => el.classList.remove('hit'), 140);
}

window.addEventListener('keydown', (event) => {
  if (event.target instanceof HTMLInputElement) return;
  const wr = state.wr;
  if (!wr) return;
  const key = event.key;

  if (key >= '0' && key <= '9') {
    // Head to tail: the last N×10% of the window, ending at the write head.
    wr.select_percent(key === '0' ? 10 : Number(key));
    flash(key);
    event.preventDefault();
    return;
  }
  switch (key) {
    case '\\': wr.home(); break;
    case ' ': toggleRun(); event.preventDefault(); break;
    case 'd': wr.set_downbeat_now(); note('downbeat set'); break;
    case 'c': case 'Enter': flash('c'); stage(); break;
    case 'h': toggleHold(); break;
    case 'm': state.wr.mark(); note('marked'); break;
    case 'n': if (!state.wr.select_from_marker()) note('no marker behind the head'); break;
    case 'Escape': wr.clear_selection(); break;
    case '[': adjustWindow(-1); break;
    case ']': adjustWindow(1); break;
    case ',': cycleUnit(-1); break;
    case '.': cycleUnit(1); break;
    default: return;
  }
  event.preventDefault();
});

function cycleUnit(direction) {
  const at = UNITS.indexOf(state.unitBars);
  state.unitBars = UNITS[Math.min(UNITS.length - 1, Math.max(0, at + direction))];
  state.wr.set_unit(state.unitBars);
}

function adjustWindow(direction) {
  const sizes = [4, 8, 16, 32, 64, 128];
  const current = Number($('window').textContent);
  const at = sizes.indexOf(current);
  const next = sizes[Math.min(sizes.length - 1, Math.max(0, (at < 0 ? 2 : at) + direction))];
  state.wr.set_window_bars(next);
}

/**
 * Materialises the selection and offers it for dragging.
 *
 * The browser can only hand a drag target a *file promise* — `DownloadURL` — which Finder accepts
 * and a good many applications do not. So the same chip is also a plain download link, which
 * always works. A native shell hands over a real path and this whole caveat goes away.
 */
function stage() {
  const wr = state.wr;
  if (!wr) return;
  // Samples since midnight, local: the Broadcast Wave timestamp a host reads to spot a file back
  // where it was captured.
  const now = new Date();
  const secondsToday = now.getHours() * 3600 + now.getMinutes() * 60 + now.getSeconds();
  const bytes = wr.stage(secondsToday * (state.ctx?.sampleRate ?? 48000));
  if (bytes.length === 0) {
    note({
      empty: 'nothing selected',
      pending: 'the last cell has not been captured yet — it will be ready in a moment',
      overwritten: 'that selection has been overwritten',
    }[wr.selection_state()] ?? 'nothing to stage');
    return;
  }
  discard();
  const name = wr.stage_name();
  const blob = new Blob([bytes], { type: 'audio/wav' });
  const url = URL.createObjectURL(blob);
  state.staged = { url, name, size: bytes.length };

  const chip = $('chip');
  chip.textContent = `${name}  ${(bytes.length / 1048576).toFixed(2)} MB`;
  chip.href = url;
  chip.download = name;
  $('tray').hidden = false;
  note(`staged ${name}`);
}

$('chip').addEventListener('dragstart', (event) => {
  if (!state.staged) return;
  const { name, url } = state.staged;
  // Chromium only, and the receiver has to ask for the bytes rather than being handed a path.
  event.dataTransfer.setData('DownloadURL', `audio/wav:${name}:${url}`);
  event.dataTransfer.setData('text/uri-list', url);
  event.dataTransfer.effectAllowed = 'copy';
});

function discard() {
  if (state.staged) URL.revokeObjectURL(state.staged.url);
  state.staged = null;
  $('tray').hidden = true;
}

function toggleHold() {
  const on = !state.wr.is_held();
  state.wr.hold(on);
  $('hold').dataset.on = on ? '1' : '0';
  $('hold').textContent = on ? 'Held' : 'Hold';
}

function toggleRun() {
  state.running = !state.running;
  $('run').dataset.on = state.running ? '1' : '0';
  $('run').textContent = state.running ? 'Stop' : 'Run';
}

let noteTimer = 0;
function note(text) {
  const el = $('adapter');
  const was = el.dataset.base ?? el.textContent;
  el.dataset.base = was;
  el.textContent = text;
  clearTimeout(noteTimer);
  noteTimer = setTimeout(() => (el.textContent = was), 1800);
}

// ---------------------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------------------

$('run').addEventListener('click', toggleRun);
$('hold').addEventListener('click', toggleHold);
$('discard').addEventListener('click', discard);
$('midi').addEventListener('click', () => enableMidi().catch((e) => note(String(e))));

async function begin(demo) {
  const boot = $('boot');
  try {
    boot.querySelector('div').innerHTML = '<h1>Waveroll</h1><p>Opening device and GPU…</p>';
    await init();
    const { rate, shared } = demo ? await openDemo() : await openCapture();
    resize();
    state.wr = await Waveroll.create(canvas, rate, state.channels, CAPACITY_LOG2);
    $('adapter').textContent =
      `${rate} Hz · ${state.channels}ch · ` +
      (demo ? 'generator' : `${state.deviceLabel} · ${shared ? 'shared ring' : 'copied ring'}`);
    $('adapter').dataset.base = $('adapter').textContent;
    boot.remove();
    toggleRun();
    requestAnimationFrame(frame);
  } catch (error) {
    boot.querySelector('div').innerHTML =
      `<h1>Waveroll</h1><p style="color:#ff8d7d">${String(error)}</p>`;
    throw error;
  }
}

listDevices().catch(() => {});
$('begin').addEventListener('click', () => begin(false));
$('demo').addEventListener('click', () => begin(true));
// `?demo` skips the picker entirely, which is what CI and a screenshot both want.
if (new URLSearchParams(location.search).has('demo')) {
  addEventListener('load', () => begin(true));
}
