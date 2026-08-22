//! The ring on the GPU, and the per-column reduction that turns it into a waveform.

use waveroll_core::ring::Reader;
use waveroll_core::tempo::TempoMap;
use waveroll_core::view::{Column, Viewport};

use crate::device::Gpu;

const WORKGROUP: u32 = 64;

fn floats_to_bytes(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// A copy of the audio ring in GPU memory, kept up to date incrementally.
///
/// Re-uploading the whole ring every frame would be 67 MB at sixty hertz, which is four gigabytes
/// a second of bus traffic to redraw audio that has not changed. Only what has been captured since
/// the last call is copied — a few hundred frames at a time — and the mirror wraps exactly as the
/// ring does, so the shader's masked index is valid against either.
pub struct RingMirror {
    buffer: wgpu::Buffer,
    capacity: usize,
    channels: usize,
    cursor: u64,
    scratch: Vec<f32>,
}

impl RingMirror {
    pub fn new(gpu: &Gpu, capacity: usize, channels: usize) -> RingMirror {
        assert!(capacity.is_power_of_two(), "the mirror inherits the ring's power-of-two capacity");
        RingMirror {
            buffer: gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ring-mirror"),
                size: (capacity * channels * 4) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            capacity,
            channels,
            cursor: 0,
            scratch: Vec::new(),
        }
    }

    pub fn buffer(&self) -> &wgpu::Buffer {
        &self.buffer
    }
    pub fn capacity(&self) -> usize {
        self.capacity
    }
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Copies everything captured since the last call.
    ///
    /// A consumer that has been lapped cannot recover what it missed, so it jumps to the oldest
    /// frame still present rather than trying — for a picture, dropping old audio beats stalling
    /// the thread that produced it.
    pub fn sync(&mut self, gpu: &Gpu, reader: &Reader) {
        let head = reader.head();
        let behind = head.saturating_sub(self.cursor);
        if behind == 0 {
            return;
        }
        if behind > self.capacity as u64 {
            self.cursor = head - self.capacity as u64;
        }
        let mut from = self.cursor;
        while from < head {
            let offset = (from as usize) & (self.capacity - 1);
            // Stop at the wrap so every upload is contiguous in the mirror as well as the ring.
            let run = ((head - from) as usize).min(self.capacity - offset);
            self.scratch.resize(run, 0.0);
            for c in 0..self.channels {
                if !reader.read_into(c, from, &mut self.scratch) {
                    // The writer overtook us mid-copy. Whatever was already sent is stale rather
                    // than wrong, and the next sync starts from the oldest surviving frame.
                    self.cursor = reader.oldest();
                    return;
                }
                let at = ((c * self.capacity + offset) * 4) as u64;
                gpu.queue.write_buffer(&self.buffer, at, &floats_to_bytes(&self.scratch));
            }
            from += run as u64;
        }
        self.cursor = head;
    }
}

/// One column of the reduced waveform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Envelope {
    pub min: f32,
    pub max: f32,
    pub rms: f32,
    /// Zero where nothing has been captured. Distinct from silence, and drawn differently.
    pub written: bool,
}

/// The reduction pass.
pub struct EnvelopePass {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    params: wgpu::Buffer,
    columns: wgpu::Buffer,
    output: wgpu::Buffer,
    readback: wgpu::Buffer,
    capacity_columns: u32,
    packed: Vec<u8>,
}

impl EnvelopePass {
    pub fn new(gpu: &Gpu, max_columns: u32) -> EnvelopePass {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("envelope"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/envelope.wgsl").into()),
        });
        let entry = |binding: u32, ty: wgpu::BindingType| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty,
            count: None,
        };
        let storage = |read_only: bool| wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("envelope"),
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(1, storage(true)),
                entry(2, storage(true)),
                entry(3, storage(false)),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("envelope"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.max(16),
                usage,
                mapped_at_creation: false,
            })
        };
        let storage_dst = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        EnvelopePass {
            pipeline: device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("envelope"),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                cache: None,
            }),
            layout,
            params: buffer("envelope-params", 32, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST),
            columns: buffer("envelope-columns", u64::from(max_columns) * 8, storage_dst),
            output: buffer(
                "envelope-out",
                u64::from(max_columns) * 16,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            ),
            readback: buffer(
                "envelope-readback",
                u64::from(max_columns) * 16,
                wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            ),
            capacity_columns: max_columns,
            packed: Vec::new(),
        }
    }

    /// Reduces the mirror into one value per column and reads the result back.
    ///
    /// Reading back is for tests and for anything the CPU has to answer questions about. The render
    /// path leaves the result on the GPU and draws from it directly — a readback there would stall
    /// the pipeline once per frame for data the CPU never looks at.
    pub fn reduce(
        &mut self,
        gpu: &Gpu,
        mirror: &RingMirror,
        viewport: &Viewport,
        map: &TempoMap,
        mix: [f32; 2],
    ) -> Vec<Envelope> {
        let mut columns = Vec::new();
        viewport.columns(map, &mut columns);
        let count = (columns.len() as u32).min(self.capacity_columns);
        self.upload_columns(gpu, &columns[..count as usize], mirror.capacity());

        let mut params = Vec::with_capacity(32);
        for v in [count, mirror.capacity() as u32, mirror.channels() as u32, 0] {
            params.extend_from_slice(&v.to_le_bytes());
        }
        for v in [mix[0], mix[1], 0.0, 0.0] {
            params.extend_from_slice(&v.to_le_bytes());
        }
        gpu.queue.write_buffer(&self.params, 0, &params);

        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("envelope"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: mirror.buffer().as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: self.columns.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: self.output.as_entire_binding() },
            ],
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("envelope") });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("envelope"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            // One workgroup per column, not one thread: each column reduces its own span.
            pass.dispatch_workgroups(count, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&self.output, 0, &self.readback, 0, u64::from(count) * 16);
        gpu.queue.submit([encoder.finish()]);

        let slice = self.readback.slice(..u64::from(count) * 16);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        gpu.device.poll(wgpu::PollType::Wait).expect("the device accepted a blocking poll");
        rx.recv().expect("map_async always reports").expect("the readback buffer mapped");

        let data = slice.get_mapped_range();
        let out = data
            .chunks_exact(16)
            .map(|c| {
                let f = |i: usize| f32::from_le_bytes(c[i..i + 4].try_into().expect("four bytes"));
                Envelope { min: f(0), max: f(4), rms: f(8), written: f(12) > 0.5 }
            })
            .collect();
        drop(data);
        self.readback.unmap();
        out
    }

    fn upload_columns(&mut self, gpu: &Gpu, columns: &[Column], capacity: usize) {
        self.packed.clear();
        self.packed.reserve(columns.len() * 8);
        let mask = (capacity - 1) as u64;
        for column in columns {
            // Masked here rather than in the shader: WGSL has no 64-bit integer, and the ring's
            // absolute counter deliberately does not fit in 32 bits.
            self.packed.extend_from_slice(&((column.start & mask) as u32).to_le_bytes());
            self.packed.extend_from_slice(&column.count.to_le_bytes());
        }
        gpu.queue.write_buffer(&self.columns, 0, &self.packed);
    }

    /// The reduced columns, left on the GPU for the render pass.
    pub fn output(&self) -> &wgpu::Buffer {
        &self.output
    }

    pub fn workgroup_size() -> u32 {
        WORKGROUP
    }
}
