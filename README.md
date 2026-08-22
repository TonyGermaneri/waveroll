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

**Spike 2 — Ableton Live 12 Suite over IAC, 21 Aug 2026.** Live at exactly 120 BPM, 14,010 ticks
over 291.9 s, 0 dropped. Trace checked in as `tests/traces/live12-120.txt`.

| | |
| --- | --- |
| Tempo, least squares over the whole stream | 119.99970 BPM — **2.5 ppm** off nominal |
| Jitter, σ of interval | 0.058 ms — 2.8 samples at 48 kHz |
| Interval range | 20.700 – 21.000 ms about a 20.8334 mean |
| Rolling tempo spread, 10-quarter windows | 0.0048 BPM |
| Song Position Pointer | **never sent** |
| Stop | never sent — the take ran unbroken |

`ClockPll` replayed against it reads **120.000 BPM, worst excursion 0.030 BPM, nothing rejected.**

**Live never sends SPP, and that is smaller than it sounds.** It sends `Start` whenever playback
begins, wherever the playhead was, so capture beat zero is simply wherever the user pressed play.
Ticks are beats, so the grid is *always* beat-aligned; it is only *bar*-aligned if playback
happened to begin on a downbeat, which is usually but not always. The correction is therefore never
more than one bar, and it lives in `TempoMap::bar_phase` — set by `set_downbeat`, which the
`set downbeat` key and its MIDI binding drive. Not a fallback: with Live it is the only way bar one
is ever established.

**MIDI clock over IAC is far better than its reputation.** 2.5 ppm of tempo accuracy and 58 µs of
jitter is a bus, not a cable — nothing is being serialised over USB and no scheduler is between the
two processes. σ of 2.8 samples through a least-squares fit over 24 ticks is about 0.008 BPM of
slope error. The window stays at 24 regardless, because USB-attached hardware is a different order
of magnitude and that is the case that has to survive, not this one.

**A correction, and the lesson in it.** The capture tool reported 119.985 BPM live on screen, and
that was first written up here as 125 ppm of clock-domain skew between Live's audio thread and
`performance.now()`. The full trace refutes it: the real figure is 2.5 ppm, and 119.985 was exactly
one standard error of the tool's own estimate — noise, read as an effect.

The cause is worth keeping, because it is a trap in an obvious-looking piece of code. The mean of
`n` intervals telescopes to `(last − first) / n`, so it depends only on the two endpoints and its
error is about `σ√2/n` — very precise. The tool *trimmed* the outer 5% of intervals before
averaging, to reject outliers, and trimming breaks the telescoping: what is left is a genuine
average of `n` noisy samples with error `σ/√n`, which is **thirty times worse** here. The fix is to
fit the timestamps rather than average the intervals, which is robust and precise at once, and is
what `ClockPll` already does. Trimming is still the right thing for reporting jitter, and is kept
there.

None of this changes the architecture. `ClockPll::feed` still takes a **frame index rather than a
timestamp**, because stamping ticks in frames of our own capture clock cancels any device-versus-
system rate difference instead of accumulating it. It is simply guarding against 0.08 ms over a
16-bar window rather than the 4 ms first claimed.

## Prior art in this repo's family

`../waveshape` — the WebGPU analyser this borrows its shaders, ring design, control panel, keymap
and MIDI binding layer from. Its README is worth reading before changing anything in `gpu/`.
