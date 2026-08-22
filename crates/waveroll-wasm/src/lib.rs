//! The browser binding.
//!
//! One object the page drives. Everything the page does — capture a block, set the transport,
//! click, drag, zoom, paint a frame — is a method here, and every decision behind those methods
//! lives in `waveroll-core` and `waveroll-gpu` where it is tested without a browser. The page
//! contributes devices, events and a canvas; it contributes no behaviour.
//!
//! That split is not tidiness. The same core has to drive a JUCE standalone and a plugin editor,
//! and anything that leaks into TypeScript here is something that has to be written a second time
//! in C++ later, and then kept in step.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use waveroll_gpu::wgpu;
use web_sys::HtmlCanvasElement;

use waveroll_core::clock::{CaptureClock, ClockMessage, ClockPll, Transport, decode_clock};
use waveroll_core::grid::{self, Ruling, Selection, Unit};
use waveroll_core::ring::{self, Producer, Reader};
use waveroll_core::tempo::Meter;
use waveroll_core::view::{View, Viewport};
use waveroll_gpu::device::Gpu;
use waveroll_gpu::envelope::{EnvelopePass, RingMirror};
use waveroll_gpu::overlay::{Overlay, OverlayPass};
use waveroll_core::wav::{self, Acid, Bext, Depth, WavMeta, WavSpec};
use waveroll_gpu::render::{OverlayStyle, Style};

#[wasm_bindgen(start)]
pub fn start() {
    // A Rust panic in wasm is otherwise an unreachable trap with no message at all.
    console_error_panic_hook::set_once();
}

/// What the page needs to draw its own chrome, handed over as one JSON blob per frame rather than
/// as a dozen getters. One crossing of the boundary is cheaper than twelve, and it cannot go stale
/// halfway through the way separate reads can.
#[derive(Default)]
struct Status {
    bpm: f64,
    playing: bool,
    lap: u64,
    head: f64,
    unit: f64,
    window_bars: f64,
    zoom: f64,
    captured: u64,
    /// Length in bars, and where the two edges sit as canvas fractions. Fractions rather than bar
    /// numbers because a selection older than the window is off screen, and its bar number
    /// relative to the *current* lap is then a meaningless negative — which is exactly the reading
    /// the first version of this produced.
    selection: Option<(f64, f64, f64)>,
    in_view: bool,
    lapped: u64,
    state: String,
    held: bool,
    markers: u32,
}

/// `4` rather than `4.00`, and `4.5` rather than `4.50`: a bar count is read, not measured.
fn format_bars(bars: f64) -> String {
    let rounded = (bars * 100.0).round() / 100.0;
    if (rounded - rounded.round()).abs() < 1e-9 {
        format!("{}", rounded.round() as i64)
    } else {
        format!("{rounded}").replace('.', "-")
    }
}

#[wasm_bindgen]
pub struct Waveroll {
    gpu: Gpu,
    surface: wgpu::Surface<'static>,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    /// Device pixels per CSS pixel. The auto grid unit is chosen against a target *apparent*
    /// spacing, so it has to reason in the pixels a person sees rather than the ones the GPU
    /// fills — otherwise the same window on a 2x display gets a grid one rung finer than on a 1x
    /// display, which is a difference in what the tool decided rather than in how sharp it looks.
    pixel_ratio: f64,

    producer: Producer,
    reader: Reader,
    channels: usize,
    planes: Vec<Vec<f32>>,

    mirror: RingMirror,
    envelope: EnvelopePass,
    waveform: waveroll_gpu::render::WaveformPass,
    overlay_pass: OverlayPass,
    overlay: Overlay,
    rulings: Vec<Ruling>,

    clock: CaptureClock,
    pll: ClockPll,
    transport: Transport,
    view: View,
    unit: Unit,
    selection: Option<Selection>,
    /// Frames at which the user said "that was good". The workflow is entirely retrospective, so
    /// this is the control that turns "I noticed" into a selection without hunting for the edge.
    markers: Vec<u64>,
    /// Where the write head was frozen, if it is. Hold does not stop capture — it stops the
    /// *picture*, by leaving the GPU mirror unsynced and reporting the frozen head.
    held_at: Option<u64>,
    style: Style,
    overlay_style: OverlayStyle,
    status: Status,
}

#[wasm_bindgen]
impl Waveroll {
    /// Opens a device on `canvas` and allocates a ring of `2^capacity_log2` frames.
    ///
    /// Async because acquiring a WebGPU adapter is, and there is no thread to block in a browser.
    pub async fn create(
        canvas: HtmlCanvasElement,
        sample_rate: u32,
        channels: u32,
        capacity_log2: u32,
    ) -> Result<Waveroll, JsValue> {
        let width = canvas.width().max(1);
        let height = canvas.height().max(1);

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
            .map_err(|e| JsValue::from_str(&format!("no surface: {e}")))?;
        let gpu = Gpu::open(&instance, Some(&surface))
            .await
            .map_err(|e| JsValue::from_str(&e))?;

        let caps = surface.get_capabilities(&gpu.adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        surface.configure(
            &gpu.device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width,
                height,
                present_mode: wgpu::PresentMode::Fifo,
                desired_maximum_frame_latency: 2,
                alpha_mode: caps.alpha_modes[0],
                view_formats: vec![],
            },
        );

        let channels = channels.clamp(1, 2) as usize;
        let capacity = 1usize << capacity_log2.clamp(16, 26);
        let (producer, reader) = ring::ring(capacity, channels, sample_rate);

        Ok(Waveroll {
            mirror: RingMirror::new(&gpu, capacity, channels),
            envelope: EnvelopePass::new(&gpu, 4096),
            waveform: waveroll_gpu::render::WaveformPass::new(&gpu, format),
            overlay_pass: OverlayPass::new(&gpu, format, 8192),
            overlay: Overlay::default(),
            rulings: Vec::new(),
            clock: CaptureClock::new(sample_rate, 120.0, Meter::FOUR_FOUR),
            pll: ClockPll::new(sample_rate, 120.0),
            transport: Transport::stopped(120.0, Meter::FOUR_FOUR),
            view: View::new(16.0),
            unit: Unit::Auto,
            selection: None,
            markers: Vec::new(),
            held_at: None,
            style: Style::default(),
            overlay_style: OverlayStyle::default(),
            status: Status::default(),
            planes: vec![Vec::new(); channels],
            channels,
            producer,
            reader,
            gpu,
            surface,
            format,
            size: (width, height),
            pixel_ratio: 1.0,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32, pixel_ratio: f64) {
        let (width, height) = (width.max(1), height.max(1));
        self.pixel_ratio = if pixel_ratio.is_finite() && pixel_ratio > 0.0 { pixel_ratio } else { 1.0 };
        if self.size == (width, height) {
            return;
        }
        self.size = (width, height);
        self.surface.configure(
            &self.gpu.device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: self.format,
                width,
                height,
                present_mode: wgpu::PresentMode::Fifo,
                desired_maximum_frame_latency: 2,
                alpha_mode: wgpu::CompositeAlphaMode::Auto,
                view_formats: vec![],
            },
        );
    }

    /// Sets the transport the next captured blocks will be attributed to.
    pub fn transport(&mut self, playing: bool, bpm: f64, num: u32, den: u32, offline: bool) {
        self.transport = Transport {
            playing,
            bpm: if bpm.is_finite() && bpm > 0.0 { bpm } else { self.transport.bpm },
            meter: Meter::new(num.max(1), den.max(1)),
            offline,
        };
    }

    /// Feeds one MIDI packet, stamped with the frame it arrived at.
    ///
    /// Frames rather than milliseconds, because that cancels any rate difference between the
    /// sender's audio clock and the browser's — measured at 2.5 ppm against Live over IAC, small
    /// but real, and it accumulates rather than cancelling out.
    pub fn midi(&mut self, data: &[u8], at_frame: f64) {
        if let Some(message) = decode_clock(data) {
            self.pll.feed(message, at_frame.max(0.0) as u64);
            match message {
                ClockMessage::Start | ClockMessage::Continue => self.transport.playing = true,
                ClockMessage::Stop => self.transport.playing = false,
                _ => {}
            }
            if self.pll.settled() {
                self.transport.bpm = self.pll.bpm();
            }
        }
    }

    /// Captures one block, interleaved. Returns how many frames were actually taken — zero when
    /// the transport is stopped or the host is rendering offline.
    pub fn push(&mut self, interleaved: &[f32]) -> u32 {
        let frames = interleaved.len() / self.channels;
        if frames == 0 {
            return 0;
        }
        let taken = self.clock.advance(&self.transport, frames);
        if taken == 0 {
            return 0;
        }
        for (c, plane) in self.planes.iter_mut().enumerate() {
            plane.clear();
            plane.extend(interleaved.iter().skip(c).step_by(self.channels).take(taken).copied());
        }
        let views: Vec<&[f32]> = self.planes.iter().map(|p| p.as_slice()).collect();
        self.producer.write(&views, taken);
        taken as u32
    }

    // ---- view ----

    pub fn home(&mut self) {
        self.view.home();
    }
    pub fn zoom(&mut self, factor: f64, anchor: f64) {
        self.view.zoom_about(factor, anchor);
    }
    pub fn scroll(&mut self, fraction: f64) {
        self.view.scroll_by(fraction);
    }
    pub fn set_window_bars(&mut self, bars: f64) {
        self.view.window_bars = bars.clamp(1.0, 512.0);
        self.view.clamp();
    }

    /// `0` means auto. Any other value snaps to the nearest rung of the ladder.
    ///
    /// Changing the quantise setting returns the view to fit, because fit is home and zoom is an
    /// excursion — and because in auto the unit is *derived* from the zoom, so changing the setting
    /// without going home would pick a unit from wherever the view happened to be.
    pub fn set_unit(&mut self, bars: f64) {
        self.unit = if bars <= 0.0 { Unit::Auto } else { Unit::Fixed(bars) };
        self.view.home();
    }

    // ---- selection ----

    /// The head the display and selection work from — frozen while held.
    fn head(&self) -> u64 {
        self.held_at.unwrap_or_else(|| self.clock.captured())
    }

    fn viewport(&self) -> Viewport {
        Viewport::resolve(&self.view, self.clock.map(), self.head(), self.size.0)
    }

    fn unit_bars(&self, viewport: &Viewport) -> f64 {
        self.unit.bars(viewport.span_bars, f64::from(self.size.0) / self.pixel_ratio)
    }

    /// A click with no drag: the one cell under the pointer.
    pub fn click(&mut self, fraction: f64) {
        let viewport = self.viewport();
        let unit = self.unit_bars(&viewport);
        if let Some(frame) = viewport.frame_at(self.clock.map(), fraction.clamp(0.0, 1.0)) {
            self.selection = Some(grid::cell_at(self.clock.map(), unit, frame));
        }
    }

    /// A drag: start snaps down, end snaps up, minimum one cell.
    pub fn drag(&mut self, from: f64, to: f64) {
        let viewport = self.viewport();
        let unit = self.unit_bars(&viewport);
        let map = self.clock.map();
        let a = viewport.frame_at(map, from.clamp(0.0, 1.0));
        let b = viewport.frame_at(map, to.clamp(0.0, 1.0));
        if let (Some(a), Some(b)) = (a, b) {
            self.selection = Some(grid::snap_range(map, unit, a, b));
        }
    }

    /// The number row: the last `tenths` tenths of the window, ending at the head.
    pub fn select_percent(&mut self, tenths: u32) {
        let viewport = self.viewport();
        let unit = self.unit_bars(&viewport);
        self.selection = Some(grid::percent_from_head(
            self.clock.map(),
            unit,
            self.clock.captured(),
            self.view.window_bars,
            tenths,
        ));
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    // ---- hold and markers ----

    /// Freezes the picture without stopping capture.
    ///
    /// The premise of the whole tool is retrospective — *that was good, grab it* — which means
    /// reaching backwards while the writer keeps eating the tail. Sixteen bars is 32 seconds at
    /// 120 and 22 at 174, which is not long to notice, decide, select and check.
    ///
    /// It freezes the display rather than the ring, by leaving the GPU mirror unsynced and
    /// reporting the frozen head. Capture continues underneath, so nothing is lost — but the ring
    /// does keep advancing, so a selection held long enough is eventually overwritten and
    /// `selection_live` starts refusing it. The grace is the difference between the ring and the
    /// window: about twelve seconds at the defaults, and the status says when it has gone.
    pub fn hold(&mut self, on: bool) {
        self.held_at = if on { Some(self.clock.captured()) } else { None };
    }

    pub fn is_held(&self) -> bool {
        self.held_at.is_some()
    }

    /// Drops a marker at the live head — not the frozen one, since a marker records when something
    /// happened rather than where you were looking.
    pub fn mark(&mut self) {
        let at = self.clock.captured();
        self.markers.push(at);
        // Anything older than the ring can never be selected again, so keeping it would only grow
        // a list nobody can act on.
        let oldest = self.reader.oldest();
        self.markers.retain(|m| *m >= oldest);
    }

    /// Selects from the most recent marker to the head, snapped.
    ///
    /// The most-used control in the finished tool, probably: you notice a good passage *after* it
    /// has happened, and this is the shortest path from noticing to having it.
    pub fn select_from_marker(&mut self) -> bool {
        let head = self.head();
        let Some(&at) = self.markers.iter().rev().find(|m| **m < head) else { return false };
        let viewport = self.viewport();
        let unit = self.unit_bars(&viewport);
        // Bounded by the head: "everything since the mark" means the material that exists, and
        // rounding the end up would select a cell the audio has not reached yet.
        match grid::snap_range_upto(self.clock.map(), unit, at, head, head) {
            Some(selection) => {
                self.selection = Some(selection);
                true
            }
            None => false,
        }
    }

    pub fn clear_markers(&mut self) {
        self.markers.clear();
    }

    // ---- staging ----

    /// Materialises the selection as a WAV file.
    ///
    /// `time_reference` is samples since midnight, for the Broadcast Wave timestamp — the field a
    /// host reads to spot a file back to where it was captured. It comes from the caller because
    /// the core has no clock of its own and should not grow one.
    ///
    /// Returns an empty vector when the selection has been overwritten, which is the only honest
    /// answer: a partly-lapped range reads back as a seam of old and new audio that looks
    /// completely plausible and is not what anybody selected.
    pub fn stage(&mut self, time_reference: f64) -> Vec<u8> {
        let Some(selection) = self.selection else { return Vec::new() };
        if !self.reader.holds(selection.start, selection.end) {
            return Vec::new();
        }
        let frames = selection.frames() as usize;
        if frames == 0 {
            return Vec::new();
        }

        let mut planes: Vec<Vec<f32>> = Vec::with_capacity(self.channels);
        for channel in 0..self.channels {
            let mut plane = vec![0.0f32; frames];
            if !self.reader.read_into(channel, selection.start, &mut plane) {
                // Lapped between the check above and here. Refusing is the whole point.
                return Vec::new();
            }
            planes.push(plane);
        }
        let views: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();

        let map = self.clock.map();
        let quarters = map.quarters_at(selection.end) - map.quarters_at(selection.start);
        let meta = WavMeta {
            // The chunk that makes a host warp the drop to its own session instead of treating it
            // as a one-shot. Its tempo and beat count must agree with the audio's actual length,
            // so both are taken from the map rather than from the transport's current reading.
            acid: Some(Acid {
                tempo: map.bpm_at(selection.start) as f32,
                quarters: quarters.round().max(1.0) as u32,
                meter: map.meter_at(selection.start),
                root: None,
            }),
            bext: Some(Bext {
                description: "Waveroll capture".into(),
                originator: "Waveroll".into(),
                time_reference: time_reference.max(0.0) as u64,
                coding_history: format!(
                    "A=PCM,F={},W=32,M={}\r\n",
                    self.producer_rate(),
                    if self.channels > 1 { "stereo" } else { "mono" }
                ),
                ..Bext::default()
            }),
        };
        let spec = WavSpec::new(self.producer_rate(), Depth::F32);
        wav::write(&views, &spec, &meta)
    }

    fn producer_rate(&self) -> u32 {
        self.reader.sample_rate()
    }

    /// A filename carrying what you will want to know about this file in six months.
    pub fn stage_name(&self) -> String {
        let Some(selection) = self.selection else { return "waveroll.wav".into() };
        let map = self.clock.map();
        let bars = map.bars_at(selection.end) - map.bars_at(selection.start);
        format!(
            "waveroll_{:.0}bpm_{}bars.wav",
            map.bpm_at(selection.start),
            format_bars(bars)
        )
    }

    /// Re-phases the bar lines so the current head is a downbeat.
    ///
    /// The only way bar one is ever established with Live, which sends Start from wherever the
    /// playhead was and never sends Song Position Pointer.
    pub fn set_downbeat_now(&mut self) {
        let at = self.clock.captured();
        self.clock.map_mut().set_downbeat(at);
    }

    /// Whether the current selection is still inside the ring. A stale one must refuse to export
    /// rather than hand over a seam of old and new audio that looks perfectly plausible.
    pub fn selection_live(&self) -> bool {
        self.selection.is_some_and(|s| self.reader.holds(s.start, s.end))
    }

    /// Why a selection cannot be staged, when it cannot.
    ///
    /// `pending` and `overwritten` are opposite problems — not recorded yet, and already gone —
    /// and one message for both sends people looking in the wrong direction. A selection dragged
    /// across the write head is *pending*: its last cell rounds up past audio that has not
    /// happened, and it becomes ready on its own a fraction of a bar later.
    pub fn selection_state(&self) -> String {
        let Some(selection) = self.selection else { return "empty".into() };
        if selection.end > self.reader.head() {
            return "pending".into();
        }
        if !self.reader.holds(selection.start, selection.end) {
            return "overwritten".into();
        }
        "ready".into()
    }

    /// Whether the selection is still on screen.
    ///
    /// Distinct from [`Waveroll::selection_live`], and the difference matters: the ring is
    /// deliberately longer than the window so that widening the window is a setting rather than a
    /// restart, which means audio can be *in the ring but past the left edge of the display*. A
    /// selection there is exportable and invisible at the same time, and a selection you cannot
    /// see is one you cannot check before dropping it into somebody's session.
    pub fn selection_in_view(&self) -> bool {
        let Some(selection) = self.selection else { return false };
        let viewport = self.viewport();
        let map = self.clock.map();
        let a = viewport.fraction_at(map.bars_at(selection.start));
        let b = viewport.fraction_at(map.bars_at(selection.end));
        b > 0.0 && a < 1.0
    }

    // ---- painting ----

    pub fn frame(&mut self) -> Result<(), JsValue> {
        // Held: the mirror is left as it was, so the picture is exactly what it was at the moment
        // hold was pressed. Capture carries on into the ring underneath.
        if self.held_at.is_none() {
            self.mirror.sync(&self.gpu, &self.reader);
        }
        let viewport = self.viewport();
        let unit = self.unit_bars(&viewport);
        let columns = self.envelope.dispatch(
            &self.gpu,
            &self.mirror,
            &viewport,
            self.clock.map(),
            [1.0, 0.0],
        );

        let surface = match self.surface.get_current_texture() {
            Ok(surface) => surface,
            // A lost or outdated surface is normal — a resize, a tab restored from the background.
            // Reconfiguring and skipping this frame is the whole recovery.
            Err(_) => {
                self.resize_force();
                return Ok(());
            }
        };
        let view = surface.texture.create_view(&wgpu::TextureViewDescriptor::default());

        self.waveform.draw(
            &self.gpu,
            &view,
            self.size,
            self.envelope.output(),
            columns,
            &self.style,
        );

        grid::rulings(&viewport, unit, &mut self.rulings);
        self.overlay.begin(self.size);
        self.overlay.grid(&self.rulings, &self.overlay_style);
        if let Some(selection) = self.selection {
            let map = self.clock.map();
            self.overlay.selection(
                viewport.fraction_at(map.bars_at(selection.start)),
                viewport.fraction_at(map.bars_at(selection.end)),
                &self.overlay_style,
            );
        }
        for marker in &self.markers {
            let fraction = viewport.fraction_at(self.clock.map().bars_at(*marker));
            if (0.0..=1.0).contains(&fraction) {
                self.overlay.vline(fraction, 1.0, [0.42, 0.78, 1.0, 0.8]);
            }
        }
        self.overlay.head(viewport.head_fraction(), &self.overlay_style);
        self.overlay_pass.draw(&self.gpu, &view, self.size, &self.overlay);

        surface.present();

        self.status = Status {
            bpm: self.transport.bpm,
            playing: self.clock.is_playing(),
            lap: viewport.lap,
            head: viewport.head_fraction(),
            unit,
            window_bars: self.view.window_bars,
            zoom: self.view.zoom,
            captured: self.clock.captured(),
            selection: self.selection.map(|s| {
                let map = self.clock.map();
                let (a, b) = (map.bars_at(s.start), map.bars_at(s.end));
                (b - a, viewport.fraction_at(a), viewport.fraction_at(b))
            }),
            in_view: self.selection_in_view(),
            lapped: self.reader.laps(),
            state: self.selection_state(),
            held: self.held_at.is_some(),
            markers: self.markers.len() as u32,
        };
        Ok(())
    }

    fn resize_force(&mut self) {
        let (w, h) = self.size;
        let dpr = self.pixel_ratio;
        self.size = (0, 0);
        self.resize(w, h, dpr);
    }

    /// Everything the page needs for its own chrome, as JSON.
    pub fn status(&self) -> String {
        let selection = match self.status.selection {
            Some((bars, a, b)) => format!("{{\"bars\":{bars:.4},\"from\":{a:.4},\"to\":{b:.4}}}"),
            None => "null".into(),
        };
        format!(
            "{{\"bpm\":{:.4},\"playing\":{},\"lap\":{},\"head\":{:.6},\"unit\":{:.6},\
             \"windowBars\":{},\"zoom\":{:.4},\"captured\":{},\"selection\":{selection},\
             \"lapped\":{},\"state\":\"{}\",\"selectionInView\":{},\"held\":{},\"markers\":{}}}",
            self.status.bpm,
            self.status.playing,
            self.status.lap,
            self.status.head,
            self.status.unit,
            self.status.window_bars,
            self.status.zoom,
            self.status.captured,
            self.status.lapped,
            self.status.state,
            self.status.in_view,
            self.status.held,
            self.status.markers,
        )
    }

    pub fn adapter(&self) -> String {
        self.gpu.describe()
    }
}

// Kept out of the exported impl: wasm-bindgen would try to expose it.
impl Waveroll {
    #[allow(dead_code)]
    fn shared(self) -> Rc<RefCell<Waveroll>> {
        Rc::new(RefCell::new(self))
    }
}
