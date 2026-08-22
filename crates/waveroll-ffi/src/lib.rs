//! The C ABI, and nothing else.
//!
//! Every function here is a thin shim over `waveroll-core`. That is deliberate and it is the whole
//! point of the boundary: the C++ side owns plugin formats, a window and the file drag, and knows
//! nothing about bars, laps, snapping or ring buffers. Anything that grew logic here would be
//! logic living where it cannot be tested.
//!
//! # Safety
//!
//! Every entry point takes a pointer the caller got from [`wr_create`] and must not use after
//! [`wr_destroy`]. Audio pointers must be valid for `frames` samples. Null is tolerated everywhere
//! and does nothing, because a crash inside somebody's DAW is the worst possible failure mode and
//! "the plugin did nothing" is a far better one than "the host died".

mod view;

use std::ffi::c_void;

use waveroll_core::clock::{CaptureClock, Transport};
use waveroll_core::grid::{self, Selection, Unit};
use waveroll_core::midi::MidiRing;
use waveroll_core::smf::{self, SmfOptions};
use waveroll_core::ring::{self, Producer, Reader};
use waveroll_core::tempo::Meter;
use waveroll_core::view::{View, Viewport};
use waveroll_core::wav::{self, Acid, Bext, Depth, WavMeta, WavSpec};

/// Runs `body`, turning any panic into `fallback`.
///
/// **Every entry point in this file goes through this.** A panic reaching an `extern "C"` boundary
/// does not unwind -- Rust aborts the process, and the process is somebody's DAW. This turns the
/// worst possible failure into the second worst: the call does nothing and the host survives.
///
/// It is a net, not a licence. Anything that panics here is still a bug and should still be fixed;
/// what this guarantees is that finding it does not cost somebody their session.
fn guard<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(_) => fallback,
    }
}

/// What the host reports, once per block.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WrTransport {
    pub playing: bool,
    pub offline: bool,
    pub bpm: f64,
    pub numerator: u32,
    pub denominator: u32,
}

/// What the editor needs to draw its chrome.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WrStatus {
    pub bpm: f64,
    pub playing: bool,
    pub held: bool,
    /// Frames captured. The grid's x axis, and the ring's absolute index.
    pub captured: u64,
    pub lap: u64,
    /// Write head as a fraction of the canvas.
    pub head: f64,
    pub window_bars: f64,
    pub unit_bars: f64,
    pub has_selection: bool,
    pub selection_bars: f64,
    /// Selection edges as canvas fractions; outside 0..1 when off screen.
    pub selection_from: f64,
    pub selection_to: f64,
    /// 0 empty, 1 pending, 2 overwritten, 3 ready.
    pub selection_state: u32,
    pub markers: u32,
}

pub struct WrCore {
    producer: Producer,
    reader: Reader,
    clock: CaptureClock,
    view: View,
    unit: Unit,
    selection: Option<Selection>,
    markers: Vec<u64>,
    held_at: Option<u64>,
    channels: usize,
    sample_rate: u32,
    width: u32,
    planes: Vec<Vec<f32>>,
    staged: Vec<u8>,
    /// The MIDI lane. Separate from the audio buffer so a selection can be dragged as both, which
    /// a host turns into two tracks -- the take and what played it.
    midi: MidiRing,
    staged_midi: Vec<u8>,
    /// Where the block being captured began, so a MIDI event's offset within it can be stamped on
    /// the same axis as the audio.
    block_start: u64,
}

/// Turns a raw pointer into a reference, or does nothing.
macro_rules! core_ref {
    ($ptr:expr) => {
        match unsafe { ($ptr as *mut WrCore).as_mut() } {
            Some(core) => core,
            None => return Default::default(),
        }
    };
    ($ptr:expr, $fallback:expr) => {
        match unsafe { ($ptr as *mut WrCore).as_mut() } {
            Some(core) => core,
            None => return $fallback,
        }
    };
}

/// Allocates a core with a ring of `2^capacity_log2` frames.
///
/// # Safety
/// The returned pointer must be released with [`wr_destroy`] exactly once.
#[unsafe(no_mangle)]
pub extern "C" fn wr_create(
    sample_rate: u32,
    channels: u32,
    capacity_log2: u32,
    max_block: u32,
) -> *mut c_void {
    guard(std::ptr::null_mut(), || {
        let channels = channels.clamp(1, 2) as usize;
        let capacity = 1usize << capacity_log2.clamp(16, 26);
        let sample_rate = sample_rate.clamp(8_000, 768_000);
        let (producer, reader) = ring::ring(capacity, channels, sample_rate);
        let core = Box::new(WrCore {
            producer,
            reader,
            clock: CaptureClock::new(sample_rate, 120.0, Meter::FOUR_FOUR),
            view: View::new(16.0),
            unit: Unit::Auto,
            selection: None,
            markers: Vec::new(),
            held_at: None,
            channels,
            sample_rate,
            width: 1024,
            // Pre-sized so `wr_capture` never allocates. A host that hands over a larger block than
            // it promised will still be served correctly, at the cost of one allocation on the audio
            // thread that block -- which is the right trade against refusing the audio.
            planes: vec![Vec::with_capacity(max_block.clamp(64, 1 << 16) as usize); channels],
            staged: Vec::new(),
            // 16k events is minutes of dense playing, and costs 128 kB.
            midi: MidiRing::new(1 << 14),
            staged_midi: Vec::new(),
            block_start: 0,
        });
        Box::into_raw(core) as *mut c_void
    })
}

/// # Safety
/// `core` must come from [`wr_create`] and must not be used afterwards.
#[unsafe(no_mangle)]
pub extern "C" fn wr_destroy(core: *mut c_void) {
    guard((), || {
        if !core.is_null() {
            drop(unsafe { Box::from_raw(core as *mut WrCore) });
        }
    })
}

/// Captures one block. Real-time safe: no allocation, no locking, no I/O.
///
/// Returns how many frames were taken — zero when the transport is stopped or the host is
/// rendering offline. The buffer is never written to; a tap has no output.
///
/// # Safety
/// `channels` pointers must each be valid for `frames` samples.
#[unsafe(no_mangle)]
pub extern "C" fn wr_capture(
    core: *mut c_void,
    channels: *const *const f32,
    frames: u32,
    transport: *const WrTransport,
) -> u32 {
    guard(0, || {
        let core = core_ref!(core, 0);
        let Some(transport) = (unsafe { transport.as_ref() }) else { return 0 };
        if channels.is_null() || frames == 0 {
            return 0;
        }
        let transport = Transport {
            playing: transport.playing,
            bpm: if transport.bpm.is_finite() && transport.bpm > 0.0 { transport.bpm } else { 120.0 },
            meter: Meter::new(transport.numerator.max(1), transport.denominator.max(1)),
            offline: transport.offline,
        };
        core.block_start = core.clock.captured();
        let taken = core.clock.advance(&transport, frames as usize);
        if taken == 0 {
            return 0;
        }
        // The planes are pre-sized on the first block and reused, so the audio thread allocates only
        // once per configuration rather than once per block.
        for (c, plane) in core.planes.iter_mut().enumerate() {
            let source = unsafe { *channels.add(c) };
            if source.is_null() {
                return 0;
            }
            let samples = unsafe { std::slice::from_raw_parts(source, taken) };
            plane.clear();
            plane.extend_from_slice(samples);
        }
        let views: Vec<&[f32]> = core.planes.iter().map(|p| p.as_slice()).collect();
        core.producer.write(&views, taken);
        taken as u32
    })
}

/// Captures one MIDI event, stamped by its offset within the block just given to [`wr_capture`].
///
/// Call after `wr_capture` for the same block. Real-time safe, and silently does nothing when the
/// block was refused -- MIDI follows the transport exactly as audio does, or a clip would contain
/// notes from a passage whose audio was never recorded.
#[unsafe(no_mangle)]
pub extern "C" fn wr_capture_midi(
    core: *mut c_void,
    offset_in_block: u32,
    status: u8,
    data1: u8,
    data2: u8,
) {
    guard((), || {
        let core = core_ref!(core, ());
        let head = core.clock.captured();
        if head <= core.block_start {
            return;
        }
        // Clamped inside the block: a host may report an offset past the end, and a frame beyond the
        // write head would make every reader treat the event as not captured yet.
        let frame = (core.block_start + u64::from(offset_in_block)).min(head - 1);
        core.midi.push(frame, status, data1, data2);
    })
}

/// Renders the selection's MIDI as a Standard MIDI File; returns its length, or 0 when there is
/// nothing on the lane.
#[unsafe(no_mangle)]
pub extern "C" fn wr_stage_midi(core: *mut c_void, let_ring: bool) -> usize {
    guard(0, || {
        let core = core_ref!(core, 0);
        core.staged_midi.clear();
        let Some(selection) = core.selection else { return 0 };
        let staged = smf::stage(&core.midi, selection, let_ring);
        if staged.is_empty() {
            return 0;
        }
        let options = SmfOptions { let_ring, ..SmfOptions::default() };
        core.staged_midi =
            smf::write(selection, core.clock.map(), &staged.clip(), &options);
        core.staged_midi.len()
    })
}

/// Valid until the next [`wr_stage_midi`].
#[unsafe(no_mangle)]
pub extern "C" fn wr_staged_midi_bytes(core: *mut c_void) -> *const u8 {
    guard(std::ptr::null(), || {
        let core = core_ref!(core, std::ptr::null());
        core.staged_midi.as_ptr()
    })
}

/// Tells the core how wide the editor is, which is what the auto grid unit is chosen against.
#[unsafe(no_mangle)]
pub extern "C" fn wr_set_width(core: *mut c_void, width: u32) {
    guard((), || {
        let core = core_ref!(core, ());
        core.width = width.max(1);
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_set_window_bars(core: *mut c_void, bars: f64) {
    guard((), || {
        let core = core_ref!(core, ());
        core.view.window_bars = bars.clamp(1.0, 512.0);
        core.view.clamp();
    })
}

/// `0` means auto; anything else snaps to the nearest rung of the ladder.
#[unsafe(no_mangle)]
pub extern "C" fn wr_set_unit(core: *mut c_void, bars: f64) {
    guard((), || {
        let core = core_ref!(core, ());
        core.unit = if bars <= 0.0 { Unit::Auto } else { Unit::Fixed(bars) };
        core.view.home();
    })
}

fn viewport(core: &WrCore) -> Viewport {
    let head = core.held_at.unwrap_or_else(|| core.clock.captured());
    Viewport::resolve(&core.view, core.clock.map(), head, core.width)
}

fn unit_bars(core: &WrCore, viewport: &Viewport) -> f64 {
    core.unit.bars(viewport.span_bars, f64::from(core.width))
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_click(core: *mut c_void, fraction: f64) {
    guard((), || {
        let core = core_ref!(core, ());
        let vp = viewport(core);
        let unit = unit_bars(core, &vp);
        if let Some(frame) = vp.frame_at(core.clock.map(), fraction.clamp(0.0, 1.0)) {
            core.selection = Some(grid::cell_at(core.clock.map(), unit, frame));
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_drag(core: *mut c_void, from: f64, to: f64) {
    guard((), || {
        let core = core_ref!(core, ());
        let vp = viewport(core);
        let unit = unit_bars(core, &vp);
        let map = core.clock.map();
        if let (Some(a), Some(b)) =
            (vp.frame_at(map, from.clamp(0.0, 1.0)), vp.frame_at(map, to.clamp(0.0, 1.0)))
        {
            core.selection = Some(grid::snap_range(map, unit, a, b));
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_select_percent(core: *mut c_void, tenths: u32) {
    guard((), || {
        let core = core_ref!(core, ());
        let vp = viewport(core);
        let unit = unit_bars(core, &vp);
        let head = core.held_at.unwrap_or_else(|| core.clock.captured());
        if let Some(selection) =
            grid::percent_from_head(core.clock.map(), unit, head, core.view.window_bars, tenths)
        {
            core.selection = Some(selection);
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_clear_selection(core: *mut c_void) {
    guard((), || {
        let core = core_ref!(core, ());
        core.selection = None;
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_hold(core: *mut c_void, on: bool) {
    guard((), || {
        let core = core_ref!(core, ());
        core.held_at = if on { Some(core.clock.captured()) } else { None };
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_mark(core: *mut c_void) {
    guard((), || {
        let core = core_ref!(core, ());
        let at = core.clock.captured();
        core.markers.push(at);
        let oldest = core.reader.oldest();
        core.markers.retain(|m| *m >= oldest);
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_select_from_marker(core: *mut c_void) -> bool {
    guard(false, || {
        let core = core_ref!(core, false);
        let head = core.held_at.unwrap_or_else(|| core.clock.captured());
        let Some(&at) = core.markers.iter().rev().find(|m| **m < head) else { return false };
        let vp = viewport(core);
        let unit = unit_bars(core, &vp);
        match grid::snap_range_upto(core.clock.map(), unit, at, head, head) {
            Some(selection) => {
                core.selection = Some(selection);
                true
            }
            None => false,
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_set_downbeat_now(core: *mut c_void) {
    guard((), || {
        let core = core_ref!(core, ());
        let at = core.clock.captured();
        core.clock.map_mut().set_downbeat(at);
    })
}

/// Renders the selection into an internal buffer and returns its length, or zero when refused.
///
/// The bytes stay valid until the next call to this function. Refusal is the honest answer for a
/// range the writer has lapped: a partly-overwritten read is a seam of old and new audio that
/// looks completely plausible and is not what anybody selected.
#[unsafe(no_mangle)]
pub extern "C" fn wr_stage(core: *mut c_void, time_reference: u64) -> usize {
    guard(0, || {
        let core = core_ref!(core, 0);
        core.staged.clear();
        let Some(selection) = core.selection else { return 0 };
        if !core.reader.holds(selection.start, selection.end) {
            return 0;
        }
        let frames = selection.frames() as usize;
        if frames == 0 {
            return 0;
        }
        let mut planes: Vec<Vec<f32>> = Vec::with_capacity(core.channels);
        for channel in 0..core.channels {
            let mut plane = vec![0.0f32; frames];
            if !core.reader.read_into(channel, selection.start, &mut plane) {
                return 0;
            }
            planes.push(plane);
        }
        let views: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
        let map = core.clock.map();
        let quarters = map.quarters_at(selection.end) - map.quarters_at(selection.start);
        let meta = WavMeta {
            acid: Some(Acid {
                tempo: map.bpm_at(selection.start) as f32,
                quarters: quarters.round().max(1.0) as u32,
                meter: map.meter_at(selection.start),
                root: None,
            }),
            bext: Some(Bext {
                description: "Waveroll capture".into(),
                originator: "Waveroll".into(),
                time_reference,
                coding_history: format!(
                    "A=PCM,F={},W=32,M={}\r\n",
                    core.sample_rate,
                    if core.channels > 1 { "stereo" } else { "mono" }
                ),
                ..Bext::default()
            }),
        };
        core.staged = wav::write(&views, &WavSpec::new(core.sample_rate, Depth::F32), &meta);
        core.staged.len()
    })
}

/// Pointer to the bytes from the last [`wr_stage`]. Valid until the next call.
#[unsafe(no_mangle)]
pub extern "C" fn wr_staged_bytes(core: *mut c_void) -> *const u8 {
    guard(std::ptr::null(), || {
        let core = core_ref!(core, std::ptr::null());
        core.staged.as_ptr()
    })
}

/// Bars in the current selection, for naming the file. Zero when there is none.
#[unsafe(no_mangle)]
pub extern "C" fn wr_selection_bars(core: *mut c_void) -> f64 {
    guard(0.0, || {
        let core = core_ref!(core, 0.0);
        let Some(selection) = core.selection else { return 0.0 };
        let map = core.clock.map();
        map.bars_at(selection.end) - map.bars_at(selection.start)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_status(core: *mut c_void, out: *mut WrStatus) {
    guard((), || {
        let core = core_ref!(core, ());
        let Some(out) = (unsafe { out.as_mut() }) else { return };
        let vp = viewport(core);
        let map = core.clock.map();
        let mut status = WrStatus {
            bpm: map.bpm_at(core.clock.captured()),
            playing: core.clock.is_playing(),
            held: core.held_at.is_some(),
            captured: core.clock.captured(),
            lap: vp.lap,
            head: vp.head_fraction(),
            window_bars: core.view.window_bars,
            unit_bars: unit_bars(core, &vp),
            markers: core.markers.len() as u32,
            ..Default::default()
        };
        if let Some(selection) = core.selection {
            let (a, b) = (map.bars_at(selection.start), map.bars_at(selection.end));
            status.has_selection = true;
            status.selection_bars = b - a;
            status.selection_from = vp.fraction_at(a);
            status.selection_to = vp.fraction_at(b);
            status.selection_state = if selection.end > core.reader.head() {
                1
            } else if !core.reader.holds(selection.start, selection.end) {
                2
            } else {
                3
            };
        }
        *out = status;
    })
}

// ---------------------------------------------------------------------------------------
// The picture
// ---------------------------------------------------------------------------------------

/// Opens a GPU surface on a native view.
///
/// # Safety
/// `native_view` must be a valid `NSView*` that outlives the returned handle.
#[unsafe(no_mangle)]
pub extern "C" fn wr_view_open(
    core: *mut c_void,
    native_view: *mut c_void,
    width: u32,
    height: u32,
    scale: f64,
) -> *mut c_void {
    guard(std::ptr::null_mut(), || {
        let core = core_ref!(core, std::ptr::null_mut());
        let capacity = core.reader.capacity();
        let channels = core.reader.channels();
        match unsafe { view::View::open(native_view, width, height, scale, capacity, channels) } {
            Ok(view) => Box::into_raw(Box::new(view)) as *mut c_void,
            // A plugin whose picture failed to open should still capture and still drag. Returning
            // null lets the editor say so and carry on rather than taking the host down with it.
            Err(_) => std::ptr::null_mut(),
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_view_resize(view: *mut c_void, width: u32, height: u32, scale: f64) {
    guard((), || {
        let Some(view) = (unsafe { (view as *mut view::View).as_mut() }) else { return };
        view.resize(width, height, scale);
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wr_view_close(view: *mut c_void) {
    guard((), || {
        if !view.is_null() {
            drop(unsafe { Box::from_raw(view as *mut view::View) });
        }
    })
}

/// Paints one frame.
#[unsafe(no_mangle)]
pub extern "C" fn wr_view_draw(core: *mut c_void, view: *mut c_void) {
    guard((), || {
        let core = core_ref!(core, ());
        let Some(view) = (unsafe { (view as *mut view::View).as_mut() }) else { return };

        // The auto grid unit is chosen against apparent spacing, so the core reasons in points while
        // the surface is in pixels.
        core.width = view.logical_width().round().max(1.0) as u32;
        let vp = viewport(core);
        let unit = unit_bars(core, &vp);
        let map = core.clock.map();

        let selection = core.selection.map(|s| {
            (vp.fraction_at(map.bars_at(s.start)), vp.fraction_at(map.bars_at(s.end)))
        });
        let markers: Vec<f64> =
            core.markers.iter().map(|m| vp.fraction_at(map.bars_at(*m))).collect();

        view.draw(&view::Frame {
            reader: &core.reader,
            map,
            viewport: &vp,
            unit_bars: unit,
            selection,
            markers: &markers,
            held: core.held_at.is_some(),
        });
    })
}

/// The most recent GPU validation error, if there has been one. Empty otherwise.
///
/// # Safety
/// `out` must point to at least `cap` bytes.
#[unsafe(no_mangle)]
pub extern "C" fn wr_view_last_error(view: *mut c_void, out: *mut u8, cap: usize) -> usize {
    guard(0, || {
        let Some(view) = (unsafe { (view as *mut view::View).as_mut() }) else { return 0 };
        let errors = view.take_errors();
        let Some(text) = errors.last() else { return 0 };
        let bytes = text.as_bytes();
        let n = bytes.len().min(cap.saturating_sub(1));
        if !out.is_null() && cap > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, n);
                *out.add(n) = 0;
            }
        }
        n
    })
}

/// What the GPU reports itself as, for the editor to show.
///
/// # Safety
/// `out` must point to at least `cap` bytes.
#[unsafe(no_mangle)]
pub extern "C" fn wr_view_describe(view: *mut c_void, out: *mut u8, cap: usize) -> usize {
    guard(0, || {
        let Some(view) = (unsafe { (view as *mut view::View).as_mut() }) else { return 0 };
        let text = view.describe();
        let bytes = text.as_bytes();
        let n = bytes.len().min(cap.saturating_sub(1));
        if !out.is_null() && cap > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, n);
                *out.add(n) = 0;
            }
        }
        n
    })
}
