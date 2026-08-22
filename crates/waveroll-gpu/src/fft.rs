//! The analysis chain: window and pack, Stockham FFT, unpack to a real spectrum.
//!
//! Three dispatches' worth of shader, vendored from waveshape unchanged, driven from Rust instead
//! of TypeScript. The shape of the chain is explained in the shaders themselves; what lives here is
//! the buffer allocation, the twiddle table, and the stage schedule.
//!
//! This runs one frame of one channel with one window variant — the configuration the self-test
//! needs. The shaders are already batched over frames, channels and reassignment variants; wiring
//! the wider batch up is the next step, and nothing here has to change shape to allow it.

use crate::device::Gpu;

/// Dynamic uniform offsets must be a multiple of the device's alignment, which is 256 everywhere
/// that matters. One stage's parameters are 32 bytes; the rest is padding to buy the offset.
const STAGE_STRIDE: u64 = 256;
/// log4 of the largest transform, plus one for the odd-power radix-2 stage.
const MAX_STAGES: u64 = 20;
const WORKGROUP: u32 = 64;

/// Little-endian byte pusher, so the crate needs no casting dependency.
#[derive(Default)]
struct Words(Vec<u8>);

impl Words {
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f32(&mut self, v: f32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn pad_to(&mut self, len: usize) {
        self.0.resize(len, 0);
    }
}

fn floats_to_bytes(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

pub struct Analyzer {
    size: usize,
    prepare: wgpu::ComputePipeline,
    radix2: wgpu::ComputePipeline,
    radix4: wgpu::ComputePipeline,
    unpack: wgpu::ComputePipeline,
    prepare_layout: wgpu::BindGroupLayout,
    fft_layout: wgpu::BindGroupLayout,
    unpack_layout: wgpu::BindGroupLayout,
    audio: wgpu::Buffer,
    windows: wgpu::Buffer,
    twiddle: wgpu::Buffer,
    fft_a: wgpu::Buffer,
    fft_b: wgpu::Buffer,
    bins: wgpu::Buffer,
    readback: wgpu::Buffer,
    prepare_uniform: wgpu::Buffer,
    fft_uniform: wgpu::Buffer,
    unpack_uniform: wgpu::Buffer,
}

impl Analyzer {
    /// `size` is the real transform length N, a power of two.
    pub fn new(gpu: &Gpu, size: usize) -> Analyzer {
        assert!(size.is_power_of_two() && size >= 8, "fft size must be a power of two, got {size}");
        let device = &gpu.device;
        let l = size / 2;

        let module = |label: &str, source: &str| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            })
        };
        let prepare_module = module("prepare", include_str!("shaders/prepare.wgsl"));
        let fft_module = module("fft", include_str!("shaders/fft.wgsl"));
        let unpack_module = module("unpack", include_str!("shaders/unpack.wgsl"));

        // Every one of the three shaders binds the same four slots in the same order: a uniform,
        // two read-only storage buffers and one read-write. Only the FFT's uniform is addressed
        // with a dynamic offset, because it is the only one dispatched more than once per frame.
        let layout = |label: &str, dynamic: bool, rw_slot: u32| {
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
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some(label),
                entries: &[
                    entry(
                        0,
                        wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: dynamic,
                            min_binding_size: None,
                        },
                    ),
                    entry(1, storage(rw_slot != 1)),
                    entry(2, storage(rw_slot != 2)),
                    entry(3, storage(rw_slot != 3)),
                ],
            })
        };
        let prepare_layout = layout("prepare", false, 3);
        let fft_layout = layout("fft", true, 2);
        let unpack_layout = layout("unpack", false, 2);

        let pipeline = |label: &str, bgl: &wgpu::BindGroupLayout, m: &wgpu::ShaderModule, entry: &str| {
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[bgl],
                push_constant_ranges: &[],
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&pl),
                module: m,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };

        let buffer = |label: &str, bytes: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: bytes.max(16),
                usage,
                mapped_at_creation: false,
            })
        };
        let storage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let uniform = wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST;
        let complex = (l as u64) * 8;
        let bins_bytes = (l as u64 + 1) * 8;

        let analyzer = Analyzer {
            size,
            prepare: pipeline("prepare", &prepare_layout, &prepare_module, "main"),
            radix2: pipeline("radix2", &fft_layout, &fft_module, "radix2"),
            radix4: pipeline("radix4", &fft_layout, &fft_module, "radix4"),
            unpack: pipeline("unpack", &unpack_layout, &unpack_module, "main"),
            prepare_layout,
            fft_layout,
            unpack_layout,
            audio: buffer("audio", size as u64 * 4, storage),
            windows: buffer("windows", size as u64 * 4, storage),
            twiddle: buffer("twiddle", size as u64 * 8, storage),
            fft_a: buffer("fft-a", complex, storage),
            fft_b: buffer("fft-b", complex, storage),
            bins: buffer("bins", bins_bytes, storage | wgpu::BufferUsages::COPY_SRC),
            readback: buffer(
                "readback",
                bins_bytes,
                wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            ),
            prepare_uniform: buffer("prepare-params", 80, uniform),
            fft_uniform: buffer("fft-params", STAGE_STRIDE * MAX_STAGES, uniform),
            unpack_uniform: buffer("unpack-params", 32, uniform),
        };
        analyzer.write_twiddles(gpu);
        analyzer
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// The twiddle table, computed in `f64` and stored as `f32`.
    ///
    /// Evaluating `sin`/`cos` in the shader is a ULP or two worse at N = 65536, and twiddle error
    /// is the dominant term in a large transform's error budget. Computing the angle in double and
    /// rounding once is free — this runs when the size changes, not per frame.
    fn write_twiddles(&self, gpu: &Gpu) {
        let n = self.size;
        let mut table = Vec::with_capacity(n * 2);
        for m in 0..n {
            let angle = -std::f64::consts::TAU * m as f64 / n as f64;
            table.push(angle.cos() as f32);
            table.push(angle.sin() as f32);
        }
        gpu.queue.write_buffer(&self.twiddle, 0, &floats_to_bytes(&table));
    }

    /// Runs the chain over one frame and reads the spectrum back.
    ///
    /// Returns `N/2 + 1` complex bins — DC through Nyquist inclusive, which is every independent
    /// value a real transform of this length has.
    pub fn spectrum(&self, gpu: &Gpu, samples: &[f32], window: &[f32]) -> Vec<(f32, f32)> {
        let n = self.size;
        let l = n / 2;
        assert_eq!(samples.len(), n, "the frame must be exactly one transform long");
        assert_eq!(window.len(), n, "the window must be exactly one transform long");

        gpu.queue.write_buffer(&self.audio, 0, &floats_to_bytes(samples));
        gpu.queue.write_buffer(&self.windows, 0, &floats_to_bytes(window));

        // prepare: one frame, one channel, one variant. `mix0` selects the ring's first plane
        // outright, which is what a mono analysis of a mono ring means.
        let mut p = Words::default();
        p.u32(n as u32).u32(l as u32).u32(n as u32).u32(0); // dims: n, l, hop, startFrame
        p.u32(1).u32(1).u32(1).u32(n as u32); // counts: frames, channels, variants, ringCapacity
        p.u32(1).u32(l as u32).u32(0).u32(0); // misc: ringChannels, totalThreads
        p.f32(1.0).f32(0.0).f32(0.0).f32(0.0); // mix0
        p.f32(1.0).f32(0.0).f32(0.0).f32(0.0); // mix1
        gpu.queue.write_buffer(&self.prepare_uniform, 0, &p.0);

        // The Stockham schedule. Radix-4 halves the dispatch count against radix-2; when log2(l)
        // is odd a single radix-2 stage runs first to make what remains even.
        struct Stage {
            radix: u32,
            threads: u32,
        }
        let mut stages: Vec<Stage> = Vec::new();
        let mut params = Vec::new();
        let mut push = |radix: u32, p_value: u32| {
            let per_transform = l as u32 / radix;
            let unit = if radix == 2 { l as u32 / p_value } else { l as u32 / (2 * p_value) };
            let mut w = Words::default();
            w.u32(l as u32).u32(p_value).u32(unit).u32(per_transform);
            w.u32(1).u32(n as u32 - 1).u32(per_transform).u32(0);
            w.pad_to(STAGE_STRIDE as usize);
            params.extend_from_slice(&w.0);
            stages.push(Stage { radix, threads: per_transform });
        };

        let mut p_value = 1u32;
        if (l.trailing_zeros() % 2) == 1 {
            push(2, 1);
            p_value = 2;
        }
        while (p_value as usize) < l {
            push(4, p_value);
            p_value *= 4;
        }
        gpu.queue.write_buffer(&self.fft_uniform, 0, &params);

        let mut u = Words::default();
        u.u32(l as u32).u32(n as u32).u32(0).u32(l as u32 + 1); // a: l, n, unused, totalThreads
        u.pad_to(32);
        gpu.queue.write_buffer(&self.unpack_uniform, 0, &u.0);

        let bind = |label: &str, bgl: &wgpu::BindGroupLayout, entries: [&wgpu::Buffer; 4], size: Option<u64>| {
            let resource = |i: usize| wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: entries[i],
                offset: 0,
                size: if i == 0 { size.and_then(std::num::NonZeroU64::new) } else { None },
            });
            gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: resource(0) },
                    wgpu::BindGroupEntry { binding: 1, resource: resource(1) },
                    wgpu::BindGroupEntry { binding: 2, resource: resource(2) },
                    wgpu::BindGroupEntry { binding: 3, resource: resource(3) },
                ],
            })
        };

        let prepare_bind = bind(
            "prepare",
            &self.prepare_layout,
            [&self.prepare_uniform, &self.audio, &self.windows, &self.fft_a],
            None,
        );
        // Two bind groups, A->B and B->A, swapped between stages: Stockham is out-of-place, and
        // ping-ponging is the whole cost of avoiding a bit-reversal scatter.
        let ab = bind(
            "fft-ab",
            &self.fft_layout,
            [&self.fft_uniform, &self.fft_a, &self.fft_b, &self.twiddle],
            Some(32),
        );
        let ba = bind(
            "fft-ba",
            &self.fft_layout,
            [&self.fft_uniform, &self.fft_b, &self.fft_a, &self.twiddle],
            Some(32),
        );

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("analyze") });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("prepare"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.prepare);
            pass.set_bind_group(0, &prepare_bind, &[]);
            pass.dispatch_workgroups((l as u32).div_ceil(WORKGROUP), 1, 1);
        }
        let mut src_is_a = true;
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("fft"),
                timestamp_writes: None,
            });
            for (i, stage) in stages.iter().enumerate() {
                pass.set_pipeline(if stage.radix == 2 { &self.radix2 } else { &self.radix4 });
                pass.set_bind_group(
                    0,
                    if src_is_a { &ab } else { &ba },
                    &[i as u32 * STAGE_STRIDE as u32],
                );
                pass.dispatch_workgroups(stage.threads.div_ceil(WORKGROUP), 1, 1);
                src_is_a = !src_is_a;
            }
        }
        // After an even number of stages the result is back in A.
        let result = if src_is_a { &self.fft_a } else { &self.fft_b };
        let unpack_bind = bind(
            "unpack",
            &self.unpack_layout,
            [&self.unpack_uniform, result, &self.bins, &self.twiddle],
            None,
        );
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("unpack"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.unpack);
            pass.set_bind_group(0, &unpack_bind, &[]);
            pass.dispatch_workgroups((l as u32 + 1).div_ceil(WORKGROUP), 1, 1);
        }
        encoder.copy_buffer_to_buffer(&self.bins, 0, &self.readback, 0, (l as u64 + 1) * 8);
        gpu.queue.submit([encoder.finish()]);

        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        gpu.device.poll(wgpu::PollType::Wait).expect("the device accepted a blocking poll");
        rx.recv().expect("map_async always reports").expect("the readback buffer mapped");

        let data = slice.get_mapped_range();
        let (pairs, _) = data.as_chunks::<8>();
        let out = pairs
            .iter()
            .map(|c| {
                (
                    f32::from_le_bytes(c[0..4].try_into().expect("four bytes")),
                    f32::from_le_bytes(c[4..8].try_into().expect("four bytes")),
                )
            })
            .collect();
        drop(data);
        self.readback.unmap();
        out
    }
}
