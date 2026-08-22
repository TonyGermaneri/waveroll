//! Drawing, and an offscreen target so it can be checked without a window.
//!
//! Rendering to a texture rather than a surface is not only for tests: the plugin editor's surface
//! belongs to the host, the wasm build draws into a canvas it is handed, and the standalone owns
//! its own window. A renderer that could only be built around a window would work in exactly one
//! of the three places this has to run.

use crate::device::Gpu;

/// Colours and geometry for the trace. Every value is linear, not sRGB.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub background: [f32; 4],
    pub peak: [f32; 4],
    pub rms: [f32; 4],
    /// Columns that have never been captured — the first lap, ahead of the head.
    pub unwritten: [f32; 4],
    pub gain: f32,
    /// Shortest a bar may be drawn, in pixels. Silence still has to leave a line, or a quiet
    /// passage becomes a hole in the trace rather than a quiet part of it.
    pub min_bar_px: f32,
}

impl Default for Style {
    fn default() -> Style {
        Style {
            background: [0.043, 0.055, 0.075, 1.0],
            peak: [0.35, 0.72, 0.85, 1.0],
            rms: [0.75, 0.92, 1.0, 1.0],
            unwritten: [0.18, 0.20, 0.24, 1.0],
            gain: 1.0,
            min_bar_px: 1.0,
        }
    }
}

/// An offscreen colour target, with the padding rules for reading it back.
pub struct Target {
    texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    readback: wgpu::Buffer,
    pub width: u32,
    pub height: u32,
    row_bytes: u32,
}

pub const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl Target {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Target {
        // A texture-to-buffer copy must have rows that are a multiple of 256 bytes. Rounding up
        // and stepping over the slack on the way out is the whole of the dance.
        let row_bytes = (width * 4).div_ceil(256) * 256;
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TARGET_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("target-readback"),
            size: u64::from(row_bytes) * u64::from(height),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Target { texture, view, readback, width, height, row_bytes }
    }

    /// Reads the target back as tightly packed RGBA8 rows.
    pub fn read(&self, gpu: &Gpu) -> Vec<[u8; 4]> {
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.row_bytes),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([encoder.finish()]);

        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        gpu.device.poll(wgpu::PollType::Wait).expect("the device accepted a blocking poll");
        rx.recv().expect("map_async always reports").expect("the readback buffer mapped");

        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity((self.width * self.height) as usize);
        for row in 0..self.height {
            let start = (row * self.row_bytes) as usize;
            for x in 0..self.width as usize {
                let at = start + x * 4;
                out.push([data[at], data[at + 1], data[at + 2], data[at + 3]]);
            }
        }
        drop(data);
        self.readback.unmap();
        out
    }

    /// The pixel at `(x, y)`, origin top left.
    pub fn pixel(pixels: &[[u8; 4]], width: u32, x: u32, y: u32) -> [u8; 4] {
        pixels[(y * width + x) as usize]
    }
}

/// Draws the reduced waveform.
pub struct WaveformPass {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
}

impl WaveformPass {
    pub fn new(gpu: &Gpu, format: wgpu::TextureFormat) -> WaveformPass {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("waveform"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/waveform.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("waveform"),
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
            label: Some("waveform"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        WaveformPass {
            pipeline: device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("waveform"),
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
                label: Some("waveform-style"),
                size: 96,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
        }
    }

    /// Clears the target and draws `columns` columns from `envelope`.
    ///
    /// Peak first, RMS over it: with no depth test the later instance range wins, which is exactly
    /// the layering wanted and costs nothing to arrange.
    pub fn draw(
        &self,
        gpu: &Gpu,
        target: &Target,
        envelope: &wgpu::Buffer,
        columns: u32,
        style: &Style,
    ) {
        let mut bytes = Vec::with_capacity(96);
        let mut push = |v: [f32; 4]| {
            for f in v {
                bytes.extend_from_slice(&f.to_le_bytes());
            }
        };
        push([
            target.width as f32,
            target.height as f32,
            1.0 / target.width as f32,
            1.0 / target.height as f32,
        ]);
        push(style.peak);
        push(style.rms);
        push(style.unwritten);
        push([style.gain, columns as f32, style.min_bar_px, 0.0]);
        gpu.queue.write_buffer(&self.uniform, 0, &bytes);

        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("waveform"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: envelope.as_entire_binding() },
            ],
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("draw") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("waveform"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(style.background[0]),
                            g: f64::from(style.background[1]),
                            b: f64::from(style.background[2]),
                            a: f64::from(style.background[3]),
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..6, 0..columns * 2);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}
