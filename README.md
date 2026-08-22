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

## Prior art in this repo's family

`../waveshape` — the WebGPU analyser this borrows its shaders, ring design, control panel, keymap
and MIDI binding layer from. Its README is worth reading before changing anything in `gpu/`.
