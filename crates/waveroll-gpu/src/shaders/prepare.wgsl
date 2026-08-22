// Stage 1 of the analysis pipeline: window the audio and pack it for a half-length complex FFT.
//
// A length-N real transform is computed as a length-N/2 complex transform by packing
// z[j] = x[2j] + i*x[2j+1]. The window is applied *before* packing, so each complex sample
// carries its own window coefficient. Stage 3 (unpack.wgsl) splits the result back apart.
//
// Everything is batched. One dispatch prepares
//     frameCount x variantCount x channelCount
// independent transforms, where variants are the three windows time-frequency reassignment
// needs (w, t*w, dw/dn). Batching is what makes a ~190 Hz analysis rate affordable: the cost
// is dominated by dispatch count, not by arithmetic.

struct Params {
  // x: n (real window length)   y: l (= n/2, complex length)
  // z: hop (frames between analysis windows)   w: startFrame (absolute, masked to 2^30)
  dims: vec4<u32>,
  // x: frameCount  y: channelCount  z: variantCount  w: ringCapacity (power of two)
  counts: vec4<u32>,
  // x: ringChannels  y: totalThreads  z/w: unused
  misc: vec4<u32>,
  // Mixing weights that turn the ring's physical channels into logical analysis channels,
  // so L / R / Mid / Side / Mono all share one code path.
  mix0: vec4<f32>,
  mix1: vec4<f32>,
}

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> audio: array<f32>;
@group(0) @binding(2) var<storage, read> windows: array<f32>;
@group(0) @binding(3) var<storage, read_write> dst: array<vec2<f32>>;

fn fetch(channel: u32, frame: u32) -> f32 {
  let cap = P.counts.w;
  let mask = cap - 1u;
  let idx = frame & mask;
  let ringChannels = P.misc.x;

  let a = audio[idx];
  var b = a;
  if (ringChannels > 1u) {
    b = audio[cap + idx];
  }

  var mix = P.mix0;
  if (channel == 1u) {
    mix = P.mix1;
  }
  return mix.x * a + mix.y * b;
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
  let tid = gid.x;
  if (tid >= P.misc.y) {
    return;
  }

  let n = P.dims.x;
  let l = P.dims.y;
  let hop = P.dims.z;
  let startFrame = P.dims.w;

  let j = tid % l;              // complex sample index within this transform
  var b = tid / l;              // batch index
  let channelCount = P.counts.y;
  let variantCount = P.counts.z;

  let c = b % channelCount;
  b = b / channelCount;
  let v = b % variantCount;
  let f = b / variantCount;

  let base = startFrame + f * hop;
  let even = base + 2u * j;
  let odd = even + 1u;

  let wEven = windows[v * n + 2u * j];
  let wOdd = windows[v * n + 2u * j + 1u];

  let batchIndex = (f * variantCount + v) * channelCount + c;
  dst[batchIndex * l + j] = vec2<f32>(fetch(c, even) * wEven, fetch(c, odd) * wOdd);
}
