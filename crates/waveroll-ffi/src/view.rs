//! The plugin editor's picture.
//!
//! The host owns the window, so the surface is built from a native view handed over from C++ and
//! wgpu draws into it directly. That is the same renderer, the same shaders and the same reduction
//! the browser build uses — the only difference is where the surface came from, which is exactly
//! the property `wgpu` was chosen for.
//!
//! Rendering to a texture and blitting the pixels into a `juce::Image` would have been easier and
//! is what a lot of plugins do. It also means a full-frame readback every paint, which is a
//! megabyte and a pipeline stall for a picture the CPU never looks at.

use std::ffi::c_void;
use std::ptr::NonNull;

use raw_window_handle::{
    AppKitDisplayHandle, AppKitWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use waveroll_core::grid::{self, Ruling};
use waveroll_core::tempo::TempoMap;
use waveroll_core::view::Viewport;
use waveroll_gpu::device::{Gpu, block_on};
use waveroll_gpu::envelope::{EnvelopePass, RingMirror};
use waveroll_gpu::overlay::{Overlay, OverlayPass};
use waveroll_gpu::render::{OverlayStyle, Style};
use waveroll_gpu::wgpu;

/// Everything one paint needs, gathered rather than passed one argument at a time.
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    pub reader: &'a waveroll_core::ring::Reader,
    pub map: &'a TempoMap,
    pub viewport: &'a Viewport,
    pub unit_bars: f64,
    pub selection: Option<(f64, f64)>,
    pub markers: &'a [f64],
    /// Held: leave the mirror unsynced so the picture is exactly what it was.
    pub held: bool,
}

pub struct View {
    gpu: Gpu,
    surface: wgpu::Surface<'static>,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    scale: f64,
    mirror: RingMirror,
    envelope: EnvelopePass,
    waveform: waveroll_gpu::render::WaveformPass,
    overlay_pass: OverlayPass,
    overlay: Overlay,
    rulings: Vec<Ruling>,
    style: Style,
    overlay_style: OverlayStyle,
}

impl View {
    /// # Safety
    /// `native_view` must be a valid `NSView*` that outlives this object.
    pub unsafe fn open(
        native_view: *mut c_void,
        width: u32,
        height: u32,
        scale: f64,
        capacity: usize,
        channels: usize,
    ) -> Result<View, String> {
        let Some(handle) = NonNull::new(native_view) else {
            return Err("null view".into());
        };
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: RawDisplayHandle::AppKit(AppKitDisplayHandle::new()),
            raw_window_handle: RawWindowHandle::AppKit(AppKitWindowHandle::new(handle)),
        };
        // Safety is the caller's: the view must outlive the surface, which the editor guarantees by
        // closing this before dropping the component.
        let surface = unsafe { instance.create_surface_unsafe(target) }
            .map_err(|e| format!("no surface: {e}"))?;
        let gpu = block_on(Gpu::open(&instance, Some(&surface)))?;

        let caps = surface.get_capabilities(&gpu.adapter);
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);

        let mut view = View {
            mirror: RingMirror::new(&gpu, capacity, channels),
            envelope: EnvelopePass::new(&gpu, 4096),
            waveform: waveroll_gpu::render::WaveformPass::new(&gpu, format),
            overlay_pass: OverlayPass::new(&gpu, format, 8192),
            overlay: Overlay::default(),
            rulings: Vec::new(),
            style: Style::default(),
            overlay_style: OverlayStyle::default(),
            gpu,
            surface,
            format,
            size: (0, 0),
            scale: scale.max(0.5),
        };
        view.resize(width, height, scale);
        Ok(view)
    }

    pub fn resize(&mut self, width: u32, height: u32, scale: f64) {
        // Clamped against what the device will actually accept. A window can be dragged bigger
        // than any texture limit, and configuring a surface past one is a validation error on a
        // path with nowhere to report it -- which is how this took a host down.
        //
        // The scale comes down with it rather than the aspect changing: a very large editor draws
        // slightly softer, which nobody notices, instead of drawing the wrong shape, which
        // everybody does.
        let max = self.gpu.max_surface().max(256);
        let (mut width, mut height) = (width.max(1), height.max(1));
        let mut scale = scale.max(0.5);
        if width > max || height > max {
            let shrink = f64::from(max) / f64::from(width.max(height));
            width = ((f64::from(width) * shrink) as u32).max(1);
            height = ((f64::from(height) * shrink) as u32).max(1);
            scale *= shrink;
        }
        let size = (width, height);
        self.scale = scale;
        if self.size == size {
            return;
        }
        self.size = size;
        self.surface.configure(
            &self.gpu.device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: self.format,
                width: size.0,
                height: size.1,
                // Fifo: an editor has no business spinning the GPU faster than the display, and a
                // plugin that did would be stealing cycles from the audio thread that pays for it.
                present_mode: wgpu::PresentMode::Fifo,
                desired_maximum_frame_latency: 2,
                alpha_mode: wgpu::CompositeAlphaMode::Auto,
                view_formats: vec![],
            },
        );
    }

    /// Points rather than pixels — what the grid's auto unit reasons in.
    pub fn logical_width(&self) -> f64 {
        f64::from(self.size.0) / self.scale
    }

    pub fn draw(&mut self, frame: &Frame<'_>) {
        let Frame { reader, map, viewport, unit_bars, selection, markers, held } = *frame;
        if !held {
            self.mirror.sync(&self.gpu, reader);
        }
        let columns = self.envelope.dispatch(&self.gpu, &self.mirror, viewport, map, [1.0, 0.0]);

        let Ok(frame) = self.surface.get_current_texture() else {
            // Lost or outdated: a resize, or the window coming back from behind another. Reconfigure
            // and skip this paint rather than treating it as a failure.
            let (w, h) = self.size;
            self.size = (0, 0);
            self.resize(w, h, self.scale);
            return;
        };
        let target = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        self.waveform.draw(
            &self.gpu,
            &target,
            self.size,
            self.envelope.output(),
            columns,
            &self.style,
        );

        grid::rulings(viewport, unit_bars, &mut self.rulings);
        self.overlay.begin(self.size);
        self.overlay.grid(&self.rulings, &self.overlay_style);
        if let Some((from, to)) = selection {
            self.overlay.selection(from, to, &self.overlay_style);
        }
        for marker in markers {
            if (0.0..=1.0).contains(marker) {
                self.overlay.vline(*marker, 1.0, [0.42, 0.78, 1.0, 0.8]);
            }
        }
        self.overlay.head(viewport.head_fraction(), &self.overlay_style);
        self.overlay_pass.draw(&self.gpu, &target, self.size, &self.overlay);

        frame.present();
    }

    pub fn describe(&self) -> String {
        self.gpu.describe()
    }

    /// Validation errors since the last call. Something has to look at these.
    pub fn take_errors(&self) -> Vec<String> {
        self.gpu.take_errors()
    }
}
