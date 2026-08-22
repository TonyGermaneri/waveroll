# Waveroll

A rolling sampler. It sits on a track as an audio effect, keeps the last sixteen bars of whatever
that track is playing, and lets you select a bar-quantised region of it and drag it straight into
the session — as audio, and as the MIDI that played it.

It makes no sound of its own. Capturing and handing over a file is the whole job; the DAW is what
plays audio. On a mix bus it is provably invisible: `processBlock` reads the buffer and returns
without writing to it, so there is no line of code in the audio path that could alter the signal.

Full build plan and the reasoning behind the decisions below:
<https://claude.ai/code/artifact/8fac4c8d-7b68-405a-840e-5da3ba29627c>

## Using it

Drop it on a track and press play. Capture follows the host transport — no transport, no capture —
and refuses offline renders, so bouncing a track cannot overwrite the take with the bounce.

**To take something: drag from inside the selection.** Anywhere else starts a new selection, and
`alt` forces a new one even over the old. The cursor says which you will get. Drop onto the
arrangement or onto a Session clip slot; both work. If MIDI played over the selection, two files
are dragged and the host makes two tracks.

| | |
| --- | --- |
| `1`–`9`, `0` | Select the last N×10% of the window, ending at the head |
| `m` · `n` | Drop a marker · select from the last marker to now |
| `h` | Hold — freeze the picture; capture carries on underneath |
| `s` | Send: write the take out without dragging |
| `d` | Set downbeat — re-phase the bar lines to now |
| `esc` | Clear the selection |
| `,` `.` | Grid finer / coarser: `auto · 1/32 · 1/16 · 1/8 · 1/4 · 1/2 · 1 · 2 · 4` bars |
| `[` `]` | Window length: 4 · 8 · 16 · 32 · 64 · 128 bars |
| scroll · `\` | Zoom about the pointer · fit to width |

Everything on that list is also in the footer, and grid, zoom and fit have buttons beside them.

Two behaviours worth knowing because they look like faults and are not. **Selections are made of
whole cells**, so "100%" is the most whole bars that fit rather than exactly the window — the price
of a selection that always loops. And **a selection erodes** as the write head sweeps into it,
giving up whole cells from the old end rather than vanishing; hold freezes that along with the
picture.

## Building it

The plugin is the product. AU, VST3 and a standalone app come out of one target, all universal, and
the AU and VST3 install themselves.

```
cmake -B native/build -S native -G Ninja     # -G matters; see below
cmake --build native/build
auval -v aumf Wvr1 Wvrl                      # Logic will not load a plugin that fails this
```

Pass the generator explicitly. Configuring without `-G` falls back to Makefiles, and if a
`build.ninja` is already lying about the next build fails with `make: Makefile: No such file` and
names nothing useful.

The browser build is a shop window rather than a target — it does everything except hand another
application a file path, which no web API can do:

```
wasm-pack build --target web --dev --out-dir ../../web/pkg crates/waveroll-wasm
python3 web/serve.py                         # then open localhost:8788/?demo
```

The Rust on its own:

```
cargo test                                   # not --workspace; see below
cargo clippy --all-targets -- -D warnings
```

**Not `--workspace`.** `waveroll-wasm` only compiles for `wasm32` — `SurfaceTarget::Canvas` is
`cfg(web)` — so it is excluded through `default-members`, and `--workspace` overrides that and
fails the build. Plain `cargo test` is what CI runs.

## Layout

```
crates/
  waveroll-core/    Rust. No I/O, no UI, no platform. Compiles to wasm and to native.
    ring.rs           wait-free SPSC ring, planar f32, overwriting
    tempo.rs          TempoMap: capture frames -> quarters -> bars, with a history
    grid.rs           the unit ladder, snapping, erosion, the number row
    view.rs           the wrapping display: laps, columns, zoom, where a bar is shown
    clock.rs          transport, the capture rule, MIDI clock estimation
    midi.rs           the MIDI event ring and note pairing
    wav.rs            WAV, with the BWF and ACID chunks that place and warp the drop
    smf.rs            Standard MIDI File, and the selection boundary policy
  waveroll-gpu/     Rust + wgpu. One WGSL, four backends.
    shaders/          vendored from waveshape, unchanged, read-only
    fft.rs            twiddles, the Stockham stage schedule, the ping-pong
    envelope.rs       the ring mirrored on the GPU, reduced to one value per column
    render.rs         the waveform; overlay.rs the grid, selection and head
    reference.rs      an O(N^2) f64 DFT: the oracle, sharing no code with the shaders
  waveroll-ffi/     The C ABI the JUCE shell talks to. No logic of its own.
  waveroll-wasm/    The browser binding: one object the page drives.
native/
  CMakeLists.txt    JUCE, and the Rust core built universal by hand
  Source/           the processor, the editor, and one .mm for the Metal view
web/                a page that drives the wasm build; no behaviour of its own
tools/
  clock-trace.html  captures a MIDI clock stream for tests/replay.rs
```

The shape is one rule: **everything that decides anything is in Rust.** What a click selects, where
the bar lines fall, which side of the head a column reads from, whether a block is captured, what
bytes a file is made of. C++ owns plugin formats, a window and the file drag; JavaScript owns
devices, events and a canvas. Neither owns a decision. That is why the same behaviour can appear in
a plugin and in a browser without being written twice and kept in step by hand.

## How it is verified

`cargo test` runs 155 checks. The ones worth knowing about are the ones that check against
something other than themselves.

| | |
| --- | --- |
| `afinfo` | CoreAudio — the code Logic opens files with — agrees with the header at all three depths, down to the exact payload byte count, which is what proves the chunk padding. |
| `ffprobe` | An independent implementation reports the same codec, rate, channels and an exact duration. |
| `fluidsynth` | An independent sequencer renders the MIDI clip. Its voice-decay tail makes an absolute duration brittle, so the check is *differential*: halving the tempo must add exactly eight seconds to a four-bar clip, which cancels the tail and can only come out right if PPQ, the tempo meta and every delta time are correct. |
| `reference.rs` | An O(N²) `f64` transform written from the definition, sharing no code with the shaders. A GPU FFT can be wrong in ways that still look like a spectrum. |
| `tests/replay.rs` | A real MIDI clock stream captured from Ableton, replayed. The synthetic tests cover the jitter someone thought to write down; this covers jitter nobody designed. |

Each skips loudly when its tool or its data is absent, so none of them is ever the reason a fresh
checkout goes red.

Three areas are tested where the bugs actually live rather than where they are easy to reach.
`crates/waveroll-ffi/tests/abi.rs` drives all 31 entry points, including every one of them with a
null core in a single test — a host unloading a plugin mid-callback has to get a call that does
nothing rather than a fault inside somebody's session. `crates/waveroll-core/tests/ring_threads.rs`
runs the ring on two threads at the plugin's own ratio of ring to read, because every other ring
test runs on one thread, which is the condition under which its interesting failures cannot happen.
And `crates/waveroll-gpu/tests/render.rs` asserts on pixels, because everything between the ring
and the screen is arithmetic that produces something plausible when it is wrong.

CI runs all of it on macOS, plus the plugin build, `auval`, and a check that all three binaries are
universal.

## Decisions of record

| | |
| --- | --- |
| **Quantise units** | Fractions of a **bar**. "1" is one bar. Ladder 1/32 … 4. |
| **Number row** | Head to tail — the last N×10% of the window, as whole cells. |
| **Window** | 16 bars by default, set in bars; the ring is allocated for the maximum. |
| **Lanes** | One stereo lane. An audio effect sees one bus. |
| **Capture** | Follows the host transport, and refuses offline renders. |
| **Clock** | Host transport in the plugin; MIDI clock in the browser, behind one `ClockSource`. |
| **Audio output** | None, by design. The plugin passes audio through untouched. |
| **Targets** | AU · VST3 · standalone, universal. Web PWA as a shop window. |
| **Plugin shell** | JUCE, which is why AU is in that list. |

One is still open: the plugin registers as `aumf`, a MIDI-controlled effect, because it takes MIDI
for the MIDI lane. That puts it under *AU MIDI-controlled Effects* in Logic rather than with the
ordinary audio effects, which is a worse place to be found. MIDI input or a natural home in the
menu — a plugin cannot be both, and it is cheap to change until somebody has sets saved with it.

## What was measured

**Spike 1 — a JUCE plugin drags into Live's arrangement. It works. 22 Aug 2026.**
`DragAndDropContainer::performExternalDragDropOfFiles`, called synchronously from `mouseDrag`, out
of a plugin editor loaded in Ableton Live 12.4.1, dropped onto the arrangement. Accepted.

Dragging out of a *plugin* is strictly harder than out of an app, because the editor lives inside
the host's view hierarchy — so this proves it everywhere, and it is the result the whole
architecture was waiting on. `auval -v aumf Wvr1 Wvrl` also passes, so Logic will load it.

**Spike 1, browser half — Ableton Live 12 refuses it, 22 Aug 2026.** A page can only put a *file
promise* on the pasteboard (`DataTransfer` + `DownloadURL`, Chromium only). Live does not accept
one. This is not a bug to fix: **there is no API by which a web page can hand another application
a file path**, so no amount of work on the web build changes it.

What that settles:

* **The JUCE build is the product, and the web build is a shop window.** It captures, displays,
  quantises, selects and writes a correct WAV — everything except the last six inches.
* The practical workflow on the web is **stage, download, then drag from the browser's own
  downloads UI**, which does hand over a real path. Two gestures instead of one. The page now says
  so rather than offering a drag that silently fails.
* `performExternalDragDropOfFiles` from JUCE hands over a real path and is the thing still to be
  measured — that is the other half of Spike 1, and it is the one that matters.

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

## Injecting a clip into the host track: why not

Investigated 22 Aug 2026. Three separate walls, and the outermost is not the one that stops it.

**A VST3 or AU cannot reach the Live API.** There is no such surface. The Live Object Model is
reachable only from a Max for Live device, or from a MIDI Remote Script — Python, unofficial, and
explicitly unsupported by Ableton. A plugin can be bridged to a Remote Script over a socket, which
is what AbletonOSC, `live_rpyc` and similar do, so this wall is passable at the cost of asking
people to install an unsupported script.

**Passing it does not help, because the Live API cannot make an audio clip from a file.**
`ClipSlot.create_clip(length)` creates a *MIDI* clip, and only on a MIDI track. There is no
`create_audio_clip` and no method taking a path. The capability is missing from the model itself,
so no amount of access reaches it.

**What people do instead is drive the browser**, selecting an item and loading it into the
highlighted clip slot — the mechanism Push uses. It is indirect, it only ever targets the
highlighted slot rather than a chosen one, and the drum-rack variant loses warping. That is a lot
of fragile machinery for something the mouse already does.

**Because dragging onto a clip slot already works.** Live accepts an audio file dropped straight
onto a Session-view slot, and the drag out of the plugin editor is a real file drag. Dropping on a
slot instead of the arrangement is the same gesture and needs nothing built.

So: not worth building. Revisit only if Ableton adds a path-taking clip API, at which point the
socket bridge becomes worth its cost.

## Environment note

This machine is `arm64` (Apple M3 Max) but every installed Rust toolchain is
`x86_64-apple-darwin`, so cargo and rustc run under Rosetta 2 — the first wgpu build took 7m15s.
`rustup toolchain install stable-aarch64-apple-darwin` would build natively, and an `aarch64` slice
is needed for the universal binaries in phase 9 regardless. The default is left alone here because
the pinned nightlies suggest the x86 toolchain is deliberate.

wgpu still reaches the real GPU through Rosetta — the self-test reports `Apple M3 Max (Metal)` —
so nothing above is measuring an emulated device.

## Prior art in this repo's family

`../waveshape` — the WebGPU analyser this borrows its shaders, ring design, control panel, keymap
and MIDI binding layer from. Its README is worth reading before changing anything in `gpu/`.
