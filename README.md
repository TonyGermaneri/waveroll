# Waveroll

A rolling sampler. It captures the last sixteen bars of whatever the DAW is playing, and lets you
select a bar-quantised region and drag it straight into the session as a file.

It makes no sound. Recording and dragging is the whole job; the DAW is what plays audio.

Full build plan, including the reasoning behind everything below:
<https://claude.ai/code/artifact/8fac4c8d-7b68-405a-840e-5da3ba29627c>

## Decisions of record

| | |
| --- | --- |
| **Quantise units** | Fractions of a **bar**. "1" is one bar. Ladder 1/32 … 4. |
| **Number row** | Head to tail — the last N×10% of the window, ending at the write head. |
| **Window** | 16 bars by default, set in bars. Allocation is sized for the maximum. |
| **Lanes** | One stereo lane. An audio effect sees one bus. |
| **Capture** | Follows the host transport. No transport, no capture. |
| **Clock** | Ableton Link and MIDI clock, behind one `ClockSource`. |
| **Audio output** | None, by design. The plugin passes audio through untouched. |
| **Targets** | Web PWA (GitHub Pages) · AU · VST3 · CLAP · standalone. |
| **Plugin shell** | JUCE, which is why AU is in that list. Electron is not. |

## Layout

```
crates/
  waveroll-core/    Rust. No I/O, no UI, no platform.
    ring.rs           wait-free SPSC ring, planar f32, overwriting
    tempo.rs          TempoMap: capture frames -> quarters -> bars
    grid.rs           the unit ladder, click/drag snapping, the number row
    clock.rs          transport, the capture rule, MIDI clock estimation
    wav.rs            WAV with the BWF and ACID chunks that place the drop
    smf.rs            Standard MIDI File, and the selection boundary policy
tools/
  clock-trace.html  captures a MIDI clock stream for tests/replay.rs
```

Everything in `waveroll-core` compiles unchanged to `wasm32-unknown-unknown` and to native. That
is the property that lets one implementation serve the browser, the standalone app and the plugin,
and it is checked in CI rather than assumed.

```
cargo test --workspace
cargo clippy --workspace --all-targets
cargo build --target wasm32-unknown-unknown -p waveroll-core
```

## Verification

Written files are checked against decoders that did not write them, because a chunk layout can be
internally consistent, pass every one of its own assertions, and still be a file a host refuses.

| Tool | What it proves |
| --- | --- |
| `afinfo` | CoreAudio — the code Logic itself opens files with — agrees with the header, at all three depths. |
| `ffprobe` | An entirely independent implementation reports the same codec, rate, channels and an exact duration. |
| `fluidsynth` | An independent sequencer renders the MIDI clip. Its voice-decay tail makes an absolute duration brittle, so the test is *differential*: halving the tempo must add exactly eight seconds to a four-bar clip, which cancels the tail and can only come out right if PPQ, the tempo meta and the delta times are all correct. |

Each skips loudly when its tool is absent, so none of them is ever the reason a fresh checkout
goes red.

## Spikes — these need a human

Three questions cannot be answered from a terminal. They gate phases 8 and 9, not phase 1, so the
core is being built in parallel; none of its behaviour depends on how they come out.

**1. Drag out of a plugin editor** — needs you. A stock JUCE plugin with one button calling
`performExternalDragDropOfFiles` on a four-bar WAV, loaded into Live and Logic, dragged into the
host's own arrangement. Dragging out of a plugin is strictly harder than out of an app, so proving
it there proves it everywhere. Then fire the same call from a callback invoked *synchronously*
inside a mouse handler rather than from a button — on macOS a drag session must start while the
originating event is still current, and that is the one place the Rust/C++ boundary is awkward.

**2. MIDI clock, and Link** — needs you. Ten minutes of raw bytes from Live over an IAC bus. Four questions the
specs will not answer: jitter on `F8`; whether Song Position Pointer is *ever* sent; what arrives
during a tempo ramp; what arrives on a loop jump. Then the same session under Link, confirming its
start/stop state is present — transport-gated capture depends on it.

**3. wgpu in wasm, against a known number.** waveshape's FFT self-test measures `3.2e-6 %` max
error against an f64 CPU reference. Getting the same figure from wgpu-on-WebGPU *and*
wgpu-on-Metal is the evidence that one renderer can serve every target. Unlike the other two this
one is not human-gated: the Metal half runs headless in CI, and the browser half can be driven
from a devtools session.

Record the results here, with host versions and dates. They will rot, and knowing when they were
last true is the point.

### Results

**Spike 2 — Ableton Live 12 Suite over IAC, 21 Aug 2026.** 408 ticks, 0 dropped.

| | |
| --- | --- |
| Tempo | 119.985 BPM against a nominal 120 |
| Jitter, σ of interval | 0.048 ms — 2.3 samples at 48 kHz |
| Worst excursion | 0.14 ms |
| Song Position Pointer | **never sent** |

Three things follow.

**Live never sends SPP, and that is smaller than it sounds.** It sends `Start` whenever playback
begins, wherever the playhead was, so capture beat zero is simply wherever the user pressed play.
Ticks are beats, so the grid is *always* beat-aligned; it is only *bar*-aligned if playback
happened to begin on a downbeat, which is usually but not always. The correction is therefore never
more than one bar, and it lives in `TempoMap::bar_phase` — set by `set_downbeat`, which the
`set downbeat` key and its MIDI binding drive. Not a fallback: with Live it is the only way bar one
is ever established.

**Jitter over IAC is twenty times better than the estimator was designed for.** σ of 2.3 samples
through a least-squares fit over 24 ticks is 0.068 samples of slope error — about 0.008 BPM. The
window stays at 24 anyway, because USB-attached hardware is a different order of magnitude and this
is the case that has to survive it, not the easy one.

**119.985 against a nominal 120 is 125 ppm, and it is a clock-domain artefact rather than an
error.** Live generates its clock on the audio thread, driven by the interface's crystal;
`performance.now()` runs off the system clock; the trace tool times one with the other. That is
4 ms of error at the far end of a 16-bar window and it grows with the window. It is also exactly
why `ClockPll::feed` takes a **frame index rather than a timestamp** — stamping ticks in frames of
our own capture clock, the same device clock Live is generating from, cancels the rate difference
instead of accumulating it. Worth confirming what the session tempo was actually set to, to rule
out the dull explanation.

## Prior art in this repo's family

`../waveshape` — the WebGPU analyser this borrows its shaders, ring design, control panel, keymap
and MIDI binding layer from. Its README is worth reading before changing anything in `gpu/`.
