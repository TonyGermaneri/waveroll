//! Everything drawn over the trace: the grid, the selection, the write head.

use waveroll_core::grid::{Rule, Ruling};

use crate::device::Gpu;
use crate::render::{OverlayStyle, Target};

/// A list of rectangles in pixel space, built each frame and uploaded in one go.
#[derive(Default)]
pub struct Overlay {
    rects: Vec<f32>,
    count: u32,
    width: f32,
    height: f32,
}

impl Overlay {
    pub fn begin(&mut self, target: &Target) {
        self.rects.clear();
        self.count = 0;
        self.width = target.width as f32;
        self.height = target.height as f32;
    }

    pub fn len(&self) -> u32 {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, colour: [f32; 4]) {
        self.rects.extend_from_slice(&[x0, y0, x1, y1]);
        self.rects.extend_from_slice(&colour);
        self.count += 1;
    }

    /// A vertical line at a canvas fraction, occupying whole pixel columns.
    ///
    /// Snapped for the same reason the thin horizontal bars are: a line from x = 40.5 to 41.5 is
    /// evaluated at the centres of columns 40 and 41, which are its own two edges, and vanishes.
    /// Snapping also guarantees a bar line and a selection edge at the same musical position land
    /// on the same pixel rather than one apart.
    pub fn vline(&mut self, fraction: f64, width_px: f32, colour: [f32; 4]) {
        let x0 = (fraction * f64::from(self.width)).floor() as f32;
        let x0 = x0.clamp(0.0, self.width - 1.0);
        let x1 = (x0 + width_px.max(1.0)).min(self.width);
        self.rect(x0, 0.0, x1, self.height, colour);
    }

    /// The grid. Drawn coarsest last so a bar line is never buried under the cell line on top of it.
    pub fn grid(&mut self, rulings: &[Ruling], style: &OverlayStyle) {
        for pass in [Rule::Cell, Rule::Bar, Rule::Lap] {
            for ruling in rulings.iter().filter(|r| r.rule == pass) {
                let (colour, width) = match pass {
                    Rule::Cell => (style.cell, 1.0),
                    Rule::Bar => (style.bar, 1.0),
                    Rule::Lap => (style.lap, 2.0),
                };
                self.vline(ruling.fraction, width, colour);
            }
        }
    }

    /// The selection: a translucent fill with a solid edge at each end.
    ///
    /// The edges are drawn even when the fill is one pixel wide, because a selection you cannot
    /// see is indistinguishable from no selection, and the smallest one the grid allows is exactly
    /// the case where that matters.
    pub fn selection(&mut self, start: f64, end: f64, style: &OverlayStyle) {
        let (start, end) = if start <= end { (start, end) } else { (end, start) };
        let x0 = (start * f64::from(self.width)).floor().clamp(0.0, f64::from(self.width)) as f32;
        let x1 = (end * f64::from(self.width)).ceil().clamp(0.0, f64::from(self.width)) as f32;
        let x1 = x1.max(x0 + 1.0);
        self.rect(x0, 0.0, x1, self.height, style.selection_fill);
        self.rect(x0, 0.0, x0 + 1.0, self.height, style.selection_edge);
        self.rect(x1 - 1.0, 0.0, x1, self.height, style.selection_edge);
    }

    /// The write head. Last, so nothing covers it.
    pub fn head(&mut self, fraction: f64, style: &OverlayStyle) {
        self.vline(fraction, 1.0, style.head);
    }
}

/// Draws an [`Overlay`].
pub struct OverlayPass {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
    rects: wgpu::Buffer,
    capacity: u32,
}

impl OverlayPass {
    pub fn new(gpu: &Gpu, format: wgpu::TextureFormat, max_rects: u32) -> OverlayPass {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/overlay.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("overlay"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("overlay"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        OverlayPass {
            pipeline: device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("overlay"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            }),
            layout,
            uniform: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("overlay-style"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            rects: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("overlay-rects"),
                size: u64::from(max_rects) * 32,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            capacity: max_rects,
        }
    }

    /// Draws over whatever is already in the target — the trace, which is why this loads rather
    /// than clears.
    pub fn draw(&self, gpu: &Gpu, target: &Target, overlay: &Overlay) {
        let count = overlay.count.min(self.capacity);
        if count == 0 {
            return;
        }
        let mut bytes = Vec::with_capacity(16);
        for f in [
            target.width as f32,
            target.height as f32,
            1.0 / target.width as f32,
            1.0 / target.height as f32,
        ] {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        gpu.queue.write_buffer(&self.uniform, 0, &bytes);

        let mut packed = Vec::with_capacity(overlay.rects.len() * 4);
        for f in &overlay.rects[..(count as usize) * 8] {
            packed.extend_from_slice(&f.to_le_bytes());
        }
        gpu.queue.write_buffer(&self.rects, 0, &packed);

        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("overlay"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: self.rects.as_entire_binding() },
            ],
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("overlay") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..6, 0..count);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}
